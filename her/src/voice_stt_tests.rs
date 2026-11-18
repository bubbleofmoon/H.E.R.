//! Unit tests of `voice_stt` (#170). No test downloads anything (`DOWNLOAD_FIXTURES`), contacts QQ or runs an engine:
//! the helper is a loopback fake (`fake_engine`) or the in-process `Fake`.

use super::*;
use std::io::{Read, Write};
use std::sync::Arc;

/// A SILK v3 record as QQ sends it (leading 0x02) with `packets` three-byte packets.
pub(crate) fn silk(packets: usize) -> Vec<u8> {
    let mut bytes = vec![0x02];
    bytes.extend_from_slice(b"#!SILK_V3");
    for index in 0..packets {
        bytes.extend_from_slice(&3i16.to_le_bytes());
        bytes.extend_from_slice(&[0xA5, (index % 251) as u8, 0x5A]);
    }
    bytes
}

/// One request the fake helper read: its request line, its `Content-Type` and its body.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) line: String,
    pub(crate) content_type: String,
    pub(crate) body: Vec<u8>,
}

pub(crate) struct FakeEngine {
    pub(crate) origin: String,
    pub(crate) requests: Arc<Mutex<Vec<Request>>>,
}

/// A loopback transcription helper. Each connection's request (head and `Content-Length` body) is read and recorded,
/// then answered with `reply` (raw HTTP bytes) and closed; with `reply` `None` the connection is held open and never
/// answered.
pub(crate) fn fake_engine(reply: Option<Vec<u8>>) -> FakeEngine {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let request = read_request(&mut stream);
            seen.lock().unwrap().push(request);
            match &reply {
                Some(bytes) => {
                    let _ = stream.write_all(bytes);
                }
                None => held.push(stream),
            }
        }
    });
    FakeEngine { origin, requests }
}

fn header(head: &str, name: &str) -> Option<String> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
    })
}

fn read_request(stream: &mut std::net::TcpStream) -> Request {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).to_string();
            let length = header(&head, "content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            if data.len() >= end + 4 + length {
                return Request {
                    line: head.split("\r\n").next().unwrap_or("").to_string(),
                    content_type: header(&head, "content-type").unwrap_or_default(),
                    body: data[end + 4..end + 4 + length].to_vec(),
                };
            }
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return Request { line: String::new(), content_type: String::new(), body: data },
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
}

/// A `200` answer carrying `json`.
pub(crate) fn ok_reply(json: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len())
        .into_bytes()
}

/// A selected runtime whose `config.json` holds `voice` as `voiceTranscription`.
fn runtime(label: &str, voice: Value) -> (PathBuf, RuntimeContext) {
    let root = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-stt-{label}-{}", crate::turn_ledger::mint_id(""))),
    );
    for dir in ["config", "data/logs", "resources", "user/dsh"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(
        root.join("config/config.json"),
        json!({"consoleToken": "synthetic-review-token", "voiceTranscription": voice}).to_string(),
    )
    .unwrap();
    std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
    std::fs::write(
        root.join("dependencies.json"),
        r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root).unwrap();
    (root, context)
}

fn record_message(url: &str) -> Value {
    json!([{"type": "record", "data": {"file": "0f0e.amr", "url": url}}])
}

/// Set the notice clock of `context`'s runtime (the test stand-in for the notice limiter's time).
fn notice_clock(context: &RuntimeContext, ms: i64) {
    NOTICE_CLOCK.lock().unwrap().insert(context.runtime_root().unwrap().to_path_buf(), ms);
}

/// A SILK record of about `bytes` bytes in 32 KB packets (1500 of them fit one minute).
fn big_silk(bytes: usize) -> Vec<u8> {
    let mut record = vec![0x02];
    record.extend_from_slice(b"#!SILK_V3");
    while record.len() < bytes {
        record.extend_from_slice(&32_000i16.to_le_bytes());
        record.extend(std::iter::repeat(0xA5u8).take(32_000));
    }
    record
}

/// A loopback helper that reads its request a kilobyte at a time, 20 ms apart, and hangs up after three seconds.
fn slow_reading_engine() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut buf = [0u8; 1024];
                while started.elapsed() < Duration::from_secs(3) {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            });
        }
    });
    origin
}

/// The owner notices admitted for `context`'s runtime.
fn notices(context: &RuntimeContext) -> Vec<String> {
    let root = context.runtime_root().unwrap().to_path_buf();
    NOTICES.lock().unwrap().iter().filter(|(r, ..)| *r == root).map(|(_, _, text, _)| text.clone()).collect()
}

/// The run-record lines of `context` naming `event`.
fn records(context: &RuntimeContext, event: &str) -> Vec<Value> {
    crate::recovery_log::flush(context);
    let text = std::fs::read_to_string(context.runtime_root().unwrap().join("data/run-record.jsonl")).unwrap_or_default();
    text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).filter(|record| record["event"] == event).collect()
}

/// Every file under `root` whose bytes contain `needle`.
fn files_holding(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if std::fs::read(&path).unwrap_or_default().windows(needle.len()).any(|w| w == needle) {
                found.push(path);
            }
        }
    }
    found
}

struct Fake {
    answer: Result<String, Failure>,
    calls: std::cell::Cell<usize>,
}

impl Fake {
    fn answering(text: &str) -> Self {
        Self { answer: Ok(text.to_string()), calls: std::cell::Cell::new(0) }
    }
}

impl Transcriber for Fake {
    fn transcribe(&self, _audio: &[u8], _deadline: Instant) -> Result<String, Failure> {
        self.calls.set(self.calls.get() + 1);
        self.answer.clone()
    }
}

#[test]
fn settings_are_off_by_default_and_clamp_each_bound_on_both_sides() {
    let off = Settings::from_config(&json!({}));
    assert!(!off.applies_to("group:1"), "no conversation is transcribed by default");
    assert_eq!(off.limits, Limits { max_bytes: 1_048_576, max_seconds: 60 });
    assert_eq!(off.timeout, Duration::from_millis(8_000));
    for (bytes, expected) in [(65_535u64, 65_536usize), (65_536, 65_536), (4_194_304, 4_194_304), (4_194_305, 4_194_304)] {
        let read = Settings::from_config(&json!({"voiceTranscription": {"maxBytes": bytes}})).limits.max_bytes;
        assert_eq!(read, expected, "maxBytes {bytes}");
    }
    for (ms, expected) in [(999u64, 1_000u64), (1_000, 1_000), (20_000, 20_000), (20_001, 20_000)] {
        let read = Settings::from_config(&json!({"voiceTranscription": {"timeoutMs": ms}})).timeout;
        assert_eq!(read, Duration::from_millis(expected), "timeoutMs {ms}");
    }
    for (seconds, expected) in [(0u64, 1u64), (1, 1), (60, 60), (61, 60)] {
        let read = Settings::from_config(&json!({"voiceTranscription": {"maxSeconds": seconds}})).limits.max_seconds;
        assert_eq!(read, expected, "maxSeconds {seconds}");
    }
    let odd = Settings::from_config(&json!({"voiceTranscription": {"maxBytes": "big", "conversations": ["group:1", 2, "private:3"]}}));
    assert_eq!(odd.limits.max_bytes, 1_048_576, "a value that is not a number reads as the default");
    assert!(odd.applies_to("group:1") && odd.applies_to("private:3"));
    assert!(!odd.applies_to("2") && !odd.applies_to("group:2"), "only key strings opt in");
}

#[test]
fn only_a_message_of_exactly_one_record_with_a_web_address_is_transcribed() {
    let url = "https://voice.fixture.test/1";
    assert_eq!(record_url(&record_message(url)).as_deref(), Some(url));
    let with_text = json!([{"type": "text", "data": {"text": "听"}}, {"type": "record", "data": {"url": url}}]);
    assert_eq!(record_url(&with_text).as_deref(), Some(url));
    let two = json!([{"type": "record", "data": {"url": url}}, {"type": "record", "data": {"url": url}}]);
    assert_eq!(record_url(&two), None, "two records are not one voice message");
    for message in [
        json!([{"type": "record", "data": {"file": "a.amr"}}]),
        json!([{"type": "record", "data": {"url": "file:///etc/passwd"}}]),
        json!([{"type": "record", "data": {"url": "ftp://voice.fixture.test/1"}}]),
        json!([{"type": "image", "data": {"url": url}}]),
        json!("[CQ:record,file=a.amr]"),
        Value::Null,
    ] {
        assert_eq!(record_url(&message), None, "{message}");
    }
}

#[test]
fn silk_framing_counts_packets_and_refuses_broken_framing() {
    assert_eq!(silk_packets(&silk(3)), Some(3));
    assert_eq!(silk_packets(&silk(2)[1..]), Some(2), "the leading 0x02 is optional");
    let mut ended = silk(2);
    ended.extend_from_slice(&(-1i16).to_le_bytes());
    assert_eq!(silk_packets(&ended), Some(2), "a final -1 ends the stream");
    let mut dtx = b"#!SILK_V3".to_vec();
    dtx.extend_from_slice(&0i16.to_le_bytes());
    assert_eq!(silk_packets(&dtx), Some(1), "a zero-length packet counts");
    let mut early = silk(1);
    early.extend_from_slice(&(-1i16).to_le_bytes());
    early.extend_from_slice(&[1, 2]);
    assert_eq!(silk_packets(&early), None, "a negative length before the end");
    let mut cut = silk(2);
    cut.pop();
    assert_eq!(silk_packets(&cut), None, "a cut packet");
    let mut half = silk(1);
    half.push(3);
    assert_eq!(silk_packets(&half), None, "half a length");
    assert_eq!(silk_packets(&silk(0)), None, "a header without packets is no record");
    assert_eq!(silk_packets(b"\x02#!SILK_V2\x03\x00abc"), None, "another header");
    assert_eq!(silk_packets(b"#!AMR\n"), None);
}

#[test]
fn a_record_longer_than_the_limit_or_not_silk_never_reaches_the_helper() {
    let (root, context) = runtime("bounds", Value::Null);
    let limits = Limits { max_bytes: 1 << 20, max_seconds: 1 };
    let deadline = || Instant::now() + Duration::from_secs(5);
    let fixtures = [("fifty", silk(50)), ("fifty-one", silk(51)), ("amr", b"#!AMR\n\x3c\x00\x00".to_vec()), ("small", silk(10))];
    for (name, bytes) in fixtures {
        DOWNLOAD_FIXTURES.lock().unwrap().insert(format!("https://voice.fixture.test/bounds-{name}"), bytes);
    }
    let url = |name: &str| format!("https://voice.fixture.test/bounds-{name}");
    let fake = Fake::answering("好");
    let fifty = transcribe_record(&context, &url("fifty"), limits, &fake, deadline());
    let fifty_one = transcribe_record(&context, &url("fifty-one"), limits, &fake, deadline());
    let amr = transcribe_record(&context, &url("amr"), limits, &fake, deadline());
    let missing = transcribe_record(&context, &url("missing"), limits, &fake, deadline());
    let tight = Limits { max_bytes: silk(10).len() - 1, max_seconds: 60 };
    let over_cap = transcribe_record(&context, &url("small"), tight, &fake, deadline());
    let calls = fake.calls.get();
    let silent = Fake::answering("<|zh|><|EMO_UNKNOWN|>");
    let empty = transcribe_record(&context, &url("small"), limits, &silent, deadline());
    let spent = transcribe_record(&context, &url("small"), limits, &fake, Instant::now());
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(fifty, Ok("好".to_string()), "50 packets fit one second");
    assert_eq!(fifty_one, Err(Failure::TooLong), "51 packets prove more than one second");
    assert_eq!(amr, Err(Failure::Unsupported));
    assert_eq!(missing, Err(Failure::Download));
    assert_eq!(over_cap, Err(Failure::Download), "one byte over maxBytes");
    assert_eq!(calls, 1, "only the record within the limits reached the helper");
    assert_eq!(empty, Err(Failure::Empty));
    assert_eq!(spent, Err(Failure::Deadline), "a spent deadline downloads nothing");
}

#[test]
fn the_transcript_loses_tags_brackets_and_line_breaks_and_is_capped() {
    assert_eq!(
        clean("<|zh|><|NEUTRAL|><|Speech|><|withitn|>今天[CQ:at,qq=1]去【会话令牌】abc\r\n好的。").as_deref(),
        Some("今天［CQ:at,qq=1］去〔会话令牌〕abc 好的。")
    );
    assert_eq!(clean("[语音]").as_deref(), Some("［语音］"), "a transcript never forms the voice label");
    assert_eq!(clean("<|zh|><|EMO_UNKNOWN|>  \n "), None);
    assert_eq!(clean(""), None);
    let exact = "字".repeat(MAX_TRANSCRIPT_CHARS);
    assert_eq!(clean(&exact), Some(exact.clone()), "600 characters stay whole");
    let over = "字".repeat(MAX_TRANSCRIPT_CHARS + 1);
    assert_eq!(clean(&over), Some(format!("{exact}…")), "601 are cut to 600 and marked");
}

#[test]
fn the_transcript_replaces_only_the_messages_own_voice_label() {
    let heard = label("你好");
    assert_eq!(apply("[引用 小明：[语音]] [语音]", &heard), format!("[引用 小明：[语音]] {heard}"));
    assert_eq!(apply("[语音]", &heard), heard);
    assert_eq!(apply("没有语音", &heard), "没有语音");
}

#[test]
fn the_label_names_no_denied_term_and_the_transcript_passes_the_model_view_strip() {
    assert!(
        crate::model_text_policy::denied_terms(&label("你好")).is_empty(),
        "the label names no harness, model maker or AI: {:?}",
        crate::model_text_policy::denied_terms(&label("你好"))
    );
    assert_eq!(LABEL, "[语音转文字]");
    let cfg = json!({"socialV2": {"context": {"stripFromModelText": ["秘密"]}}});
    assert_eq!(crate::model_view::strip_from_model_text(&label("说了秘密"), &cfg), label("说了"));
}

#[test]
fn the_helper_must_be_a_loopback_ip_origin() {
    for origin in ["http://127.0.0.1:9000", "http://[::1]:9000", "http://127.8.9.10:9000"] {
        assert!(HttpTranscriber::new(origin).is_some(), "{origin}");
    }
    for origin in [
        "http://192.168.1.5:9000",
        "http://10.0.0.2:9000",
        "http://8.8.8.8:80",
        "http://localhost:9000",
        "https://127.0.0.1:9000",
        "http://127.0.0.1:9000/",
        "http://127.0.0.1",
        "http://user@127.0.0.1:9000",
        "http://127.0.0.1:0",
        "",
    ] {
        assert!(HttpTranscriber::new(origin).is_none(), "member audio goes to this host only: {origin}");
    }
}

#[test]
fn the_helper_gets_the_record_bytes_and_its_text_is_read() {
    let engine = fake_engine(Some(ok_reply(r#"{"text":"你好","lang":"zh","emotion":"NEUTRAL"}"#)));
    let helper = HttpTranscriber::new(&engine.origin).unwrap();
    let audio = silk(3);
    let text = helper.transcribe(&audio, Instant::now() + Duration::from_secs(5));
    let requests = engine.requests.lock().unwrap().clone();
    assert_eq!(text, Ok("你好".to_string()));
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].line, "POST /v1/transcribe HTTP/1.1");
    assert_eq!(requests[0].content_type, "application/octet-stream");
    assert_eq!(requests[0].body, audio, "the record goes as it is");
}

#[test]
fn helper_failures_are_classified_and_bounded_by_the_deadline() {
    let ask = |reply: Vec<u8>| {
        let engine = fake_engine(Some(reply));
        HttpTranscriber::new(&engine.origin).unwrap().transcribe(&silk(2), Instant::now() + Duration::from_secs(5))
    };
    assert_eq!(ask(b"HTTP/1.1 500 Internal\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno".to_vec()), Err(Failure::Refused), "a 500 is a refusal");
    assert_eq!(ask(ok_reply("not json")), Err(Failure::InvalidReply));
    assert_eq!(ask(ok_reply(r#"{"lang":"zh"}"#)), Err(Failure::InvalidReply), "no text");
    assert_eq!(
        ask(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec()),
        Err(Failure::InvalidReply),
        "only a Content-Length body"
    );
    let fits = format!(r#"{{"text":"{}"}}"#, "a".repeat(REPLY_MAX_BYTES - 11));
    assert_eq!(fits.len(), REPLY_MAX_BYTES);
    assert_eq!(ask(ok_reply(&fits)), Ok("a".repeat(REPLY_MAX_BYTES - 11)), "an answer of exactly 16 KiB is read");
    let over = format!(r#"{{"text":"{}"}}"#, "a".repeat(REPLY_MAX_BYTES - 10));
    assert_eq!(ask(ok_reply(&over)), Err(Failure::InvalidReply), "one byte more is refused");

    let silent = fake_engine(None);
    let started = Instant::now();
    let held = HttpTranscriber::new(&silent.origin).unwrap().transcribe(&silk(2), started + Duration::from_millis(300));
    let waited = started.elapsed();
    assert_eq!(held, Err(Failure::TimedOut));
    assert!(waited < Duration::from_secs(2), "the deadline bounds a silent helper: {waited:?}");

    // Port 1: nothing listens there and no test fake can bind it (a privileged port), so the refusal is certain.
    let closed = HttpTranscriber::new("http://127.0.0.1:1").unwrap();
    assert_eq!(closed.transcribe(&silk(2), Instant::now() + Duration::from_secs(5)), Err(Failure::Unreachable));

    // A helper that reads its request slowly cannot stretch the deadline: the write is bounded, not only each step of
    // it, and the runtime's one transcription slot is free again afterwards.
    let (root, context) = runtime("slow-reader", Value::Null);
    let url = "https://voice.fixture.test/slow-reader";
    DOWNLOAD_FIXTURES.lock().unwrap().insert(url.to_string(), big_silk(48 << 20));
    let helper = HttpTranscriber::new(&slow_reading_engine()).unwrap();
    let limits = Limits { max_bytes: 64 << 20, max_seconds: 60 };
    let slot = context.runtime_root().unwrap().to_path_buf();
    let permit = Running::claim(&slot).expect("the slot is free");
    let started = Instant::now();
    let deadline = started + Duration::from_millis(600);
    let task_context = context.clone();
    let task = std::thread::spawn(move || {
        let _permit = permit;
        transcribe_record(&task_context, url, limits, &helper, deadline)
    });
    let slow = task.join().unwrap();
    let waited = started.elapsed();
    let freed = Running::claim(&slot).is_some();
    DOWNLOAD_FIXTURES.lock().unwrap().remove(url);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(slow, Err(Failure::TimedOut), "a helper that takes the record too slowly is a timeout");
    assert!(waited < Duration::from_secs(2), "the deadline bounds a slow reader: {waited:?}");
    assert!(freed, "the transcription slot is released");
}

#[test]
fn transcripts_are_kept_a_day_and_at_most_2000() {
    let (root, context) = runtime("store", Value::Null);
    let now = 10 * KEEP_MS;
    let store_file = root.join("data/voice/transcripts.json");
    std::fs::create_dir_all(root.join("data/voice")).unwrap();
    std::fs::write(&store_file, "not json").unwrap();
    remember(&context, "k-new", "新的", now).unwrap();
    let replaced = cached(&context, "k-new", now);
    let aged = json!({"v": 1, "entries": {
        "k-old": {"at": now - KEEP_MS, "text": "旧"},
        "k-young": {"at": now - KEEP_MS + 1, "text": "新近"}}});
    std::fs::write(&store_file, aged.to_string()).unwrap();
    let (old_read, young_read) = (cached(&context, "k-old", now), cached(&context, "k-young", now));
    remember(&context, "k-other", "另一个", now).unwrap();
    let after_age: Value = serde_json::from_str(&std::fs::read_to_string(&store_file).unwrap()).unwrap();
    let mut many = Map::new();
    for index in 0..KEEP_MAX {
        many.insert(format!("k-{index}"), json!({"at": now - 5_000 + index as i64, "text": "x"}));
    }
    std::fs::write(&store_file, json!({"v": 1, "entries": many}).to_string()).unwrap();
    remember(&context, "k-last", "最后", now - 5_000).unwrap();
    let after_cap: Value = serde_json::from_str(&std::fs::read_to_string(&store_file).unwrap()).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(replaced.as_deref(), Some("新的"), "an invalid store is replaced");
    assert_eq!(old_read, None, "an entry exactly a day old is not read");
    assert_eq!(young_read.as_deref(), Some("新近"));
    assert!(after_age["entries"].get("k-old").is_none(), "an entry exactly a day old is dropped: {after_age}");
    assert!(after_age["entries"].get("k-young").is_some() && after_age["entries"].get("k-other").is_some());
    let entries = after_cap["entries"].as_object().unwrap();
    assert_eq!(entries.len(), KEEP_MAX, "at most 2000 are kept");
    assert!(entries.get("k-0").is_none(), "the oldest goes first");
    assert!(entries.contains_key("k-1") && entries.contains_key("k-last"), "the new one stays even at the oldest time");
}

#[tokio::test]
async fn a_store_that_cannot_be_written_still_gives_the_transcript() {
    let engine = fake_engine(Some(ok_reply(r#"{"text":"记下来"}"#)));
    let (root, context) = runtime("unwritable", json!({"conversations": ["group:777003"], "helper": engine.origin}));
    std::fs::create_dir_all(root.join("data/voice/transcripts.json")).unwrap();
    let cfg = context.load_config().unwrap();
    let url = "https://voice.fixture.test/unwritable";
    DOWNLOAD_FIXTURES.lock().unwrap().insert(url.to_string(), silk(5));
    let heard = for_message(&context, &cfg, "group:777003", "group:777003|message:1", &record_message(url)).await;
    let failed = records(&context, "voice-transcript-store");
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(heard, Some(label("记下来")), "a full disk or a broken store loses only the replay copy");
    assert_eq!(failed.len(), 1, "the store failure is on the record");
    assert_eq!(failed[0]["component"], "stt");
}

#[test]
fn transfer_leftovers_are_removed_before_a_download() {
    let (root, context) = runtime("leftovers", Value::Null);
    std::fs::create_dir_all(root.join("data/voice/transfer")).unwrap();
    std::fs::write(root.join("data/voice/transfer/her-fetch-1-2-3.bin"), b"#!SILK_V3 left by a crash").unwrap();
    std::fs::write(root.join("data/voice/transfer/keep.txt"), b"other").unwrap();
    let folder = transfer_folder(&context).unwrap();
    let mut left: Vec<String> =
        std::fs::read_dir(&folder).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(left, ["keep.txt"], "a crashed download's file is removed before the next one");
}

#[tokio::test]
async fn an_opted_in_voice_reaches_the_model_as_its_transcript_and_a_replay_asks_the_helper_once() {
    let engine = fake_engine(Some(ok_reply(r#"{"text":"<|zh|><|NEUTRAL|><|Speech|>明天见"}"#)));
    let (root, context) = runtime("replay", json!({"conversations": ["private:10001"], "helper": engine.origin}));
    let cfg = context.load_config().unwrap();
    let url = "https://voice.fixture.test/replay";
    DOWNLOAD_FIXTURES.lock().unwrap().insert(url.to_string(), silk(25));
    let message = record_message(url);
    let first = for_message(&context, &cfg, "private:10001", "private:10001|message:7", &message).await;
    let again = for_message(&context, &cfg, "private:10001", "private:10001|message:7", &message).await;
    let other = for_message(&context, &cfg, "private:10002", "private:10002|message:8", &message).await;
    let requests = engine.requests.lock().unwrap().len();
    let leaked = files_holding(&root, b"#!SILK_V3");
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(first, Some(label("明天见")));
    assert_eq!(again, first, "a replay reads the kept transcript");
    assert_eq!(other, None, "a conversation that did not opt in keeps the label");
    assert_eq!(requests, 1, "the helper was asked once");
    assert!(leaked.is_empty(), "no file under the runtime holds the record's bytes: {leaked:?}");
}

#[tokio::test]
async fn no_helper_keeps_the_label_and_tells_the_owner_once() {
    let (root, context) = runtime("no-helper", json!({"conversations": ["group:777001"]}));
    let cfg = context.load_config().unwrap();
    let url = "https://voice.fixture.test/no-helper";
    DOWNLOAD_FIXTURES.lock().unwrap().insert(url.to_string(), silk(5));
    let first = for_message(&context, &cfg, "group:777001", "group:777001|message:1", &record_message(url)).await;
    let second = for_message(&context, &cfg, "group:777001", "group:777001|message:2", &record_message(url)).await;
    let told = notices(&context);
    let edges = records(&context, "voice-transcription-unavailable");
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!((first, second), (None, None), "the messages keep [语音]");
    assert_eq!(told.len(), 1, "the owner is told once per outage: {told:?}");
    assert!(told[0].contains("没有配置本机转写引擎"), "{told:?}");
    assert_eq!(edges.len(), 1, "one unavailable record: {edges:?}");
}

#[tokio::test]
async fn a_silent_helper_times_out_pauses_for_a_minute_and_its_recovery_is_told() {
    let silent = fake_engine(None);
    let (root, context) = runtime("silent", json!({"conversations": ["group:777002"], "helper": silent.origin, "timeoutMs": 1000}));
    let cfg = context.load_config().unwrap();
    for id in 1..=3 {
        DOWNLOAD_FIXTURES.lock().unwrap().insert(format!("https://voice.fixture.test/silent-{id}"), silk(5));
    }
    let message = |id: u32| record_message(&format!("https://voice.fixture.test/silent-{id}"));
    let started = Instant::now();
    let first = for_message(&context, &cfg, "group:777002", "group:777002|message:1", &message(1)).await;
    let waited = started.elapsed();
    let second = for_message(&context, &cfg, "group:777002", "group:777002|message:2", &message(2)).await;
    let asked = silent.requests.lock().unwrap().len();
    // The minute has passed (the pause ended a millisecond ago) and the helper answers again.
    health().lock().unwrap().get_mut(context.runtime_root().unwrap()).unwrap().cool_until =
        Instant::now().checked_sub(Duration::from_millis(1));
    let answering = fake_engine(Some(ok_reply(r#"{"text":"好了"}"#)));
    let cfg = json!({"voiceTranscription": {"conversations": ["group:777002"], "helper": answering.origin}});
    let third = for_message(&context, &cfg, "group:777002", "group:777002|message:3", &message(3)).await;
    let told = notices(&context);
    let recovered = records(&context, "voice-transcription-recovered");
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(first, None);
    assert!(waited < Duration::from_millis(2_500), "timeoutMs bounds the wait: {waited:?}");
    assert_eq!(second, None, "the pause keeps the label");
    assert_eq!(asked, 1, "the pause asked the helper nothing");
    assert_eq!(third, Some(label("好了")));
    assert_eq!(told.len(), 2, "one notice down, one up: {told:?}");
    assert!(told[0].contains("没有按时回复"), "{told:?}");
    assert_eq!(told[1], "语音转文字已恢复。");
    assert_eq!(recovered.len(), 1);
}

#[test]
fn the_helper_keeps_a_share_of_the_attempt_and_is_asked_only_with_time_left() {
    assert_eq!(helper_share(Duration::from_secs(8)), HELPER_RESERVE);
    assert_eq!(helper_share(HELPER_RESERVE * 2), HELPER_RESERVE, "half of six seconds is the whole reserve");
    assert_eq!(helper_share(HELPER_RESERVE * 2 - Duration::from_millis(1)), (HELPER_RESERVE * 2 - Duration::from_millis(1)) / 2, "less is half");
    assert_eq!(helper_share(Duration::from_secs(1)), Duration::from_millis(500), "a short attempt still leaves the download half");
    let now = Instant::now();
    assert!(helper_has_time(now + HELPER_FLOOR, now), "exactly the floor is enough");
    assert!(!helper_has_time(now + HELPER_FLOOR - Duration::from_millis(1), now), "a millisecond less is not");
    assert!(!helper_has_time(now, now) && !helper_has_time(now, now + Duration::from_secs(1)), "none left, or past");

    let (root, context) = runtime("reserve", Value::Null);
    let url = "https://voice.fixture.test/reserve";
    DOWNLOAD_FIXTURES.lock().unwrap().insert(url.to_string(), silk(5));
    let fake = Fake::answering("好");
    for (total, expected) in [(Duration::from_secs(8), Duration::from_secs(5)), (Duration::from_secs(1), Duration::from_millis(500))] {
        let got = transcribe_record(&context, url, Limits { max_bytes: 1 << 20, max_seconds: 60 }, &fake, Instant::now() + total);
        let budget = DOWNLOAD_BUDGETS.lock().unwrap().get(url).copied().unwrap();
        assert_eq!(got, Ok("好".to_string()));
        assert!(
            budget <= expected && budget > expected - Duration::from_millis(500),
            "of {total:?} the download gets all but the helper's share ({expected:?}): {budget:?}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_download_that_uses_its_share_leaves_the_helper_time_and_one_that_overruns_is_no_outage() {
    let engine = fake_engine(Some(ok_reply(r#"{"text":"到了"}"#)));
    let (root, context) =
        runtime("share", json!({"conversations": ["group:777006"], "helper": engine.origin, "timeoutMs": 1000}));
    let cfg = context.load_config().unwrap();
    let (share, overrun) = ("https://voice.fixture.test/share", "https://voice.fixture.test/overrun");
    for url in [share, overrun] {
        DOWNLOAD_FIXTURES.lock().unwrap().insert(url.to_string(), silk(5));
    }
    DOWNLOAD_DELAYS.lock().unwrap().insert(share.to_string(), DownloadDelay::Share);
    DOWNLOAD_DELAYS.lock().unwrap().insert(overrun.to_string(), DownloadDelay::Fixed(Duration::from_millis(1100)));
    let used = for_message(&context, &cfg, "group:777006", "group:777006|message:1", &record_message(share)).await;
    let asked_after_share = engine.requests.lock().unwrap().len();
    let overran = for_message(&context, &cfg, "group:777006", "group:777006|message:2", &record_message(overrun)).await;
    let asked = engine.requests.lock().unwrap().len();
    let told = notices(&context);
    let state = health().lock().unwrap().get(context.runtime_root().unwrap()).map(|state| (state.down, state.told));
    let failed = records(&context, "voice-transcription");
    let outages = records(&context, "voice-transcription-unavailable");
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(used, Some(label("到了")), "the helper still has its share after a download that took all of its own");
    assert_eq!(asked_after_share, 1);
    assert_eq!(overran, None, "the label stays");
    assert_eq!(asked, 1, "a healthy helper is not asked with no time left");
    assert!(told.is_empty(), "a slow download is no helper outage: {told:?}");
    assert_eq!(state, Some((false, false)), "the helper is not marked down");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["reason"].as_str(), Some(Failure::Deadline.reason()));
    assert!(outages.is_empty());
}

#[test]
fn an_outage_notice_the_limiter_refused_is_tried_again_and_a_recovery_has_its_own_key() {
    let (root, context) = runtime("notices", Value::Null);
    let cfg = json!({});
    let down = Err(Failure::Unreachable);
    notice_clock(&context, 0);
    settle(&context, &cfg, down);
    notice_clock(&context, 30_000);
    settle(&context, &cfg, Ok(()));
    notice_clock(&context, 40_000);
    settle(&context, &cfg, down);
    let after_refusal = notices(&context).len();
    notice_clock(&context, 70_000);
    settle(&context, &cfg, down);
    notice_clock(&context, 80_000);
    settle(&context, &cfg, down);
    let told = notices(&context);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(after_refusal, 2, "the second outage came within a minute of the first notice: refused");
    assert_eq!(told.len(), 3, "one outage notice each time it was admitted and nothing else: {told:?}");
    assert!(told[0].contains("连接不上") && told[2].contains("连接不上"), "{told:?}");
    assert_eq!(told[1], "语音转文字已恢复。", "the recovery is told although an outage was told a moment before");
}

#[test]
fn a_recovery_is_not_told_for_an_outage_the_owner_never_heard_of() {
    let (root, context) = runtime("untold", Value::Null);
    let cfg = json!({});
    notice_clock(&context, 0);
    assert!(tell_owner(&context, &cfg, "earlier", DOWN_KEY), "the stand-in admits the first notice of a key");
    notice_clock(&context, 10_000);
    settle(&context, &cfg, Err(Failure::Refused));
    notice_clock(&context, 100_000);
    settle(&context, &cfg, Ok(()));
    let told = notices(&context);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(told, ["earlier"], "the refused outage notice leaves nothing to recover from");
}

#[test]
fn the_pause_ends_exactly_at_its_deadline() {
    let now = Instant::now();
    assert!(cooling(Some(now + Duration::from_millis(1)), now), "a millisecond before the end it holds");
    assert!(!cooling(Some(now), now), "at exactly the deadline the pause has ended");
    assert!(!cooling(None, now));
}

#[tokio::test]
async fn a_second_voice_while_one_runs_keeps_the_label() {
    let engine = fake_engine(Some(ok_reply(r#"{"text":"不会被问到"}"#)));
    let (root, context) = runtime("busy", json!({"conversations": ["group:777004"], "helper": engine.origin}));
    let cfg = context.load_config().unwrap();
    let url = "https://voice.fixture.test/busy";
    DOWNLOAD_FIXTURES.lock().unwrap().insert(url.to_string(), silk(5));
    let held = Running::claim(context.runtime_root().unwrap()).expect("the test holds the runtime's one transcription");
    let result = for_message(&context, &cfg, "group:777004", "group:777004|message:1", &record_message(url)).await;
    drop(held);
    let asked = engine.requests.lock().unwrap().len();
    let skipped = records(&context, "voice-transcription");
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result, None, "the second voice keeps [语音]");
    assert_eq!(asked, 0);
    assert_eq!(skipped.len(), 1);
    assert_eq!((skipped[0]["outcome"].as_str(), skipped[0]["reason"].as_str()), (Some("skipped"), Some(Failure::Busy.reason())));
}

#[tokio::test]
async fn a_failed_download_keeps_the_label_and_is_no_helper_outage() {
    let engine = fake_engine(Some(ok_reply(r#"{"text":"不会被问到"}"#)));
    let (root, context) = runtime("download", json!({"conversations": ["group:777005"], "helper": engine.origin}));
    let cfg = context.load_config().unwrap();
    let url = "https://voice.fixture.test/no-fixture?rkey=secret-rkey-0451";
    let result = for_message(&context, &cfg, "group:777005", "group:777005|message:1", &record_message(url)).await;
    let failed = records(&context, "voice-transcription");
    let told = notices(&context);
    let cool_until = health().lock().unwrap().get(context.runtime_root().unwrap()).and_then(|state| state.cool_until);
    let leaked = files_holding(&root, b"secret-rkey");
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result, None);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["reason"].as_str(), Some(Failure::Download.reason()));
    assert!(told.is_empty(), "a download failure is not a helper outage: {told:?}");
    assert_eq!(cool_until, None, "and pauses nothing");
    assert!(leaked.is_empty(), "the record's address is never written: {leaked:?}");
}
