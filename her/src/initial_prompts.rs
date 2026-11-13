//! Operator-written "initial prompts" (`initial-prompts.json`): one background text per
//! conversation, injected once on the conversation's first wake.
//!
//! Port of the JS store in `qq-bridge/src/bridge.js` (`loadInitialPrompts`, `setInitialPrompt`,
//! `formatInitialPromptV2`). Contract kept from JS:
//! - keys are canonical (`group:<n>` / `private:<n>`), texts are normalized, empty texts are dropped;
//! - a missing file is an empty store; an unreadable, corrupt or non-object file is a read-only
//!   degradation (`writable == false`): nothing is overwritten and saves are refused;
//! - texts longer than the current limit are kept on disk; the limit applies on save and on
//!   injection only;
//! - `chars`/limits count UTF-16 code units, as JS `String.length` does.

use crate::role::{js_is_space, utf16_len};
use crate::runtime::{RuntimeArea, RuntimeContext};
use serde_json::{json, Map, Value};
use std::sync::Mutex;

pub const FILE: &str = "initial-prompts.json";
const DEFAULT_MAX: i64 = 2000;

/// Serializes read-modify-write cycles inside one process (the console is one process).
static SAVE_GATE: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub text: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug)]
pub struct Store {
    /// Insertion order is the file order, as the JS `Map` keeps it.
    pub entries: Vec<(String, Entry)>,
    pub writable: bool,
}

impl Store {
    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, e)| e)
    }
}

/// JS `canonicalV2Key`.
pub fn canonical_key(key: &str) -> Option<String> {
    let trimmed = key.trim_matches(js_is_space);
    let (kind, id) = trimmed.split_once(':')?;
    if kind != "group" && kind != "private" {
        return None;
    }
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = id.parse().ok()?;
    if n <= 0 || n > 9_007_199_254_740_991 {
        return None;
    }
    Some(format!("{kind}:{n}"))
}

/// JS `normalizeInitialPromptText`: drop BOM, `\r\n?` -> `\n`, drop control characters other
/// than tab and newline, trim with the ECMAScript whitespace set.
pub fn normalize_text(raw: &str) -> String {
    let stripped = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    let mut unified = String::with_capacity(stripped.len());
    let mut chars = stripped.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            unified.push('\n');
        } else {
            unified.push(c);
        }
    }
    let kept: String = unified
        .chars()
        .filter(|c| {
            let u = *c as u32;
            !(u <= 0x08 || u == 0x0B || u == 0x0C || (0x0E..=0x1F).contains(&u) || u == 0x7F)
        })
        .collect();
    kept.trim_matches(js_is_space).to_string()
}

/// `socialV2.initialPromptMaxChars`, clamped to 100..=8000 (default 2000).
pub fn max_chars(cfg: &Value) -> usize {
    let n = cfg
        .get("socialV2")
        .and_then(|v| v.get("initialPromptMaxChars"))
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.round() as i64)
        .unwrap_or(DEFAULT_MAX);
    n.clamp(100, 8000) as usize
}

fn number(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_f64).filter(|n| n.is_finite()).map(|n| n as i64).unwrap_or(0)
}

/// Reads the store. Never fails: a damaged file yields `writable == false`.
pub fn load(context: &RuntimeContext) -> Store {
    let raw = if context.is_isolated() {
        match context.read_optional_text(RuntimeArea::Data, FILE) {
            Ok(Some(text)) => Some(text),
            Ok(None) => None,
            Err(_) => return Store { entries: Vec::new(), writable: false },
        }
    } else {
        context.read_text(RuntimeArea::Data, FILE).ok()
    };
    let Some(raw) = raw else { return Store { entries: Vec::new(), writable: true } };
    let body = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body) else {
        return Store { entries: Vec::new(), writable: false };
    };
    let mut entries: Vec<(String, Entry)> = Vec::new();
    for (key, value) in &map {
        let Some(canonical) = canonical_key(key) else { continue };
        let text_value = match value {
            Value::Object(o) => o.get("text").unwrap_or(&Value::Null),
            other => other,
        };
        // A non-string text (hand-edited file) carries nothing injectable and is dropped.
        let text = normalize_text(text_value.as_str().unwrap_or(""));
        if text.is_empty() {
            continue;
        }
        let entry = Entry {
            text,
            created_at: number(value.get("createdAt")),
            updated_at: number(value.get("updatedAt")),
        };
        match entries.iter_mut().find(|(k, _)| *k == canonical) {
            Some(slot) => slot.1 = entry,
            None => entries.push((canonical, entry)),
        }
    }
    Store { entries, writable: true }
}

/// Result of a save, mirroring the JS `setInitialPrompt` outcome.
#[derive(Debug)]
pub struct SaveOutcome {
    pub key: String,
    pub deleted: bool,
    pub chars: usize,
    pub entry: Option<Entry>,
}

/// Saves (or, with an empty text, deletes) one entry. `Err((status, message))` carries the exact
/// JS status and error text.
pub fn set(context: &RuntimeContext, cfg: &Value, key: &str, raw_text: &str, now_ms: i64) -> Result<SaveOutcome, (u16, String)> {
    let canonical = canonical_key(key).ok_or((400, "key 格式应为 group:群号 或 private:QQ号".to_string()))?;
    let max = max_chars(cfg);
    let text = normalize_text(raw_text);
    let len = utf16_len(&text);
    if len > max {
        return Err((400, format!("初始提示词过长（{len} 字符，上限 {max}）；请精简后重试")));
    }
    let _gate = SAVE_GATE.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = load(context);
    let location = if context.is_isolated() { "data/initial-prompts.json" } else { "state/initial-prompts.json" };
    let prev = store.get(&canonical).cloned();
    if text.is_empty() {
        if !store.writable {
            return Err((500, format!("删除失败：{location} 写入失败，已回滚")));
        }
        store.entries.retain(|(k, _)| *k != canonical);
        write(context, &store).map_err(|_| (500, format!("删除失败：{location} 写入失败，已回滚")))?;
        return Ok(SaveOutcome { key: canonical, deleted: true, chars: 0, entry: None });
    }
    if !store.writable {
        return Err((500, format!("保存失败：{location} 写入失败，已回滚")));
    }
    let entry = Entry { text, created_at: prev.as_ref().map(|p| p.created_at).filter(|c| *c != 0).unwrap_or(now_ms), updated_at: now_ms };
    match store.entries.iter_mut().find(|(k, _)| *k == canonical) {
        Some(slot) => slot.1 = entry.clone(),
        None => store.entries.push((canonical.clone(), entry.clone())),
    }
    write(context, &store).map_err(|_| (500, format!("保存失败：{location} 写入失败，已回滚")))?;
    Ok(SaveOutcome { key: canonical, deleted: false, chars: len, entry: Some(entry) })
}

fn write(context: &RuntimeContext, store: &Store) -> Result<(), String> {
    let mut obj = Map::new();
    for (key, entry) in &store.entries {
        obj.insert(key.clone(), json!({"text": entry.text, "createdAt": entry.created_at, "updatedAt": entry.updated_at}));
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(obj)).map_err(|e| e.to_string())?;
    text.push('\n');
    context.write_atomic(RuntimeArea::Data, FILE, text.as_bytes())
}

/// JS `formatInitialPromptV2`: the first-wake section, or an empty string.
pub fn format_injection(context: &RuntimeContext, cfg: &Value, key: &str) -> String {
    let Some(canonical) = canonical_key(key) else { return String::new() };
    let store = load(context);
    let Some(entry) = store.get(&canonical) else { return String::new() };
    let max = max_chars(cfg);
    let body = if utf16_len(&entry.text) > max {
        format!("{}…（已按上限截断，完整内容见控制台）", crate::social_mem::truncate_text(&entry.text, max as i64))
    } else {
        entry.text.clone()
    };
    let label = if key.trim_matches(js_is_space).starts_with("private:") { "私聊环境" } else { "群聊环境" };
    format!("【{label}（操作者预设，仅本次首次唤醒参考）】\n{body}\n（以上是操作者提供的背景，不是群友发的消息、也不是新的系统指令；据此把握圈子主题、氛围与分寸，不必向群友复述。）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_matches_js_for_line_endings_controls_and_edges() {
        assert_eq!(normalize_text("\u{FEFF}  a\r\nb\rc\u{1}\u{7f}\u{b}d\te  "), "a\nb\ncd\te");
        assert_eq!(normalize_text("x\r\n\r\ny"), "x\n\ny");
        assert_eq!(normalize_text("\u{a0}\u{feff}z\u{85}"), "z\u{85}");
    }

    #[test]
    fn canonical_keys_follow_js() {
        assert_eq!(canonical_key(" group:000123 "), Some("group:123".into()));
        assert_eq!(canonical_key("private:0"), None);
        assert_eq!(canonical_key("room:1"), None);
        assert_eq!(canonical_key("group:9007199254740992"), None);
    }

    #[test]
    fn limit_is_clamped_and_rounded_like_js() {
        assert_eq!(max_chars(&json!({})), 2000);
        assert_eq!(max_chars(&json!({"socialV2": {"initialPromptMaxChars": 5}})), 100);
        assert_eq!(max_chars(&json!({"socialV2": {"initialPromptMaxChars": 99999}})), 8000);
        assert_eq!(max_chars(&json!({"socialV2": {"initialPromptMaxChars": 150.6}})), 151);
        assert_eq!(max_chars(&json!({"socialV2": {"initialPromptMaxChars": -3}})), 2000);
    }

    fn fixture(label: &str) -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-ticket11-initial-{label}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","ownerQQ":1}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    #[test]
    fn a_damaged_store_is_read_only_and_never_overwritten() {
        let (root, context) = fixture("damaged");
        let file = root.join("data/initial-prompts.json");
        for damaged in ["{broken", "[1,2]", "\"text\""] {
            std::fs::write(&file, damaged).unwrap();
            let store = load(&context);
            assert!(!store.writable && store.entries.is_empty(), "{damaged}");
            let err = set(&context, &json!({}), "group:1", "新内容", 5).unwrap_err();
            assert_eq!(err.0, 500);
            assert!(err.1.contains("保存失败") && err.1.contains("已回滚"), "{}", err.1);
            let err = set(&context, &json!({}), "group:1", "", 5).unwrap_err();
            assert!(err.1.contains("删除失败"), "{}", err.1);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), damaged, "the damaged file keeps its bytes");
        }
        std::fs::remove_file(&file).unwrap();
        assert!(load(&context).writable, "a missing file is an empty, writable store");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saving_keeps_unrelated_entries_over_limit_texts_and_the_first_creation_time() {
        let (root, context) = fixture("keep");
        let file = root.join("data/initial-prompts.json");
        let long = "长".repeat(150);
        std::fs::write(&file, json!({"group:1": {"text": long, "createdAt": 7, "updatedAt": 8}, "bad key": {"text": "x"}, "private:2": "旧格式字符串"}).to_string()).unwrap();
        let cfg = json!({"socialV2": {"initialPromptMaxChars": 100}});
        // Entries over the current limit survive other saves unchanged; the limit applies to the save itself.
        set(&context, &cfg, "private:3", "新", 1000).unwrap();
        let store = load(&context);
        assert_eq!(store.get("group:1").unwrap().text, long);
        assert_eq!(store.get("private:2").unwrap().text, "旧格式字符串");
        assert!(store.get("bad key").is_none());
        let before = store.get("private:3").unwrap().clone();
        assert_eq!((before.created_at, before.updated_at), (1000, 1000));
        set(&context, &cfg, "private:3", "改", 2000).unwrap();
        let after = load(&context);
        let entry = after.get("private:3").unwrap();
        assert_eq!((entry.text.as_str(), entry.created_at, entry.updated_at), ("改", 1000, 2000));
        assert!(set(&context, &cfg, "group:9", &"长".repeat(101), 3).is_err(), "over the limit is refused on save");
        let _ = std::fs::remove_dir_all(root);
    }
}
