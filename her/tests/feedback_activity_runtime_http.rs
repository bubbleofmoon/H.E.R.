//! Ticket 23: public HTTP behavior of AI feedback, the activity log and the pause switch in a
//! selected runtime root. Everything is synthetic: a loopback OneBot fake and disposable roots.
//! Unix decoys reproduce relative `C:/qqbot/...` defaults. Windows foreign sentinels stay
//! inside the disposable root; selected-root results and unchanged foreign bytes are asserted.
#[path = "../src/install_root.rs"]
mod install_root;
mod support;

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSOLE: &str = "ticket23-console-token-abcdefghij";
const AGENT_A: &str = "ticket23-agent-token-a";
const AGENT_B: &str = "ticket23-agent-token-b";
const KEY_A: &str = "group:70001";
const KEY_B: &str = "group:70002";
const AGENT_P: &str = "ticket23-agent-token-p";
const KEY_P: &str = "private:9002";
const OWNER: i64 = 9001;

struct Root(PathBuf, Option<Vec<(PathBuf, Vec<u8>)>>);
impl Root {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "ticket23-feedback-{label}-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        for dir in ["config", "data", "resources"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        Self(path, None)
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
    fn json(&self, rel: &str) -> Value {
        serde_json::from_slice(&fs::read(self.p(rel)).unwrap()).unwrap()
    }
    #[allow(dead_code)]
    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.p(rel)).unwrap_or_default()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        if let (Some(before), false) = (self.1.take(), thread::panicking()) {
            let after = legacy_snapshot(self);
            let _ = fs::remove_dir_all(&self.0);
            assert_eq!(after, before, "default production paths were touched");
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Selected root: two conversations (A, B) with distinct session tokens, `social_extra` merged
/// into `socialV2`, `extra` merged into the config top level.
fn fixture(label: &str, onebot_url: &str, social_extra: Value, extra: Value) -> Root {
    let root = Root::new(label);
    let mut social = json!({ "send": { "maxSendPerMinute": 0, "maxSendPerHour": 0, "firstGapEnabled": false, "burstIntervalMinMs": 0, "maxGapMs": 0 } });
    for (k, v) in social_extra.as_object().cloned().unwrap_or_default() {
        social[&k] = v;
    }
    let mut config = json!({
        "consoleToken": CONSOLE,
        "ownerQQ": OWNER,
        "allow": { "groups": [70001, 70002], "private": [9002] },
        "snowluma": { "httpUrl": "http://127.0.0.1:1", "accessToken": "ticket23-onebot-token" },
        "socialV2": social
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        config[&k] = v;
    }
    fs::write(root.p("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let state = json!({ "paused": false, "conversations": {
        KEY_A: { "agentToken": AGENT_A, "sendTimes": [], "recentMessages": [], "unread": [] },
        KEY_B: { "agentToken": AGENT_B, "sendTimes": [], "recentMessages": [], "unread": [] },
        KEY_P: { "agentToken": AGENT_P, "sendTimes": [], "recentMessages": [], "unread": [] }
    }});
    fs::write(root.p("data/social-v2.json"), state.to_string()).unwrap();
    fs::write(root.p("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    fs::write(
        root.p("dependencies.json"),
        json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:2","onebot":onebot_url}).to_string(),
    )
    .unwrap();
    install_legacy_decoy(&root);
    let mut root = root;
    root.1 = Some(legacy_snapshot(&root));
    root
}

fn install_legacy_decoy(root: &Root) {
    for (rel, bytes) in [
        ("C:/qqbot/qq-bridge/state/feedback.json", &b"[{\"id\":\"legacy\",\"key\":\"group:1\",\"level\":\"info\",\"message\":\"legacy\",\"time\":\"t\"}]"[..]),
        ("C:/qqbot/qq-bridge/state/qq-activity.log", b"[00:00:00] legacy decoy\n"),
        ("C:/qqbot/qq-bridge/state/social-v2.json", b"{}"),
        ("C:/qqbot/qq-bridge/config.json", b"{}"),
    ] {
        let rel = if cfg!(windows) { rel.strip_prefix("C:/").unwrap() } else { rel };
        let path = root.p("cwd").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

fn legacy_snapshot(root: &Root) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.p("cwd"), &mut out);
    out
}

struct Console {
    child: Child,
    port: u16,
}
impl Console {
    fn start(root: &Root) -> Self {
        let (child, port) = crate::support::ports::start_console(Duration::from_secs(10), |port| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
            command
                .args(["--console-serve", "--runtime-root"])
                .arg(&root.0)
                .args(["--console-port", &port.to_string()])
                .env_remove("HER_RUNTIME_ROOT")
                .current_dir(root.p("cwd"))
                .stdin(Stdio::null())
                .stdout(fs::File::create(root.p("console.stdout.log")).unwrap())
                .stderr(fs::File::create(root.p("console.stderr.log")).unwrap());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            command.spawn().expect("start console")
        });
        let mut console = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(console.child.try_wait().unwrap().is_none(), "console exited early");
            if http(port, "GET", "/api/health", None, None, &Value::Null).map(|r| r.0 == 200).unwrap_or(false) {
                return console;
            }
            assert!(Instant::now() < deadline, "console did not become healthy");
            thread::sleep(Duration::from_millis(30));
        }
    }
    fn admin(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        http(self.port, method, path, Some(CONSOLE), None, &body).unwrap()
    }
    fn agent(&self, token: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
        http(self.port, method, path, Some(CONSOLE), Some(token), &body).unwrap()
    }
    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    console: Option<&str>,
    agent: Option<&str>,
    body: &Value,
) -> Result<(u16, Value), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = if body.is_null() { String::new() } else { body.to_string() };
    let mut headers = String::new();
    if let Some(v) = console {
        headers += &format!("X-Console-Token: {v}\r\n");
    }
    if let Some(v) = agent {
        headers += &format!("X-Agent-Token: {v}\r\n");
    }
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let raw = String::from_utf8(raw).map_err(|e| e.to_string())?;
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("no header separator")?;
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).ok_or("bad status")?;
    Ok((status, serde_json::from_str(body).map_err(|e| format!("{e}: {body}"))?))
}

/// Loopback OneBot that records every action and answers ok.
struct OneBot {
    url: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl OneBot {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (seen2, stop2) = (seen.clone(), stop.clone());
        let counter = Arc::new(AtomicU64::new(0));
        let worker = thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let (seen, counter) = (seen2.clone(), counter.clone());
                        thread::spawn(move || serve(stream, seen, counter));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self { url, seen, stop, worker: Some(worker) }
    }
    fn seen(&self, action: &str) -> Vec<Value> {
        self.seen.lock().unwrap().iter().filter(|s| s.0 == action).map(|s| s.1.clone()).collect()
    }
    fn wait_for(&self, action: &str, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = self.seen(action);
            if got.len() >= count || Instant::now() > deadline {
                return got;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for OneBot {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn serve(stream: TcpStream, seen: Arc<Mutex<Vec<(String, Value)>>>, counter: Arc<AtomicU64>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let Some(path) = line.split_whitespace().nth(1).map(str::to_string) else { return };
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let action = path.trim_start_matches('/').to_string();
    seen.lock().unwrap().push((action, body));
    let id = 91000 + counter.fetch_add(1, Ordering::Relaxed);
    let response = json!({"status":"ok","retcode":0,"data":{"message_id":id}}).to_string();
    let stream = reader.get_mut();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
}

fn entries(console: &Console) -> Vec<Value> {
    let (status, body) = console.admin("GET", "/api/socialV2/feedback", Value::Null);
    assert_eq!(status, 200, "{body}");
    body["entries"].as_array().cloned().unwrap()
}

fn history() -> Value {
    json!([
        { "id": "old1-aaaaaa", "key": KEY_A, "level": "info", "message": "history one", "time": "2026-10-01T00:00:00.000Z" },
        { "id": "old2-bbbbbb", "key": KEY_B, "level": "error", "message": "history two", "time": "2026-10-02T00:00:00.000Z" }
    ])
}

fn id_shape_ok(id: &str) -> bool {
    let Some((head, tail)) = id.split_once('-') else { return false };
    !head.is_empty()
        && head.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        && tail.len() == 6
        && tail.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

#[test]
fn history_stays_visible_and_new_feedback_appends_per_session() {
    let ob = OneBot::start();
    let root = fixture("append", &ob.url, json!({}), json!({}));
    fs::write(root.p("data/feedback.json"), history().to_string()).unwrap();
    let console = Console::start(&root);

    assert_eq!(entries(&console), history().as_array().cloned().unwrap(), "imported history is returned unchanged");

    let (status, body) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "level": "warning", "message": "  needs help  "}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "key": KEY_A, "level": "warning", "message": "needs help"}));

    let (status, _) = console.agent(AGENT_B, "POST", "/api/socialV2/feedback", json!({"key": KEY_B, "level": "weird", "message": "other"}));
    assert_eq!(status, 200);
    let (status, body) = console.admin("POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "from console"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["level"], "info");

    let all = entries(&console);
    assert_eq!(all.len(), 5);
    assert_eq!(&all[..2], history().as_array().unwrap().as_slice(), "history is kept in front, in order");
    let tail: Vec<(String, String, String)> = all[2..]
        .iter()
        .map(|e| (e["key"].as_str().unwrap().to_string(), e["level"].as_str().unwrap().to_string(), e["message"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        tail,
        vec![
            (KEY_A.to_string(), "warning".to_string(), "needs help".to_string()),
            (KEY_B.to_string(), "info".to_string(), "other".to_string()),
            (KEY_A.to_string(), "info".to_string(), "from console".to_string()),
        ]
    );
    for e in &all[2..] {
        assert!(id_shape_ok(e["id"].as_str().unwrap()), "id shape: {e}");
        assert!(e["time"].as_str().unwrap().ends_with('Z') && e["time"].as_str().unwrap().contains('T'));
    }
    assert_eq!(root.json("data/feedback.json").as_array().unwrap().len(), 5, "persisted in the selected root");
    drop(console);
}

#[test]
fn feedback_rules_follow_the_js_checks_in_order() {
    let ob = OneBot::start();
    let root = fixture("rules", &ob.url, json!({ "feedback": { "maxLength": 12 }, "tools": { "feedback": true } }), json!({}));
    let console = Console::start(&root);
    let post = |token: Option<&str>, body: Value| match token {
        Some(t) => console.agent(t, "POST", "/api/socialV2/feedback", body),
        None => console.admin("POST", "/api/socialV2/feedback", body),
    };

    let (s, b) = post(Some(AGENT_A), json!({"key": KEY_A, "message": "   "}));
    assert_eq!((s, b["error"].as_str().unwrap()), (400, "key 和 message 不能为空"));
    let (s, b) = post(None, json!({"message": "x"}));
    assert_eq!((s, b["error"].as_str().unwrap()), (400, "key 和 message 不能为空"));
    // A token of another session is not valid for this key, and neither is an unknown key.
    let (s, b) = post(Some(AGENT_B), json!({"key": KEY_A, "message": "x"}));
    assert_eq!((s, b["error"].as_str().unwrap()), (403, "agent token 无效"));
    let (s, _) = post(Some(AGENT_A), json!({"key": "group:99999", "message": "x"}));
    assert_eq!(s, 403);
    assert!(entries(&console).is_empty(), "refused calls write nothing");

    // maxLength truncates the stored and the returned text.
    let (s, b) = post(Some(AGENT_A), json!({"key": KEY_A, "message": "abcdefghijabcdefghij"}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["message"], "abcdefghijab");
    assert_eq!(entries(&console)[0]["message"], "abcdefghijab");

    // Operator setting changes take effect on the next call (maxLength 5, then tool off).
    let (s, b) = console.admin("POST", "/api/socialV2/config", json!({"feedback": {"maxLength": 5}}));
    assert_eq!(s, 200, "{b}");
    let (_, b) = post(Some(AGENT_A), json!({"key": KEY_A, "message": "abcdefghij"}));
    assert_eq!(b["message"], "abcde");
    let (s, _) = console.admin("POST", "/api/socialV2/config", json!({"tools": {"feedback": false}}));
    assert_eq!(s, 200);
    let (s, b) = post(Some(AGENT_A), json!({"key": KEY_A, "message": "x"}));
    assert_eq!((s, b["error"].as_str().unwrap()), (403, "工具未启用：qq_report_feedback"));
    let (s, _) = post(None, json!({"key": KEY_A, "message": "operator can still file"}));
    assert_eq!(s, 200, "the console channel is not gated by the model tool switch");
    assert_eq!(entries(&console).len(), 3);
    drop(console);
}

#[test]
fn feedback_is_rate_limited_per_session_and_the_sixth_in_a_minute_is_refused() {
    let ob = OneBot::start();
    let root = fixture("rate", &ob.url, json!({}), json!({}));
    let console = Console::start(&root);
    for i in 0..5 {
        let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": format!("m{i}")}));
        assert_eq!(s, 200, "{b}");
    }
    let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "sixth"}));
    assert_eq!((s, b["error"].as_str().unwrap()), (429, "反馈过于频繁，请稍后再试"));
    // Another session's quota is separate.
    let (s, _) = console.agent(AGENT_B, "POST", "/api/socialV2/feedback", json!({"key": KEY_B, "message": "fresh"}));
    assert_eq!(s, 200);
    assert_eq!(entries(&console).len(), 6);
    drop(console);
}

#[test]
fn sensitive_text_is_stored_redacted_but_returned_as_sent_and_history_is_capped() {
    let ob = OneBot::start();
    let root = fixture("redact", &ob.url, json!({}), json!({}));
    let old: Vec<Value> = (0..499)
        .map(|i| json!({"id": format!("h{i}-aaaaaa"), "key": KEY_A, "level": "info", "message": format!("h{i}"), "time": "t"}))
        .collect();
    fs::write(root.p("data/feedback.json"), Value::Array(old).to_string()).unwrap();
    let console = Console::start(&root);
    let secret = format!("path /root/secret/file and token=abcdef123 and {AGENT_A} end");
    let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": secret}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["message"], secret, "JS answers with the unredacted text");
    let stored = entries(&console);
    assert_eq!(stored.len(), 500);
    assert_eq!(stored[499]["message"], "path *** and *** and *** end");
    // One more entry evicts the oldest.
    let (s, _) = console.agent(AGENT_B, "POST", "/api/socialV2/feedback", json!({"key": KEY_B, "message": "newest"}));
    assert_eq!(s, 200);
    let stored = entries(&console);
    assert_eq!(stored.len(), 500);
    assert_eq!(stored[0]["message"], "h1");
    assert_eq!(stored[499]["message"], "newest");
    drop(console);
}

#[test]
fn clear_is_console_only_and_agents_cannot_read_the_list() {
    let ob = OneBot::start();
    let root = fixture("clear", &ob.url, json!({}), json!({}));
    fs::write(root.p("data/feedback.json"), history().to_string()).unwrap();
    let console = Console::start(&root);
    let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback-clear", json!({}));
    assert_eq!((s, b["error"].as_str().unwrap()), (403, "该接口仅控制台可用"));
    let (s, b) = console.agent(AGENT_A, "GET", "/api/socialV2/feedback", Value::Null);
    assert_eq!((s, b["error"].as_str().unwrap()), (403, "该接口仅控制台可用"));
    assert_eq!(entries(&console).len(), 2, "refused calls change nothing");
    let (s, b) = console.admin("POST", "/api/socialV2/feedback-clear", json!({}));
    assert_eq!((s, b), (200, json!({"ok": true})));
    assert!(entries(&console).is_empty());
    assert_eq!(root.json("data/feedback.json"), json!([]));
    drop(console);
}

#[test]
fn concurrent_writers_lose_no_feedback() {
    let ob = OneBot::start();
    let root = fixture("concurrent", &ob.url, json!({}), json!({}));
    fs::write(root.p("data/feedback.json"), history().to_string()).unwrap();
    let console = Console::start(&root);
    let port = console.port;
    // Five writers per session fit inside the per-session limit.
    let handles: Vec<_> = (0..10)
        .map(|i| {
            thread::spawn(move || {
                let key = if i % 2 == 0 { KEY_A } else { KEY_B };
                let token = if i % 2 == 0 { AGENT_A } else { AGENT_B };
                http(port, "POST", "/api/socialV2/feedback", Some(CONSOLE), Some(token), &json!({"key": key, "message": format!("concurrent {i}")})).unwrap()
            })
        })
        .collect();
    for h in handles {
        let (s, b) = h.join().unwrap();
        assert_eq!(s, 200, "{b}");
    }
    let all = entries(&console);
    assert_eq!(all.len(), 12, "two history entries plus ten concurrent writes");
    let mut messages: Vec<String> = all[2..].iter().map(|e| e["message"].as_str().unwrap().to_string()).collect();
    messages.sort();
    let mut want: Vec<String> = (0..10).map(|i| format!("concurrent {i}")).collect();
    want.sort();
    assert_eq!(messages, want);

    drop(console);
}

fn backups(root: &Root) -> Vec<(String, Vec<u8>)> {
    let mut found: Vec<_> = fs::read_dir(root.p("data"))
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("feedback.json.corrupt-").then(|| (name, fs::read(e.path()).unwrap()))
        })
        .collect();
    found.sort();
    found
}

#[test]
fn a_damaged_store_is_renamed_to_a_backup_and_then_behaves_like_js_empty() {
    let ob = OneBot::start();
    let root = fixture("damaged", &ob.url, json!({}), json!({}));
    fs::write(root.p("data/feedback.json"), history().to_string()).unwrap();
    let console = Console::start(&root);

    // Read of a half-written file: JS `readJsonSafe(..., [])` answers an empty list; the damaged
    // bytes are kept under a timestamped name next to the store (user decision 2026-10-30).
    fs::write(root.p("data/feedback.json"), b"{not json, partly written").unwrap();
    let (s, b) = console.admin("GET", "/api/socialV2/feedback", Value::Null);
    assert_eq!((s, b), (200, json!({"ok": true, "entries": []})));
    let kept = backups(&root);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].1, b"{not json, partly written");
    assert!(!root.p("data/feedback.json").exists(), "the damaged file was renamed away, not copied");

    // Append after damage: a new list starts (JS `appendFeedbackEntry`).
    fs::write(root.p("data/feedback.json"), b"{\"valid\":\"json but not an array\"}").unwrap();
    let (s, b) = console.admin("POST", "/api/socialV2/feedback", json!({"key": KEY_P, "message": "after damage"}));
    assert_eq!(s, 200, "{b}");
    let all = entries(&console);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["message"], "after damage");
    let kept = backups(&root);
    assert_eq!(kept.len(), 2, "a second damaged file gets its own backup: {kept:?}");
    assert!(kept.iter().any(|(_, bytes)| bytes == b"{\"valid\":\"json but not an array\"}"));
    assert!(kept.iter().any(|(_, bytes)| bytes == b"{not json, partly written"));

    // Healthy store: no backup is made.
    let (s, _) = console.admin("POST", "/api/socialV2/feedback", json!({"key": KEY_P, "message": "second"}));
    assert_eq!(s, 200);
    assert_eq!(entries(&console).len(), 2);
    assert_eq!(backups(&root).len(), 2);
    drop(console);
}

#[test]
fn one_conversation_cannot_post_under_another_and_feedback_survives_restart() {
    let ob = OneBot::start();
    let root = fixture("restart", &ob.url, json!({}), json!({}));
    let console = Console::start(&root);
    let (s, _) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "before restart"}));
    assert_eq!(s, 200);
    let (s, _) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_B, "message": "spoof"}));
    assert_eq!(s, 403);
    console.stop();
    let console = Console::start(&root);
    let all = entries(&console);
    assert_eq!(all.len(), 1);
    assert_eq!((all[0]["key"].as_str().unwrap(), all[0]["message"].as_str().unwrap()), (KEY_A, "before restart"));
    drop(console);
}

#[test]
fn error_feedback_notifies_the_owner_once_per_window_only_when_enabled() {
    let ob = OneBot::start();
    let root = fixture("notify", &ob.url, json!({ "feedback": { "notifyOwnerOnError": true } }), json!({}));
    let console = Console::start(&root);
    let (s, _) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "level": "info", "message": "just info"}));
    assert_eq!(s, 200);
    let (s, _) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "level": "error", "message": "broken tool"}));
    assert_eq!(s, 200);
    let sent = ob.wait_for("send_private_msg", 1);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["user_id"], OWNER);
    assert!(sent[0].to_string().contains("[group:70001] AI 报错反馈：broken tool"), "{}", sent[0]);
    // Same dedupe key within 60 s: recorded, not pushed again.
    let (s, _) = console.agent(AGENT_B, "POST", "/api/socialV2/feedback", json!({"key": KEY_B, "level": "error", "message": "second error"}));
    assert_eq!(s, 200);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(ob.seen("send_private_msg").len(), 1, "60 s dedupe");
    assert_eq!(entries(&console).len(), 3);
    // Setting off: a fresh process (empty dedupe) sends nothing.
    let (s, _) = console.admin("POST", "/api/socialV2/config", json!({"feedback": {"notifyOwnerOnError": false}}));
    assert_eq!(s, 200);
    console.stop();
    let console = Console::start(&root);
    let (s, _) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "level": "error", "message": "quiet now"}));
    assert_eq!(s, 200);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(ob.seen("send_private_msg").len(), 1);
    drop(console);
}

#[test]
fn feedback_is_refused_for_the_model_while_paused_but_not_for_the_operator() {
    let ob = OneBot::start();
    let root = fixture("gates", &ob.url, json!({}), json!({}));
    let console = Console::start(&root);
    let (s, _) = console.admin("POST", "/api/socialV2/activity", json!({"paused": true}));
    assert_eq!(s, 200);
    let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "x"}));
    assert_eq!((s, b["error"].as_str().unwrap()), (403, "AI 已暂停，当前不允许执行 v2 工具"));
    let (s, _) = console.admin("POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "operator note"}));
    assert_eq!(s, 200);
    let (s, _) = console.admin("POST", "/api/socialV2/activity", json!({"paused": false}));
    assert_eq!(s, 200);
    let (s, _) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "back"}));
    assert_eq!(s, 200);
    assert_eq!(entries(&console).len(), 2);
    drop(console);
}

fn status_activity(console: &Console) -> Vec<String> {
    let (s, b) = console.admin("GET", "/api/status", Value::Null);
    assert_eq!(s, 200, "{b}");
    b["activity"].as_str().expect("status carries the activity tail").lines().map(str::to_string).collect()
}

/// `[HH:MM:SS] text` -> text.
fn unclocked(line: &str) -> &str {
    let (clock, rest) = line.split_once("] ").expect(line);
    assert!(clock.len() == 9 && clock.starts_with('[') && clock.as_bytes()[3] == b':' && clock.as_bytes()[6] == b':', "{line}");
    rest
}

#[test]
fn sends_feedback_and_pokes_leave_activity_lines_the_status_shows_to_the_operator_only() {
    let ob = OneBot::start();
    let root = fixture("activity", &ob.url, json!({}), json!({}));
    let console = Console::start(&root);
    assert_eq!(status_activity(&console), Vec::<String>::new(), "no log yet: empty tail");

    let (s, _) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "hello\r\nthere"}));
    assert_eq!(s, 200);
    let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/send-message", json!({"key": KEY_A, "messages": ["alpha"]}));
    assert_eq!(s, 200, "{b}");
    let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/send-burst", json!({"key": KEY_A, "messages": ["beta"]}));
    assert_eq!(s, 200, "{b}");
    let (s, b) = console.admin("POST", "/api/send/group", json!({"groupId": 70001, "token": AGENT_A, "message": "gamma"}));
    assert_eq!(s, 200, "{b}");
    let (s, b) = console.agent(AGENT_P, "POST", "/api/socialV2/send-poke", json!({"key": KEY_P}));
    assert_eq!(s, 200, "{b}");
    // Refused calls leave nothing behind.
    let (s, _) = console.agent(AGENT_B, "POST", "/api/socialV2/send-message", json!({"key": KEY_A, "messages": ["denied"]}));
    assert_eq!(s, 403);

    let lines = status_activity(&console);
    let texts: Vec<&str> = lines.iter().map(|l| unclocked(l)).collect();
    assert_eq!(
        texts,
        vec![
            "group:70001 [reserved2] AI 反馈 [info]：hello there",
            "group:70001 [reserved2] 工具统一发送：成功 1/1 条",
            "group:70001 [reserved2] 工具分条发送：成功 1/1 条",
            "group:70001 [send] 成功 1/1 条：gamma",
            "private:9002 [reserved2] 工具拍一拍",
        ]
    );
    assert_eq!(root.text("data/qq-activity.log").lines().count(), 5, "persisted in the selected root's data dir");
    // A session token (agent) never sees the cross-session summary.
    let (s, b) = console.agent(AGENT_A, "GET", "/api/status", Value::Null);
    assert_eq!(s, 200);
    assert!(b.get("activity").is_none() && b.get("ownerQQ").is_none(), "{b}");
    drop(console);
}

#[test]
fn activity_text_is_redacted_and_the_log_rotates_at_five_hundred_lines() {
    let ob = OneBot::start();
    let root = fixture("rotate", &ob.url, json!({ "feedback": { "maxLength": 400 } }), json!({}));
    let seed: String = (0..500).map(|i| format!("[00:00:00] seeded {i}\n")).collect();
    fs::write(root.p("data/qq-activity.log"), seed).unwrap();
    let console = Console::start(&root);
    let (s, _) = console.admin("POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": format!("see /root/private/file and {AGENT_A}")}));
    assert_eq!(s, 200);
    // 500 seeded lines + this one is 501 > 500: the file moved to the backup, the new file is empty.
    assert_eq!(root.text("data/qq-activity.log"), "");
    let backup = root.text("data/qq-activity.log.1");
    assert_eq!(backup.lines().count(), 501);
    let last = backup.lines().last().unwrap();
    assert_eq!(unclocked(last), "group:70001 [reserved2] AI 反馈 [info]：see *** and ***");
    assert_eq!(status_activity(&console), Vec::<String>::new());
    drop(console);
}

/// Block atomic replacement of a state file, restoring its original permissions on drop.
struct Unwritable {
    path: PathBuf,
    original_permissions: Option<fs::Permissions>,
    active: bool,
    #[cfg(windows)]
    handle: Option<fs::File>,
}

impl Unwritable {
    fn new(path: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Permit reads but withhold write/delete sharing so atomic replacement
            // fails, then release this exact handle to restore writes.
            let handle = fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(path)
                .unwrap();
            Self {
                path: path.to_path_buf(),
                original_permissions: None,
                active: true,
                handle: Some(handle),
            }
        }
        #[cfg(unix)]
        {
            let is_root = Command::new("id")
                .arg("-u")
                .output()
                .map(|out| out.stdout == b"0\n")
                .unwrap_or(false);
            if is_root {
                let status = Command::new("chattr")
                    .arg("+i")
                    .arg(path)
                    .status()
                    .expect("run chattr");
                assert!(
                    status.success(),
                    "chattr must be able to set the immutable flag as root"
                );
                Self {
                    path: path.to_path_buf(),
                    original_permissions: None,
                    active: true,
                }
            } else {
                // Atomic replacement writes a temporary file beside the target, so the directory,
                // not just the existing file, must be unwritable for an unprivileged process.
                let dir = path.parent().expect("state file parent").to_path_buf();
                let original = fs::metadata(&dir).unwrap().permissions();
                let mut readonly = original.clone();
                readonly.set_readonly(true);
                fs::set_permissions(&dir, readonly).unwrap();
                Self {
                    path: dir,
                    original_permissions: Some(original),
                    active: true,
                }
            }
        }
    }

    fn restore(&mut self) {
        #[cfg(windows)]
        {
            self.handle.take();
            self.active = false;
            return;
        }
        #[cfg(unix)]
        {
            if let Some(permissions) = &self.original_permissions {
                fs::set_permissions(&self.path, permissions.clone())
                    .expect("restore directory permissions");
            } else {
                let status = Command::new("chattr")
                    .arg("-i")
                    .arg(&self.path)
                    .status()
                    .expect("run chattr");
                assert!(
                    status.success(),
                    "chattr must be able to clear the immutable flag as root"
                );
            }
            self.active = false;
        }
    }
}

impl Drop for Unwritable {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.handle.take();
            return;
        }
        #[cfg(unix)]
        {
            if !self.active {
                return;
            }
            if let Some(permissions) = self.original_permissions.take() {
                let _ = fs::set_permissions(&self.path, permissions);
            } else {
                let _ = Command::new("chattr").arg("-i").arg(&self.path).status();
            }
        }
    }
}

#[test]
fn a_failed_write_is_reported_not_shown_as_success_and_nothing_is_lost_or_half_applied() {
    let ob = OneBot::start();
    let root = fixture("failwrite", &ob.url, json!({}), json!({}));
    fs::write(root.p("data/feedback.json"), history().to_string()).unwrap();
    // The file gates must remain available when the data directory cannot create temp files.
    fs::create_dir_all(root.p("data/locks")).unwrap();
    let console = Console::start(&root);

    let mut blocked_feedback = Unwritable::new(&root.p("data/feedback.json"));
    let (s, b) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback", json!({"key": KEY_A, "message": "cannot be stored"}));
    assert_eq!(s, 500, "{b}");
    assert_eq!(b["ok"], false);
    blocked_feedback.restore();
    assert_eq!(entries(&console), history().as_array().cloned().unwrap(), "history intact, the failed report is not listed");
    assert!(!root.text("data/qq-activity.log").contains("AI 反馈"), "no activity line for a report that was not stored");
    let (s, _) = console.agent(AGENT_B, "POST", "/api/socialV2/feedback", json!({"key": KEY_B, "message": "stored after recovery"}));
    assert_eq!(s, 200);
    assert_eq!(entries(&console).len(), 3);

    // The pause switch: a state file that cannot be replaced leaves the switch where it was and says so.
    let mut blocked_social = Unwritable::new(&root.p("data/social-v2.json"));
    let (s, b) = console.admin("POST", "/api/socialV2/activity", json!({"paused": true}));
    blocked_social.restore();
    assert!(s >= 500, "a failed pause must not answer success: {s} {b}");
    let (s, b) = console.admin("GET", "/api/socialV2/activity", Value::Null);
    assert_eq!((s, b["paused"].clone()), (200, json!(false)), "{b}");
    let (s, _) = console.admin("POST", "/api/socialV2/activity", json!({"paused": true}));
    assert_eq!(s, 200);
    let (_, b) = console.admin("GET", "/api/status", Value::Null);
    assert_eq!(b["socialV2Paused"], true);
    assert_eq!(root.json("data/social-v2.json")["paused"], true, "persisted");
    drop(console);
}

#[test]
fn pause_changes_persist_across_restart_without_touching_sessions_or_feedback() {
    let ob = OneBot::start();
    let root = fixture("pause-persist", &ob.url, json!({}), json!({}));
    fs::write(root.p("data/feedback.json"), history().to_string()).unwrap();
    let console = Console::start(&root);
    let (s, b) = console.admin("POST", "/api/socialV2/activity", json!({"paused": true}));
    assert_eq!((s, b), (200, json!({"ok": true, "paused": true})));
    console.stop();
    let console = Console::start(&root);
    let (_, b) = console.admin("GET", "/api/socialV2/activity", Value::Null);
    assert_eq!(b, json!({"ok": true, "paused": true}));
    let state = root.json("data/social-v2.json");
    assert_eq!(state["conversations"][KEY_A]["agentToken"], AGENT_A, "the toggle keeps every session as it was");
    assert_eq!(state["conversations"][KEY_B]["agentToken"], AGENT_B);
    assert_eq!(entries(&console), history().as_array().cloned().unwrap());
    let (s, b) = console.admin("POST", "/api/socialV2/activity", json!({"paused": false}));
    assert_eq!((s, b), (200, json!({"ok": true, "paused": false})));
    let (_, b) = console.admin("GET", "/api/status", Value::Null);
    assert_eq!(b["socialV2Paused"], false);
    drop(console);
}

#[test]
fn feedback_settings_read_back_with_their_defaults_and_normalize_on_write() {
    let ob = OneBot::start();
    let root = fixture("settings", &ob.url, json!({}), json!({}));
    let console = Console::start(&root);
    let feedback_of = |b: &Value| b["config"]["feedback"].clone();

    let (s, b) = console.admin("GET", "/api/socialV2/config", Value::Null);
    assert_eq!(s, 200);
    assert_eq!(feedback_of(&b), json!({"maxLength": 500, "notifyOwnerOnError": false}), "a config without the section shows the JS defaults");
    assert_eq!(b["config"]["tools"]["feedback"], true);

    let (s, b) = console.admin("POST", "/api/socialV2/config", json!({"feedback": {"maxLength": "12.4"}}));
    assert_eq!(s, 200, "{b}");
    // JS merges into the section the file has (none yet), so only the written member is stored.
    assert_eq!(feedback_of(&b), json!({"maxLength": 12}));
    assert_eq!(root.json("config/config.json")["socialV2"]["feedback"], json!({"maxLength": 12}));
    // Only the boolean true switches the owner push on; a truthy non-boolean does not.
    let (_, b) = console.admin("POST", "/api/socialV2/config", json!({"feedback": {"notifyOwnerOnError": 1}}));
    assert_eq!(feedback_of(&b), json!({"maxLength": 12, "notifyOwnerOnError": false}));
    let (_, b) = console.admin("POST", "/api/socialV2/config", json!({"feedback": {"notifyOwnerOnError": true, "maxLength": 0}}));
    assert_eq!(feedback_of(&b), json!({"maxLength": 1, "notifyOwnerOnError": true}), "the limit never drops below one character");
    let (_, b) = console.admin("POST", "/api/socialV2/config", json!({"feedback": "nope"}));
    assert_eq!(feedback_of(&b), json!({"maxLength": 1, "notifyOwnerOnError": true}), "a non-object keeps the current section");
    let (_, b) = console.admin("GET", "/api/socialV2/config", Value::Null);
    assert_eq!(feedback_of(&b), json!({"maxLength": 1, "notifyOwnerOnError": true}));
    drop(console);
}

#[test]
fn changing_a_default_wake_setting_reaches_only_conversations_still_on_the_defaults() {
    let ob = OneBot::start();
    let root = fixture("wake-defaults", &ob.url, json!({}), json!({}));
    let mut state = root.json("data/social-v2.json");
    state["conversations"][KEY_A]["wakeConfig"] = json!({
        "mode": "diving", "infinite": true, "sleepUntil": null,
        "triggers": {"atMention": true, "nameMention": true, "speakerIds": [], "keywords": ["k1"], "question": true, "poke": true, "anyMessage": false, "probability": 0},
        "batchWindowMs": 3000, "lastWakeAt": 111, "wakeCount": 7, "noActionCount": 2, "confirmedAt": 222, "confirmedBy": "default", "marker": "default-conversation"
    });
    state["conversations"][KEY_B]["wakeConfig"] = json!({
        "mode": "active", "infinite": true, "sleepUntil": null,
        "triggers": {"atMention": true, "nameMention": false, "speakerIds": [], "keywords": [], "question": false, "poke": false, "anyMessage": true, "probability": 0},
        "batchWindowMs": 3000, "lastWakeAt": 0, "wakeCount": 0, "noActionCount": 0, "confirmedAt": 333, "confirmedBy": "set_wake_config", "marker": "model-set-conversation"
    });
    fs::write(root.p("data/social-v2.json"), state.to_string()).unwrap();
    let console = Console::start(&root);
    let wake = |key: &str| -> Value { root.json("data/social-v2.json")["conversations"][key]["wakeConfig"].clone() };

    // A setting that does not touch the defaults leaves every wake plan alone.
    let (s, b) = console.admin("POST", "/api/socialV2/config", json!({"send": {"maxSendPerMinute": 9}}));
    assert_eq!(s, 200, "{b}");
    assert_eq!(wake(KEY_A)["marker"], "default-conversation");

    let (s, b) = console.admin("POST", "/api/socialV2/config", json!({"wake": {"batchWindowMs": 4500}}));
    assert_eq!(s, 200, "{b}");
    let a = wake(KEY_A);
    assert_eq!(a["batchWindowMs"], 4500, "{a}");
    assert_eq!(a["confirmedBy"], "default");
    assert!(a.get("marker").is_none(), "the default record was rebuilt: {a}");
    assert_eq!((a["lastWakeAt"].clone(), a["wakeCount"].clone(), a["noActionCount"].clone(), a["confirmedAt"].clone()), (json!(111), json!(7), json!(2), json!(222)), "counters stay");
    assert_eq!(a["triggers"]["keywords"], json!(["k1"]), "the operator's keywords stay");
    let b_conv = wake(KEY_B);
    assert_eq!((b_conv["batchWindowMs"].clone(), b_conv["marker"].clone()), (json!(3000), json!("model-set-conversation")), "a confirmed plan is not touched");
    let (_, state) = console.admin("GET", &format!("/api/socialV2/state?key={KEY_A}"), Value::Null);
    assert_eq!(state["wakeConfig"]["batchWindowMs"], 4500);
    drop(console);
}

#[test]
fn hotfix128_fix1_feedback_cut_keeps_token_fragments_out_of_activity_and_storage() {
    for limit in [80, 200] {
        let ob = OneBot::start();
        let root = fixture("token-preview-fix1", &ob.url,
            json!({"feedback":{"maxLength":limit},"tools":{"feedback":true}}),
            json!({"security":{"outboundGuard":false}}));
        let console = Console::start(&root);
        for token in [AGENT_A, "0123456789abcdef0123456789abcdef0123456789abcdef"] {
            let text = format!("{}{token} trailing", "x ".repeat((limit - 10) / 2));
            let (status, body) = console.agent(AGENT_A, "POST", "/api/socialV2/feedback",
                json!({"key":KEY_A,"message":text,"level":"info"}));
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["message"], text.chars().take(limit).collect::<String>(), "HTTP response text keeps its existing contract");
            for (name, saved) in [("activity", root.text("data/qq-activity.log")),
                ("feedback", root.text("data/feedback.json")), ("stdout", root.text("console.stdout.log"))] {
                assert!(!saved.contains(&token[..8]), "{limit}/{name}: truncated token leaked: {saved}");
                if name == "feedback" || limit == 80 {
                    assert!(saved.contains("***"), "missing token mask: {saved}");
                }
            }
        }
    }
}
