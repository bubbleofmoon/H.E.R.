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

/// A delivery-only overlay. Never save this value back to config.json.
pub fn with_blacklist(context: &crate::runtime::RuntimeContext, cfg: &Value) -> Value {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Mutex, OnceLock};
    // The retired legacy layout has no selected config directory.
    if !context.is_isolated() { return cfg.clone(); }
    let (state, file_items) = read_blacklist(context);
    static STATES: OnceLock<Mutex<HashMap<std::path::PathBuf, &'static str>>> = OnceLock::new();
    let mut states = STATES.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|p| p.into_inner());
    let scope = context.runtime_root().unwrap().to_path_buf();
    if states.insert(scope, state) != Some(state) && state != "ready" {
        eprintln!("[model-text-blacklist] state={state}; using an empty file list");
    }
    drop(states);
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

/// Filter only the copy delivered to a model. Transport codes and media markers are atoms.
pub fn strip_from_model_text(text: &str, cfg: &Value) -> String {
    let Some(items) = cfg.pointer("/socialV2/context/stripFromModelText").and_then(Value::as_array) else {
        return text.to_string();
    };
    let strips: Vec<&str> = items.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).collect();
    if strips.is_empty() {
        return text.to_string();
    }
    static ATOMS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let atoms = ATOMS.get_or_init(|| regex::Regex::new(
        r"【会话令牌】[^\r\n]*|\[CQ:[^\]\r\n]*\]|\[(?:图片|语音|视频|媒体|卡片消息|表情[^\]\r\n]*|文件[^\]\r\n]*|转发消息[^\]\r\n]*)\]"
    ).unwrap());
    let strip = |part: &str| {
        strips.iter().fold(part.to_string(), |out, item| out.replace(item, ""))
    };
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    for atom in atoms.find_iter(text) {
        out.push_str(&strip(&text[start..atom.start()]));
        out.push_str(atom.as_str());
        start = atom.end();
    }
    out.push_str(&strip(&text[start..]));
    out
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
    fn strip_model_text_defaults_and_literal_strings() {
        let text = "奸坏词 badbad 😀";
        for cfg in [json!({}), json!({"socialV2":{"context":{"stripFromModelText":[]}}})] {
            assert_eq!(strip_from_model_text(text, &cfg), text);
        }
        let cfg = json!({"socialV2":{"context":{"stripFromModelText":["奸","坏词","bad","😀","",null,3]}}});
        assert_eq!(strip_from_model_text(text, &cfg), "  ");
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
