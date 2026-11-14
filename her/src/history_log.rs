//! Local chat history log (#58). When config `history.enabled` is true (isolated runtimes only),
//! ingress appends every message frame before self filtering, the allow list and mode dispatch,
//! and the guarded send boundary (`send_outbox::append_record`) appends each intent part and
//! confirmation after the outbox record is durable. Records use schema v1 from
//! `scripts/strip_history.py`, in `data/history/<UTC day>.jsonl`, appended without fsync and
//! never pruned. A failed append never blocks ingress or fails a send.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

pub const DIR: &str = "history";

/// Whether `history.enabled` is the boolean true in an isolated runtime. config.json is read only when its
/// length or modification time changes, so a disabled log costs one stat per frame.
pub fn enabled(context: &RuntimeContext) -> bool {
    type FlagCache = HashMap<String, ((u64, SystemTime), bool)>;
    static CACHE: Mutex<Option<FlagCache>> = Mutex::new(None);
    if !context.is_isolated() {
        return false;
    }
    let Some(identity) = config_identity(context) else { return false };
    let scope = scope_key(context);
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some((seen, flag)) = cache.get(&scope) {
        if *seen == identity {
            return *flag;
        }
    }
    let flag = read_flag(context);
    cache.insert(scope, (identity, flag));
    flag
}

fn scope_key(context: &RuntimeContext) -> String {
    context.runtime_root().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
}

fn config_identity(context: &RuntimeContext) -> Option<(u64, SystemTime)> {
    let path = context.resolve_path(RuntimeArea::Config, "config.json", PathIntent::ReadOptionalFile).ok()?;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().unwrap_or(UNIX_EPOCH)))
}

fn read_flag(context: &RuntimeContext) -> bool {
    #[cfg(test)]
    CONFIG_READS.lock().unwrap().push(scope_key(context));
    context.load_config().map(|cfg| cfg["history"]["enabled"].as_bool() == Some(true)).unwrap_or(false)
}

#[cfg(test)]
static CONFIG_READS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Config reads of `context`'s runtime so far (test hook for the enablement cache).
#[cfg(test)]
fn config_reads(context: &RuntimeContext) -> usize {
    let scope = scope_key(context);
    CONFIG_READS.lock().unwrap().iter().filter(|read| **read == scope).count()
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

const CARD_TYPES: [&str; 3] = ["json", "xml", "markdown"];

fn unescape(text: &str) -> String {
    text.replace("&#44;", ",").replace("&#91;", "[").replace("&#93;", "]").replace("&amp;", "&")
}

/// The `,key=value` parts and end offset of a CQ code whose parameters start at `pos`, or None when
/// the code does not close (`]` never comes, or a `[` appears first).
fn cq_close(message: &str, mut pos: usize) -> Option<(Vec<&str>, usize)> {
    let bytes = message.as_bytes();
    let mut parts = Vec::new();
    loop {
        match *bytes.get(pos)? {
            b']' => return Some((parts, pos + 1)),
            b',' => {
                let begin = pos + 1;
                let len = bytes[begin..].iter().take_while(|b| !matches!(b, b',' | b'[' | b']')).count();
                parts.push(&message[begin..begin + len]);
                pos = begin + len;
            }
            _ => return None,
        }
    }
}

/// CQ-string frame to the same items the array form carries, mirroring `parse_cq` in
/// scripts/snowluma_export.py: a card or unparsable code ends the message and its payload never
/// becomes text.
fn cq_items(message: &str) -> Vec<Value> {
    let mut items = Vec::new();
    let mut at = 0;
    while let Some(offset) = message[at..].find("[CQ:") {
        let start = at + offset;
        if start > at {
            items.push(json!({"type": "text", "data": {"text": unescape(&message[at..start])}}));
        }
        let name_start = start + 4;
        let name_len = message.as_bytes()[name_start..].iter().take_while(|b| b.is_ascii_alphabetic() || **b == b'_').count();
        let name = &message[name_start..name_start + name_len];
        let closed = if name.is_empty() { None } else { cq_close(message, name_start + name_len) };
        if CARD_TYPES.contains(&name) || closed.is_none() {
            let kind = if CARD_TYPES.contains(&name) { name } else { "other" };
            items.push(json!({"type": kind, "data": {}}));
            return items;
        }
        let (parts, end) = closed.expect("closed code");
        let mut data = serde_json::Map::new();
        for part in parts {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            data.insert(key.to_owned(), json!(unescape(value)));
        }
        items.push(json!({"type": name, "data": data}));
        at = end;
    }
    if at < message.len() {
        items.push(json!({"type": "text", "data": {"text": unescape(&message[at..])}}));
    }
    items
}

fn segments(message: &Value) -> Vec<Value> {
    let items = match message {
        Value::String(text) => cq_items(text),
        Value::Array(items) => items.clone(),
        _ => return Vec::new(),
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
    let t = event["time"].as_i64().filter(|s| *s > 0).and_then(|s| s.checked_mul(1000)).unwrap_or(received_ms);
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

/// Claims the one append-failure warning allowed per minute. False when another failure in that minute
/// already claimed it; the compare-exchange makes concurrent failures claim it once.
fn claim_warning(last: &AtomicI64, at_ms: i64) -> bool {
    let mut seen = last.load(Ordering::Relaxed);
    loop {
        if seen != i64::MIN && at_ms.saturating_sub(seen) < 60_000 {
            return false;
        }
        match last.compare_exchange_weak(seen, at_ms, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => seen = current,
        }
    }
}

fn append(context: &RuntimeContext, record: &Value, at_ms: i64) {
    let safe = crate::activity::mask_json_session_tokens(record, &crate::activity::known_session_tokens(context));
    let Ok(mut line) = serde_json::to_vec(&safe) else { return };
    line.push(b'\n');
    if context.append_bytes(RuntimeArea::Data, &file_for(at_ms), &line).is_err() {
        static LAST_WARN: AtomicI64 = AtomicI64::new(i64::MIN);
        if claim_warning(&LAST_WARN, at_ms) {
            crate::safe_println!("[history] append failed; ingress and sends continue");
        }
    }
}

/// Ingress hook: called for every non-replayed frame after meta events, before any filter.
pub fn record_inbound(context: &RuntimeContext, frame: &str) {
    if !enabled(context) { return; }
    let Ok(event) = serde_json::from_str::<Value>(frame) else { return };
    let received = crate::turn_ledger::now_ms();
    let Some(record) = inbound_record(&event, received) else { return };
    append(context, &record, received);
}

/// Send-boundary hook: called by `send_outbox::append_record` after the outbox record is durable.
pub fn record_send(context: &RuntimeContext, outbox: &Value) {
    if !enabled(context) { return; }
    let now = crate::turn_ledger::now_ms();
    for record in &send_records(outbox, now) { append(context, record, now); }
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
        runtime_with(tag, json!({"enabled": enabled}))
    }

    fn runtime_with(tag: &str, history: Value) -> Sandbox {
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id(tag)));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let config = json!({"consoleToken": "synthetic-review-token-abcdefghij", "history": history});
        std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        Sandbox { root, context }
    }

    fn sandbox_set_history(sandbox: &Sandbox, history: Value) {
        let config = json!({"consoleToken": "synthetic-review-token-abcdefghij", "history": history});
        std::fs::write(sandbox.root.join("config/config.json"), config.to_string()).unwrap();
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

    #[test]
    fn a_time_that_cannot_become_milliseconds_falls_back_to_the_receipt_time() {
        let mut frame = group_frame(41, 1001, "时间过大");
        frame["time"] = json!(i64::MAX);
        let record = inbound_record(&frame, 5000).unwrap();
        assert_eq!(record["t"], 5000);
        assert_eq!(record["rt"], 5000);
    }

    #[test]
    fn cq_string_frames_become_segments_and_never_leak_card_or_url_text() {
        let mut frame = group_frame(51, 1001, "");
        frame["message"] = json!(
            "[CQ:reply,id=-55][CQ:at,qq=9001] 看图[CQ:image,file=a.jpg,url=https://example.com/pic.jpg]\
             &#91;不是码&#93;[CQ:json,data=&#123;&quot;title&quot;:&quot;synthetic card title&quot;&#125;]\
             之后的文字 https://example.com/after"
        );
        let record = inbound_record(&frame, 5000).unwrap();
        assert_eq!(
            record["segs"],
            json!([
                {"k": "reply", "mid": "-55"}, {"k": "at", "id": "9001"}, {"k": "text", "text": " 看图"},
                {"k": "media", "kind": "image"}, {"k": "text", "text": "[不是码]"}, {"k": "media", "kind": "card"}
            ])
        );
        let text = record.to_string();
        assert!(!text.contains("pic.jpg") && !text.contains("synthetic card title") && !text.contains("之后的文字"), "{text}");

        frame["message"] = json!("前文[CQ:image,file=q.jpg 之后");
        let record = inbound_record(&frame, 5000).unwrap();
        assert_eq!(
            record["segs"],
            json!([{"k": "text", "text": "前文"}, {"k": "media", "kind": "other"}]),
            "an unclosed code is one other marker and the text after it is dropped"
        );
    }

    #[test]
    fn a_disabled_log_reads_the_config_once_until_the_file_changes() {
        let sandbox = runtime("her-history-cache-", false);
        let frame = group_frame(61, 1001, "未开启").to_string();
        for _ in 0..50 {
            record_inbound(&sandbox.context, &frame);
        }
        assert!(history_lines(&sandbox.root).is_empty());
        assert_eq!(config_reads(&sandbox.context), 1, "one config read serves every frame while config.json is unchanged");

        sandbox_set_history(&sandbox, json!({"enabled": true}));
        record_inbound(&sandbox.context, &frame);
        assert_eq!(history_lines(&sandbox.root).len(), 1, "a changed config.json is read again and switches the log on");
        assert_eq!(config_reads(&sandbox.context), 2);
    }

    #[test]
    fn one_append_warning_is_claimed_per_minute_even_when_failures_race() {
        let last = AtomicI64::new(i64::MIN);
        assert!(claim_warning(&last, 1_000), "the first failure claims the warning");
        assert!(!claim_warning(&last, 1_500), "a second failure inside the minute stays silent");
        assert!(!claim_warning(&last, 60_999), "59.999 s after the claim is still inside the minute");
        assert!(claim_warning(&last, 61_000), "a minute after the claim, the next warning is claimed");

        let raced = AtomicI64::new(i64::MIN);
        let claimed = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| claim_warning(&raced, 5_000))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).filter(|claim| *claim).count()
        });
        assert_eq!(claimed, 1, "concurrent failures in one minute claim the warning once");
    }

    #[test]
    fn only_a_boolean_true_switches_the_log_on() {
        for value in [json!("true"), json!(1), json!("yes"), json!(null)] {
            let sandbox = runtime_with("her-history-nonbool-", json!({"enabled": value.clone()}));
            record_inbound(&sandbox.context, &group_frame(71, 1001, "非布尔").to_string());
            assert!(history_lines(&sandbox.root).is_empty(), "history.enabled must be the boolean true, not {value}");
        }
    }

    #[test]
    fn a_legacy_runtime_never_turns_the_log_on() {
        assert!(!enabled(&RuntimeContext::legacy()));
    }

    #[test]
    fn session_tokens_are_masked_in_logged_text() {
        let sandbox = runtime("her-history-token-", true);
        let token = "abcdef".repeat(8);
        std::fs::write(
            sandbox.root.join("data/social-v2.json"),
            json!({"conversations": {"g1": {"agentToken": token}}}).to_string(),
        )
        .unwrap();
        record_inbound(&sandbox.context, &group_frame(81, 1001, &format!("令牌是 {token} 请保密")).to_string());
        let lines = history_lines(&sandbox.root);
        assert_eq!(lines.len(), 1);
        let logged = lines[0].to_string();
        assert!(!logged.contains(&token), "the session token must not reach the history log: {logged}");
        assert!(logged.contains("***"), "the token is replaced by the mask: {logged}");
    }
}
