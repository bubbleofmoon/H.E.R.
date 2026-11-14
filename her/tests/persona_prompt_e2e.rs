//! ticket 11 external-behaviour acceptance: persona (role) injection, the delivered prompt text
//! (clock line, briefing before current state), the operator's initial prompt (first actual session hydration, again after a
//! session reset, never twice across restarts) and the console that maintains all of it.
//!
//! Everything runs against real `her` processes (live bridge + `--console-serve`) on a
//! synthetic runtime root, a fake DSH (the simulated model reads the prompt it was given, asks
//! `qq_get_prompt` through the public console route with the token from the prompt, then sends
//! through the public send route), a fake OneBot WebSocket gateway and HTTP API. No real QQ,
//! model or DSH endpoint is contacted.

#[path = "../src/install_root.rs"]
mod install_root;
#[path = "support/console.rs"]
mod console_fixture;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::{accept, Message};

const GROUP_ID: i64 = 420042;
const GROUP_KEY: &str = "group:420042";
const SELF_ID: i64 = 9001;
const ONEBOT_TOKEN: &str = "ticket11-onebot-token-0001";
const CONSOLE_TOKEN: &str = "ticket11-console-token-0001";
const SEED_TOKEN: &str = "ticket11-seed-agent-token-0001";
const SECRET: &str = "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n";
const LONG: Duration = Duration::from_secs(45);

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_millis() as i64
}

fn wait_for(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ───────────────────────── small HTTP helpers ─────────────────────────

struct HttpReq {
    request_line: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<HttpReq> {
    stream.set_read_timeout(Some(Duration::from_secs(8))).ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&bytes[..split]).to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
            })
            .unwrap_or(0);
        if bytes.len() >= split + 4 + length {
            return Some(HttpReq {
                request_line: head.lines().next()?.to_string(),
                body: bytes[split + 4..split + 4 + length].to_vec(),
            });
        }
    }
}

fn respond(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = value.to_string();
    let text = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes());
}

fn http_call(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>) -> Option<(u16, Value)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if body.is_some() {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", payload.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).ok()?;
    stream.write_all(payload.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, serde_json::from_str(body).unwrap_or(Value::Null)))
}

// ───────────────────────── fake DSH with a simulated model ─────────────────────────

#[derive(Default)]
struct Sess {
    events: Vec<Value>,
    seq: i64,
    turn_no: i64,
}

#[derive(Clone, Debug)]
struct PromptRec {
    session: String,
    text: String,
    /// What `qq_get_prompt` answered while this turn ran (the model's view of its role).
    get_prompt: Value,
}

struct DshInner {
    sessions: HashMap<String, Sess>,
    next_sid: u32,
    rpc: Vec<String>,
    prompts: Vec<PromptRec>,
}

struct Dsh {
    inner: Mutex<DshInner>,
    console_port: AtomicU16,
}

fn event(sess: &mut Sess, kind: &str, data: Value) {
    sess.seq += 1;
    sess.events.push(json!({"seq": sess.seq, "type": kind, "time": now_ms(), "data": data}));
}

impl Dsh {
    fn new(console_port: u16) -> Arc<Self> {
        Arc::new(Self {
            console_port: AtomicU16::new(console_port),
            inner: Mutex::new(DshInner { sessions: HashMap::new(), next_sid: 0, rpc: Vec::new(), prompts: Vec::new() }),
        })
    }

    fn prompts(&self) -> Vec<PromptRec> {
        lock(&self.inner).prompts.clone()
    }

    fn count(&self, method: &str) -> usize {
        lock(&self.inner).rpc.iter().filter(|m| m.as_str() == method).count()
    }

    fn rpc(self: &Arc<Self>, method: &str, args: &Value) -> Result<Value, (String, String)> {
        let mut inner = lock(&self.inner);
        inner.rpc.push(method.to_string());
        let sid = args.pointer("/request/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        match method {
            "workspace/create" => Ok(json!({"workspace": {"workspaceId": "ws-ticket11"}, "created": false})),
            "workspace/rename" | "agentPresets/list" | "session/selectModel" | "workspace/archiveSession" => Ok(json!({})),
            "session/create" => {
                inner.next_sid += 1;
                let id = format!("fake-session-{}", inner.next_sid);
                inner.sessions.insert(id.clone(), Sess::default());
                Ok(json!({"sessionId": id}))
            }
            "session/projections" => {
                let seq = inner.sessions.get(&sid).map(|s| s.seq).unwrap_or(0);
                Ok(json!({"asOfSeq": seq, "values": {"inbox": {"next-turn": [], "next-step": []}}}))
            }
            "session/updateQueue" | "session/cancel" => Ok(json!({"accepted": true})),
            _ => Err(("unexpected".into(), format!("unexpected fake DSH method {method}"))),
        }
    }

    fn prompt(self: &Arc<Self>, args: &Value, rpc_id: &str) -> Result<Value, (String, String)> {
        let sid = args.pointer("/request/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        let text = args.pointer("/request/content/0/text").and_then(Value::as_str).unwrap_or("").to_string();
        {
            let mut inner = lock(&self.inner);
            inner.rpc.push("session/prompt".into());
            let Some(sess) = inner.sessions.get_mut(&sid) else {
                return Err(("session-not-found".into(), "session not found".into()));
            };
            event(
                sess,
                "user/message",
                json!({"role": "user", "id": format!("msg-{rpc_id}"), "content": [{"type": "text", "text": text}],
                       "source": {"kind": "user", "rpcId": rpc_id}}),
            );
            inner.prompts.push(PromptRec { session: sid.clone(), text: text.clone(), get_prompt: Value::Null });
        }
        let world = self.clone();
        thread::spawn(move || world.run_turn(&sid, &text));
        Ok(json!({"accepted": true}))
    }

    /// The simulated model: reads key and token from the prompt it received, calls the public
    /// `qq_get_prompt` route, then sends one message through the public send route.
    fn run_turn(self: &Arc<Self>, sid: &str, prompt: &str) {
        let turn = {
            let mut inner = lock(&self.inner);
            let sess = inner.sessions.get_mut(sid).expect("session");
            sess.turn_no += 1;
            let turn = sess.turn_no;
            event(sess, "turn/start", json!({"turn": turn}));
            event(
                sess,
                "tool/call",
                json!({"turn": turn, "step": 0, "callId": format!("c{turn}"), "name": "mcp__snowluma__qq_send_message", "arguments": "{}"}),
            );
            turn
        };
        let key = prompt
            .lines()
            .find_map(|l| l.strip_prefix("【唤醒】").or_else(|| l.strip_prefix("【引导唤醒】你已接入 QQ 会话 ")))
            .map(|l| l.trim().trim_end_matches('。').to_string());
        let token = prompt
            .lines()
            .find_map(|line| line.strip_prefix("【会话令牌】"))
            .and_then(|line| line.split_once('（').map(|(token, _)| token.trim().to_string()));
        let mut prompt_view = Value::Null;
        if let (Some(key), Some(token)) = (key.clone(), token.clone()) {
            let path = format!("/api/socialV2/prompt?key={key}");
            let got = http_call(self.console_port.load(Ordering::Relaxed), "GET", &path, &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", &token)], None);
            prompt_view = got.map(|(status, body)| json!({"status": status, "body": body})).unwrap_or(Value::Null);
            let _ = http_call(
                self.console_port.load(Ordering::Relaxed),
                "POST",
                "/api/socialV2/send-message",
                &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", &token)],
                Some(&json!({"key": key, "messages": ["收到"]})),
            );
        }
        let mut inner = lock(&self.inner);
        if let Some(rec) = inner.prompts.iter_mut().rev().find(|p| p.text == prompt && p.session == sid) {
            rec.get_prompt = prompt_view;
        }
        let sess = inner.sessions.get_mut(sid).expect("session");
        event(
            sess,
            "tool/result",
            json!({"turn": turn, "step": 0, "message": {"isError": false, "source": {"callId": format!("c{turn}")}, "content": [{"type": "text", "text": "ok"}]}}),
        );
        event(
            sess,
            "assistant/message",
            json!({"turn": turn, "step": 1, "provider": "ticket11-provider",
                   "usage": {"inputTokens": 7, "outputTokens": 3},
                   "message": {"content": [{"type": "text", "text": "synthetic turn complete"}]}}),
        );
        event(sess, "turn/end", json!({"turn": turn, "reason": {"kind": "completed"}}));
    }
}

fn dsh_connection(world: Arc<Dsh>, mut stream: TcpStream) {
    let mut peek = [0u8; 64];
    let Ok(n) = stream.peek(&mut peek) else { return };
    if String::from_utf8_lossy(&peek[..n]).starts_with("GET /api/remote.mux") {
        dsh_follow(world, stream);
        return;
    }
    let Some(request) = read_request(&mut stream) else { return };
    let Ok(envelope) = serde_json::from_slice::<Value>(&request.body) else { return };
    let method = envelope.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let rpc_id = envelope.get("rpcId").and_then(Value::as_str).unwrap_or("").to_string();
    let args = envelope.pointer("/payload/args").cloned().unwrap_or(Value::Null);
    let outcome = if method == "session/prompt" { world.prompt(&args, &rpc_id) } else { world.rpc(&method, &args) };
    let reply = match outcome {
        Ok(value) => json!({"type": "server-response", "rpcId": rpc_id, "result": {"ok": true, "value": value}}),
        Err((code, message)) => {
            json!({"type": "server-response", "rpcId": rpc_id, "result": {"ok": false, "error": {"code": code, "message": message}}})
        }
    };
    respond(&mut stream, 200, &reply);
}

fn dsh_follow(world: Arc<Dsh>, stream: TcpStream) {
    let Ok(mut socket) = accept(stream) else { return };
    let Ok(Message::Text(open)) = socket.read() else { return };
    let Ok(frame) = serde_json::from_str::<Value>(&open) else { return };
    let stream_id = frame.get("streamId").and_then(Value::as_str).unwrap_or("").to_string();
    let sid = frame.pointer("/payload/args/request/address/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let events = lock(&world.inner).sessions.get(&sid).map(|s| s.events.clone()).unwrap_or_default();
    let mut sent = events.last().and_then(|e| e["seq"].as_i64()).unwrap_or(0);
    let records: Vec<Value> = events.iter().map(|e| json!({"type": "event", "event": e})).collect();
    let snapshot = json!({
        "type": "item", "streamId": stream_id,
        "value": {"type": "snapshot", "records": records, "hasMore": false,
                  "projections": {"values": {"inbox": {"next-turn": [], "next-step": []}}}}
    });
    if socket.send(Message::Text(snapshot.to_string())).is_err() {
        return;
    }
    let _ = socket.get_ref().set_read_timeout(Some(Duration::from_millis(50)));
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let fresh: Vec<Value> = {
            let inner = lock(&world.inner);
            inner
                .sessions
                .get(&sid)
                .map(|s| s.events.iter().filter(|e| e["seq"].as_i64().unwrap_or(0) > sent).cloned().collect())
                .unwrap_or_default()
        };
        for ev in fresh {
            sent = ev["seq"].as_i64().unwrap_or(sent);
            let item = json!({"type": "item", "streamId": stream_id, "value": {"type": "event", "event": ev}});
            if socket.send(Message::Text(item.to_string())).is_err() {
                return;
            }
        }
        match socket.read() {
            Ok(Message::Close(_)) => return,
            Err(tokio_tungstenite::tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn serve_dsh(listener: TcpListener, world: Arc<Dsh>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let world = world.clone();
        thread::spawn(move || dsh_connection(world, stream));
    }
}

// ───────────────────────── fake OneBot ─────────────────────────

#[derive(Default)]
struct Gateway {
    sends: Vec<Value>,
    next_message_id: i64,
    scripts: Vec<Vec<Step>>,
}

#[derive(Clone)]
enum Step {
    Mention { message_id: i64, text: &'static str },
    Wait(u64),
}

fn mention_event(message_id: i64, text: &str) -> Value {
    json!({
        "time": now_ms() / 1000, "self_id": SELF_ID, "post_type": "message", "message_type": "group",
        "sub_type": "normal", "message_id": message_id, "group_id": GROUP_ID, "user_id": 1001,
        "sender": {"nickname": "tester", "card": ""},
        "message": [{"type": "at", "data": {"qq": SELF_ID.to_string()}}, {"type": "text", "data": {"text": format!(" {text}")}}],
        "raw_message": format!("[CQ:at,qq={SELF_ID}] {text}"), "font": 0
    })
}

fn serve_gateway_ws(listener: TcpListener, gateway: Arc<Mutex<Gateway>>) {
    loop {
        let Ok((stream, _)) = listener.accept() else { return };
        let Ok(mut socket) = accept(stream) else { continue };
        let script = {
            let mut g = lock(&gateway);
            if g.scripts.is_empty() { Vec::new() } else { g.scripts.remove(0) }
        };
        for step in script {
            match step {
                Step::Mention { message_id, text } => {
                    if socket.send(Message::Text(mention_event(message_id, text).to_string())).is_err() {
                        break;
                    }
                }
                Step::Wait(ms) => thread::sleep(Duration::from_millis(ms)),
            }
        }
        let _ = socket.get_ref().set_read_timeout(Some(Duration::from_millis(200)));
        let end = Instant::now() + Duration::from_secs(300);
        while Instant::now() < end {
            match socket.read() {
                Ok(Message::Close(_)) => break,
                Err(tokio_tungstenite::tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => break,
                Ok(_) => {}
            }
        }
    }
}

fn serve_onebot(listener: TcpListener, gateway: Arc<Mutex<Gateway>>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let gateway = gateway.clone();
        thread::spawn(move || {
            let Some(request) = read_request(&mut stream) else { return };
            let path = request.request_line.split_whitespace().nth(1).unwrap_or("").to_string();
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let reply = {
                let mut g = lock(&gateway);
                match path.as_str() {
                    "/send_group_msg" | "/send_msg" => {
                        g.sends.push(body.clone());
                        g.next_message_id += 1;
                        json!({"status": "ok", "retcode": 0, "data": {"message_id": 880000 + g.next_message_id}})
                    }
                    "/get_group_msg_history" => json!({"status": "ok", "retcode": 0, "data": {"messages": []}}),
                    _ => json!({"status": "ok", "retcode": 0, "data": {"card": "", "nickname": "tester", "user_id": SELF_ID}}),
                }
            };
            respond(&mut stream, 200, &reply);
        });
    }
}

// ───────────────────────── rig ─────────────────────────

fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

struct Rig {
    root: PathBuf,
    console_port: u16,
    dsh: Arc<Dsh>,
    gateway: Arc<Mutex<Gateway>>,
    console: Child,
    candidate: Option<Child>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if thread::panicking() {
            for name in ["candidate-out.log", "candidate-err.log", "console-out.log", "console-err.log"] {
                let text = fs::read_to_string(self.root.join(name)).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(60).collect();
                eprintln!("---- {name} (tail) ----\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
            }
        }
        if let Some(mut c) = self.candidate.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = self.console.kill();
        let _ = self.console.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Initial files of the synthetic root.
struct Seed {
    bootstrap_sent: bool,
    /// `role-map.json` content, if any.
    role_map: Option<Value>,
    /// `current-role.json` content, if any.
    current_role: Option<Value>,
    /// `(name, content)` role cards under `resources/roles`.
    roles: Vec<(&'static str, &'static str)>,
    /// `initial-prompts.json` content, if any.
    initial_prompts: Option<Value>,
    /// `personas.json` content, if any.
    personas: Option<Value>,
    /// Extra keys merged into the `config.json` top level.
    config: Value,
}

impl Default for Seed {
    fn default() -> Self {
        Seed { bootstrap_sent: false, role_map: None, current_role: None, roles: Vec::new(), initial_prompts: None, personas: None, config: json!({}) }
    }
}

impl Rig {
    fn new(name: &str, seed: Seed) -> Self {
        Self::new_with_console_hook(name, seed, |_| {})
    }

    fn new_with_console_hook(name: &str, seed: Seed, mut after_release: impl FnMut(u16)) -> Self {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-ticket11-{name}-{}-{}", std::process::id(), now_ms())));
        for dir in ["config", "data/logs", "resources/roles", "user/dsh", "ambient"] {
            fs::create_dir_all(root.join(dir)).expect("create runtime dir");
        }
        // Keep the port unavailable to the fake listeners until the console can bind.
        let reservation = console_fixture::reserve_port();
        let console_port = reservation.local_addr().expect("console addr").port();
        let dsh = Dsh::new(console_port);
        let gateway = Arc::new(Mutex::new(Gateway::default()));
        let dsh_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind dsh");
        let dsh_url = format!("http://127.0.0.1:{}", dsh_listener.local_addr().unwrap().port());
        let ob_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind onebot");
        let ob_url = format!("http://127.0.0.1:{}", ob_listener.local_addr().unwrap().port());
        let ws_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ws");
        let ws_url = format!("ws://127.0.0.1:{}", ws_listener.local_addr().unwrap().port());
        {
            let dsh = dsh.clone();
            thread::spawn(move || serve_dsh(dsh_listener, dsh));
            let g = gateway.clone();
            thread::spawn(move || serve_onebot(ob_listener, g));
            let g = gateway.clone();
            thread::spawn(move || serve_gateway_ws(ws_listener, g));
        }
        let mut config = json!({
            "consoleToken": CONSOLE_TOKEN, "ownerQQ": 8808,
            "allow": {"groups": [GROUP_ID], "private": []},
            "dsh": {"baseUrl": dsh_url, "provider": "ticket11-provider", "model": "ticket11-model", "reasoningEffort": "medium", "fallbacks": []},
            "snowluma": {"accessToken": ONEBOT_TOKEN, "httpUrl": ob_url, "wsUrl": ws_url},
            "socialV2": {
                "agentPreset": "ticket11-preset",
                "wake": {"batchWindowMs": 1000, "maxWakePerMinute": 30, "maxWakePerHour": 100},
                "social": {"skipProbability": 0.0},
                "tools": {"sendMessage": true, "getPrompt": true},
                "sticker": {"enabled": false},
                "send": {"mergeMultiIntoOne": true, "maxMessageChars": 800, "maxSendPerMinute": 0, "maxSendPerHour": 0, "burstIntervalMinMs": 0, "maxGapMs": 0, "gapBaseMs": 0, "gapPerCharMs": 0}
            },
            "slang": {"enabled": false},
            "rollout": {"enabled": false, "rustGroups": []}
        });
        for (key, value) in seed.config.as_object().cloned().unwrap_or_default() {
            match (config.get_mut(&key), value) {
                (Some(Value::Object(existing)), Value::Object(extra)) => existing.extend(extra),
                (_, value) => config[&key] = value,
            }
        }
        fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        fs::write(
            root.join("data/social-v2.json"),
            json!({"conversations": {GROUP_KEY: {
                "agentToken": SEED_TOKEN, "unread": [], "recentMessages": [], "bootstrapSent": seed.bootstrap_sent,
                "wakeConfig": {"mode": "active", "infinite": true,
                    "triggers": {"atMention": true, "nameMention": false, "question": false, "poke": false, "anyMessage": false, "probability": 0.0},
                    "batchWindowMs": 1000}
            }}})
            .to_string(),
        )
        .unwrap();
        fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        fs::write(root.join("user/dsh/.credentials.yaml"), SECRET).unwrap();
        fs::write(
            root.join("dependencies.json"),
            json!({"dsh": dsh_url, "model": dsh_url, "onebot": ob_url, "onebot_ws": ws_url}).to_string(),
        )
        .unwrap();
        if let Some(map) = &seed.role_map {
            fs::write(root.join("data/role-map.json"), map.to_string()).unwrap();
        }
        if let Some(current) = &seed.current_role {
            fs::write(root.join("data/current-role.json"), current.to_string()).unwrap();
        }
        for (name, content) in &seed.roles {
            fs::write(root.join(format!("resources/roles/{name}.md")), content).unwrap();
        }
        if let Some(prompts) = &seed.initial_prompts {
            fs::write(root.join("data/initial-prompts.json"), prompts.to_string()).unwrap();
        }
        if let Some(personas) = &seed.personas {
            fs::write(root.join("data/personas.json"), personas.to_string()).unwrap();
        }
        drop(reservation);
        after_release(console_port);
        let console = spawn_console(&root, console_port);
        let mut rig = Self { root, console_port, dsh, gateway, console, candidate: None };
        console_fixture::wait_for_console(&mut rig.console, &mut rig.console_port, &rig.root, CONSOLE_TOKEN, Duration::from_secs(20), |port| {
            rig.dsh.console_port.store(port, Ordering::Relaxed);
            after_release(port);
            spawn_console(&rig.root, port)
        });
        rig
    }

    fn start_candidate(&mut self, script: Vec<Step>) {
        lock(&self.gateway).scripts.push(script);
        let log = |name: &str| fs::OpenOptions::new().create(true).append(true).open(self.root.join(name)).unwrap();
        let ambient = self.root.join("ambient");
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .arg("--runtime-root").arg(&self.root)
            .arg("--live").arg("--live-groups").arg(GROUP_KEY)
            .arg("--console-port").arg(self.console_port.to_string())
            .env_remove("HER_RUNTIME_ROOT")
            .env("TZ", "UTC")
            .env("HOME", &ambient).env("USERPROFILE", &ambient).env("DSH_HOME", &ambient)
            .env("APPDATA", ambient.join("Roaming")).env("LOCALAPPDATA", ambient.join("Local"))
            .stdout(Stdio::from(log("candidate-out.log")))
            .stderr(Stdio::from(log("candidate-err.log")));
        hide_window(&mut command);
        self.candidate = Some(command.spawn().expect("spawn candidate"));
    }

    fn kill_candidate(&mut self) {
        if let Some(mut c) = self.candidate.take() {
            c.kill().expect("hard-kill candidate");
            let _ = c.wait();
        }
    }

    fn restart(&mut self, script: Vec<Step>) {
        self.kill_candidate();
        self.start_candidate(script);
    }

    fn console(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        http_call(self.console_port, method, path, &[("x-console-token", CONSOLE_TOKEN)], body).unwrap_or((0, Value::Null))
    }

    fn data_json(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.root.join("data").join(name)).unwrap_or_default()).unwrap_or(Value::Null)
    }

    fn onebot_sends(&self) -> usize {
        lock(&self.gateway).sends.len()
    }

    /// Wait until `count` prompts reached DSH and every simulated turn has finished its tool calls.
    fn wait_prompts(&self, count: usize) -> Vec<PromptRec> {
        assert!(wait_for(LONG, || self.dsh.prompts().len() >= count), "prompt {count} never reached DSH");
        assert!(
            wait_for(LONG, || self.dsh.prompts().iter().take(count).all(|p| !p.get_prompt.is_null())),
            "the simulated model did not finish its tool calls: {:?}",
            self.dsh.prompts()
        );
        self.dsh.prompts()
    }
}

fn spawn_console(root: &Path, port: u16) -> Child {
    let ambient = root.join("ambient");
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--console-serve").arg("--console-port").arg(port.to_string())
        .arg("--runtime-root").arg(root)
        .env_remove("HER_RUNTIME_ROOT")
        .env("HOME", &ambient).env("USERPROFILE", &ambient).env("DSH_HOME", &ambient)
        .env("APPDATA", ambient.join("Roaming")).env("LOCALAPPDATA", ambient.join("Local"))
        .stdout(Stdio::from(fs::OpenOptions::new().create(true).append(true).open(root.join("console-out.log")).unwrap()))
        .stderr(Stdio::from(fs::OpenOptions::new().create(true).append(true).open(root.join("console-err.log")).unwrap()));
    hide_window(&mut command);
    command.spawn().expect("spawn console")
}

fn mention(id: i64, text: &'static str) -> Vec<Step> {
    vec![Step::Wait(500), Step::Mention { message_id: id, text }]
}

// ───────────────────────── fixtures ─────────────────────────

/// A card with both mode-tagged sections and a first-generation example section.
const CARD_SHARED: &str = "# 小鲸鱼\n你是一只爱聊天的小鲸鱼。\n\n## 表情包体系 〔二代〕\n只在二代出现。\n\n## 分条方式 〔一代〕\n用空格分条，沉默时输出 [SILENT]。\n\n## 回复示例\n你好 世界\n";
const CARD_OVERRIDE: &str = "你是会计助手，只讲数字。\n";
const INITIAL_TEXT: &str = "这是一个动漫讨论群。\n氛围轻松，别太正式。";

/// A stored library with one approved default persona.
fn library(name: &str, core: &str) -> Value {
    json!({"format": 1, "nextId": 2, "default": "p1", "overrides": {}, "importedRolesAt": null,
        "personas": {"p1": {"id": "p1", "activeVersion": 1, "versions": [{"version": 1, "approved": true, "name": name, "core": core}]}}})
}

/// Creates and approves a persona through the console; returns its id.
fn approved_persona(rig: &Rig, name: &str, core: &str) -> String {
    let (status, created) = rig.console("POST", "/api/personas/create", Some(&json!({"name": name, "core": core})));
    assert_eq!(status, 200, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let (status, approved) = rig.console("POST", "/api/personas/approve", Some(&json!({"id": id, "version": 1})));
    assert_eq!(status, 200, "{approved}");
    id
}

// ───────────────────────── assertions on the delivered prompt ─────────────────────────

/// Positions of the section markers of the delivered prompt, in the order JS buildWakePromptV2
/// emits them. Returns the first missing marker as an error.
fn positions(text: &str, markers: &[&str]) -> Result<Vec<usize>, String> {
    markers.iter().map(|m| text.find(m).ok_or_else(|| format!("missing {m}"))).collect()
}

fn assert_in_order(text: &str, markers: &[&str]) {
    let pos = positions(text, markers).unwrap_or_else(|e| panic!("{e} in prompt:\n{text}"));
    assert!(pos.windows(2).all(|w| w[0] < w[1]), "sections out of order {markers:?} -> {pos:?} in prompt:\n{text}");
}

fn clock_line(text: &str) -> (String, String, i64) {
    let first = text.lines().next().unwrap_or("");
    let rest = first.strip_prefix("【当前时间】").unwrap_or_else(|| panic!("prompt must start with the clock line:\n{text}"));
    let (stamp, tz) = rest.split_once('（').unwrap_or_else(|| panic!("clock line has no zone: {first}"));
    let tz = tz.trim_end_matches('）').to_string();
    // `Y/M/D HH:MM:SS` (month and day without padding)
    let (date, time) = stamp.split_once(' ').expect("date and time");
    let d: Vec<i64> = date.split('/').map(|x| x.parse().expect("date part")).collect();
    let t: Vec<i64> = time.split(':').map(|x| x.parse().expect("time part")).collect();
    assert_eq!(time.len(), 8, "time is zero padded: {time}");
    assert_eq!(text.split_once('\n').map(|(_, r)| r.starts_with('\n')), Some(true), "clock line must be followed by a blank line");
    let days = days_from_civil(d[0], d[1], d[2]);
    (stamp.to_string(), tz, (days * 86_400 + t[0] * 3600 + t[1] * 60 + t[2]) * 1000)
}

/// Howard Hinnant's days_from_civil (test-local, independent of the product code).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ───────────────────────── AC3: delivered prompt text and order ─────────────────────────

#[test]
fn strip_model_text_dsh_filters_message_nickname_and_reply_but_keeps_stored_chat() {
    let mut rig = Rig::new("strip-model-text", Seed {
        config: json!({"socialV2":{"context":{"stripFromModelText":["奸"]}}}),
        ..Seed::default()
    });
    let message = json!({"seq":1,"messageId":"82","userId":"1002","sender":"奸昵称",
        "text":"奸消息 [引用 奸人：奸原话] [图片] [CQ:at,qq=123]","time":now_ms()});
    let mut state = rig.data_json("social-v2.json");
    state["conversations"][GROUP_KEY]["recentMessages"] = json!([message]);
    // Seed its consumed cursor so the arriving message receives a distinct local sequence.
    state["conversations"][GROUP_KEY]["lastUnreadSeq"] = json!(1);
    state["conversations"][GROUP_KEY]["lastReadThroughSeq"] = json!(1);
    fs::write(rig.root.join("data/social-v2.json"), state.to_string()).unwrap();
    rig.restart(mention(1001, "奸新消息"));
    let prompts = rig.wait_prompts(1);
    let text = &prompts[0].text;
    assert!(!text.contains('奸'), "{text}");
    assert!(text.contains("昵称"), "nickname must reach model: {text}");
    assert!(text.contains("[引用 人：原话] [图片] [CQ:at,qq=123]"), "{text}");
    let stored = rig.data_json("social-v2.json");
    assert!(stored["conversations"][GROUP_KEY]["recentMessages"].as_array().unwrap().contains(&message));
    let (status, view) = rig.console("GET", &format!("/api/socialV2/recent?key={GROUP_KEY}"), None);
    assert_eq!(status, 200);
    assert!(view.to_string().contains('奸'), "console view must retain original chat");
}

#[test]
fn first_wake_delivers_a_briefing_before_current_authority_state_and_snapshot() {
    let mut rig = Rig::new(
        "first",
        Seed {
            personas: Some(library("小鲸鱼", "你是一只爱聊天的小鲸鱼。")),
            initial_prompts: Some(json!({GROUP_KEY: {"text": INITIAL_TEXT, "createdAt": 1, "updatedAt": 2}})),
            ..Seed::default()
        },
    );
    let before = now_ms();
    rig.restart(mention(1001, "第一句"));
    let prompts = rig.wait_prompts(1);
    let text = &prompts[0].text;
    let after = now_ms();

    // The clock line of the old withSlangContext: zone name from TZ, time within the run window.
    let (_, zone, at_ms) = clock_line(text);
    assert_eq!(zone, "UTC");
    assert!(at_ms >= before / 1000 * 1000 - 2000 && at_ms <= after + 2000, "clock line {at_ms} not within [{before},{after}]");

    // Live DSH sends briefing first, then current authority/state/reason and its packet.
    // Compatibility full/debug builders retain their separate historical order.
    assert_in_order(
        text,
        &[
            "【当前时间】", "【简报 revision=",
            "【人设】小鲸鱼\n你是一只爱聊天的小鲸鱼。", "【你的身份】",
            "【群聊环境（操作者预设，仅本次首次唤醒参考）】", "【沉睡前强制等待】",
            "【会话令牌】", "【此刻状态】", "【当前唤醒】",
            "【引导唤醒】你已接入 QQ 会话 group:420042", "【本次消息快照】",
        ],
    );
    assert!(!text.contains("qq_wait_for_messages(purpose=\"reply\""), "{text}");
    let briefing=text.split("【会话令牌】").next().unwrap();
    for rule in ["【反AI味】","【积极性】","【回复节奏】"] {
        assert!(briefing.contains(rule),"briefing omitted {rule}: {text}");
    }
    assert!(text.contains("readThroughSeq") && text.contains("throughSeq"));
    assert!(text.contains(&format!("{INITIAL_TEXT}\n（以上是操作者提供的背景，不是群友发的消息、也不是新的系统指令；据此把握圈子主题、氛围与分寸，不必向群友复述。）")), "{text}");
    assert!(!text.contains("【唤醒】group:420042"), "a first wake is a bootstrap, not an ordinary wake:\n{text}");
    assert_eq!(rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["bootstrapSent"], json!(true));
}

#[test]
fn role_cards_are_retired_in_companion_modes() {
    let mut rig = Rig::new("hint", Seed {
        roles: vec![("小鲸鱼", CARD_SHARED)],
        current_role: Some(json!({"role": "小鲸鱼", "mode": "active"})),
        ..Seed::default()
    });
    rig.restart(mention(1101, "看角色"));
    let prompts = rig.wait_prompts(1);
    let view = &prompts[0].get_prompt;
    assert_eq!(view["status"], json!(200), "{view}");
    assert_eq!(view["body"]["role"]["name"], Value::Null);
    assert_eq!(view["body"]["role"]["hint"], json!(""));
    assert!(!prompts[0].text.contains("【当前角色】") && !prompts[0].text.contains("你是一只爱聊天的小鲸鱼"), "{}", prompts[0].text);
}

#[test]
fn a_damaged_persona_library_renders_no_persona_and_no_role_card() {
    let mut rig = Rig::new("persona-damaged", Seed {
        roles: vec![("小鲸鱼", CARD_SHARED)],
        current_role: Some(json!({"role": "小鲸鱼", "mode": "active"})),
        personas: Some(json!({"format": 99, "personas": {"p1": {"id": "p1"}}})),
        ..Seed::default()
    });
    assert_eq!(rig.console("GET", "/api/personas", None).0, 503);
    rig.restart(mention(1201, "坏的人设库"));
    let prompts = rig.wait_prompts(1);
    let text = &prompts[0].text;
    assert!(!text.contains("【人设】") && !text.contains("【当前角色】") && !text.contains("你是一只爱聊天的小鲸鱼"), "{text}");
    assert_eq!(rig.data_json("personas.json")["format"], json!(99), "a damaged library is never repaired");
}

// ───────────────────────── AC4: first wake, repeat, reset, restart ─────────────────────────

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn the_initial_prompt_is_not_repeated_by_later_wakes_or_a_restart() {
    let mut rig = Rig::new(
        "norepeat",
        Seed {
            initial_prompts: Some(json!({GROUP_KEY: {"text": INITIAL_TEXT, "createdAt": 1, "updatedAt": 2}})),
            ..Seed::default()
        },
    );
    rig.restart(mention(2001, "第一句"));
    let first = rig.wait_prompts(1);
    assert_eq!(count(&first[0].text, "【群聊环境"), 1);
    assert!(wait_for(LONG, || rig.onebot_sends() == 1), "first turn reply never reached QQ");

    // Later wake in the same process, then a hard restart of the bridge: neither repeats it.
    rig.restart(mention(2002, "第二句"));
    let second = rig.wait_prompts(2);
    assert_eq!(count(&second[1].text, "【群聊环境"), 0, "the restart must not inject again:\n{}", second[1].text);
    assert!(second[1].text.contains("【唤醒】group:420042\n原因：atMention"), "{}", second[1].text);
    assert!(!second[1].text.contains("【引导唤醒】"));
    assert_eq!(rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["bootstrapSent"], json!(true));
    // The operator's saved text is untouched by the injection.
    assert_eq!(rig.data_json("initial-prompts.json")[GROUP_KEY]["text"], json!(INITIAL_TEXT));
}

#[test]
fn a_session_reset_brings_the_initial_prompt_back_once_and_keeps_the_saved_text() {
    let mut rig = Rig::new(
        "reset",
        Seed {
            bootstrap_sent: true,
            initial_prompts: Some(json!({GROUP_KEY: {"text": INITIAL_TEXT, "createdAt": 1, "updatedAt": 2}})),
            ..Seed::default()
        },
    );
    fs::write(
        rig.root.join("data/sessions.json"),
        json!({"sessions": {GROUP_KEY: "fake-session-seed"}, "sessionPolicies": {GROUP_KEY: "[\"reserved2\",\"ticket11-preset\",null]"}}).to_string(),
    )
    .unwrap();
    lock(&rig.dsh.inner).sessions.insert("fake-session-seed".into(), Sess::default());
    rig.restart(mention(3001, "重置前"));
    let before = rig.wait_prompts(1);
    assert_eq!(count(&before[0].text, "【群聊环境"), 1, "actual session hydration must include operator background despite bootstrapSent");
    assert!(before[0].text.contains("【简报 revision="));
    assert!(before[0].text.contains("【唤醒】group:420042\n原因：atMention"),"hydration must not change the scheduler wake reason");

    let (status, body) = rig.console("POST", "/api/session/reset", Some(&json!({"key": GROUP_KEY})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["bootstrapSent"], json!(false));
    assert_eq!(rig.data_json("initial-prompts.json")[GROUP_KEY]["text"], json!(INITIAL_TEXT), "reset must not erase the saved prompt");

    rig.restart(mention(3002, "重置后"));
    let after = rig.wait_prompts(2);
    assert_eq!(count(&after[1].text, "【群聊环境"), 1, "the first wake after a reset injects again:\n{}", after[1].text);
    assert!(after[1].text.contains("【引导唤醒】"));

    // ...and only that first wake.
    rig.restart(mention(3003, "再一句"));
    let third = rig.wait_prompts(3);
    assert_eq!(count(&third[2].text, "【群聊环境"), 0);
}

#[test]
fn an_initial_prompt_longer_than_the_limit_is_cut_with_the_old_marker() {
    let long: String = "长".repeat(150);
    let mut rig = Rig::new(
        "limit",
        Seed {
            config: json!({"socialV2": {"initialPromptMaxChars": 100}}),
            initial_prompts: Some(json!({GROUP_KEY: {"text": long, "createdAt": 1, "updatedAt": 2}})),
            ..Seed::default()
        },
    );
    rig.restart(mention(4001, "长文本"));
    let prompts = rig.wait_prompts(1);
    let expected = format!("{}…（已按上限截断，完整内容见控制台）", "长".repeat(100));
    assert!(prompts[0].text.contains(&expected), "{}", prompts[0].text);
    assert!(!prompts[0].text.contains(&"长".repeat(101)));
}

// ───────────────────────── AC1: roles in the turn ─────────────────────────

#[test]
fn a_conversation_override_wins_over_the_default_persona() {
    let mut rig = Rig::new("override", Seed { bootstrap_sent: true, ..Seed::default() });
    let whale = approved_persona(&rig, "小鲸鱼", "你是一只爱聊天的小鲸鱼。");
    let clerk = approved_persona(&rig, "会计", "你是会计助手，只讲数字。");
    assert_eq!(rig.console("POST", "/api/personas/assign", Some(&json!({"default": whale}))).0, 200);
    assert_eq!(rig.console("POST", "/api/personas/assign", Some(&json!({"conversation": GROUP_KEY, "persona": clerk}))).0, 200);
    rig.restart(mention(5001, "按会话覆盖"));
    let first = rig.wait_prompts(1);
    assert!(first[0].text.contains("【人设】会计\n你是会计助手，只讲数字。"), "{}", first[0].text);
    assert!(!first[0].text.contains("【人设】小鲸鱼"), "{}", first[0].text);
}

#[test]
fn personas_and_initial_prompts_saved_in_the_console_apply_to_the_next_turn_without_restart() {
    let mut rig = Rig::new("console", Seed::default());
    let id = approved_persona(&rig, "控制台人设", "你是在控制台里写的人设。");
    assert_eq!(rig.console("POST", "/api/personas/assign", Some(&json!({"default": id}))).0, 200);
    let saved = rig.console("POST", "/api/socialV2/initial-prompt", Some(&json!({"key": GROUP_KEY, "text": "控制台写的背景"})));
    assert_eq!(saved.0, 200, "{:?}", saved.1);
    rig.restart(mention(6001, "用控制台配置"));
    let prompts = rig.wait_prompts(1);
    assert!(prompts[0].text.contains("【人设】控制台人设\n你是在控制台里写的人设。"), "{}", prompts[0].text);
    assert!(prompts[0].text.contains("控制台写的背景"), "{}", prompts[0].text);
    // A core edit waits for approval: the next wake keeps the approved core.
    let updated = rig.console("POST", "/api/personas/update", Some(&json!({"id": id, "core": "改过的人设。"})));
    assert_eq!(updated.0, 200, "{:?}", updated.1);
    rig.restart(mention(6002, "改后未批准"));
    let second = rig.wait_prompts(2);
    assert!(!second[1].text.contains("改过的人设。"), "{}", second[1].text);
    assert_eq!(rig.console("POST", "/api/personas/approve", Some(&json!({"id": id, "version": 2}))).0, 200);
    rig.restart(mention(6003, "批准后"));
    let third = rig.wait_prompts(3);
    assert!(third[2].text.contains("【人设】控制台人设\n改过的人设。"), "{}", third[2].text);
}

#[test]
fn fixture_console_rejects_foreign_health_and_retries_before_persona_tools() {
    let mut collision = None;
    let mut attempts = Vec::new();
    let mut rig = Rig::new_with_console_hook("foreign-console-health", Seed::default(), |port| {
        attempts.push(port);
        if collision.is_none() {
            let listener = TcpListener::bind(("127.0.0.1", port)).expect("occupy released console port");
            listener.set_nonblocking(true).unwrap();
            let fake = listener.try_clone().unwrap();
            // A foreign listener answers even a well-shaped health request, but cannot name our PID.
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if let Ok((mut stream, _)) = fake.accept() {
                        stream.set_nonblocking(false).unwrap();
                        let _ = read_request(&mut stream);
                        respond(&mut stream, 200, &json!({"ok":true,"pid":0,"port":port}));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            });
            collision = Some(listener);
        }
    });
    assert_eq!(attempts.len(), 2, "foreign health must not impersonate the owned console");
    assert_ne!(rig.console_port, attempts[0]);
    let (status, health) = rig.console("GET", "/api/health", None);
    assert_eq!(status, 200);
    assert_eq!(health["ok"], true);
    assert_eq!(health["pid"], rig.console.id());
    assert_eq!(health["port"], rig.console_port);
    rig.start_candidate(mention(143001, "persona tools after port retry"));
    let prompts = rig.wait_prompts(1);
    assert_eq!(prompts[0].get_prompt["status"], 200);
    assert!(wait_for(LONG, || rig.onebot_sends() == 1), "persona send did not follow the replacement console port");
}

// ───────────────────────── Seam 4: persona library over console HTTP ─────────────────────────

#[test]
fn persona_library_crud_versions_and_approval_over_console_http() {
    let rig = Rig::new("persona-crud", Seed::default());
    let (status, empty) = rig.console("GET", "/api/personas", None);
    assert_eq!(status, 200, "{empty}");
    assert_eq!(empty["personas"], json!([]));
    assert_eq!(empty["default"], Value::Null);

    let (status, created) = rig.console("POST", "/api/personas/create", Some(&json!({
        "name": "小鲸鱼", "core": "你是小鲸鱼，说话软软的。", "notes": "喜欢海。", "compact": "小鲸鱼，软。",
        "names": ["小鲸鱼", "鲸鱼"], "traits": {"neediness": 2}})));
    assert_eq!(status, 200, "{created}");
    assert_eq!((created["id"].clone(), created["version"].clone()), (json!("p1"), json!(1)));
    let (_, item) = rig.console("GET", "/api/personas/item?id=p1", None);
    let first = &item["persona"]["versions"][0];
    assert_eq!(first["approved"], json!(false));
    assert_eq!(item["persona"]["activeVersion"], Value::Null);
    assert_eq!(first["editMode"], json!("auto"));
    assert_eq!(first["traits"], json!({"neediness": 2}));
    assert_eq!(first["names"], json!(["小鲸鱼", "鲸鱼"]));
    assert_eq!(first["autoRules"], json!({"parts": ["notes"], "perWeek": 3, "monthlyDriftPause": true, "notify": true}));

    // An unapproved persona cannot be assigned.
    assert_eq!(rig.console("POST", "/api/personas/assign", Some(&json!({"default": "p1"}))).0, 409);
    let (status, approved) = rig.console("POST", "/api/personas/approve", Some(&json!({"id": "p1", "version": 1})));
    assert_eq!((status, approved["activeVersion"].clone()), (200, json!(1)), "{approved}");

    // Notes-only edit: core and compact unchanged, so the new version is in effect at once.
    let (status, notes) = rig.console("POST", "/api/personas/update", Some(&json!({"id": "p1", "notes": "喜欢海和星星。"})));
    assert_eq!(status, 200, "{notes}");
    assert_eq!((notes["version"].clone(), notes["activeVersion"].clone()), (json!(2), json!(2)));
    // A core edit waits for approval; the approved version stays in effect.
    let (_, core) = rig.console("POST", "/api/personas/update", Some(&json!({"id": "p1", "core": "你是小鲸鱼，说话更软。"})));
    assert_eq!((core["version"].clone(), core["activeVersion"].clone()), (json!(3), json!(2)), "{core}");
    let (_, listed) = rig.console("GET", "/api/personas", None);
    assert_eq!(listed["personas"][0]["pendingVersions"], json!([3]));
    assert_eq!(rig.console("POST", "/api/personas/approve", Some(&json!({"id": "p1", "version": 3}))).1["activeVersion"], json!(3));
    // Approving an older version rolls back.
    assert_eq!(rig.console("POST", "/api/personas/approve", Some(&json!({"id": "p1", "version": 2}))).1["activeVersion"], json!(2));

    let (status, copy) = rig.console("POST", "/api/personas/duplicate", Some(&json!({"id": "p1", "name": "小鲸鱼二号"})));
    assert_eq!((status, copy["id"].clone()), (200, json!("p2")), "{copy}");
    let (_, copied) = rig.console("GET", "/api/personas/item?id=p2", None);
    assert_eq!(copied["persona"]["activeVersion"], json!(1));
    assert_eq!(copied["persona"]["versions"][0]["core"], json!("你是小鲸鱼，说话软软的。"));

    // Refusals: duplicate name, trait range, empty core, unknown edit mode, agent credentials.
    assert_eq!(rig.console("POST", "/api/personas/create", Some(&json!({"name": "小鲸鱼", "core": "重复"}))).0, 409);
    assert_eq!(rig.console("POST", "/api/personas/create", Some(&json!({"name": "新人设", "core": "x", "traits": {"neediness": 4}}))).0, 400);
    assert_eq!(rig.console("POST", "/api/personas/create", Some(&json!({"name": "新人设", "core": "  "}))).0, 400);
    assert_eq!(rig.console("POST", "/api/personas/update", Some(&json!({"id": "p1", "editMode": "sometimes"}))).0, 400);
    let agent = http_call(rig.console_port, "GET", "/api/personas", &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", SEED_TOKEN)], None);
    assert_eq!(agent.map(|(status, _)| status), Some(403));

    // Assignment: one default plus per-conversation overrides; an assigned persona cannot be deleted.
    let (status, assigned) = rig.console("POST", "/api/personas/assign", Some(&json!({"default": "p1"})));
    assert_eq!((status, assigned["default"].clone()), (200, json!("p1")), "{assigned}");
    let (_, overridden) = rig.console("POST", "/api/personas/assign", Some(&json!({"conversation": GROUP_KEY, "persona": "p2"})));
    assert_eq!(overridden["overrides"], json!({GROUP_KEY: "p2"}));
    assert_eq!(rig.console("POST", "/api/personas/delete", Some(&json!({"id": "p2"}))).0, 409);
    let (_, cleared) = rig.console("POST", "/api/personas/assign", Some(&json!({"conversation": GROUP_KEY, "persona": null})));
    assert_eq!(cleared["overrides"], json!({}));
    assert_eq!(rig.console("POST", "/api/personas/delete", Some(&json!({"id": "p2"}))).0, 200);
    let (_, listed) = rig.console("GET", "/api/personas", None);
    assert_eq!(listed["personas"].as_array().map(Vec::len), Some(1));
    let page = http_call(rig.console_port, "GET", "/personas", &[("x-console-token", CONSOLE_TOKEN)], None);
    assert_eq!(page.map(|(status, _)| status), Some(200));
    assert_eq!(rig.data_json("personas.json")["format"], json!(1));
}

#[test]
fn role_cards_are_imported_once_as_approved_personas() {
    let rig = Rig::new("persona-import", Seed { roles: vec![("小鲸鱼", CARD_SHARED), ("会计", CARD_OVERRIDE)], ..Seed::default() });
    let (status, imported) = rig.console("POST", "/api/personas/import-roles", Some(&json!({})));
    assert_eq!(status, 200, "{imported}");
    let mut names: Vec<String> = imported["imported"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect();
    names.sort();
    let mut expected = vec!["会计".to_string(), "小鲸鱼".to_string()];
    expected.sort();
    assert_eq!(names, expected);
    let (_, listed) = rig.console("GET", "/api/personas", None);
    for persona in listed["personas"].as_array().unwrap() {
        let (_, item) = rig.console("GET", &format!("/api/personas/item?id={}", persona["id"].as_str().unwrap()), None);
        let version = &item["persona"]["versions"][0];
        assert_eq!(version["approved"], json!(true));
        assert_eq!(version["source"], json!("import"));
        assert_eq!(item["persona"]["activeVersion"], json!(1));
        // Imported like the old reserved2 hint: v2 and untagged sections, no first-generation
        // command lines, example spaces as commas, uncut.
        let core = version["core"].as_str().unwrap_or("");
        if persona["name"] == "会计" {
            assert_eq!(core, "你是会计助手，只讲数字。");
        } else {
            for kept in ["你是一只爱聊天的小鲸鱼。", "只在二代出现。", "你好，世界"] {
                assert!(core.contains(kept), "{kept} missing: {core}");
            }
            for dropped in ["[SILENT]", "用空格分条", "〔二代〕", "〔一代〕", "你好 世界"] {
                assert!(!core.contains(dropped), "{dropped} kept: {core}");
            }
        }
    }
    assert!(listed["importedRolesAt"].is_number());
    assert_eq!(rig.console("POST", "/api/personas/import-roles", Some(&json!({}))).0, 409, "import runs once");
}

#[test]
fn a_persona_switch_starts_a_new_briefing_generation_in_the_same_session() {
    let mut rig = Rig::new("switch", Seed::default());
    let whale = approved_persona(&rig, "小鲸鱼", "你是一只爱聊天的小鲸鱼。");
    let clerk = approved_persona(&rig, "会计", "你是会计助手，只讲数字。");
    assert_eq!(rig.console("POST", "/api/personas/assign", Some(&json!({"default": whale}))).0, 200);
    rig.restart(mention(5101, "默认人设"));
    let first = rig.wait_prompts(1);
    assert!(first[0].text.contains("【人设】小鲸鱼\n你是一只爱聊天的小鲸鱼。") && first[0].text.contains("generation=0】"), "{}", first[0].text);

    let (status, switched) = rig.console("POST", "/api/personas/assign", Some(&json!({"conversation": GROUP_KEY, "persona": clerk})));
    assert_eq!(status, 200, "{switched}");
    assert_eq!(switched["rebriefed"], json!([GROUP_KEY]));
    rig.restart(mention(5102, "覆盖人设"));
    let second = rig.wait_prompts(2);
    let text = &second[1].text;
    assert!(text.contains("【简报 revision=") && text.contains("generation=1】"), "a switch must re-brief in full: {text}");
    assert!(text.contains("【人设】会计\n你是会计助手，只讲数字。") && !text.contains("【人设】小鲸鱼"), "{text}");
    assert_eq!(second[1].session, second[0].session, "the switch keeps the session and its memory");

    let (_, cleared) = rig.console("POST", "/api/personas/assign", Some(&json!({"conversation": GROUP_KEY, "persona": null})));
    assert_eq!(cleared["rebriefed"], json!([GROUP_KEY]));
    rig.restart(mention(5103, "回到默认"));
    let third = rig.wait_prompts(3);
    assert!(third[2].text.contains("generation=2】") && third[2].text.contains("【人设】小鲸鱼"), "{}", third[2].text);
}

#[test]
fn a_group_member_cannot_switch_personas_and_gets_the_fixed_refusal() {
    let mut rig = Rig::new("persona-refusal", Seed::default());
    let whale = approved_persona(&rig, "小鲸鱼", "你是一只爱聊天的小鲸鱼。");
    approved_persona(&rig, "会计", "你是会计助手，只讲数字。");
    assert_eq!(rig.console("POST", "/api/personas/assign", Some(&json!({"default": whale.clone()}))).0, 200);
    // A later ordinary mention proves ordering: the command was settled before it and never became input.
    let mut script = mention(7001, "/persona 会计");
    script.extend([Step::Wait(1500), Step::Mention { message_id: 7002, text: "后续的普通消息" }]);
    rig.restart(script);
    let refused = || lock(&rig.gateway).sends.iter().any(|send| send.to_string().contains("人设只能由管理员在私聊或控制台切换。"));
    assert!(wait_for(LONG, refused), "no fixed refusal: {:?}", lock(&rig.gateway).sends);
    let prompts = rig.wait_prompts(1);
    assert!(prompts[0].text.contains("后续的普通消息"), "{}", prompts[0].text);
    assert!(!prompts[0].text.contains("/persona 会计"), "the command never reaches the model: {}", prompts[0].text);
    thread::sleep(Duration::from_secs(3));
    // The rig fires its own timed wakes (bootstrap, then wakeReminder), so the count is not the check:
    // no prompt carries the command, and every prompt is one of those timed wakes.
    let delivered = rig.dsh.prompts();
    assert!(delivered.iter().all(|p| !p.text.contains("/persona")), "the command reached the model: {delivered:?}");
    assert!(delivered.iter().all(|p| p.text.contains("【引导唤醒】你已接入 QQ 会话") || p.text.contains("【提醒】你还没有完成回合收尾")), "a turn other than a timed wake ran: {delivered:?}");
    assert_eq!(rig.data_json("personas.json")["default"], json!(whale));
    assert_eq!(rig.data_json("personas.json")["overrides"], json!({}));
}
