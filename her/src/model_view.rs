//! Model-facing compaction of bridge data (port of `qq-bridge/src/qq-model-view.js`).
//!
//! Only the view handed to the model is compacted. Stored messages and console/API data keep the original
//! fields. `serialize_model_data` is the JSON text every read tool returns (`JSON.stringify` of the compacted
//! value, compact separators, insertion order kept by serde_json's `preserve_order`).

use serde_json::{Map, Value};

const BLACKLIST_MAX_BYTES: u64 = 64 * 1024;
const BLACKLIST_MAX_ENTRIES: usize = 500;
const BLACKLIST_MAX_CHARS: usize = 64;

fn read_blacklist(context: &crate::runtime::RuntimeContext) -> (&'static str, Vec<Value>) {
    use crate::runtime::{PathIntent, RuntimeArea};
    use std::io::Read;
    let read = || -> Result<Option<Vec<u8>>, &'static str> {
        let path = context.resolve_path(RuntimeArea::Config, "model-text-blacklist.json", PathIntent::ReadOptionalFile)
            .map_err(|_| "unreadable")?;
        let checked = context.resolve_path(RuntimeArea::Config, "model-text-blacklist.json", PathIntent::ReadOptionalFile)
            .map_err(|_| "unreadable")?;
        if checked != path { return Err("unreadable"); }
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("unreadable"),
        };
        // Bound the actual read as well as metadata: an external writer may grow
        // the file after it is opened. Never allocate the whole external file.
        if file.metadata().map_err(|_| "unreadable")?.len() > BLACKLIST_MAX_BYTES { return Err("invalid"); }
        let mut bytes = Vec::new();
        file.take(BLACKLIST_MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|_| "unreadable")?;
        if bytes.len() as u64 > BLACKLIST_MAX_BYTES { return Err("invalid"); }
        Ok(Some(bytes))
    };
    let bytes = match read() {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return ("missing", Vec::new()),
        Err(state) => return (state, Vec::new()),
    };
    let items = String::from_utf8(bytes).ok()
        .and_then(|text| serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')).ok())
        .and_then(|mut value| value.get_mut("strip").map(Value::take));
    match items {
        Some(Value::Array(items)) if items.len() <= BLACKLIST_MAX_ENTRIES && items.iter().all(|item|
            item.as_str().is_some_and(|s| s.chars().take(BLACKLIST_MAX_CHARS + 1).count() <= BLACKLIST_MAX_CHARS)) => ("ready", items),
        _ => ("invalid", Vec::new()),
    }
}

/// The last failure state this process reported, per runtime root.
static PROCESS_STATES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, &'static str>>> =
    std::sync::OnceLock::new();

fn warn_blacklist_state(state: &'static str) {
    #[cfg(test)]
    TEST_WARNINGS.with(|warnings| warnings.borrow_mut().push(state));
    #[cfg(not(test))]
    eprintln!("[model-text-blacklist] state={state}; using an empty file list");
}

#[cfg(test)]
std::thread_local! {
    static TEST_WARNINGS: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Test hook: this runtime's in-process record is forgotten, as a new process would have none.
#[cfg(test)]
fn forget_process_states(context: &crate::runtime::RuntimeContext) {
    if let Some(states) = PROCESS_STATES.get() {
        states.lock().unwrap_or_else(|p| p.into_inner()).remove(context.runtime_root().unwrap());
    }
}

/// Test hook: the warnings this thread would have printed, in order.
#[cfg(test)]
fn take_warnings() -> Vec<&'static str> {
    TEST_WARNINGS.with(|warnings| std::mem::take(&mut *warnings.borrow_mut()))
}

/// The runtime's last reported state, shared by every process that uses the runtime.
const STATE_FILE: &str = "model-text-blacklist.state";
/// Serializes the read and write of `STATE_FILE`. Never replaced or unlinked.
const STATE_LOCK: &str = "locks/model-text-blacklist.lock";
const STATE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Records `state` as the runtime's last reported state under the state lock. Returns whether to warn, or `None`
/// when the shared record cannot be read or written; the caller then uses the process record.
fn shared_state_transition(context: &crate::runtime::RuntimeContext, state: &'static str) -> Option<bool> {
    use crate::runtime::RuntimeArea;
    let deadline = std::time::Instant::now() + STATE_LOCK_WAIT;
    let _gate = loop {
        match context.open_locked_file(RuntimeArea::Data, STATE_LOCK) {
            Ok(gate) => break gate,
            Err(error) if error == "runtime file is already locked" && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    };
    let persisted = context.read_optional_text(RuntimeArea::Data, STATE_FILE).ok()?;
    if persisted.as_deref().map(str::trim) == Some(state) {
        return Some(false);
    }
    context.write_atomic(RuntimeArea::Data, STATE_FILE, state.as_bytes()).ok()?;
    Some(state != "ready")
}

/// Records `state` as this process's last seen state for the runtime, on every read. Returns whether to warn by the
/// process record alone, which is the fallback when the shared record cannot be used.
fn process_state_transition(context: &crate::runtime::RuntimeContext, state: &'static str) -> bool {
    let mut states = PROCESS_STATES
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock().unwrap_or_else(|p| p.into_inner());
    let scope = context.runtime_root().unwrap().to_path_buf();
    states.insert(scope, state) != Some(state) && state != "ready"
}

/// A delivery-only overlay. Never save this value back to config.json.
pub fn with_blacklist(context: &crate::runtime::RuntimeContext, cfg: &Value) -> Value {
    use std::collections::HashSet;
    // The retired legacy layout has no selected config directory.
    if !context.is_isolated() { return cfg.clone(); }
    let (state, file_items) = read_blacklist(context);
    // The process record is updated on every read, so a fallback never compares against a stale state.
    let process_warn = process_state_transition(context, state);
    let warn = shared_state_transition(context, state).unwrap_or(process_warn);
    if warn { warn_blacklist_state(state); }
    let configured = cfg.pointer("/socialV2/context/stripFromModelText").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    let items = configured.into_iter().chain(file_items).filter(|item| {
        item.as_str().is_none_or(|s| seen.insert(s.to_string()))
    }).collect();
    let mut result = cfg.clone();
    if !result.is_object() { result = serde_json::json!({}); }
    if !result["socialV2"].is_object() { result["socialV2"] = serde_json::json!({}); }
    if !result["socialV2"]["context"].is_object() { result["socialV2"]["context"] = serde_json::json!({}); }
    result["socialV2"]["context"]["stripFromModelText"] = Value::Array(items);
    result
}

pub fn strip_text_with_runtime(context: &crate::runtime::RuntimeContext, text: &str, cfg: &Value) -> String {
    strip_text(text, &with_blacklist(context, cfg))
}

pub fn strip_text(text: &str, cfg: &Value) -> String {
    // The packet is already structured bridge data: apply the chat-field filter so IDs,
    // cursors and tokens are never treated as chat text, even for a numeric strip entry.
    if let Some((before, packet)) = text.rsplit_once("【本次消息快照】\n") {
        let mut values = serde_json::Deserializer::from_str(packet).into_iter::<Value>();
        if let Some(Ok(value)) = values.next() {
            let consumed = values.byte_offset();
            return format!("{}【本次消息快照】\n{}{}", strip_from_model_text(before, &cfg),
                strip_model_chat_data(&value, &cfg), strip_from_model_text(&packet[consumed..], &cfg));
        }
    }
    strip_from_model_text(text, &cfg)
}

pub fn strip_chat_with_runtime(context: &crate::runtime::RuntimeContext, data: &Value, cfg: &Value) -> Value {
    strip_model_chat_data(data, &with_blacklist(context, cfg))
}

/// The model-view strip of `text` (`delivery` from `with_blacklist`), unless it had to refuse a strip entry
/// (`strip_model_text`): then `None`, and the caller uses its accepted fallback.
pub fn strip_checked(text: &str, delivery: &Value) -> Option<String> {
    let (stripped, refused) = strip_model_text(text, delivery);
    (!refused).then_some(stripped)
}

/// Filter only the copy delivered to a model. Transport codes and media markers are atoms. See `strip_model_text`.
pub fn strip_from_model_text(text: &str, cfg: &Value) -> String {
    strip_model_text(text, cfg).0
}

/// The one model-text strip, used by every delivery site and by the checked reads of the prompt texts. Each entry of
/// `stripFromModelText` is removed from the text outside atoms, the entries in order, and the round repeats until it
/// removes nothing more, so a removal that joins a new match is not left behind. An entry whose removal would give the
/// text a denied term (`settings::denied_in_text`) that the input did not hold is not applied: the text stays as it
/// was for that entry. So the result never holds a denied term the input lacked, and stripping the result again
/// changes nothing. The flag is whether an entry that still matches the result was refused.
fn strip_model_text(text: &str, cfg: &Value) -> (String, bool) {
    let Some(items) = cfg.pointer("/socialV2/context/stripFromModelText").and_then(Value::as_array) else {
        return (text.to_string(), false);
    };
    let strips: Vec<&str> = items.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).collect();
    if strips.is_empty() {
        return (text.to_string(), false);
    }
    static ATOMS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let atoms = ATOMS.get_or_init(|| regex::Regex::new(
        r"【会话令牌】[^\r\n]*|\[CQ:[^\]\r\n]*\]|\[(?:图片|语音|视频|媒体|卡片消息|表情[^\]\r\n]*|文件[^\]\r\n]*|转发消息[^\]\r\n]*)\]"
    ).unwrap());
    let remove = |text: &str, item: &str| {
        let mut out = String::with_capacity(text.len());
        let mut start = 0;
        for atom in atoms.find_iter(text) {
            out.push_str(&text[start..atom.start()].replace(item, ""));
            out.push_str(atom.as_str());
            start = atom.end();
        }
        out.push_str(&text[start..].replace(item, ""));
        out
    };
    let mut held: Option<Vec<String>> = None;
    let mut current = text.to_string();
    loop {
        let (mut changed, mut refused) = (false, false);
        for item in &strips {
            let next = remove(&current, item);
            if next == current {
                continue;
            }
            let held = held.get_or_insert_with(|| crate::settings::denied_in_text(text));
            if crate::settings::denied_in_text(&next).iter().all(|term| held.contains(term)) {
                current = next;
                changed = true;
            } else {
                refused = true;
            }
        }
        // Each applied entry shortens the text, so the rounds end.
        if !changed {
            return (current, refused);
        }
    }
}

/// Keep JSON keys, cursors, tokens and media references intact; filter chat-bearing values.
pub fn strip_model_chat_data(data: &Value, cfg: &Value) -> Value {
    match data {
        Value::Array(items) => Value::Array(items.iter().map(|v| strip_model_chat_data(v, cfg)).collect()),
        Value::Object(obj) => Value::Object(obj.iter().map(|(key, value)| {
            let filtered = match (key.as_str(), value) {
                ("text" | "plain" | "tail" | "sender" | "nickname" | "card" | "ownerLabel"
                    | "raw_message" | "message" | "messages" | "content" | "subject" | "query" | "q"
                    | "formatted" | "block" | "meaning" | "usage" | "example", Value::String(text)) => {
                    Value::String(strip_from_model_text(text, cfg))
                }
                ("terms" | "messages", Value::Array(items)) => Value::Array(items.iter().map(|v| match v {
                    Value::String(text) => Value::String(strip_from_model_text(text, cfg)),
                    _ => strip_model_chat_data(v, cfg),
                }).collect()),
                _ => strip_model_chat_data(value, cfg),
            };
            (key.clone(), filtered)
        }).collect()),
        _ => data.clone(),
    }
}

const DEFAULT_FALSE_FIELDS: [&str; 5] = ["quoteTargetIsSelf", "isOwner", "isSelf", "hasMedia", "hasForward"];
const OPTIONAL_FIELDS: [&str; 12] = [
    "seq", "messageId", "sender", "userId", "text", "plain", "tail", "kind", "ownerLabel", "media", "forwardIds", "nestedForwardIds",
];

fn is_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

fn compact_media(media: &[Value], can_resolve_by_message: bool) -> Vec<Value> {
    media
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let Some(obj) = item.as_object() else { return item.clone() };
            let mut result = obj.clone();
            if can_resolve_by_message {
                result.shift_remove("file");
                result.shift_remove("url");
                let missing = matches!(result.get("index"), None | Some(Value::Null));
                if missing {
                    result.insert("index".to_string(), Value::from(index as u64 + 1));
                }
            } else if result.get("file").is_some() && result.get("file") == result.get("url") {
                result.shift_remove("file");
            }
            for key in ["file", "url", "faceId"] {
                if is_empty(result.get(key)) {
                    result.shift_remove(key);
                }
            }
            Value::Object(result)
        })
        .collect()
}

pub fn compact_model_message(message: &Value) -> Value {
    let Some(obj) = message.as_object() else { return message.clone() };
    let mut result: Map<String, Value> = obj.clone();
    // The stored full text is read through `qq_get_message_detail`; message lists carry the preview,
    // `textTruncated` and `textChars`.
    result.shift_remove("fullText");
    // No trimming or truncation: differing plain text can carry resolved quote context, and tail can carry the
    // end of a long message absent from text.
    let mut seen_text: Vec<String> = Vec::new();
    for key in ["text", "plain", "tail"] {
        let Some(Value::String(s)) = result.get(key) else { continue };
        if seen_text.contains(s) {
            result.shift_remove(key);
        } else {
            seen_text.push(s.clone());
        }
    }
    for key in OPTIONAL_FIELDS {
        if is_empty(result.get(key)) {
            result.shift_remove(key);
        }
    }
    for key in DEFAULT_FALSE_FIELDS {
        if result.get(key) == Some(&Value::Bool(false)) {
            result.shift_remove(key);
        }
    }
    if let Some(Value::Array(media)) = result.get("media") {
        let can_resolve = obj
            .get("seq")
            .and_then(|s| s.as_f64())
            .map(|s| s.fract() == 0.0 && s > 0.0 && s <= 9_007_199_254_740_991.0)
            .unwrap_or(false);
        let compacted = compact_media(media, can_resolve);
        result.insert("media".to_string(), Value::Array(compacted));
    }
    Value::Object(result)
}

pub fn compact_model_data(data: &Value) -> Value {
    match data {
        Value::Array(items) => Value::Array(items.iter().map(compact_model_data).collect()),
        Value::Object(obj) => {
            let mut out = Map::new();
            for (key, value) in obj {
                if key == "fullText" {
                    continue;
                }
                let compacted = if (key == "messages" || key == "newMessages") && value.is_array() {
                    Value::Array(value.as_array().unwrap().iter().map(compact_model_message).collect())
                } else {
                    compact_model_data(value)
                };
                out.insert(key.clone(), compacted);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

pub fn serialize_model_data(data: &Value) -> String {
    serde_json::to_string(&compact_model_data(data)).unwrap_or_else(|_| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_result_chat_schemas_filter_every_text_field_and_keep_transport() {
        let cfg = json!({"socialV2":{"context":{"stripFromModelText":["WORD"]}}});
        for (field, original, expected) in [
            ("formatted", json!("WORD memory"), json!(" memory")),
            ("block", json!("WORD slang"), json!(" slang")),
            ("meaning", json!("WORD meaning"), json!(" meaning")),
            ("usage", json!("WORD usage"), json!(" usage")),
            ("example", json!("WORD example"), json!(" example")),
            ("messages", json!("WORD message"), json!(" message")),
            ("messages", json!(["WORD first", "WORD second", {"text":"WORD nested","messageId":"WORD-id"}]),
                json!([" first", " second", {"text":" nested","messageId":"WORD-id"}])),
        ] {
            let data = json!({field:original,"messageId":"WORD-id","token":"WORD-token"});
            let filtered = strip_model_chat_data(&data, &cfg);
            assert_eq!(filtered[field], expected, "{field}");
            assert_eq!(data[field], original, "delivery must retain the source");
            assert_eq!(filtered["messageId"], "WORD-id");
            assert_eq!(filtered["token"], "WORD-token");
        }
    }

    #[test]
    fn hotfix83_blacklist_reloads_bounded_files_without_changing_originals() {
        use crate::runtime::RuntimeContext;
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-blacklist-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let cfg = json!({"consoleToken":"blacklist-fixture","socialV2":{"context":{"stripFromModelText":["CFG"]}}});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let original = json!({"messages":[{"messageId":"83002","seq":3,"sender":"WORD name","text":"CFG WORD NEW 83002 [图片]"}]});
        let file = root.join("config/model-text-blacklist.json");
        let render = || strip_chat_with_runtime(&context, &original, &cfg);
        assert_eq!(render()["messages"][0]["text"], " WORD NEW 83002 [图片]");
        std::fs::write(&file, r#"{"strip":["WORD","83002"]}"#).unwrap();
        let filtered = render();
        assert_eq!(filtered["messages"][0]["text"], "  NEW  [图片]");
        assert_eq!(filtered["messages"][0]["messageId"], "83002");
        assert_eq!(filtered["messages"][0]["seq"], 3);
        std::fs::write(&file, r#"{"strip":["NEW"]}"#).unwrap();
        assert_eq!(render()["messages"][0]["text"], " WORD  83002 [图片]");
        for invalid in ["invalid JSON".to_string(), json!({"strip":["WORD"],"padding":"x".repeat(65536)}).to_string(), json!({"strip":vec!["WORD";501]}).to_string(), json!({"strip":["WORD","字".repeat(65)]}).to_string()] {
            std::fs::write(&file, invalid).unwrap();
            for _ in 0..2 { assert_eq!(render()["messages"][0]["text"], " WORD NEW 83002 [图片]"); }
        }
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        for _ in 0..2 { assert_eq!(render()["messages"][0]["text"], " WORD NEW 83002 [图片]"); }
        assert_eq!(original["messages"][0]["text"], "CFG WORD NEW 83002 [图片]");
        assert_eq!(std::fs::read_to_string(root.join("config/config.json")).unwrap(), cfg.to_string());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blacklist_missing_state_warns_once_per_runtime_across_processes() {
        use crate::runtime::RuntimeContext;
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-blacklist-state-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let cfg = json!({"consoleToken":"blacklist-state-fixture"});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let file = root.join("config/model-text-blacklist.json");
        let view = || with_blacklist(&context, &cfg);

        // First process: the missing state is reported once.
        forget_process_states(&context);
        view();
        assert_eq!(take_warnings(), vec!["missing"]);
        // Same process, same state: silent.
        view();
        assert!(take_warnings().is_empty());
        // A second process reading the same runtime, same state: silent.
        forget_process_states(&context);
        view();
        assert!(take_warnings().is_empty(), "a second process repeated the missing-state warning");
        // A state change reports again: missing -> invalid -> ready (silent) -> missing.
        std::fs::write(&file, "invalid JSON").unwrap();
        forget_process_states(&context);
        view();
        assert_eq!(take_warnings(), vec!["invalid"]);
        std::fs::write(&file, r#"{"strip":["WORD"]}"#).unwrap();
        forget_process_states(&context);
        view();
        assert!(take_warnings().is_empty());
        std::fs::remove_file(&file).unwrap();
        forget_process_states(&context);
        view();
        assert_eq!(take_warnings(), vec!["missing"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blacklist_unusable_state_file_falls_back_to_once_per_process() {
        use crate::runtime::RuntimeContext;
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-blacklist-fallback-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let cfg = json!({"consoleToken":"blacklist-fallback-fixture","socialV2":{"context":{"stripFromModelText":["CFG"]}}});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        // A directory where the state file belongs: the shared record can be neither read nor written.
        std::fs::create_dir(root.join("data/model-text-blacklist.state")).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let view = || with_blacklist(&context, &cfg);

        forget_process_states(&context);
        let filtered = view();
        assert_eq!(take_warnings(), vec!["missing"]);
        assert_eq!(filtered["socialV2"]["context"]["stripFromModelText"], json!(["CFG"]), "delivery must continue");
        view();
        assert!(take_warnings().is_empty(), "the process record must stop a repeat");
        forget_process_states(&context);
        view();
        assert_eq!(take_warnings(), vec!["missing"], "each process warns once when the shared record is unusable");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn strip_model_text_defaults_and_literal_strings() {
        let text = "奸坏词 badbad 😀";
        for cfg in [json!({}), json!({"socialV2":{"context":{"stripFromModelText":[]}}})] {
            assert_eq!(strip_from_model_text(text, &cfg), text);
        }
        let cfg = json!({"socialV2":{"context":{"stripFromModelText":["奸","坏词","bad","😀","",null,3]}}});
        assert_eq!(strip_from_model_text(text, &cfg), "  ");
    }

    fn strip_cfg(items: &[&str]) -> Value {
        json!({"socialV2":{"context":{"stripFromModelText":items}}})
    }

    #[test]
    fn prompt_text_model_strip_repeats_until_nothing_more_goes() {
        // One pass over `x鲸鲸鱼鱼y` takes the inner `鲸鱼` and leaves a new one behind.
        assert_eq!(strip_from_model_text("x鲸鲸鱼鱼y", &strip_cfg(&["鲸鱼"])), "xy");
        assert_eq!(strip_from_model_text("a鲸鲸鲸鱼鱼鱼b", &strip_cfg(&["鲸鱼"])), "ab");
        // The first literal's removal makes a match for the second one.
        assert_eq!(strip_from_model_text("甲乙X丙丁", &strip_cfg(&["乙丙", "X"])), "甲丁");
    }

    #[test]
    fn prompt_text_model_strip_never_adds_a_denied_term() {
        let strip = |text: &str, items: &[&str]| strip_from_model_text(text, &strip_cfg(items));
        // The strip would make `DSH` of a text that held none: the literal is not applied.
        assert_eq!(strip("D鲸SH", &["鲸"]), "D鲸SH");
        // The first pass is harmless and kept; the second would make `DSH`.
        assert_eq!(strip("D鲸鲸鱼鱼SH", &["鲸鱼"]), "D鲸鱼SH");
        // A literal that is harmless still applies.
        assert_eq!(strip("D鲸SH 坏", &["鲸", "坏"]), "D鲸SH ");
        // A strip that joins text across a line break makes the term too.
        assert_eq!(strip("D\n\nSH", &["\n\n"]), "D\n\nSH");
        assert_eq!(strip("甲\n\n乙", &["\n\n"]), "甲乙");
        // A term the text already held is not protected, and is not a reason to keep other strips out.
        assert_eq!(strip("DSH 鲸x", &["鲸"]), "DSH x");
        // Applying the strip again changes nothing, so a delivery strip after a checked one adds no term.
        for (text, items) in [
            ("D鲸鲸鱼鱼SH", &["鲸鱼"][..]),
            ("D鲸SH 坏", &["鲸", "坏"][..]),
            ("D\n\nSH", &["\n\n"][..]),
            ("x鲸鲸鱼鱼y", &["鲸鱼"][..]),
        ] {
            let once = strip(text, items);
            assert_eq!(strip(&once, items), once, "{text:?}");
        }
    }

    #[test]
    fn prompt_text_checked_strip_refuses_a_text_the_strip_would_turn_into_a_denied_term() {
        let delivery = strip_cfg(&["鲸鱼"]);
        // The second pass would make `DSH`, so the text is refused whole, not delivered half stripped.
        assert_eq!(strip_checked("D鲸鲸鱼鱼SH", &delivery), None);
        assert_eq!(strip_checked("D鲸鱼SH", &delivery), None);
        assert_eq!(strip_checked("D鲸SH", &strip_cfg(&["鲸"])), None);
        assert_eq!(strip_checked("a鲸鱼b", &delivery), Some("ab".to_string()));
        assert_eq!(strip_checked("a鲸鲸鱼鱼b", &delivery), Some("ab".to_string()));
        // A term the text already held is not the strip's doing.
        assert_eq!(strip_checked("DSH 鲸鱼x", &delivery), Some("DSH x".to_string()));
    }

    fn delivery_fixture(name: &str, strip: &[&str]) -> (std::path::PathBuf, crate::runtime::RuntimeContext, Value) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-{name}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let mut cfg = strip_cfg(strip);
        cfg["consoleToken"] = json!("delivery-fixture");
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        let context = crate::runtime::RuntimeContext::from_root(&root).unwrap();
        (root, context, cfg)
    }

    #[test]
    fn prompt_text_dsh_delivery_strip_never_adds_a_denied_term() {
        // The compatibility render passed its check as `D鲸鱼SH`; the DSH delivery strips the prompt again.
        let (root, context, cfg) = delivery_fixture("prompt-text-dsh", &["鲸鱼"]);
        let delivered = strip_text_with_runtime(&context, "【别像客服】D鲸鱼SH\n\n后文", &cfg);
        assert!(!crate::settings::denied_in_text(&delivered).contains(&"dsh".to_string()), "{delivered}");
        assert_eq!(delivered, "【别像客服】D鲸鱼SH\n\n后文");
        // The snapshot packet form strips its chat fields and the text around it the same way.
        let packet = "前D鲸鱼SH\n【本次消息快照】\n{\"messages\":[{\"seq\":1,\"text\":\"D鲸鱼SH 鲸鱼好\"}]}后D鲸鱼SH";
        let delivered = strip_text_with_runtime(&context, packet, &cfg);
        assert!(!crate::settings::denied_in_text(&delivered).contains(&"dsh".to_string()), "{delivered}");
        assert_eq!(delivered, packet, "the literal that would make the term is not applied anywhere in the text");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn strip_model_text_keeps_cq_media_and_json_transport_fields_intact() {
        let cfg = json!({"socialV2":{"context":{"stripFromModelText":["奸","bad","[","图","text","\"","\\"]}}});
        let data = json!({"key":"group:text","token":"bad-token","query":"奸bad query","q":"奸bad q","messages":[{
            "seq":82,"messageId":"bad-id","sender":"奸昵称",
            "text":"奸badtext\"\\ [CQ:image,file=bad.png] [图片] [语音] [视频] [表情14] [文件bad.png]",
            "reply":{"nickname":"奸人","text":"奸bad原话"},
            "media":[{"kind":"image","file":"bad.png","url":"https://example.test/bad.png"}]
        }]});
        let out = strip_model_chat_data(&data, &cfg);
        assert_eq!(out["key"], "group:text");
        assert_eq!(out["token"], "bad-token");
        assert_eq!(out["query"], " query");
        assert_eq!(out["q"], " q");
        assert_eq!(out["messages"][0]["seq"], 82);
        assert_eq!(out["messages"][0]["messageId"], "bad-id");
        assert_eq!(out["messages"][0]["sender"], "昵称");
        assert_eq!(out["messages"][0]["text"], " [CQ:image,file=bad.png] [图片] [语音] [视频] [表情14] [文件bad.png]");
        assert_eq!(out["messages"][0]["reply"], json!({"nickname":"人","text":"原话"}));
        assert_eq!(out["messages"][0]["media"], data["messages"][0]["media"]);
        assert_eq!(data["messages"][0]["sender"], "奸昵称");
        let encoded = serialize_model_data(&out);
        let _: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(strip_from_model_text("【会话令牌】bad-token\n奸bad", &cfg), "【会话令牌】bad-token\n");
    }

    #[test]
    fn compaction_drops_defaults_and_duplicate_text_only_in_messages() {
        let data = json!({
            "ok": true,
            "messages": [{
                "seq": 3, "messageId": "", "sender": "a", "text": "hi", "plain": "hi", "tail": "hi", "isOwner": false, "isSelf": true,
                "media": [{"kind": "image", "file": "f", "url": "u", "faceId": ""}], "hasMedia": true, "forwardIds": [], "hasForward": false,
                "quoteTargetIsSelf": false, "ownerLabel": "", "extra": ""
            }],
            "info": {"text": "x", "plain": "x", "isOwner": false}
        });
        let out = compact_model_data(&data);
        assert_eq!(
            out["messages"][0],
            json!({"seq": 3, "sender": "a", "text": "hi", "isSelf": true, "media": [{"kind": "image", "index": 1}], "hasMedia": true, "extra": ""})
        );
        assert_eq!(out["info"], data["info"], "only messages/newMessages are compacted");
    }

    #[test]
    fn media_without_a_resolvable_seq_keeps_transport_references() {
        let msg = json!({"seq": 0, "media": [{"kind": "image", "file": "same", "url": "same"}, {"kind": "image", "file": "a", "url": "b", "index": 4}]});
        let out = compact_model_message(&msg);
        assert_eq!(out["media"], json!([{"kind": "image", "url": "same"}, {"kind": "image", "file": "a", "url": "b", "index": 4}]));
    }
    #[test]
    fn message_lists_never_carry_the_stored_full_text() {
        let message = json!({"seq": 3, "text": "预览", "fullText": "完整正文".repeat(100), "textTruncated": true, "textChars": 400});
        let compact = compact_model_message(&message);
        assert!(compact.get("fullText").is_none(), "{compact}");
        assert_eq!((&compact["textTruncated"], &compact["textChars"]), (&json!(true), &json!(400)));
        let data = compact_model_data(&json!({"recent": [message.clone()], "newMessages": [message]}));
        assert!(!data.to_string().contains("完整正文"), "{data}");
        assert!(data.to_string().contains("textTruncated"));
    }
}
