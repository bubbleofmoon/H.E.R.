//! Memory digest jobs through the real console, with only a synthetic runtime and loopback DSH.
mod support;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
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
fn session(id: &str, title: &str, updated: i64, tokens: i64, window: i64) -> Value {
    json!({"sessionId":id,"updatedAt":updated,"running":false,
        "projections":{"values":{"title":title,"contextPressure":{"pressureTokens":tokens,"contextWindow":window}}}})
}

fn authenticated(header: &str, port: u16) -> bool {
    let cookie = header
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                .map(|(_, v)| v.trim())
        })
        .unwrap_or("");
    let Some((name, value)) = cookie.split_once('=') else {
        return false;
    };
    if name
        != format!(
            "dsh-auth-{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(format!("127.0.0.1:{port}").as_bytes()))
        )
    {
        return false;
    }
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 3 || parts[0] != "v1" {
        return false;
    }
    let Ok(signature) = URL_SAFE_NO_PAD.decode(parts[2]) else {
        return false;
    };
    let Ok(payload) = URL_SAFE_NO_PAD.decode(parts[1]) else {
        return false;
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&payload) else {
        return false;
    };
    if payload["authority"] != format!("127.0.0.1:{port}") {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(&[37; 32]).unwrap();
    mac.update(parts[1].as_bytes());
    mac.verify_slice(&signature).is_ok()
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

#[derive(Clone, Copy)]
enum Fault {
    Refuse,
    Disconnect,
    WrongId,
    HttpError,
    Malformed,
    Stall,
    ReauthStall,
    Pending401,
}

/// What the fake DSH durably holds per session: the event log and the queued inbox. A prompt it
/// accepts is written here (keyed by the JSON-RPC id, as DSH's `source.rpcId`) before it answers.
#[derive(Default)]
struct Held {
    events: HashMap<String, Vec<Value>>,
    inbox: HashMap<String, Vec<Value>>,
}

/// How the fake answers `session/follow`.
const FOLLOW_WHOLE_LOG: u8 = 0;
/// The snapshot says more log exists than it carries, so absence cannot be proved from it.
const FOLLOW_TRUNCATED: u8 = 1;
/// The stream is closed without a snapshot.
const FOLLOW_DROPPED: u8 = 2;

/// Serve one `session/follow` stream: the opening snapshot of the session's event log and inbox.
fn follow(stream: TcpStream, held: &Mutex<Held>, seen: &Mutex<Vec<Value>>, mode: u8) {
    use tokio_tungstenite::tungstenite::{accept, Message};
    let Ok(mut socket) = accept(stream) else {
        return;
    };
    let Ok(Message::Text(open)) = socket.read() else {
        return;
    };
    let frame: Value = serde_json::from_str(&open).unwrap_or(Value::Null);
    let sid = frame["payload"]["args"]["request"]["address"]["sessionId"]
        .as_str()
        .unwrap_or("")
        .to_string();
    seen.lock()
        .unwrap()
        .push(json!({"path":"/api/remote.mux","body":frame}));
    if mode == FOLLOW_DROPPED {
        return;
    }
    let (records, inbox) = {
        let held = held.lock().unwrap();
        (
            held.events
                .get(&sid)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|event| json!({"type":"event","event":event}))
                .collect::<Vec<_>>(),
            held.inbox.get(&sid).cloned().unwrap_or_default(),
        )
    };
    let snapshot = json!({"type":"item","streamId":frame["streamId"],"value":{"type":"snapshot",
        "records":records,"hasMore":mode == FOLLOW_TRUNCATED,
        "projections":{"values":{"inbox":{"next-turn":inbox,"next-step":[]}}}}});
    let _ = socket.send(Message::Text(snapshot.to_string()));
    let _ = socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(2)));
    let _ = socket.read();
}

struct Fake {
    port: u16,
    calls: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    stalled: Arc<AtomicBool>,
    refresh_credentials: Arc<Mutex<Option<PathBuf>>>,
    held: Arc<Mutex<Held>>,
    /// Accepted prompts wait in the inbox instead of starting a turn (a busy session).
    queue_accepted: Arc<AtomicBool>,
    follow_mode: Arc<AtomicU8>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Fake {
    fn start(items: Vec<Value>, failures: Vec<&str>) -> Self {
        Self::with_faults(
            items,
            failures.into_iter().map(|id| (id, Fault::Refuse)).collect(),
        )
    }
    fn with_faults(items: Vec<Value>, faults: Vec<(&str, Fault)>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let refresh_credentials = Arc::new(Mutex::new(None::<PathBuf>));
        let held = Arc::new(Mutex::new(Held::default()));
        let queue_accepted = Arc::new(AtomicBool::new(false));
        let follow_mode = Arc::new(AtomicU8::new(FOLLOW_WHOLE_LOG));
        let (holds, queues, follows) = (held.clone(), queue_accepted.clone(), follow_mode.clone());
        let (resume, blocked, refresh) = (
            release.clone(),
            stalled.clone(),
            refresh_credentials.clone(),
        );
        let (seen, end) = (calls.clone(), stop.clone());
        let faults: Vec<(String, Fault)> = faults
            .into_iter()
            .map(|(id, f)| (id.to_string(), f))
            .collect();
        let worker = thread::spawn(move || {
            while !end.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let mut peek = [0; 24];
                let start = Instant::now();
                let n = loop {
                    let n = stream.peek(&mut peek).unwrap_or(0);
                    if n >= peek.len() || start.elapsed() > Duration::from_secs(3) {
                        break n;
                    }
                    thread::sleep(Duration::from_millis(1));
                };
                if peek[..n].starts_with(b"GET /api/remote.mux") {
                    follow(stream, &holds, &seen, follows.load(Ordering::SeqCst));
                    continue;
                }
                let Some((path, header, body)) = request(&mut stream) else {
                    continue;
                };
                seen.lock()
                    .unwrap()
                    .push(json!({"path":path,"header":header,"body":body}));
                if !authenticated(&header, port) {
                    if let Some(path) = refresh.lock().unwrap().take() {
                        std::fs::write(
                            path,
                            format!("secret: {}\n", URL_SAFE_NO_PAD.encode([37; 32])),
                        )
                        .unwrap();
                    }
                    let _ = write!(stream,"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let id = body["payload"]["args"]["request"]["sessionId"]
                    .as_str()
                    .unwrap_or("");
                if path == "/api/session/projections" {
                    let inbox = holds
                        .lock()
                        .unwrap()
                        .inbox
                        .get(id)
                        .cloned()
                        .unwrap_or_default();
                    let text =
                        json!({"type":"server-response","rpcId":body["rpcId"],"result":{"ok":true,
                        "value":{"values":{"inbox":{"next-turn":inbox,"next-step":[]}}}}})
                        .to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    continue;
                }
                let fault = faults.iter().find(|(s, _)| s == id).map(|(_, f)| *f);
                let attempt = seen
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| {
                        call["path"] == path
                            && call["body"]["payload"]["args"]["request"]["sessionId"]
                                .as_str()
                                .unwrap_or("")
                                == id
                    })
                    .count();
                if matches!(fault, Some(Fault::ReauthStall)) && attempt == 1 {
                    let _ = write!(stream,"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                if path == "/api/session/prompt"
                    && !matches!(
                        fault,
                        Some(
                            Fault::Refuse
                                | Fault::Disconnect
                                | Fault::HttpError
                                | Fault::Pending401
                        )
                    )
                {
                    let message = json!({"source":{"rpcId":body["rpcId"]},
                        "content":body["payload"]["args"]["request"]["content"]});
                    let mut held = holds.lock().unwrap();
                    if queues.load(Ordering::SeqCst) {
                        held.inbox.entry(id.to_string()).or_default().push(message);
                    } else {
                        let log = held.events.entry(id.to_string()).or_default();
                        let seq = log.len() as i64 + 1;
                        log.push(json!({"seq":seq,"type":"user/message","data":message}));
                        log.push(json!({"seq":seq + 1,"type":"turn/start","data":{"turn":seq}}));
                    }
                }
                if matches!(
                    fault,
                    Some(Fault::Stall | Fault::ReauthStall | Fault::Pending401)
                ) {
                    if matches!(fault, Some(Fault::Pending401)) {
                        let _ = write!(stream,"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    blocked.store(true, Ordering::SeqCst);
                    while !resume.load(Ordering::SeqCst) && !end.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    if matches!(fault, Some(Fault::Pending401)) {
                        continue;
                    }
                }
                if matches!(fault, Some(Fault::Disconnect)) {
                    continue;
                }
                let result = if path == "/api/session/list" {
                    json!({"ok":true,"value":{"items":items}})
                } else {
                    if matches!(fault, Some(Fault::Refuse)) {
                        json!({"ok":false,"error":{"code":"refused","message":CHAT}})
                    } else {
                        json!({"ok":true,"value":{"accepted":true,"text":CHAT}})
                    }
                };
                let rpc_id = if matches!(fault, Some(Fault::WrongId)) {
                    json!("wrong-rpc-id")
                } else {
                    body["rpcId"].clone()
                };
                let text =
                    json!({"type":"server-response","rpcId":rpc_id,"result":result}).to_string();
                let text = if matches!(fault, Some(Fault::Malformed)) {
                    CHAT.to_string()
                } else {
                    text
                };
                let status = if matches!(fault, Some(Fault::HttpError)) {
                    "500 Error"
                } else {
                    "200 OK"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Self {
            port,
            calls,
            stop,
            release,
            stalled,
            refresh_credentials,
            held,
            queue_accepted,
            follow_mode,
            thread: Some(worker),
        }
    }
    fn prompts(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["path"] == "/api/session/prompt")
            .map(|c| c["body"]["payload"]["args"]["request"].clone())
            .collect()
    }
    fn wait_stalled(&self, rig: &Rig, run_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.stalled.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "fake did not reach the stalled response; calls={:?}; run={:?}",
                self.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|c| (
                        c["path"].clone(),
                        c["body"]["payload"]["args"]["request"]["sessionId"].clone()
                    ))
                    .collect::<Vec<_>>(),
                rig.http("GET", &format!("/api/tasks/runs/{run_id}"), None)
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

struct Rig {
    root: crate::support::RuntimeRoot,
    port: u16,
    child: Option<Child>,
    reservation: Option<crate::support::ports::Reserved>, // held until the first console spawn; restarts reuse the port
}
impl Rig {
    fn new(fake: &Fake, auto: Value, state: Option<Value>) -> Self {
        let root = crate::support::RuntimeRoot::new("digest");
        let reservation = crate::support::ports::reserve();
        let port = reservation.port();
        std::fs::write(root.join("config/config.json"), json!({"consoleToken":"fixture-digest-token","consolePort":port,
            "memory":{"autoFlush":auto},"tasks":{"nodePath":root.join("nonexistent-node.exe"),"installRoot":root.path()}}).to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            json!({"dsh":format!("http://127.0.0.1:{}",fake.port),
            "model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            format!("secret: {}\n", URL_SAFE_NO_PAD.encode([37; 32])),
        )
        .unwrap();
        if let Some(state) = state {
            std::fs::write(root.join("data/digest-state.json"), state.to_string()).unwrap();
        }
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
    fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.root.join("data/digest-state.json")).unwrap())
            .unwrap()
    }
    fn dispatch(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.root.join("data/digest-dispatch.json")).unwrap())
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

#[test]
fn daily_digest_uses_selected_runtime_and_preserves_existing_progress_and_memory() {
    let updated = now();
    let fake = Fake::start(
        vec![session(
            "selected-session",
            "QQ private synthetic",
            updated,
            321,
            1000,
        )],
        vec![],
    );
    let old =
        json!({"at":"2020-01-02T03:04:05.006Z","tokens":99,"title":"QQ old","lastUpdatedAt":1});
    let mut rig = Rig::new(
        &fake,
        json!({}),
        Some(json!({"unknown":{"keep":true},"sessions":{"old-session":old}})),
    );
    let before = std::fs::read(rig.root.join("data/memory.db")).unwrap();
    rig.start();
    let run = rig.run("memory-digest", "digest-daily-first");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(run["exitCode"], 0);
    assert!(run["pid"].is_null(), "digest must be a Rust action");
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls[0]["body"]["method"], "session/list");
    assert_eq!(calls[0]["body"]["payload"]["args"], json!({"_request":{}}));
    assert!(calls[0]["header"]
        .as_str()
        .unwrap()
        .contains("Cookie: dsh-auth-"));
    drop(calls);
    let prompts = fake.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["sessionId"], "selected-session");
    assert_eq!(prompts[0]["mode"], "queue");
    assert!(prompts[0]["requestId"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert!(prompts[0].get("agentPreset").is_none());
    let prompt = prompts[0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        prompt,
        "【记忆整理】现在做一次内部整理，不要把这段话说给用户听。
请回顾这次会话，把**以后还会用到**的内容写进长期记忆，用 memory_write，一条一句：
- 用户/群友的偏好、要求、禁忌
- 重要决定与它的理由（例如\"为什么选 A 不选 B\"）
- 踩过的坑、报错与解法
- 这个群/这个人的长期话题、关系、称呼习惯
要求：3~8 条；只写以后还会用到的，不要流水账；每条 200 字以内；subject 填话题名或人名。
写完后**不要**发 QQ 消息，也不要回\"好的\"，直接结束。"
    );
    let state = rig.state();
    assert_eq!(state["unknown"], json!({"keep":true}));
    assert_eq!(state["sessions"]["old-session"], old);
    assert_eq!(state["sessions"]["selected-session"]["tokens"], 321);
    assert_eq!(
        state["sessions"]["selected-session"]["lastUpdatedAt"],
        updated
    );
    assert_eq!(
        state["sessions"]["selected-session"]["title"],
        "QQ private synthetic"
    );
    assert_eq!(
        state["sessions"]["selected-session"]["at"]
            .as_str()
            .unwrap()
            .len(),
        24
    );
    assert!(state["sessions"]["selected-session"]["at"]
        .as_str()
        .unwrap()
        .ends_with('Z'));
    assert_eq!(
        std::fs::read(rig.root.join("data/memory.db")).unwrap(),
        before
    );
    assert!(!rig.root.join("must-not-create.db").exists());
    let repeat = rig.run("memory-digest", "digest-daily-iso-repeat");
    assert_eq!(repeat["state"], "succeeded", "{repeat}");
    assert_eq!(
        fake.prompts().len(),
        2,
        "saved ISO timestamps retain the JS interval behaviour"
    );
}

#[test]
fn check_digest_uses_pressure_growth_and_existing_state_instead_of_daily_activity() {
    let mut running = session("running", "QQ running", now(), 900, 1000);
    running["running"] = json!(true);
    let mut strings = session("strings", "QQ string numbers", now(), 0, 0);
    strings["projections"]["values"]["contextPressure"] =
        json!({"pressureTokens":"140","contextWindow":"200"});
    let fake = Fake::start(
        vec![
            session(
                "pressure",
                "QQ inactive but full",
                now() - 3 * 86_400_000,
                700,
                1000,
            ),
            session("old-duplicate", "QQ duplicate", now() - 2, 950, 1000),
            session("new-duplicate", "QQ duplicate", now() - 1, 100, 1000),
            session("unchanged", "QQ no growth", now(), 800, 1000),
            session("fallback-low", "QQ fallback low", now(), 699_999, 0),
            session("fallback-high", "QQ fallback high", now(), 700_000, 0),
            session("recent", "QQ recent numeric", now(), 900, 1000),
            session("iso", "QQ existing ISO", now(), 900, 1000),
            session("other", "Not QQ", now(), 900, 1000),
            running,
            strings,
        ],
        vec![],
    );
    let mut rig = Rig::new(
        &fake,
        json!({"contextPct":"70","minIntervalMinutes":"60"}),
        Some(json!({"sessions":{
            "unchanged":{"tokens":800},"recent":{"at":now(),"tokens":1},
            "iso":{"at":"2020-01-02T03:04:05.006Z","tokens":1,"lastUpdatedAt":0}
        }})),
    );
    rig.start();
    let run = rig.run("memory-digest-check", "digest-check-first");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert!(run["pid"].is_null());
    let ids: Vec<Value> = fake
        .prompts()
        .iter()
        .map(|p| p["sessionId"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            json!("pressure"),
            json!("fallback-high"),
            json!("iso"),
            json!("strings")
        ]
    );
    assert_eq!(rig.state()["sessions"]["strings"]["tokens"], 140);
    let repeat = rig.run("memory-digest-check", "digest-check-repeat");
    assert_eq!(repeat["state"], "succeeded", "{repeat}");
    assert_eq!(
        fake.prompts().len(),
        4,
        "unchanged pressure is not consolidated again"
    );
}

#[test]
fn prompt_failures_continue_other_sessions_exit_nonzero_and_never_leak_chat_to_logs() {
    let ids = [
        "first",
        "refusal",
        "network",
        "wrong-id",
        "http-error",
        "malformed",
        "last",
    ];
    let fake = Fake::with_faults(
        ids.iter()
            .map(|id| session(id, &format!("QQ {CHAT} {id}"), now(), 100, 1000))
            .collect(),
        vec![
            ("refusal", Fault::Refuse),
            ("network", Fault::Disconnect),
            ("wrong-id", Fault::WrongId),
            ("http-error", Fault::HttpError),
            ("malformed", Fault::Malformed),
        ],
    );
    let mut rig = Rig::new(&fake, json!({}), None);
    rig.start();
    let run = rig.run("memory-digest", "digest-mixed-failures");
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(run["exitCode"], 1);
    assert_eq!(
        fake.prompts()
            .iter()
            .map(|p| p["sessionId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ids
    );
    let state = rig.state();
    let saved: Vec<&str> = state["sessions"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        saved,
        ["first", "last"],
        "failed RPCs must never advance digest progress"
    );
    let id = run["runId"].as_str().unwrap();
    let raw =
        std::fs::read_to_string(rig.root.join(format!("data/tasks/logs/{id}.jsonl"))).unwrap();
    let (_, log) = rig
        .http("GET", &format!("/api/tasks/runs/{id}/log"), None)
        .unwrap();
    assert_eq!(log["stdoutHidden"], true);
    assert!(
        !raw.contains(CHAT) && !log.to_string().contains(CHAT) && !run.to_string().contains(CHAT)
    );
}

#[test]
fn digest_uses_bridge_credentials_and_reauthenticates_from_the_selected_file() {
    for source in ["yaml-key", "refreshed-key", "logs-only"] {
        let fake = Fake::start(
            vec![session("auth-session", "QQ auth", now(), 1, 100)],
            vec![],
        );
        let mut rig = Rig::new(&fake, json!({}), None);
        let credentials = rig.root.join("user/dsh/.credentials.yaml");
        if source == "yaml-key" {
            std::fs::write(&credentials, format!("records:\n  client-connection/browser-session:\n    kind: grant\n    payload:\n      version: 1\n      secret: '{}'\n", URL_SAFE_NO_PAD.encode([37;32]))).unwrap();
        } else if source == "refreshed-key" {
            std::fs::write(
                &credentials,
                format!("secret: {}\n", URL_SAFE_NO_PAD.encode([91; 32])),
            )
            .unwrap();
            *fake.refresh_credentials.lock().unwrap() = Some(credentials.clone());
        } else {
            std::fs::remove_file(&credentials).unwrap();
        }
        let logs = rig.root.join("user/dsh/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("server-fixture.out.log"),
            "?token=selected_fake_launch_token",
        )
        .unwrap();
        rig.start();
        let run = rig.run("memory-digest", "digest-selected-auth");
        assert_eq!(
            run["state"],
            if source == "logs-only" {
                "failed"
            } else {
                "succeeded"
            },
            "{source}: {run}"
        );
        assert_eq!(fake.prompts().len(), usize::from(source != "logs-only"));
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .all(|call| !call["path"].as_str().unwrap().starts_with("/?token=")),
            "digest must use the bridge authentication path"
        );
        if source == "refreshed-key" {
            let lists: Vec<_> = calls
                .iter()
                .filter(|c| c["path"] == "/api/session/list")
                .collect();
            assert_eq!(lists.len(), 2);
            assert_eq!(
                lists[0]["body"], lists[1]["body"],
                "401 retries the same request once"
            );
            assert_ne!(
                lists[0]["header"], lists[1]["header"],
                "401 must re-read the selected credentials"
            );
        } else if source == "logs-only" {
            assert!(calls.is_empty());
        }
    }
}

#[test]
fn concurrent_check_skips_daily_gate_and_later_preserves_both_actions_progress() {
    let fake = Fake::with_faults(
        vec![
            session("shared", "QQ shared", now(), 80, 100),
            session("daily-only", "QQ daily only", now(), 10, 100),
            session(
                "check-only",
                "QQ check only",
                now() - 3 * 86_400_000,
                90,
                100,
            ),
        ],
        vec![("shared", Fault::Stall)],
    );
    let original = json!({"keep":{"unknown":true},"sessions":{"old":{"tokens":1}}});
    let mut rig = Rig::new(&fake, json!({}), Some(original.clone()));
    rig.start();
    let daily = rig.trigger("memory-digest", "digest-concurrent-daily");
    fake.wait_stalled(&rig, &daily);
    let check = rig.trigger("memory-digest-check", "digest-concurrent-check");
    let skipped = rig.wait_run(&check, Duration::from_secs(2));
    assert_eq!(skipped["state"], "succeeded", "{skipped}");
    let (_, log) = rig
        .http("GET", &format!("/api/tasks/runs/{check}/log"), None)
        .unwrap();
    assert!(
        log.to_string().contains("digest skipped: already running"),
        "{log}"
    );
    assert_eq!(
        fake.calls.lock().unwrap().len(),
        2,
        "second action must skip before selection/DSH"
    );
    assert_eq!(rig.state(), original, "first action is still dispatching");
    fake.release.store(true, Ordering::SeqCst);
    assert_eq!(
        rig.wait_run(&daily, Duration::from_secs(5))["state"],
        "succeeded"
    );
    assert_eq!(
        rig.run("memory-digest-check", "digest-check-after-daily")["state"],
        "succeeded"
    );
    assert_eq!(
        fake.prompts()
            .iter()
            .map(|p| p["sessionId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["shared", "daily-only", "check-only"],
        "check reloads state and avoids redispatching unchanged pressure"
    );
    let state = rig.state();
    assert_eq!(state["keep"], original["keep"]);
    assert_eq!(state["sessions"]["old"], original["sessions"]["old"]);
    for (id, tokens) in [("shared", 80), ("daily-only", 10), ("check-only", 90)] {
        assert_eq!(state["sessions"][id]["tokens"], tokens);
    }
    assert!(
        rig.root.join("data/locks/memory-digest.lock").is_file(),
        "persistent OS gate is retained after release"
    );
}

#[test]
fn stop_cancels_stalled_dsh_calls_and_401_recovery_without_more_dispatch() {
    for (fault, id, attempts) in [
        (Fault::Stall, "", 0),
        (Fault::Stall, "blocked", 1),
        (Fault::ReauthStall, "blocked", 2),
        (Fault::Pending401, "blocked", 1),
    ] {
        let fake = Fake::with_faults(
            vec![
                session("blocked", "QQ blocked", now(), 80, 100),
                session("later", "QQ later", now(), 80, 100),
            ],
            vec![(id, fault)],
        );
        let original = json!({"unknown":"keep","sessions":{"old":{"tokens":1}}});
        let mut rig = Rig::new(&fake, json!({}), Some(original.clone()));
        rig.start();
        let run_id = rig.trigger("memory-digest", "digest-stop-stalled");
        fake.wait_stalled(&rig, &run_id);
        let stopped = Instant::now();
        rig.stop_jobs();
        // Pending401 deliberately keeps the refused response connection open across stop.
        // The client must cancel the read, never finish recovery and repeat the prompt.
        let run = rig.wait_run(&run_id, Duration::from_secs(3));
        assert_eq!(run["state"], "cancelled", "{run}");
        assert!(
            stopped.elapsed() < Duration::from_secs(3),
            "must finish inside the supervisor's 10 s stop wait"
        );
        fake.release.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(150));
        let prompts = fake.prompts();
        assert_eq!(
            prompts.len(),
            attempts,
            "no prompt retry or later session after stop"
        );
        if attempts == 2 {
            assert_eq!(prompts[0], prompts[1], "401 retry retains the same request");
        }
        assert_eq!(rig.state(), original);
    }
}

#[test]
fn digest_reloads_bom_config_and_dependencies() {
    let fake = Fake::start(
        vec![session("bom-session", "QQ BOM", now(), 10, 100)],
        vec![],
    );
    let mut rig = Rig::new(&fake, json!({}), None);
    // Startup and later fresh-context construction share the runtime JSON loaders.
    for name in ["config/config.json", "dependencies.json"] {
        let path = rig.root.join(name);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("\u{feff}{text}")).unwrap();
    }
    rig.start();
    assert_eq!(
        rig.run("memory-digest", "digest-bom-daily")["state"],
        "succeeded"
    );
    assert_eq!(
        rig.run("memory-digest-check", "digest-bom-check")["state"],
        "succeeded"
    );
    assert_eq!(fake.prompts().len(), 1);
    assert_eq!(rig.state()["sessions"]["bom-session"]["tokens"], 10);
}

#[test]
fn digest_job_reloads_the_selected_dependency_endpoint_before_dispatch() {
    let old = Fake::start(vec![], vec![]);
    let selected = Fake::start(
        vec![session(
            "current-endpoint",
            "QQ selected current",
            now(),
            1,
            100,
        )],
        vec![],
    );
    let mut rig = Rig::new(&old, json!({}), None);
    rig.start();
    std::fs::write(
        rig.root.join("dependencies.json"),
        json!({"dsh":format!("http://127.0.0.1:{}",selected.port),
        "model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"})
        .to_string(),
    )
    .unwrap();
    let run = rig.run("memory-digest", "digest-reload-endpoint");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(
        selected.prompts().len(),
        1,
        "job must read current selected dependencies.json"
    );
    assert!(
        old.calls.lock().unwrap().is_empty(),
        "job must not contact the cached endpoint"
    );
    assert_eq!(rig.state()["sessions"]["current-endpoint"]["tokens"], 1);
}

#[test]
fn daily_selection_deduplicates_retains_last_seen_activity_and_uses_the_existing_daily_cap() {
    let updated = now();
    let mut busy = session("busy", "QQ busy", updated, 10, 100);
    busy["running"] = json!(true);
    let fake = Fake::start(
        vec![
            session("old", "QQ duplicate", updated - 2, 10, 100),
            session("new", "QQ duplicate", updated - 1, 10, 100),
            session("tie", "QQ duplicate", updated - 1, 10, 100),
            session("inactive", "QQ stale", updated - 3 * 86_400_000, 10, 100),
            session("seen", "QQ last seen", updated - 3 * 86_400_000, 10, 100),
            session("recent", "QQ numeric interval", updated, 10, 100),
            session("daily", "QQ unchanged tokens", updated, 10, 100),
            session("other", "not QQ", updated, 10, 100),
            busy,
        ],
        vec![],
    );
    let mut rig = Rig::new(
        &fake,
        json!({"maxSessionsPerDay":"1","minIntervalMinutes":"60","activeWindowHours":"24"}),
        Some(json!({
            "keep":"existing", "sessions":{"seen":{"at":"2020-01-01T00:00:00.000Z","lastUpdatedAt":updated},
                "recent":{"at":updated,"tokens":1},"daily":{"at":"2020-01-01T00:00:00.000Z","tokens":10}}
        })),
    );
    rig.start();
    let run = rig.run("memory-digest", "digest-daily-selection");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(
        fake.prompts()
            .iter()
            .map(|p| p["sessionId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["new", "seen", "daily"]
    );
    let before = std::fs::read(rig.root.join("data/digest-state.json")).unwrap();
    let run = rig.run("memory-digest", "digest-daily-capped");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(
        fake.prompts().len(),
        3,
        "the cap counts completed ISO entries before selecting"
    );
    assert_eq!(
        std::fs::read(rig.root.join("data/digest-state.json")).unwrap(),
        before
    );
    assert_eq!(rig.state()["keep"], "existing");
}

#[test]
fn disabled_empty_and_corrupt_state_jobs_preserve_the_existing_state_without_prompts() {
    for case in ["disabled", "non-qq", "empty", "corrupt"] {
        let items = if case == "empty" {
            vec![]
        } else {
            vec![session(
                "state-session",
                if case == "non-qq" {
                    "not QQ"
                } else {
                    "QQ state"
                },
                now(),
                1,
                100,
            )]
        };
        let fake = Fake::start(items, vec![]);
        let mut rig = Rig::new(&fake, json!({"enabled":case!="disabled"}), None);
        let file = rig.root.join("data/digest-state.json");
        if case != "empty" {
            std::fs::write(&file, CHAT).unwrap();
        }
        if case == "disabled" {
            std::fs::remove_file(rig.root.join("user/dsh/.credentials.yaml")).unwrap();
        }
        rig.start();
        let run = rig.run("memory-digest", "digest-state-cases");
        assert_eq!(
            run["state"],
            if case == "corrupt" {
                "failed"
            } else {
                "succeeded"
            },
            "{case}: {run}"
        );
        assert_eq!(run["exitCode"], if case == "corrupt" { 1 } else { 0 });
        assert!(fake.prompts().is_empty());
        if case == "disabled" {
            assert!(fake.calls.lock().unwrap().is_empty());
        }
        if case == "empty" {
            assert!(!file.exists());
        } else {
            assert_eq!(std::fs::read_to_string(file).unwrap(), CHAT);
        }
        assert!(!run.to_string().contains(CHAT));
    }
}

struct Redirect(PathBuf);
impl Redirect {
    fn directory(link: PathBuf, target: &std::path::Path) -> Self {
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &link).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let output = Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(link.to_string_lossy().replace('/', "\\"))
                .arg(target.to_string_lossy().replace('/', "\\"))
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "synthetic junction failed: link={} target={} exit={:?} out={} err={}",
                link.display(),
                target.display(),
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Self(link)
    }
}
impl Drop for Redirect {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.0);
        #[cfg(windows)]
        let _ = std::fs::remove_dir(&self.0);
    }
}

#[test]
fn redirected_dsh_credentials_are_refused_before_contacting_dsh() {
    let fake = Fake::start(
        vec![session("redirect", "QQ redirect", now(), 1, 100)],
        vec![],
    );
    let mut rig = Rig::new(&fake, json!({}), None);
    let selected = rig.root.join("user/dsh");
    let target = rig.root.join("redirected-credentials");
    std::fs::rename(&selected, &target).unwrap();
    let redirect = Redirect::directory(selected, &target);
    rig.start();
    let run = rig.run("memory-digest", "digest-redirect-refusal");
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(run["exitCode"], 1);
    assert!(
        fake.calls.lock().unwrap().is_empty(),
        "redirect reached DSH"
    );
    assert!(!rig.root.join("data/digest-state.json").exists());
    drop(redirect);
}

#[test]
fn rerun_after_kill_between_accept_and_checkpoint_reconciles_instead_of_resending() {
    for queued in [false, true] {
        let fake = Fake::with_faults(
            vec![
                session("accepted", "QQ accepted before kill", now(), 300, 1000),
                session("other", "QQ ordinary", now(), 10, 1000),
            ],
            vec![("accepted", Fault::Stall)],
        );
        fake.queue_accepted.store(queued, Ordering::SeqCst);
        let original = json!({"keep":"unknown","sessions":{"old":{"tokens":1}}});
        let mut rig = Rig::new(&fake, json!({}), Some(original.clone()));
        rig.start();
        let killed = rig.trigger("memory-digest", "digest-killed-after-accept");
        fake.wait_stalled(&rig, &killed);
        // DSH has accepted the prompt and holds it; the job dies before its checkpoint.
        rig.kill();
        let sent = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|c| c["path"] == "/api/session/prompt")
            .cloned()
            .unwrap();
        let intent = rig.dispatch()["pending"]["accepted"].clone();
        assert_eq!(intent["rpcId"], sent["body"]["rpcId"], "{queued}: {intent}");
        assert_eq!(
            intent["requestId"], sent["body"]["payload"]["args"]["request"]["requestId"],
            "{queued}: {intent}"
        );
        assert_eq!(
            rig.state(),
            original,
            "{queued}: no checkpoint before the kill"
        );
        fake.release.store(true, Ordering::SeqCst);

        rig.start();
        let rerun = rig.run("memory-digest", "digest-rerun-after-kill");
        assert_eq!(rerun["state"], "succeeded", "{queued}: {rerun}");
        assert_eq!(
            fake.prompts()
                .iter()
                .map(|p| p["sessionId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["accepted", "other"],
            "{queued}: the accepted prompt is not sent again; ordinary selection continues"
        );
        let state = rig.state();
        assert_eq!(state["keep"], "unknown");
        assert_eq!(state["sessions"]["old"], original["sessions"]["old"]);
        assert_eq!(state["sessions"]["accepted"]["tokens"], 300, "{queued}");
        assert_eq!(
            state["sessions"]["accepted"]["at"], intent["at"],
            "{queued}"
        );
        assert_eq!(
            state["sessions"]["accepted"]["title"],
            "QQ accepted before kill"
        );
        assert_eq!(state["sessions"]["other"]["tokens"], 10);
        assert_eq!(rig.dispatch()["pending"], json!({}), "{queued}");
        assert!(
            meta_lines(&rig, &rerun)
                .iter()
                .any(|line| line == "digest reconciled accepted=1 absent=0 unresolved=0"),
            "{queued}: {:?}",
            meta_lines(&rig, &rerun)
        );
    }
}

#[test]
fn recorded_intent_that_dsh_never_accepted_is_sent_on_rerun() {
    let fake = Fake::with_faults(
        vec![session("lost", "QQ lost in transit", now(), 50, 1000)],
        vec![("lost", Fault::Disconnect)],
    );
    let mut rig = Rig::new(&fake, json!({}), None);
    rig.start();
    let first = rig.run("memory-digest", "digest-transport-lost");
    assert_eq!(first["state"], "failed", "{first}");
    let intent = rig.dispatch()["pending"]["lost"].clone();
    assert!(
        intent["rpcId"].as_str().is_some_and(|id| !id.is_empty()),
        "an unknown outcome keeps its intent: {intent}"
    );
    assert!(!rig.root.join("data/digest-state.json").exists());
    let rerun = rig.run("memory-digest", "digest-transport-lost-rerun");
    assert_eq!(rerun["state"], "failed", "{rerun}");
    let prompts = fake.prompts();
    assert_eq!(
        prompts.len(),
        2,
        "DSH proves it never held the prompt, so the rerun dispatches it"
    );
    let resent = rig.dispatch()["pending"]["lost"].clone();
    assert_ne!(
        resent["rpcId"], intent["rpcId"],
        "a new dispatch has a new id"
    );
}

#[test]
fn completed_dispatch_and_explicit_refusal_leave_no_intent() {
    let fake = Fake::with_faults(
        vec![
            session("done", "QQ done", now(), 5, 1000),
            session("refused", "QQ refused", now(), 6, 1000),
        ],
        vec![("refused", Fault::Refuse)],
    );
    let mut rig = Rig::new(&fake, json!({}), None);
    rig.start();
    let run = rig.run("memory-digest", "digest-no-leftover-intent");
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(rig.dispatch()["pending"], json!({}));
    let repeat = rig.run("memory-digest", "digest-no-leftover-repeat");
    assert_eq!(repeat["state"], "failed", "{repeat}");
    assert_eq!(
        fake.prompts()
            .iter()
            .map(|p| p["sessionId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["done", "refused", "done", "refused"],
        "ordinary daily selection is unchanged"
    );
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c["path"] != "/api/remote.mux" && c["path"] != "/api/session/projections"),
        "nothing to reconcile"
    );
}

fn dispatch_intent(rpc_id: &str, at_ms: i64) -> Value {
    json!({"rpcId":rpc_id,"requestId":"seeded-request","task":"memory-digest",
        "at":"2026-11-02T00:00:00.000Z","atMs":at_ms,"tokens":40,"title":"QQ held","lastUpdatedAt":at_ms})
}

fn seed_dispatch(rig: &Rig, session: &str, intent: Value) {
    std::fs::write(
        rig.root.join("data/digest-dispatch.json"),
        json!({"v":1,"pending":{session:intent}}).to_string(),
    )
    .unwrap();
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

fn sessions_prompted(fake: &Fake) -> Vec<String> {
    fake.prompts()
        .iter()
        .map(|p| p["sessionId"].as_str().unwrap().to_string())
        .collect()
}

const HOUR_MS: i64 = 3_600_000;

#[test]
fn an_intent_dsh_cannot_settle_holds_its_session_while_recent_and_not_the_others() {
    for (mode, outcome) in [(FOLLOW_TRUNCATED, "succeeded"), (FOLLOW_DROPPED, "failed")] {
        let fake = Fake::start(
            vec![
                session("held", "QQ held", now(), 40, 1000),
                session("other", "QQ ordinary", now(), 10, 1000),
            ],
            vec![],
        );
        fake.follow_mode.store(mode, Ordering::SeqCst);
        let mut rig = Rig::new(&fake, json!({}), None);
        let intent = dispatch_intent("rpc-unsettled", now() - 19 * HOUR_MS);
        seed_dispatch(&rig, "held", intent.clone());
        rig.start();
        let run = rig.run("memory-digest", "digest-unresolved-recent");
        assert_eq!(
            run["state"], outcome,
            "{mode}: only an unreachable DSH counts as a failure: {run}"
        );
        assert_eq!(
            sessions_prompted(&fake),
            ["other"],
            "{mode}: the unresolved session is not prompted again"
        );
        assert_eq!(
            rig.dispatch()["pending"]["held"],
            intent,
            "{mode}: the intent is kept for the next run"
        );
        assert!(rig.state()["sessions"].get("held").is_none(), "{mode}");
        assert!(
            meta_lines(&rig, &run)
                .iter()
                .any(|line| line == "digest reconciled accepted=0 absent=0 unresolved=1"),
            "{mode}: {:?}",
            meta_lines(&rig, &run)
        );
    }
}

#[test]
fn an_unsettled_intent_older_than_the_hold_is_dropped_and_its_session_is_sent_normally() {
    for mode in [FOLLOW_TRUNCATED, FOLLOW_DROPPED] {
        let fake = Fake::start(vec![session("held", "QQ held", now(), 40, 1000)], vec![]);
        fake.follow_mode.store(mode, Ordering::SeqCst);
        let mut rig = Rig::new(&fake, json!({}), None);
        let intent = dispatch_intent("rpc-stale", now() - 21 * HOUR_MS);
        seed_dispatch(&rig, "held", intent.clone());
        rig.start();
        let run = rig.run("memory-digest", "digest-unresolved-stale");
        assert_eq!(run["state"], "succeeded", "{mode}: {run}");
        assert_eq!(sessions_prompted(&fake), ["held"], "{mode}");
        assert_eq!(rig.dispatch()["pending"], json!({}), "{mode}");
        assert_eq!(rig.state()["sessions"]["held"]["tokens"], 40, "{mode}");
        assert!(
            meta_lines(&rig, &run)
                .iter()
                .any(|line| line == "digest reconciled accepted=0 absent=0 unresolved=1"),
            "{mode}: {:?}",
            meta_lines(&rig, &run)
        );
    }
}

#[test]
fn a_session_log_holding_only_another_rpc_id_does_not_hold_the_intent() {
    let fake = Fake::start(vec![session("held", "QQ held", now(), 40, 1000)], vec![]);
    {
        // An older digest prompt, already answered, sits in the same session log.
        let mut held = fake.held.lock().unwrap();
        let log = held.events.entry("held".to_string()).or_default();
        log.push(
            json!({"seq":1,"type":"user/message","data":{"source":{"rpcId":"rpc-older"},
            "content":[{"type":"text","text":"older digest prompt"}]}}),
        );
        log.push(json!({"seq":2,"type":"turn/start","data":{"turn":1}}));
    }
    let mut rig = Rig::new(&fake, json!({}), None);
    seed_dispatch(&rig, "held", dispatch_intent("rpc-lost", now() - HOUR_MS));
    rig.start();
    let run = rig.run("memory-digest", "digest-other-rpc-id");
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(
        sessions_prompted(&fake),
        ["held"],
        "a probe that matched any user message would have held the session"
    );
    assert_eq!(rig.dispatch()["pending"], json!({}));
    assert!(
        meta_lines(&rig, &run)
            .iter()
            .any(|line| line == "digest reconciled accepted=0 absent=1 unresolved=0"),
        "{:?}",
        meta_lines(&rig, &run)
    );
}
