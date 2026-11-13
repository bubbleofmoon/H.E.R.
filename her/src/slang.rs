//! 黑话（slang）的展示层：公开条目、已确认列表、注入块。
//!
//! 逐字移植自 `qq-bridge/src/bridge.js` 的 `publicSlangEntry` /
//! `confirmedSlangListV2` / `buildSlangContext` 与 `slang-learner.js` 的
//! `escapeLearnerText`。
//!
//! 这一层是**共享给 AI 的**，所以有两条硬规矩（注释在 JS 里写得很清楚，照抄）：
//!   1. 公开条目**不含 evidence** —— 证据里有别的群的会话名和原话。
//!   2. 送进提示词前要转义 HTML 并去掉控制字符，防止群友用黑话注入格式。

use crate::runtime::{RuntimeArea, RuntimeContext};
use serde_json::{json, Value};

/// 词库文件的默认位置（与 JS 的 `SLANG_FILE` 一致）。
pub const SLANG_FILE: &str = "C:/qqbot/qq-bridge/state/slang.json";

/// All selected-runtime slang writers use the same cross-process read/modify/write gate.
pub fn store_gate(context: &RuntimeContext) -> Result<Option<std::fs::File>, String> {
    if !context.is_isolated() {
        return Ok(None);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match context.open_locked_file(RuntimeArea::Data, "slang-store.lock") {
            Ok(file) => return Ok(Some(file)),
            Err(error)
                if error == "runtime file is already locked"
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod legacy_load_scope {
    use super::RuntimeContext;
    use std::cell::RefCell;
    use std::sync::Arc;

    std::thread_local! {
        static CONTEXTS: RefCell<Vec<Arc<RuntimeContext>>> = RefCell::new(Vec::new());
    }

    struct ScopeGuard;

    impl Drop for ScopeGuard {
        fn drop(&mut self) {
            CONTEXTS.with(|contexts| {
                let popped = contexts.borrow_mut().pop();
                debug_assert!(popped.is_some(), "legacy slang load scope stack underflow");
            });
        }
    }

    pub(super) fn current() -> Option<Arc<RuntimeContext>> {
        CONTEXTS.with(|contexts| contexts.borrow().last().cloned())
    }

    pub(super) fn with_context<T>(context: RuntimeContext, action: impl FnOnce() -> T) -> T {
        CONTEXTS.with(|contexts| contexts.borrow_mut().push(Arc::new(context)));
        let _scope = ScopeGuard;
        action()
    }
}

/// 读词库：磁盘上是数组；历史上也出现过 `{entries: [...]}`，两种都认。
pub fn load_entries() -> Vec<Value> {
    #[cfg(test)]
    if let Some(context) = legacy_load_scope::current() {
        return load_entries_for_runtime(context.as_ref());
    }
    load_entries_for_runtime(&RuntimeContext::legacy())
}

/// Read slang entries from the selected runtime. Legacy callers retain their historical path via
/// `RuntimeContext::legacy`; isolated callers can only read `data/slang.json` under their root.
pub fn load_entries_for_runtime(context: &RuntimeContext) -> Vec<Value> {
    let entries = match context
        .read_text(RuntimeArea::Data, "slang.json")
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Array(a)) => a,
        Some(other) => other
            .get("entries")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default(),
        None => Vec::new(),
    };
    normalize_loaded_entries(entries)
}

fn normalize_loaded_entries(entries: Vec<Value>) -> Vec<Value> {
    use sha2::{Digest, Sha256};
    let mut occurrences = std::collections::HashMap::<String, usize>::new();
    entries
        .into_iter()
        .map(|mut entry| {
            if entry
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                let digest = format!("{:x}", Sha256::digest(entry.to_string().as_bytes()));
                let occurrence = occurrences.entry(digest.clone()).or_default();
                let id = format!("legacy-{digest}-{occurrence}");
                *occurrence += 1;
                if let Some(object) = entry.as_object_mut() {
                    object.insert("id".to_string(), json!(id));
                }
            }
            normalize_entry(&entry)
        })
        .filter(|entry| {
            entry
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| !content.is_empty())
        })
        .collect()
}

/// Load a slang store without converting selected-root read/parse failures into an empty list.
/// Missing candidate data is a valid first-run state; a present but corrupt or malformed file is
/// an error so an extraction cannot overwrite data that needs repair. Legacy callers retain the
/// historical permissive behavior through `load_entries_for_runtime`.
pub fn load_entries_checked_for_runtime(context: &RuntimeContext) -> Result<Vec<Value>, String> {
    if !context.is_isolated() {
        return Ok(load_entries_for_runtime(context));
    }
    let Some(text) = context
        .read_optional_text(RuntimeArea::Data, "slang.json")
        .map_err(|error| format!("读取候选黑话库失败：{error}"))?
    else {
        return Ok(Vec::new());
    };
    let parsed: Value =
        serde_json::from_str(&text).map_err(|error| format!("候选黑话库 JSON 损坏：{error}"))?;
    match parsed {
        Value::Array(entries) => Ok(normalize_loaded_entries(entries)),
        Value::Object(object) => object
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .map(normalize_loaded_entries)
            .ok_or_else(|| "候选黑话库结构异常".to_string()),
        _ => Err("候选黑话库结构异常".to_string()),
    }
}

/// 写词库：磁盘上是**数组**（与 JS 一致），先写临时文件再改名。
pub fn save_entries(entries: &[Value]) -> Result<(), String> {
    save_entries_for_runtime(&RuntimeContext::legacy(), entries)
}

/// Atomically replace the selected runtime's slang data file.
pub fn save_entries_for_runtime(context: &RuntimeContext, entries: &[Value]) -> Result<(), String> {
    // The frozen store retains at most 2,000 entries, preferring confirmed entries,
    // then candidates, then rejected entries; ties use count and last update.
    // Like frozen `saveSlang`, normalize every entry (field clipping, status, id) and drop
    // entries without content before capping and serializing.
    let mut retained: Vec<Value> = entries
        .iter()
        .map(normalize_entry)
        .filter(|entry| {
            entry
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| !content.is_empty())
        })
        .collect();
    if retained.len() > 2_000 {
        let rank = |entry: &Value| match entry.get("status").and_then(Value::as_str) {
            Some("confirmed") => 2,
            Some("candidate") => 1,
            _ => 0,
        };
        retained.sort_by(|a, b| {
            rank(b)
                .cmp(&rank(a))
                .then_with(|| {
                    js_number(b.get("count"))
                        .partial_cmp(&js_number(a.get("count")))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    b.get("updatedAt")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .cmp(a.get("updatedAt").and_then(Value::as_str).unwrap_or(""))
                })
        });
        retained.truncate(2_000);
    }
    let text = serde_json::to_string_pretty(&retained).map_err(|e| e.to_string())?;
    context
        .write_atomic(RuntimeArea::Data, "slang.json", text.as_bytes())
        .map_err(|e| format!("写词库失败：{e}"))
}
/// JS 的 `escapeLearnerText`：先转义 HTML 特殊字符，再删控制字符。
pub fn escape_learner_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\u{0000}'..='\u{0008}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000E}'..='\u{001F}'
            | '\u{007F}' => {}
            _ => out.push(c),
        }
    }
    out
}

/// `[CQ:` → `[CQ：`（大小写不敏感），避免 NeBot 码在提示词里被当成真的 CQ 码。
fn defuse_cq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '['
            && chars
                .get(i + 1)
                .is_some_and(|c| c.eq_ignore_ascii_case(&'c'))
            && chars
                .get(i + 2)
                .is_some_and(|c| c.eq_ignore_ascii_case(&'q'))
            && chars.get(i + 3) == Some(&':')
        {
            out.push_str("[CQ：");
            i += 4;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn js_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

fn js_number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
        Some(Value::Bool(true)) => 1.0,
        Some(Value::Bool(false)) | Some(Value::Null) | None => 0.0,
        _ => f64::NAN,
    }
}

/// 非空字符串（JS 里的 `e.content && e.meaning`）。
fn truthy_str(v: Option<&Value>) -> bool {
    v.and_then(|x| x.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// `publicSlangEntry`：只暴露安全字段，**不含 evidence**。
/// Remove controls and truncate using JS's UTF-16 limit without splitting a surrogate pair.
fn clip_text(v: Option<&Value>, max: usize) -> String {
    let raw = match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let cleaned: String = raw
        .chars()
        .filter(|c| {
            let u = *c as u32;
            !((0x00..=0x08).contains(&u)
                || u == 0x0B
                || u == 0x0C
                || (0x0E..=0x1F).contains(&u)
                || u == 0x7F)
        })
        .collect();
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut units = 0;
    for character in cleaned.trim().chars() {
        let next = units + character.len_utf16();
        if next > max {
            if out.is_empty() && max == 1 {
                out.push(character);
            }
            break;
        }
        out.push(character);
        units = next;
    }
    out
}

/// A time-prefixed ID with a process-local sequence; a single extraction can create several
/// entries in the same millisecond, including while the console is another writer process.
fn create_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64;
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u32;
    let suffix = ((std::process::id() as u64) << 32) | u64::from(sequence);
    format!("{}-{}", to_base36(ms), to_base36(suffix))
}

fn to_base36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let chars = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(chars[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8_lossy(&out).to_string()
}

/// JS 的 `normalizeSlangEntry`：字段截断 + 状态/来源归一。
pub fn normalize_entry(raw: &Value) -> Value {
    const FIELD_MAX: usize = 300;
    const CONTENT_MAX: usize = 50;
    // 只认三个合法状态，其余（含缺失）一律 candidate —— 与 JS 的三元式一致
    let status = raw
        .get("status")
        .and_then(|v| v.as_str())
        .filter(|s| matches!(*s, "candidate" | "confirmed" | "rejected"))
        .unwrap_or("candidate");
    let sources: Vec<Value> = raw
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|s| json!(clip_text(Some(s), FIELD_MAX)))
                .filter(|s| !s.as_str().unwrap_or("").is_empty())
                .collect::<Vec<_>>()
        })
        .map(|v| {
            v.into_iter()
                .rev()
                .take(10)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        })
        .unwrap_or_default();
    let evidence: Vec<Value> = raw
        .get("evidence")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .skip(items.len().saturating_sub(20))
                .map(|item| {
                    let mut item = item.clone();
                    if let Some(object) = item.as_object_mut() {
                        let text = clip_text(object.get("text"), 80);
                        object.insert("text".to_string(), json!(text));
                    }
                    item
                })
                .collect()
        })
        .unwrap_or_default();
    let now = crate::sticker::now_iso();
    json!({
        "id": raw.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .map(|s| s.to_string()).unwrap_or_else(create_id),
        "content": clip_text(raw.get("content"), CONTENT_MAX),
        "meaning": clip_text(raw.get("meaning"), FIELD_MAX),
        "usage": clip_text(raw.get("usage"), FIELD_MAX),
        "example": clip_text(raw.get("example"), FIELD_MAX),
        "risk": clip_text(raw.get("risk"), FIELD_MAX),
        "sources": sources,
        "status": status,
        "source": if raw.get("source").and_then(|v| v.as_str()) == Some("manual") { "manual" } else { "ai" },
        "count": raw.get("count").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
        "evidence": evidence,
        "lastInferenceCount": raw.get("lastInferenceCount").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
        "createdAt": raw.get("createdAt").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or(&now),
        "updatedAt": raw.get("updatedAt").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or(&now),
    })
}
pub fn public_entry(e: &Value) -> Value {
    let status = e
        .get("status")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("candidate");
    json!({
        "id": e.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "content": e.get("content").and_then(|v| v.as_str()).unwrap_or(""),
        "meaning": e.get("meaning").and_then(|v| v.as_str()).unwrap_or(""),
        "usage": e.get("usage").and_then(|v| v.as_str()).unwrap_or(""),
        "example": e.get("example").and_then(|v| v.as_str()).unwrap_or(""),
        "risk": e.get("risk").and_then(|v| v.as_str()).unwrap_or(""),
        "status": status,
        "source": if e.get("source").and_then(|v| v.as_str()) == Some("manual") { "manual" } else { "ai" },
        "count": js_number(e.get("count")),
        "updatedAt": e.get("updatedAt").and_then(|v| v.as_str()).unwrap_or(""),
    })
}

/// `Math.max(1, Math.min(30, Number(max) || 8))`
pub fn clamp_inject_max(max: Option<&Value>) -> usize {
    let n = js_number(max);
    let n = if n == 0.0 || n.is_nan() { 8.0 } else { n };
    n.clamp(1.0, 30.0) as usize
}

/// `confirmedSlangListV2`：已确认（且内容/含义都非空）按出现次数降序取前 max 条。
pub fn confirmed_list(entries: &[Value], max: Option<&Value>) -> Vec<Value> {
    let limit = clamp_inject_max(max);
    let mut picked: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            e.get("status").and_then(|v| v.as_str()) == Some("confirmed")
                && truthy_str(e.get("content"))
                && truthy_str(e.get("meaning"))
        })
        .collect();
    // 稳定排序：并列时保持词库原顺序（JS 的 Array.sort 在 V8 里也是稳定的）
    picked.sort_by(|a, b| {
        let ac = js_number(a.get("count"));
        let bc = js_number(b.get("count"));
        bc.partial_cmp(&ac).unwrap_or(std::cmp::Ordering::Equal)
    });
    picked.into_iter().take(limit).map(public_entry).collect()
}

/// `buildSlangContext`：拼成给提示词用的一段文本。
pub fn build_context(entries: &[Value], max: Option<&Value>) -> String {
    let limit = clamp_inject_max(max);
    let mut picked: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            e.get("status").and_then(|v| v.as_str()) == Some("confirmed")
                && truthy_str(e.get("content"))
                && truthy_str(e.get("meaning"))
        })
        .collect();
    picked.sort_by(|a, b| {
        let ac = js_number(a.get("count"));
        let bc = js_number(b.get("count"));
        bc.partial_cmp(&ac).unwrap_or(std::cmp::Ordering::Equal)
    });
    picked.truncate(limit);
    if picked.is_empty() {
        return String::new();
    }
    let clean = |v: Option<&Value>| escape_learner_text(&defuse_cq(&js_string(v)));
    let lines: Vec<String> = picked
        .iter()
        .map(|e| {
            let mut line = format!("- {}：{}", clean(e.get("content")), clean(e.get("meaning")));
            let usage = clean(e.get("usage"));
            if !usage.is_empty() {
                line.push_str(&format!("（用法：{usage}）"));
            }
            let example = clean(e.get("example"));
            if !example.is_empty() {
                line.push_str(&format!("（例：{example}）"));
            }
            line
        })
        .collect();
    format!(
        "【群聊黑话表】群里已确认/常用的网络用语和梗（按出现次数排序，知道即可，不要刻意堆砌）：\n{}",
        lines.join("\n")
    )
}

/// `GET /api/socialV2/slang/query`：按关键词过滤已确认黑话。
pub fn query(entries: &[Value], key: &str, q: &str, inject_max: Option<&Value>) -> Value {
    let needle = q.trim().to_lowercase();
    let list: Vec<Value> = confirmed_list(entries, inject_max)
        .into_iter()
        .filter(|e| {
            if needle.is_empty() {
                return true;
            }
            ["content", "meaning", "usage", "example"].iter().any(|f| {
                e.get(*f)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_lowercase().contains(&needle))
                    .unwrap_or(false)
            })
        })
        .collect();
    json!({
        "ok": true,
        "key": key,
        "total": list.len(),
        "entries": list,
        "block": build_context(entries, inject_max),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_fixture(tag: &str) -> (std::path::PathBuf, RuntimeContext) {
        let manifest = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let root = manifest
            .parent()
            .expect("manifest is inside the ticket worktree")
            .join("backups/ticket04 slang fixtures 中文")
            .join(format!(
                "{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock follows UNIX epoch")
                    .as_nanos(),
            ));
        let root = crate::install_root::development_root(root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("resources")).unwrap();
        std::fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"fixture-token"}"#,
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43301","model":"http://127.0.0.1:43302","onebot":"http://127.0.0.1:43303"}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    fn entry(id: &str, content: &str, meaning: &str, status: &str, count: i64) -> Value {
        json!({
            "id": id, "content": content, "meaning": meaning, "usage": "", "example": "",
            "risk": "", "sources": [], "status": status, "source": "ai", "count": count,
            "createdAt": "t", "updatedAt": "t", "lastInferenceCount": 0,
            "evidence": [{ "key": "group:1", "sender": "某人", "text": "证据", "time": 1 }]
        })
    }

    #[test]
    fn legacy_load_wrapper_reads_only_scoped_selected_root_and_preserves_both_files() {
        let (root_a, context_a) = runtime_fixture("legacy-load-a");
        let (root_b, context_b) = runtime_fixture("legacy-load-b");
        let entries_a = vec![entry("a", "甲词", "A meaning", "confirmed", 2)];
        let entries_b = vec![entry("b", "乙词", "B meaning", "candidate", 5)];
        let bytes_a = serde_json::to_vec(&entries_a).unwrap();
        let historical_b = json!({ "entries": entries_b });
        let bytes_b = serde_json::to_vec(&historical_b).unwrap();
        context_a
            .write_atomic(RuntimeArea::Data, "slang.json", &bytes_a)
            .unwrap();
        context_b
            .write_atomic(RuntimeArea::Data, "slang.json", &bytes_b)
            .unwrap();

        assert_eq!(
            legacy_load_scope::with_context(context_a.clone(), load_entries),
            entries_a
        );
        assert_eq!(
            context_a
                .read_file(RuntimeArea::Data, "slang.json")
                .unwrap(),
            bytes_a
        );
        assert_eq!(
            context_b
                .read_file(RuntimeArea::Data, "slang.json")
                .unwrap(),
            bytes_b
        );

        assert_eq!(
            legacy_load_scope::with_context(context_b.clone(), load_entries),
            entries_b
        );
        assert_eq!(
            context_a
                .read_file(RuntimeArea::Data, "slang.json")
                .unwrap(),
            bytes_a
        );
        assert_eq!(
            context_b
                .read_file(RuntimeArea::Data, "slang.json")
                .unwrap(),
            bytes_b
        );

        std::fs::remove_dir_all(root_a).unwrap();
        std::fs::remove_dir_all(root_b).unwrap();
    }

    #[test]
    fn public_entry_never_leaks_evidence() {
        let e = entry("a", "集美", "姐妹", "confirmed", 2);
        let p = public_entry(&e);
        assert!(
            p.get("evidence").is_none(),
            "证据里有别的群的会话名，绝不能给 AI"
        );
        // 字段顺序也要对：JS 是按这个顺序建对象的
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "content",
                "meaning",
                "usage",
                "example",
                "risk",
                "status",
                "source",
                "count",
                "updatedAt"
            ]
        );
    }

    #[test]
    fn confirmed_list_filters_and_sorts_by_count() {
        let entries = vec![
            entry("a", "低", "低", "confirmed", 1),
            entry("b", "高", "高", "confirmed", 9),
            entry("c", "候选", "x", "candidate", 99),
            entry("d", "", "空内容", "confirmed", 50),
            entry("e", "缺含义", "", "confirmed", 50),
        ];
        let list = confirmed_list(&entries, None);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], "b");
        assert_eq!(list[1]["id"], "a");
    }

    #[test]
    fn inject_max_clamps_like_js() {
        assert_eq!(clamp_inject_max(None), 8);
        assert_eq!(clamp_inject_max(Some(&json!(0))), 8);
        assert_eq!(clamp_inject_max(Some(&json!(999))), 30);
        assert_eq!(clamp_inject_max(Some(&json!(3))), 3);
    }

    #[test]
    fn context_escapes_and_marks_usage() {
        let entries = vec![json!({
            "id": "a", "content": "yyds", "meaning": "永远的神<脚本>", "usage": "夸人",
            "example": "[CQ:at,qq=1] 好家伙", "status": "confirmed", "count": 3
        })];
        let ctx = build_context(&entries, None);
        assert!(ctx.contains("【群聊黑话表】"));
        assert!(ctx.contains("永远的神&lt;脚本&gt;"));
        assert!(ctx.contains("（用法：夸人）"));
        assert!(ctx.contains("[CQ：at"), "CQ 码要被拆成全角冒号");
        assert!(!ctx.contains("[CQ:"));
    }

    #[test]
    fn empty_context_when_nothing_confirmed() {
        let entries = vec![entry("a", "候选", "x", "candidate", 1)];
        assert_eq!(build_context(&entries, None), "");
    }

    #[test]
    fn query_matches_all_four_fields_and_keeps_block() {
        let entries = vec![
            json!({ "id": "a", "content": "破防", "meaning": "心态崩了", "usage": "", "example": "", "status": "confirmed", "count": 2 }),
            json!({ "id": "b", "content": "绝绝子", "meaning": "好极了", "usage": "夸", "example": "", "status": "confirmed", "count": 1 }),
        ];
        let all = query(&entries, "group:1", "", None);
        assert_eq!(all["total"], 2);
        let hit = query(&entries, "group:1", "心态", None);
        assert_eq!(hit["total"], 1);
        assert_eq!(hit["entries"][0]["id"], "a");
        // block 永远用全量词库拼，不受 q 过滤影响
        assert!(hit["block"].as_str().unwrap().contains("绝绝子"));
    }

    #[test]
    fn checked_runtime_loader_distinguishes_missing_valid_and_corrupt_selected_data() {
        let (root, context) = runtime_fixture("checked-loader");
        assert!(load_entries_checked_for_runtime(&context)
            .unwrap()
            .is_empty());

        let valid = json!({"entries": [{"id":"fixture", "content":"候选", "meaning":"",
            "usage":"", "example":"", "risk":"", "sources":[], "status":"candidate",
            "source":"ai", "count":0, "evidence":[], "lastInferenceCount":0,
            "createdAt":"t", "updatedAt":"t"}]});
        context
            .write_atomic(
                RuntimeArea::Data,
                "slang.json",
                valid.to_string().as_bytes(),
            )
            .unwrap();
        assert_eq!(
            load_entries_checked_for_runtime(&context).unwrap(),
            valid["entries"].as_array().unwrap().clone()
        );

        let corrupt = b"{bad-json";
        context
            .write_atomic(RuntimeArea::Data, "slang.json", corrupt)
            .unwrap();
        let error = load_entries_checked_for_runtime(&context).unwrap_err();
        assert!(error.contains("候选黑话库 JSON 损坏"));
        assert_eq!(
            context.read_file(RuntimeArea::Data, "slang.json").unwrap(),
            corrupt
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_entries_are_normalized_on_both_runtime_read_paths() {
        let (root, context) = runtime_fixture("historical-normalization");
        let prior = json!({
            "id": "prior", "content": "旧梗", "meaning": "旧解释",
            "status": "confirmed", "source": "manual", "count": 3,
            "evidence": [{"key": "group:420042", "sender": "tester", "text": "旧证据", "time": 1}],
            "lastInferenceCount": 2, "createdAt": "2025-10-09T08:53:20.000Z",
            "updatedAt": "2025-10-09T08:53:20.000Z"
        });
        let bytes = serde_json::to_vec(&json!([prior])).unwrap();
        context
            .write_atomic(RuntimeArea::Data, "slang.json", &bytes)
            .unwrap();
        let expected = json!([{
            "id": "prior", "content": "旧梗", "meaning": "旧解释", "usage": "",
            "example": "", "risk": "", "sources": [], "status": "confirmed",
            "source": "manual", "count": 3,
            "evidence": [{"key": "group:420042", "sender": "tester", "text": "旧证据", "time": 1}],
            "lastInferenceCount": 2, "createdAt": "2025-10-09T08:53:20.000Z",
            "updatedAt": "2025-10-09T08:53:20.000Z"
        }]);
        assert_eq!(
            load_entries_for_runtime(&context),
            expected.as_array().unwrap().clone()
        );
        assert_eq!(
            load_entries_checked_for_runtime(&context).unwrap(),
            expected.as_array().unwrap().clone()
        );
        assert_eq!(
            context.read_file(RuntimeArea::Data, "slang.json").unwrap(),
            bytes
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn save_normalizes_every_entry_field_before_persisting() {
        let (root, context) = runtime_fixture("save-normalizes");
        let mut big = entry("big", "大词", &"义".repeat(2_000_000), "candidate", 1);
        big["usage"] = json!("u".repeat(5_000));
        big["content"] = json!("c".repeat(500));
        let blank = entry("blank", "", "no content", "candidate", 1);
        save_entries_for_runtime(&context, &[big, blank]).unwrap();
        let bytes = context.read_file(RuntimeArea::Data, "slang.json").unwrap();
        assert!(bytes.len() < 10_000, "persisted {} bytes", bytes.len());
        let stored: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(stored.len(), 1, "empty-content entries are dropped on save");
        assert_eq!(stored[0]["content"].as_str().unwrap().chars().count(), 50);
        assert_eq!(stored[0]["meaning"].as_str().unwrap().chars().count(), 300);
        assert_eq!(stored[0]["usage"].as_str().unwrap().chars().count(), 300);
        std::fs::remove_dir_all(root).unwrap();
    }
}
