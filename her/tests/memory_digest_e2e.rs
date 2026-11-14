//! Memory digest jobs through the real console, with a synthetic runtime and a loopback model relay.
mod support;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CHAT: &str = "SYNTHETIC-PRIVATE-CHAT";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn request(stream: &mut TcpStream) -> Option<(String, String, Value)> {
    // Windows accepts inherit the listener's nonblocking mode. A read timeout does
    // not clear it: an accept before the first bytes would look like disconnect.
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut bytes = Vec::new();
    let (end, length) = loop {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..end]);
            let length = header
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let header = String::from_utf8_lossy(&bytes[..end]).to_string();
    let path = header.split_whitespace().nth(1)?.to_string();
    let body = serde_json::from_slice(&bytes[end..end + length]).unwrap_or(Value::Null);
    Some((path, header, body))
}

/// Loopback OpenAI-compatible relay: each request gets the next scripted reply, then `[]`.
struct Model {
    port: u16,
    requests: Arc<Mutex<Vec<(String, String, Value)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Model {
    fn start(replies: Vec<(u16, String)>) -> Self {
        Self::start_delayed(replies.into_iter().map(|(s, b)| (s, b, 0)).collect())
    }

    /// Like `start`; each reply waits its delay (ms) after the request is recorded.
    fn start_delayed(replies: Vec<(u16, String, u64)>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (requests, stop) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicBool::new(false)));
        let (log, done) = (requests.clone(), stop.clone());
        let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
        let thread = thread::spawn(move || while !done.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some((path, header, body)) = request(&mut stream) else { continue };
                    log.lock().unwrap().push((path, header, body));
                    let (status, body, delay) = replies.lock().unwrap().pop_front().unwrap_or_else(|| {
                        let (s, b) = completion("[]");
                        (s, b, 0)
                    });
                    thread::sleep(Duration::from_millis(delay));
                    let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        });
        Self { port, requests, stop, thread: Some(thread) }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for Model {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn completion(content: &str) -> (u16, String) {
    (200, json!({"id":"fixture","object":"chat.completion","created":1,"model":"digest-fixture-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string())
}

struct Rig {
    root: crate::support::RuntimeRoot,
    port: u16,
    child: Option<Child>,
    reservation: Option<crate::support::ports::Reserved>, // held until the first console spawn; restarts reuse the port
}
impl Rig {
    fn new(model: &Model, auto: Value, social: Value) -> Self {
        let root = crate::support::RuntimeRoot::new("digest");
        let reservation = crate::support::ports::reserve();
        let port = reservation.port();
        std::fs::write(root.join("config/config.json"), json!({"consoleToken":"fixture-digest-token","consolePort":port,
            "ownerQQ":9001,"memory":{"autoFlush":auto},"harness":{"model":"digest-fixture-model"},
            "tasks":{"nodePath":root.join("nonexistent-node.exe"),"installRoot":root.path()}}).to_string()).unwrap();
        std::fs::write(root.join("config/harness-api-key"), "fixture-digest-key").unwrap();
        std::fs::write(root.join("data/social-v2.json"), social.to_string()).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            json!({"dsh":"http://127.0.0.1:1",
            "model":format!("http://127.0.0.1:{}", model.port),"onebot":"http://127.0.0.1:1"})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            format!("secret: {}\n", URL_SAFE_NO_PAD.encode([37; 32])),
        )
        .unwrap();
        let db = rusqlite::Connection::open(root.join("data/memory.db")).unwrap();
        db.execute_batch("CREATE TABLE memory (
            id INTEGER PRIMARY KEY AUTOINCREMENT, owner_type TEXT NOT NULL, owner_id TEXT NOT NULL,
            scope TEXT NOT NULL, kind TEXT NOT NULL, subject TEXT, content TEXT NOT NULL, tags TEXT,
            source TEXT NOT NULL, importance REAL NOT NULL DEFAULT 0.5, hits INTEGER NOT NULL DEFAULT 0,
            pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
            last_used_at INTEGER, expires_at INTEGER);
            CREATE VIRTUAL TABLE memory_fts USING fts5(seg, tokenize='unicode61');").unwrap();
        db.execute("INSERT INTO memory(owner_type,owner_id,scope,kind,content,source,created_at) VALUES('session','group:42','group','fact',?1,'manual',1)",[CHAT]).unwrap();
        drop(db);
        Self {
            root,
            port,
            child: None,
            reservation: Some(reservation),
        }
    }
    fn start(&mut self) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_her-cli"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        drop(self.reservation.take()); // free the console port on the line before its first spawn
        self.child = Some(
            cmd.args([
                "--console-serve",
                "--console-port",
                &self.port.to_string(),
                "--runtime-root",
            ])
            .arg(self.root.path())
            .env("DSH_HOME", self.root.join("ambient-decoy"))
            .env("DSH_LAUNCH_TOKEN", "ambient_fake_launch_token")
            .env("MEMORY_DB_PATH", self.root.join("must-not-create.db"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "console exited"
            );
            if self
                .http("GET", "/api/health", None)
                .is_some_and(|r| r.0 == 200)
            {
                break;
            }
            assert!(Instant::now() < deadline, "console not ready");
            thread::sleep(Duration::from_millis(30));
        }
    }
    fn http(&self, method: &str, path: &str, body: Option<Value>) -> Option<(u16, Value)> {
        let body = body.map(|v| v.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        write!(stream,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://127.0.0.1:{}\r\nx-console-token: fixture-digest-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",self.port,self.port,body.len()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        Some((
            head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            serde_json::from_str(body).unwrap(),
        ))
    }
    fn run(&self, task: &str, key: &str) -> Value {
        let id = self.trigger(task, key);
        self.wait_run(&id, Duration::from_secs(15))
    }
    fn trigger(&self, task: &str, key: &str) -> String {
        let (code, start) = self
            .http(
                "POST",
                &format!("/api/tasks/{task}/run"),
                Some(json!({"requestId":key})),
            )
            .unwrap();
        assert_eq!(code, 202, "{start}");
        start["runId"].as_str().unwrap().to_string()
    }
    fn wait_run(&self, id: &str, budget: Duration) -> Value {
        let deadline = Instant::now() + budget;
        loop {
            let (_, result) = self
                .http("GET", &format!("/api/tasks/runs/{id}"), None)
                .unwrap();
            if !matches!(result["run"]["state"].as_str(), Some("queued" | "running")) {
                return result["run"].clone();
            }
            assert!(Instant::now() < deadline, "digest did not finish: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn cursor_rows(&self) -> i64 {
        let db = rusqlite::Connection::open(self.root.join("data/memory.db")).unwrap();
        db.query_row("SELECT count(*) FROM digest_cursor", [], |r| r.get(0)).unwrap()
    }
    /// Digest memory rows, in write order.
    fn digest_rows(&self) -> Vec<(String, String, String, String)> {
        let db = rusqlite::Connection::open(self.root.join("data/memory.db")).unwrap();
        let mut stmt = db
            .prepare("SELECT owner_id,kind,source,content FROM memory WHERE source LIKE 'digest:%' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    fn count_rows(&self, source: &str) -> i64 {
        let db = rusqlite::Connection::open(self.root.join("data/memory.db")).unwrap();
        db.query_row("SELECT count(*) FROM memory WHERE source = ?1", [source], |r| r.get(0))
            .unwrap()
    }
    /// Kill the console process the digest runs in, as an abrupt crash or forced restart would.
    fn kill(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
    fn stop_jobs(&self) {
        let dir = self.root.join("data/lifecycle");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("desired-state.json"), json!({"v":1,"state":"stopped","by":"fixture","reason":"digest stop test","at":now(),"revision":1}).to_string()).unwrap();
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn meta_lines(rig: &Rig, run: &Value) -> Vec<String> {
    let id = run["runId"].as_str().unwrap();
    let (_, log) = rig
        .http("GET", &format!("/api/tasks/runs/{id}/log"), None)
        .unwrap();
    log["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["text"].as_str().unwrap().to_string())
        .collect()
}

/// Whether the raw run log, the `/log` route or the run record carries `needle`.
fn leaks(rig: &Rig, run: &Value, needle: &str) -> bool {
    let id = run["runId"].as_str().unwrap();
    let raw =
        std::fs::read_to_string(rig.root.join(format!("data/tasks/logs/{id}.jsonl"))).unwrap();
    let (_, log) = rig
        .http("GET", &format!("/api/tasks/runs/{id}/log"), None)
        .unwrap();
    raw.contains(needle) || log.to_string().contains(needle) || run.to_string().contains(needle)
}

fn wait_for_request(model: &Model, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while model.count() < count {
        assert!(Instant::now() < deadline, "the model call never started");
        thread::sleep(Duration::from_millis(10));
    }
}

fn two() -> Value {
    json!({"conversations":{"group:42":{"agentToken":"t42","recentMessages":three()},
        "group:43":{"agentToken":"t43","recentMessages":[{"seq":1,"sender":"丙","userId":"5003","text":"下周开会","isSelf":false}]}}})
}

fn social(messages: Value) -> Value {
    json!({"conversations":{"group:42":{"agentToken":"t42","recentMessages":messages},"group:43":{"agentToken":"t43","recentMessages":[]}}})
}

fn three() -> Value {
    json!([
        {"seq":1,"sender":"甲","userId":"5001","text":format!("{CHAT} 周六去爬山"),"isSelf":false},
        {"seq":2,"sender":"管理员","userId":"9001","isOwner":true,"text":"/reset","isSelf":false},
        {"seq":3,"sender":"乙","userId":"5002","text":"下周搬家","isSelf":false}
    ])
}

#[test]
fn nightly_digest_writes_new_messages_with_bridge_provenance_and_never_logs_chat() {
    let model = Model::start(vec![completion(r#"[{"subject":"爬山","content":"甲周六去爬山","source":"model-claimed"}]"#)]);
    let mut rig = Rig::new(&model, json!({}), social(three()));
    rig.start();
    let run = rig.run("memory-digest", "digest-provenance");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(model.count(), 1);
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests[0].0, "/v1/chat/completions");
        assert!(requests[0]
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-digest-key"));
        assert!(requests[0].2.to_string().contains("周六去爬山"));
        assert!(!requests[0].2.to_string().contains("/reset"));
    }
    assert_eq!(
        rig.digest_rows(),
        vec![(
            "group:42".to_string(),
            "inference".to_string(),
            "digest:group:42:msg:1-3".to_string(),
            "甲周六去爬山".to_string(),
        )]
    );
    assert_eq!(rig.count_rows("model-claimed"), 0);
    assert!(!leaks(&rig, &run, CHAT));
    let lines = meta_lines(&rig, &run);
    assert!(
        lines.iter().any(|l| l.contains("digest written=1 memories=1 quiet=1 failures=0")),
        "{lines:?}"
    );
    let repeat = rig.run("memory-digest", "digest-provenance-repeat");
    assert_eq!(repeat["state"], "succeeded", "{repeat}");
    assert_eq!(model.count(), 1);
}

#[test]
fn model_failures_write_nothing_fail_the_run_and_retry_the_same_messages() {
    let model = Model::start(vec![
        (429, r#"{"error":{"message":"private-quota-text"}}"#.to_string()),
        (500, "{}".to_string()),
        completion("I cannot assist with that."),
        completion(""),
        completion("[]"),
    ]);
    let mut rig = Rig::new(&model, json!({"minIntervalMinutes":0}), social(three()));
    rig.start();
    let first = rig.run("memory-digest", "digest-fail-1");
    assert_eq!(first["state"], "failed", "{first}");
    assert_eq!(first["exitCode"], 1);
    assert!(rig.digest_rows().is_empty());
    assert_eq!(model.count(), 1);
    assert!(!leaks(&rig, &first, "private-quota-text"));
    assert!(meta_lines(&rig, &first)
        .iter()
        .any(|l| l.contains("digest failed step=model kind=quota")));
    for (key, reply) in [("digest-fail-2", 2), ("digest-fail-3", 3), ("digest-fail-4", 4)] {
        let run = rig.run("memory-digest", key);
        assert_eq!(run["state"], "failed", "{run}");
        assert!(rig.digest_rows().is_empty());
        assert_eq!(model.count(), reply);
        if reply == 3 {
            assert!(meta_lines(&rig, &run)
                .iter()
                .any(|l| l.contains("step=digest-result kind=refusal-or-error")));
        }
    }
    let fifth = rig.run("memory-digest", "digest-fail-5");
    assert_eq!(fifth["state"], "succeeded", "{fifth}");
    assert!(rig.digest_rows().is_empty());
    assert_eq!(model.count(), 5);
    let sixth = rig.run("memory-digest", "digest-fail-6");
    assert_eq!(sixth["state"], "succeeded", "{sixth}");
    assert_eq!(model.count(), 5, "the cursor moved, so the new run is quiet");
    {
        let requests = model.requests.lock().unwrap();
        assert!(requests[0].2.to_string().contains("下周搬家"));
        assert!(requests[4].2.to_string().contains("下周搬家"));
    }
}

#[test]
fn disabled_digest_contacts_no_model() {
    let model = Model::start(vec![]);
    let mut rig = Rig::new(&model, json!({"enabled":false}), social(three()));
    rig.start();
    let run = rig.run("memory-digest", "digest-disabled");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(model.count(), 0);
    assert!(meta_lines(&rig, &run)
        .iter()
        .any(|l| l.contains("digest disabled")));
}

#[test]
fn check_digest_waits_for_a_backlog() {
    let model = Model::start(vec![completion(r#"[{"subject":"爬山","content":"甲周六去爬山"}]"#)]);
    let mut rig = Rig::new(
        &model,
        json!({}),
        social(json!([{"seq":1,"sender":"甲","userId":"5001","text":"周六去爬山","isSelf":false}])),
    );
    rig.start();
    let check = rig.run("memory-digest-check", "digest-check-young");
    assert_eq!(check["state"], "succeeded", "{check}");
    assert_eq!(model.count(), 0, "one message is below the backlog");
    let daily = rig.run("memory-digest", "digest-daily-after-check");
    assert_eq!(daily["state"], "succeeded", "{daily}");
    assert_eq!(model.count(), 1);
}

/// Guard: admission refuses a stopped runtime. The in-run hold is covered by `stop_during_a_model_call_...`.
#[test]
fn a_stopped_runtime_refuses_digest_admission_with_409() {
    let model = Model::start(vec![]);
    let mut rig = Rig::new(&model, json!({}), social(three()));
    rig.start();
    rig.stop_jobs();
    let (code, body) = rig
        .http("POST", "/api/tasks/memory-digest/run", Some(json!({"requestId":"digest-stopped"})))
        .unwrap();
    assert_eq!(code, 409, "{body}");
    assert_eq!(model.count(), 0);
}

#[test]
fn stop_during_a_model_call_ends_the_run_cancelled_without_a_write_or_a_second_request() {
    let model = Model::start_delayed(vec![(200, completion(r#"[{"subject":"爬山","content":"甲周六去爬山"}]"#).1, 1500)]);
    let mut rig = Rig::new(&model, json!({}), two());
    rig.start();
    let id = rig.trigger("memory-digest", "digest-stop-mid-run");
    wait_for_request(&model, 1);
    rig.stop_jobs();
    let run = rig.wait_run(&id, Duration::from_secs(15));
    assert_eq!(run["state"], "cancelled", "{run}");
    assert!(rig.digest_rows().is_empty(), "a held digest wrote memories: {:?}", rig.digest_rows());
    assert_eq!(rig.cursor_rows(), 0, "a held digest moved a cursor");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(model.count(), 1, "the second conversation reached the model");
    assert!(!model.requests.lock().unwrap()[0].2.to_string().contains("下周开会"), "another conversation's message reached the input");
}

#[test]
fn a_check_during_a_running_digest_skips_on_the_lock() {
    let model = Model::start_delayed(vec![(200, completion("[]").1, 3000)]);
    let mut rig = Rig::new(&model, json!({}), social(three()));
    rig.start();
    let daily = rig.trigger("memory-digest", "digest-gate-daily");
    wait_for_request(&model, 1);
    let check = rig.run("memory-digest-check", "digest-gate-check");
    assert_eq!(check["state"], "succeeded", "{check}");
    assert!(meta_lines(&rig, &check).iter().any(|l| l.contains("digest skipped: already running")), "check did not skip on the lock");
    assert_eq!(rig.wait_run(&daily, Duration::from_secs(15))["state"], "succeeded");
    assert_eq!(model.count(), 1);
}

#[test]
fn each_job_reloads_the_selected_config_and_model_endpoint() {
    let old = Model::start(vec![]);
    let current = Model::start(vec![completion(r#"[{"subject":"爬山","content":"甲周六去爬山"}]"#)]);
    let mut rig = Rig::new(&old, json!({"enabled":false}), social(three()));
    rig.start();
    assert_eq!(rig.run("memory-digest", "digest-reload-disabled")["state"], "succeeded");
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    cfg["memory"]["autoFlush"]["enabled"] = json!(true);
    std::fs::write(&path, cfg.to_string()).unwrap();
    std::fs::write(rig.root.join("dependencies.json"), json!({"dsh":"http://127.0.0.1:1",
        "model":format!("http://127.0.0.1:{}", current.port),"onebot":"http://127.0.0.1:1"}).to_string()).unwrap();
    let run = rig.run("memory-digest", "digest-reload-enabled");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(current.count(), 1, "the job must use the current dependencies.json");
    assert_eq!(old.count(), 0, "the job contacted the endpoint cached at console start");
    assert_eq!(rig.digest_rows().len(), 1);
}
