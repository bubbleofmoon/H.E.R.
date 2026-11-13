//! Local chat history log (#58). When config `history.enabled` is true (isolated runtimes only),
//! ingress appends every message frame before self filtering, the allow list and mode dispatch,
//! and the guarded send boundary (`send_outbox::append_record`) appends each intent part and
//! confirmation after the outbox record is durable. Records use schema v1 from
//! `scripts/strip_history.py`, in `data/history/<UTC day>.jsonl`, appended without fsync and
//! never pruned. A failed append never blocks ingress or fails a send.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};

use crate::runtime::{RuntimeArea, RuntimeContext};

pub const DIR: &str = "history";

pub fn enabled(context: &RuntimeContext) -> bool {
    context.is_isolated()
        && context.load_config().map(|cfg| cfg["history"]["enabled"].as_bool() == Some(true)).unwrap_or(false)
}

fn file_for(ms: i64) -> String {
    let (y, m, d) = crate::sticker::civil_from_days(ms.div_euclid(86_400_000));
    format!("{DIR}/{y:04}-{m:02}-{d:02}.jsonl")
}

fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => n.as_i64().map(|v| v.to_string()),
        Value::String(s) => {
            let s = s.trim();
            let digits = s.strip_prefix('-').unwrap_or(s);
            (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| s.to_owned())
        }
        _ => None,
    }
}

fn media_kind(kind: &str) -> &'static str {
    match kind {
        "image" => "image", "video" => "video", "record" => "voice", "file" | "flash_file" => "file",
        "mface" => "sticker", "json" | "xml" => "card", "markdown" => "markdown", _ => "other",
    }
}

fn segments(message: &Value) -> Vec<Value> {
    let Some(items) = message.as_array() else {
        // A CQ-string frame stays one text segment; the strip step removes ids and links in text.
        return message.as_str().filter(|t| !t.is_empty()).map(|t| vec![json!({"k":"text","text":t})]).unwrap_or_default();
    };
    items.iter().filter_map(|item| {
        let data = &item["data"];
        Some(match item["type"].as_str()? {
            "text" => json!({"k":"text","text": data["text"].as_str().unwrap_or("")}),
            "at" => json!({"k":"at","id": if data["qq"] == "all" { json!("all") } else { json!(id_text(&data["qq"])) }}),
            "reply" => json!({"k":"reply","mid": id_text(&data["id"])}),
            "forward" | "node" => json!({"k":"forward"}),
            "face" => json!({"k":"face","id": id_text(&data["id"]).unwrap_or_default()}),
            other => json!({"k":"media","kind": media_kind(other)}),
        })
    }).collect()
}

pub(crate) fn inbound_record(event: &Value, received_ms: i64) -> Option<Value> {
    let sent = event["post_type"] == "message_sent";
    if event["post_type"] != "message" && !sent { return None; }
    let sender = &event["sender"];
    let sender_id = id_text(&event["user_id"]).or_else(|| id_text(&sender["user_id"]));
    let own = sent || (sender_id.is_some() && sender_id == id_text(&event["self_id"]));
    let conv = match event["message_type"].as_str()? {
        "group" => format!("group:{}", id_text(&event["group_id"])?),
        "private" => format!("private:{}", if own { id_text(&event["target_id"]) } else { sender_id.clone() }?),
        _ => return None,
    };
    let t = event["time"].as_i64().filter(|s| *s > 0).map(|s| s * 1000).unwrap_or(received_ms);
    Some(json!({
        "v": 1, "src": "ingress", "conv": conv, "dir": if own { "out" } else { "in" },
        "sender": sender_id.map(|id| json!({"id": id, "card": sender["card"].as_str().unwrap_or(""),
                                            "nick": sender["nickname"].as_str().unwrap_or("")})),
        "convName": event["group_name"].as_str().filter(|n| !n.is_empty()),
        "t": t, "rt": received_ms,
        "mid": id_text(&event["message_id"]).or_else(|| id_text(&event["msg_id"])),
        "segs": segments(&event["message"]),
    }))
}

pub(crate) fn send_records(outbox: &Value, now: i64) -> Vec<Value> {
    let t = outbox["t"].as_i64().unwrap_or(now);
    let batch = outbox["batch"].as_str().unwrap_or("");
    match outbox["k"].as_str() {
        Some("intent") => {
            let Some(messages) = outbox["messages"].as_array() else { return Vec::new() };
            let kind = outbox["kind"].as_str().unwrap_or("text");
            messages.iter().enumerate().map(|(idx, text)| {
                let mut segs = Vec::new();
                if idx == 0 {
                    if let Some(mid) = id_text(&outbox["replyTo"]) { segs.push(json!({"k":"reply","mid": mid})); }
                    if let Some(user) = id_text(&outbox["atUser"]) { segs.push(json!({"k":"at","id": user})); }
                }
                segs.push(match kind {
                    "voice" => json!({"k":"media","kind":"voice"}),
                    "sticker" => json!({"k":"media","kind":"sticker"}),
                    _ => json!({"k":"text","text": text.as_str().unwrap_or("")}),
                });
                json!({"v":1,"src":"send","phase":"intent","conv": outbox["key"].as_str(),"dir":"out","sender":null,
                       "t":t,"mid":null,"batch":batch,"idx":idx,"segs":segs})
            }).collect()
        }
        Some("confirmed") => vec![json!({"v":1,"src":"send","phase":"confirmed","conv":null,"dir":"out","sender":null,
            "t":t,"mid": id_text(&outbox["messageId"]),"batch":batch,"idx": outbox["idx"].as_u64().unwrap_or(0),"segs":[]})],
        _ => Vec::new(),
    }
}

fn append(context: &RuntimeContext, record: &Value, at_ms: i64) {
    let safe = crate::activity::mask_json_session_tokens(record, &crate::activity::known_session_tokens(context));
    let Ok(mut line) = serde_json::to_vec(&safe) else { return };
    line.push(b'\n');
    if context.append_bytes(RuntimeArea::Data, &file_for(at_ms), &line).is_err() {
        static LAST_WARN: AtomicI64 = AtomicI64::new(i64::MIN);
        let last = LAST_WARN.load(Ordering::Relaxed);
        if last == i64::MIN || at_ms.saturating_sub(last) >= 60_000 {
            LAST_WARN.store(at_ms, Ordering::Relaxed);
            crate::safe_println!("[history] append failed; ingress and sends continue");
        }
    }
}

/// Ingress hook: called for every non-replayed frame after meta events, before any filter.
pub fn record_inbound(context: &RuntimeContext, frame: &str) {
    let Ok(event) = serde_json::from_str::<Value>(frame) else { return };
    let received = crate::turn_ledger::now_ms();
    let Some(record) = inbound_record(&event, received) else { return };
    if enabled(context) { append(context, &record, received); }
}

/// Send-boundary hook: called by `send_outbox::append_record` after the outbox record is durable.
pub fn record_send(context: &RuntimeContext, outbox: &Value) {
    let now = crate::turn_ledger::now_ms();
    let records = send_records(outbox, now);
    if records.is_empty() || !enabled(context) { return; }
    for record in &records { append(context, record, now); }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox {
        root: std::path::PathBuf,
        context: RuntimeContext,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn runtime(tag: &str, enabled: bool) -> Sandbox {
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id(tag)));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let config = json!({"consoleToken": "synthetic-review-token-abcdefghij", "history": {"enabled": enabled}});
        std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        Sandbox { root, context }
    }

    fn history_lines(root: &std::path::Path) -> Vec<Value> {
        let Ok(entries) = std::fs::read_dir(root.join("data/history")) else { return Vec::new() };
        let mut paths: Vec<std::path::PathBuf> = entries.map(|e| e.unwrap().path()).collect();
        paths.sort();
        paths
            .iter()
            .flat_map(|path| {
                std::fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn group_frame(message_id: i64, user: i64, text: &str) -> Value {
        json!({
            "time": 1000, "self_id": 9001, "post_type": "message", "message_type": "group",
            "message_id": message_id, "group_id": 420042, "user_id": user,
            "sender": {"user_id": user, "nickname": "小明", "card": "明明"},
            "message": [{"type": "text", "data": {"text": text}}]
        })
    }

    #[test]
    fn inbound_record_maps_a_group_frame_to_schema_v1() {
        let frame = json!({
            "time": 1000, "self_id": 9001, "post_type": "message", "message_type": "group",
            "message_id": -77, "group_id": 420042, "group_name": "测试群", "user_id": 1001,
            "sender": {"user_id": 1001, "nickname": "小明", "card": "明明"},
            "message": [
                {"type": "reply", "data": {"id": "-55"}},
                {"type": "at", "data": {"qq": "9001"}},
                {"type": "text", "data": {"text": " 今天吃什么"}},
                {"type": "image", "data": {}},
                {"type": "forward", "data": {}},
                {"type": "face", "data": {"id": "14"}},
                {"type": "record", "data": {}}
            ]
        });
        assert_eq!(
            inbound_record(&frame, 5000).unwrap(),
            json!({
                "v": 1, "src": "ingress", "conv": "group:420042", "dir": "in",
                "sender": {"id": "1001", "card": "明明", "nick": "小明"},
                "convName": "测试群", "t": 1_000_000, "rt": 5000, "mid": "-77",
                "segs": [
                    {"k": "reply", "mid": "-55"}, {"k": "at", "id": "9001"},
                    {"k": "text", "text": " 今天吃什么"}, {"k": "media", "kind": "image"},
                    {"k": "forward"}, {"k": "face", "id": "14"}, {"k": "media", "kind": "voice"}
                ]
            })
        );
    }

    #[test]
    fn own_echoes_and_private_sends_are_out() {
        let own_echo = group_frame(11, 9001, "回声");
        let record = inbound_record(&own_echo, 5000).unwrap();
        assert_eq!(record["conv"], "group:420042");
        assert_eq!(record["dir"], "out");

        let private_send = json!({
            "time": 1000, "self_id": 9001, "post_type": "message_sent", "message_type": "private",
            "message_id": 12, "target_id": 1002, "user_id": 9001, "message": [{"type": "text", "data": {"text": "私聊"}}]
        });
        let record = inbound_record(&private_send, 5000).unwrap();
        assert_eq!(record["conv"], "private:1002");
        assert_eq!(record["dir"], "out");

        let private_in = json!({
            "time": 1000, "self_id": 9001, "post_type": "message", "message_type": "private",
            "message_id": 13, "user_id": 1002, "sender": {"user_id": 1002}, "message": [{"type": "text", "data": {"text": "你好"}}]
        });
        let record = inbound_record(&private_in, 5000).unwrap();
        assert_eq!(record["conv"], "private:1002");
        assert_eq!(record["dir"], "in");
    }

    #[test]
    fn non_message_frames_are_not_records() {
        let notice = json!({"time": 1000, "self_id": 9001, "post_type": "notice", "notice_type": "notify", "sub_type": "poke", "group_id": 420042, "user_id": 1001});
        let heartbeat = json!({"time": 1000, "self_id": 9001, "post_type": "meta_event", "meta_event_type": "heartbeat"});
        let no_group = json!({"time": 1000, "self_id": 9001, "post_type": "message", "message_type": "group", "message_id": 1, "user_id": 1001, "message": []});
        assert!(inbound_record(&notice, 5000).is_none());
        assert!(inbound_record(&heartbeat, 5000).is_none());
        assert!(inbound_record(&no_group, 5000).is_none());
    }

    #[test]
    fn record_inbound_writes_only_when_enabled() {
        let frame = group_frame(21, 1001, "在吗").to_string();

        let off = runtime("her-history-off-", false);
        record_inbound(&off.context, &frame);
        assert!(history_lines(&off.root).is_empty());
        assert!(!off.root.join("data/history").exists());

        let on = runtime("her-history-on-", true);
        record_inbound(&on.context, &frame);
        let lines = history_lines(&on.root);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["src"], "ingress");
    }

    #[test]
    fn send_records_cover_text_voice_sticker_and_confirmations() {
        let text = json!({"k": "intent", "batch": "b1", "key": "group:420042", "kind": "text",
            "messages": ["早上好", "在吗"], "replyTo": -55, "atUser": 1001, "t": 1000});
        let records = send_records(&text, 0);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["segs"], json!([{"k": "reply", "mid": "-55"}, {"k": "at", "id": "1001"}, {"k": "text", "text": "早上好"}]));
        assert_eq!(records[1]["segs"], json!([{"k": "text", "text": "在吗"}]));
        assert_eq!((records[0]["idx"].clone(), records[1]["idx"].clone()), (json!(0), json!(1)));
        assert_eq!(records[0]["phase"], "intent");
        assert_eq!(records[0]["conv"], "group:420042");

        let voice = json!({"k": "intent", "batch": "b2", "key": "group:1", "kind": "voice", "messages": ["x"], "t": 1});
        assert_eq!(send_records(&voice, 0)[0]["segs"], json!([{"k": "media", "kind": "voice"}]));
        let sticker = json!({"k": "intent", "batch": "b3", "key": "group:1", "kind": "sticker", "messages": ["x"], "t": 1});
        assert_eq!(send_records(&sticker, 0)[0]["segs"], json!([{"k": "media", "kind": "sticker"}]));

        let confirmed = json!({"k": "confirmed", "batch": "b1", "idx": 1, "messageId": 4242, "t": 1000});
        let records = send_records(&confirmed, 0);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["mid"], "4242");
        assert_eq!(records[0]["phase"], "confirmed");
        assert_eq!(records[0]["idx"], json!(1));

        let attempt = json!({"k": "attempt", "batch": "b1", "idx": 0, "t": 1});
        assert!(send_records(&attempt, 0).is_empty());
    }

    #[test]
    fn append_failure_is_swallowed() {
        let on = runtime("her-history-fail-", true);
        // A regular file where the history directory must be makes the append fail.
        std::fs::write(on.root.join("data/history"), "not a directory").unwrap();
        record_inbound(&on.context, &group_frame(31, 1001, "失败不阻断").to_string());
        assert!(on.root.join("data/history").is_file());
    }

    #[test]
    fn file_for_uses_the_utc_day() {
        assert_eq!(file_for(0), "history/1970-01-01.jsonl");
        assert_eq!(file_for(86_399_999), "history/1970-01-01.jsonl");
        assert_eq!(file_for(86_400_000), "history/1970-01-02.jsonl");
        assert_eq!(file_for(-1), "history/1969-12-31.jsonl");
    }
}
