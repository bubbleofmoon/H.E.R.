// Shared fake DSH, QQ gateway and selected-runtime process fixture.
#[path = "../../src/install_root.rs"]
mod install_root;
#[path = "console.rs"]
mod console_fixture;

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::{accept, Message};

const GROUP_ID: i64 = 420042;
const GROUP_KEY: &str = "group:420042";
const SELF_ID: i64 = 9001;
const OWNER: i64 = 8808;
const OWNER_KEY: &str = "private:8808";
const STRANGER: i64 = 7707;
const ONEBOT_TOKEN: &str = "ticket12-onebot-token-0001";
const CONSOLE_TOKEN: &str = "ticket12-console-token-0001";
const SECRET: &str = "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n";
/// The conversations the candidate is told to serve (an isolated live runtime needs the explicit list).
const LIVE_GROUPS: &str = "group:420042,group:420043,group:999,private:8808,private:7707,private:5150";
const ACK: &str = "🤔 收到，正在思考…";
const LONG: Duration = Duration::from_secs(40);

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
    headers: String,
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
            return Some(HttpReq { request_line: head.lines().next()?.to_string(), headers: head, body: bytes[split + 4..split + 4 + length].to_vec() });
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

// ───────────────────────── fake DSH with a marker-driven model ─────────────────────────

#[derive(Default)]
struct Sess {
    events: Vec<Value>,
    seq: i64,
    turn_no: i64,
    provider: String,
    cancelled: bool,
}

#[derive(Clone, Debug)]
struct PromptRec {
    session: String,
    text: String,
}

struct DshInner {
    sessions: HashMap<String, Sess>,
    next_sid: u32,
    rpc: Vec<(String, Value)>,
    prompts: Vec<PromptRec>,
    refuse_prompt: bool,
    preview_fault: Option<(String, String)>,
    interactions: Vec<Value>,
    event_clients: usize,
    drop_interaction_stream: bool,
    stall_handshakes: bool,
    stalled_handshakes: usize,
    fail_settings: bool,
    presets: Value,
    settings: Value,
    rejected_results: usize,
    accepted_results: Vec<Value>,
    drop_result_response: usize,
    hold_result_response: bool,
    hold_settings_update: bool,
}

struct Dsh {
    loop_fixture: Mutex<Option<(PathBuf, u16, Arc<Mutex<Gateway>>)>>,
    loop_results: Mutex<Vec<Value>>,
    requests: Mutex<Vec<Value>>,
    inner: Mutex<DshInner>,
    trace: Arc<Mutex<Vec<String>>>,
}

fn event(sess: &mut Sess, kind: &str, data: Value) {
    sess.seq += 1;
    sess.events.push(json!({"seq": sess.seq, "type": kind, "time": now_ms(), "data": data}));
}

impl Dsh {
    fn new() -> Arc<Self> {
        Arc::new(Self { loop_fixture: Mutex::new(None), loop_results: Mutex::new(Vec::new()), requests: Mutex::default(), inner: Mutex::new(DshInner { sessions: HashMap::new(), next_sid: 0, rpc: Vec::new(), prompts: Vec::new(), refuse_prompt: false, preview_fault: None, interactions: Vec::new(), event_clients: 0, drop_interaction_stream: false, stall_handshakes: false, stalled_handshakes: 0, fail_settings: false,
            presets: json!([{"id":"ticket12-chat"},{"id":"ticket12-v2-preset"},{"id":"ticket13-full","isDefault":true}]), settings: json!({"namespaces":[]}), rejected_results: 0, accepted_results: Vec::new(), drop_result_response: 0, hold_result_response: false, hold_settings_update: false }), trace: Arc::default() })
    }

    fn prompts(&self) -> Vec<PromptRec> {
        lock(&self.inner).prompts.clone()
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        lock(&self.inner).rpc.iter().filter(|(m, _)| m == method).map(|(_, a)| a.clone()).collect()
    }

    fn rpc(self: &Arc<Self>, method: &str, args: &Value) -> Result<Value, (String, String)> {
        lock(&self.trace).push(method.to_string());
        let mut inner = lock(&self.inner);
        inner.rpc.push((method.to_string(), args.clone()));
        let sid = args.pointer("/request/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        match method {
            "settings/update" => Ok(json!({})),
            "workspace/create" => Ok(json!({"workspace": {"workspaceId": "ws-ticket12"}, "created": false})),
            "agentPresets/list" => Ok(json!({"presets": inner.presets})),
            "settings/describe" if inner.fail_settings => Err(("fixture-failure".into(), "synthetic settings failure".into())),
            "settings/describe" => Ok(inner.settings.clone()),
            "$events/result" => {
                if inner.rejected_results > 0 {
                    inner.rejected_results -= 1;
                    Err(("fixture-rejected".into(), "synthetic rejection".into()))
                } else {
                    inner.accepted_results.push(args.clone());
                    Ok(json!({}))
                }
            }
            "workspace/rename" | "session/rename" | "workspace/archiveSession" => Ok(json!({})),
            "session/selectModel" => {
                let provider = args.pointer("/request/provider").and_then(Value::as_str).unwrap_or("").to_string();
                if provider == "preview-refuse" {
                    if let Some((_, message)) = &inner.preview_fault { return Err(("fixture".into(), message.clone())); }
                }
                if let Some(s) = inner.sessions.get_mut(&sid) {
                    s.provider = provider;
                }
                Ok(json!({}))
            }
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
            "session/cancel" => {
                if let Some(s) = inner.sessions.get_mut(&sid) {
                    s.cancelled = true;
                }
                Ok(json!({"accepted": true}))
            }
            "session/updateQueue" => Ok(json!({"accepted": true})),
            _ => Err(("unexpected".into(), format!("unexpected fake DSH method {method}"))),
        }
    }

    fn prompt(self: &Arc<Self>, args: &Value, rpc_id: &str) -> Result<Value, (String, String)> {
        let sid = args.pointer("/request/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        let text = args.pointer("/request/content/0/text").and_then(Value::as_str).unwrap_or("").to_string();
        {
            let mut inner = lock(&self.inner);
            inner.rpc.push(("session/prompt".into(), args.clone()));
            if let Some((method, message)) = &inner.preview_fault {
                if method == "prompt" { return Err(("INVALID_REQUEST".into(), message.clone())); }
            }
            if inner.refuse_prompt {
                return Ok(json!({"accepted":false}));
            }
            if let Some((root, _, _)) = lock(&self.loop_fixture).clone().filter(|(r,_,_)| r.file_name().unwrap().to_string_lossy().contains("hf129-fix1-refused")) {
                if inner.rpc.iter().filter(|(m,_)| m == "session/prompt").count() == 2 {
                    fs::write(root.join("refused-prompt"), &text).unwrap();
                    return Ok(json!({"accepted":false}));
                }
            }
            // This submission is accepted into DSH's queue while a different prompt runs.
            // Its own user/message is deliberately absent: acceptance is not model delivery.
            if let Some((root, _, _)) = lock(&self.loop_fixture).clone().filter(|(r,_,_)| r.file_name().unwrap().to_string_lossy().contains("hf129-fix2-queued")) {
                if inner.rpc.iter().filter(|(m,_)| m == "session/prompt").count() == 2 {
                    let sess = inner.sessions.get_mut(&sid).unwrap();
                    event(sess, "user/message", json!({"source":{"rpcId":"unrelated-rpc"},"content":[]}));
                    sess.turn_no += 1;
                    let turn = sess.turn_no;
                    event(sess, "turn/start", json!({"turn":turn}));
                    event(sess, "turn/end", json!({"turn":turn,"reason":{"kind":"completed"}}));
                    inner.prompts.push(PromptRec { session:sid.clone(), text:text.clone() });
                    fs::write(root.join("queued-prompt"), &text).unwrap();
                    return Ok(json!({"accepted":true}));
                }
            }
            let Some(sess) = inner.sessions.get_mut(&sid) else {
                return Err(("session-not-found".into(), "session not found".into()));
            };
            if text.contains("/model") {
                // A slash command DSH executes itself: the result comes back, no model turn runs.
                inner.prompts.push(PromptRec { session: sid, text });
                return Ok(json!({"accepted": true, "command": {"text": "当前模型：**fake-model**"}}));
            }
            sess.cancelled = false;
            event(
                sess,
                "user/message",
                json!({"role": "user", "id": format!("msg-{rpc_id}"), "content": [{"type": "text", "text": text}],
                       "source": {"kind": "user", "rpcId": rpc_id}}),
            );
            inner.prompts.push(PromptRec { session: sid.clone(), text: text.clone() });
        }
        let world = self.clone();
        thread::spawn(move || world.run_turn(&sid, &text));
        Ok(json!({"accepted": true}))
    }

    fn run_turn(self: &Arc<Self>, sid: &str, prompt: &str) {
        let (turn, provider) = {
            let mut inner = lock(&self.inner);
            let sess = inner.sessions.get_mut(sid).expect("session");
            sess.turn_no += 1;
            let turn = sess.turn_no;
            event(sess, "turn/start", json!({"turn": turn}));
            (turn, sess.provider.clone())
        };
        let finish = |kind: Value| {
            let mut inner = lock(&self.inner);
            if let Some(sess) = inner.sessions.get_mut(sid) {
                event(sess, "turn/end", json!({"turn": turn, "reason": kind}));
            }
        };
        if prompt.contains("【提醒】") {
            if let Some((root,port,_))=lock(&self.loop_fixture).clone().filter(|(root,_,_)|root.file_name().unwrap().to_string_lossy().contains("hf129-reminder")) {
                let state:Value=serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap();
                let token=state["conversations"][GROUP_KEY]["agentToken"].as_str().unwrap();
                let (status,packet)=http_call(port,"POST","/api/socialV2/wait",&[("x-console-token",CONSOLE_TOKEN),("x-agent-token",token)],
                    Some(&json!({"key":GROUP_KEY,"purpose":"messages","timeoutMs":1}))).unwrap();
                assert_eq!(status,200,"{packet}");
                fs::write(root.join("hf129-reminder-read"),packet.to_string()).unwrap();
                assert!(wait_for(LONG, || root.join("hf129-reminder-release").exists()));
                return finish(json!({"kind":"completed"}));
            }
        }
        if prompt.contains("#HOLD129#") {
            let (root, _, _) = lock(&self.loop_fixture).clone().expect("wake fixture");
            if turn == 1 && root.file_name().unwrap().to_string_lossy().contains("hf129-fix1-stream") {
                assert!(wait_for(LONG, || lock(&self.inner).sessions.get(sid).is_some_and(|s|s.cancelled)), "collector did not cancel the failed stream");
                return finish(json!({"kind":"aborted"}));
            }
            assert!(wait_for(LONG, || root.join("hf129-release").exists()), "wake fixture never released");
            if turn == 1 && ["hf129-fix1-refused", "hf129-fix1-replacement", "hf129-fix2-queued"].iter().any(|label|root.file_name().unwrap().to_string_lossy().contains(label)) {
                return finish(json!({"kind":"error","error":{"code":"boom","message":"synthetic model failure"}}));
            }
            if turn > 1 {
                // A replacement/follow-up model explicitly reads the backlog; snapshots alone
                // grant no urgent coverage.
                let (_, port, _) = lock(&self.loop_fixture).clone().unwrap();
                let state:Value=serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap();
                let token=state["conversations"][GROUP_KEY]["agentToken"].as_str().unwrap();
                let (status,packet)=http_call(port,"GET",&format!("/api/socialV2/unread?key={GROUP_KEY}"),
                    &[("x-console-token",CONSOLE_TOKEN),("x-agent-token",token)],None).unwrap();
                assert_eq!(status,200,"{packet}");
                fs::write(root.join("hf129-followup-read"),packet.to_string()).unwrap();
            }
            return finish(json!({"kind":"completed"}));
        }
        if prompt.contains("#LOOP123#") {
            let (root, port, gateway) = lock(&self.loop_fixture).clone().expect("loop fixture");
            let state: Value = serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap();
            let token = state["conversations"][GROUP_KEY]["agentToken"].as_str().unwrap();
            let call = |name: &str, path: &str, body: Value| {
                let index = lock(&self.loop_results).len();
                let id = format!("loop123-{index}");
                {
                    let mut inner = lock(&self.inner);
                    event(inner.sessions.get_mut(sid).unwrap(), "tool/call", json!({"turn":turn,"step":index,"callId":id,"name":name,"arguments":body.to_string()}));
                }
                let (status, result) = http_call(port, "POST", path,
                    &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", token)], Some(&body)).expect("fake model tool result");
                lock(&self.loop_results).push(json!({"tool":name,"status":status,"result":result}));
                {
                    let mut inner = lock(&self.inner);
                    event(inner.sessions.get_mut(sid).unwrap(), "tool/result", json!({"turn":turn,"step":index,"message":{"isError":status!=200,"source":{"callId":id},"content":[{"type":"text","text":result.to_string()}]}}));
                }
                (status, result)
            };
            // The incident starts after the observation slots have already been spent.
            call("qq_wait_for_messages", "/api/socialV2/wait", json!({"key":GROUP_KEY,"purpose":"reply","quietMs":0,"timeoutMs":100}));
            let before = state["conversations"][GROUP_KEY]["lastUnreadSeq"].as_i64().unwrap_or(0);
            lock(&gateway).queue.push_back(group_event(123002, 1001, "tester", "", vec![text_seg("still talking after observation")]));
            assert!(wait_for(LONG, || {
                let current: Value = fs::read(root.join("data/social-v2.json")).ok()
                    .and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or(Value::Null);
                current["conversations"][GROUP_KEY]["lastUnreadSeq"].as_i64().unwrap_or(0) > before
            }), "the new arrival must invalidate observation before the old model loop starts");
            for _ in 0..8 {
                call("qq_send_message", "/api/socialV2/send-message", json!({"key":GROUP_KEY,"messages":["[表情28]"]}));
                call("qq_wait_for_messages", "/api/socialV2/wait", json!({"key":GROUP_KEY,"purpose":"reply","quietMs":0,"timeoutMs":100}));
                let (status, _result) = call("qq_mark_read", "/api/socialV2/mark-read", json!({"key":GROUP_KEY,"throughSeq":0}));
                // This model used to choose the only accepted action, another send, on a refusal.
                if status == 200 {
                    return finish(json!({"kind":"completed"}));
                }
            }
            return finish(json!({"kind":"error","error":{"code":"LOOP123","message":"old tool loop exhausted its fixture bound"}}));
        }
        if prompt.contains("#QUESTION#") {
            if !wait_for(Duration::from_secs(10), || lock(&self.inner).event_clients > 0) { return; }
            lock(&self.inner).interactions.push(json!({"type":"waterfall", "event":"user-questions/request", "agentId":sid,
                "eventId":"question-13", "request":{"questions":[{"id":"q1","question":"Choose a color", "options":[{"label":"Blue"},{"label":"Red"}]}]}}));
            if !wait_for(Duration::from_secs(20), || lock(&self.inner).accepted_results.iter().any(|c| c["eventId"] == "question-13")) { return; }
            if lock(&self.inner).sessions.get(sid).is_some_and(|s| s.cancelled) { return; }
        }
        if prompt.contains("#APPROVAL#") {
            if !wait_for(Duration::from_secs(10), || lock(&self.inner).event_clients > 0) { return; }
            let event_id = format!("approval-{turn}");
            lock(&self.inner).interactions.push(json!({"type":"waterfall", "event":"approval/request", "agentId":sid,
                "eventId":event_id, "request":{"toolName":"file_write", "reason":"save report", "approvalId":"approval-id-13"}}));
            if !wait_for(Duration::from_secs(20), || lock(&self.inner).accepted_results.iter().any(|c| c["eventId"] == event_id)) { return; }
            if lock(&self.inner).sessions.get(sid).is_some_and(|s| s.cancelled) { return; }
        }
        if prompt.contains("#SLOW#") {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if lock(&self.inner).sessions.get(sid).map(|s| s.cancelled).unwrap_or(true) {
                    finish(json!({"kind": "aborted"}));
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        if prompt.contains("#ABORT#") {
            return finish(json!({"kind": "aborted"}));
        }
        let preview_fault = lock(&self.inner).preview_fault.clone();
        if let Some((method, message)) = preview_fault {
            if method != "prompt" { return finish(json!({"kind":"error","error":{"code":"fixture","message":message}})); }
        }
        if prompt.contains("#FAIL#") || provider == "bad-provider" {
            return finish(json!({"kind": "error", "error": {"code": "boom", "message": "model exploded at C:\\internal\\secret"}}));
        }
        if prompt.contains("#RATE81#") {
            return finish(json!({"kind":"error","error":{"code":"QUOTA_EXCEEDED","message":"HTTP 429 quota exceeded CHAT_SECRET_81"}}));
        }
        if prompt.contains("#REFUSE83#") {
            return finish(json!({"kind":"error","failure":{"code":"EMPTY_RESPONSE","message":"model returned a completed response with no content CHAT_SECRET_83"}}));
        }
        if prompt.contains("#EMPTY81#") {
            return finish(json!({"kind":"error","error":{"code":"EMPTY_ANSWER","message":"empty answer"}}));
        }
        let mut reply = "synthetic reply".to_string();
        if prompt.contains("#TOOLOK#") || prompt.contains("#TOOLERR#") {
            let bad = prompt.contains("#TOOLERR#");
            let mut inner = lock(&self.inner);
            let sess = inner.sessions.get_mut(sid).expect("session");
            event(sess, "tool/call", json!({"turn": turn, "step": 0, "callId": format!("c{turn}"), "name": "mcp__snowluma__qq_send_message", "arguments": "{}"}));
            event(
                sess,
                "tool/result",
                json!({"turn": turn, "step": 0, "message": {"isError": bad, "source": {"callId": format!("c{turn}")}, "content": [{"type": "text", "text": "x"}]}}),
            );
            reply = if bad { "tool failed reply".into() } else { "已发送".into() };
        } else if prompt.contains("#WRAPPEDLF128#") {
            reply = "prefix known-session- \n token-128 suffix".into();
        } else if prompt.contains("#WRAPPEDCRLF128#") {
            reply = "prefix known-session- \r\n token-128 suffix".into();
        } else if prompt.contains("#REPEAT128#") {
            reply = "known-session-token-128 plus known-session-token-128".into();
        } else if prompt.contains("#TOKEN128#") {
            reply = format!("{}known-session-token-128 trailing", "x".repeat(70));
        } else if prompt.contains("#SHAPE128#") {
            reply = format!("{}0123456789abcdef0123456789abcdef0123456789abcdef trailing", "x".repeat(70));
        } else if prompt.contains("#THOUGHT128#") {
            reply = "thought\nprivate plan that should not be in the summary\n\npublic final128".into();
        } else if prompt.contains("#MD#") {
            reply = "**加粗** 和 [链接](http://example.invalid/x)\n# 标题\n- 条目".into();
        } else if prompt.contains("#URLSPLIT15#") {
            reply = "甲，乙，https://example.invalid/long-path，尾".into();
        } else if prompt.contains("#SPLIT15#") {
            reply = "第一句 hello world 第二句".into();
        } else if prompt.contains("#SUMMARY124#") {
            reply = "前[表情:给你 糖]后".into();
        } else if prompt.contains("#FACEBOUNDARY124#") {
            reply = format!("{}[表情028]后", "a".repeat(3998));
        } else if prompt.contains("#FACERESERVED124#") {
            reply = "abcdef[表情028]后".into();
        } else if prompt.contains("#FACESMALL124#") {
            reply = "前[表情028]后".into();
        } else if prompt.contains("#FACENEWLINE124#") {
            reply = format!("{}\n[表情028]后", "a".repeat(3998));
        } else if prompt.contains("#LONG#") {
            reply = "a".repeat(4500);
        } else if prompt.contains("#SILENT#") {
            reply = "[SILENT]".into();
        } else if prompt.contains("#PATH#") {
            reply = "文件在 C:\\Users\\tester\\secret.txt 里".into();
        } else if prompt.contains("#GUARD88#") {
            reply = "文件在 C:\\Users\\tester\\secret.txt，密码：qwerty123".into();
        } else if prompt.contains("#EMPTY#") {
            reply = "**  **".into();
        }
        {
            let mut inner = lock(&self.inner);
            let sess = inner.sessions.get_mut(sid).expect("session");
            event(
                sess,
                "assistant/message",
                json!({"turn": turn, "step": 1, "provider": "ticket12-provider", "usage": {"inputTokens": 7, "outputTokens": 3},
                       "message": {"content": [{"type": "text", "text": reply}]}}),
            );
        }
        finish(json!({"kind": "completed"}));
    }
}

fn dsh_connection(world: Arc<Dsh>, mut stream: TcpStream) {
    let mut peek = [0u8; 64];
    let Ok(n) = stream.peek(&mut peek) else { return };
    if String::from_utf8_lossy(&peek[..n]).starts_with("GET /api/remote.mux") {
        let stall = {
            let mut inner = lock(&world.inner);
            if inner.stall_handshakes { inner.stalled_handshakes += 1; true } else { false }
        };
        if stall {
            while lock(&world.inner).stall_handshakes { thread::sleep(Duration::from_millis(20)); }
            return;
        }
        dsh_follow(world, stream);
        return;
    }
    let Some(request) = read_request(&mut stream) else { return };
    let envelope = serde_json::from_slice::<Value>(&request.body).unwrap_or(Value::Null);
    lock(&world.requests).push(json!({"request":request.request_line,"body":envelope}));
    let method = envelope.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let rpc_id = envelope.get("rpcId").and_then(Value::as_str).unwrap_or("").to_string();
    let args = envelope.pointer("/payload/args").cloned().unwrap_or(Value::Null);
    let outcome = if method == "session/prompt" { world.prompt(&args, &rpc_id) } else { world.rpc(&method, &args) };
    if method == "$events/result" && outcome.is_ok() {
        let mut inner = lock(&world.inner);
        if inner.drop_result_response > 0 { inner.drop_result_response -= 1; return; }
        drop(inner);
        while lock(&world.inner).hold_result_response { thread::sleep(Duration::from_millis(20)); }
    }
    if method == "settings/update" {
        while lock(&world.inner).hold_settings_update { thread::sleep(Duration::from_millis(20)); }
    }
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
    lock(&world.requests).push(json!({"request":"GET /api/remote.mux", "body":frame}));
    let stream_id = frame.get("streamId").and_then(Value::as_str).unwrap_or("").to_string();
    let sid = frame.pointer("/payload/args/request/address/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let is_interactions = frame["endpoint"] == "$events";
    let events = lock(&world.inner).sessions.get(&sid).map(|s| s.events.clone()).unwrap_or_default();
    let mut sent = events.last().and_then(|e| e["seq"].as_i64()).unwrap_or(0);
    let records: Vec<Value> = events.iter().map(|e| json!({"type": "event", "event": e})).collect();
    let snapshot = json!({
        "type": "item", "streamId": stream_id,
        "value": {"type": "snapshot", "records": records, "hasMore": false,
                  "projections": {"values": {"inbox": {"next-turn": [], "next-step": []}}}}
    });
    let initial = if is_interactions { json!({"type":"item", "streamId":stream_id, "value":{"type":"ready", "clientId":"ticket13-client"}}) } else { snapshot };
    if socket.send(Message::Text(initial.to_string())).is_err() {
        return;
    }
    let _ = socket.get_ref().set_read_timeout(Some(Duration::from_millis(50)));
    let mut interaction_stream = is_interactions.then(|| stream_id.clone());
    let mut interaction_sent = lock(&world.inner).interactions.len();
    if is_interactions { lock(&world.inner).event_clients += 1; }
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if !is_interactions {
            if let Some((root,_,_))=lock(&world.loop_fixture).clone() {
                if fs::remove_file(root.join("hf129-stream-break")).is_ok() {
                    let _=socket.close(None);
                    return;
                }
            }
        }
        if is_interactions && std::mem::take(&mut lock(&world.inner).drop_interaction_stream) {
            let _ = socket.close(None);
            return;
        }
        if let Some(id) = &interaction_stream {
            let fresh = lock(&world.inner).interactions[interaction_sent..].to_vec();
            interaction_sent += fresh.len();
            for value in fresh {
                if socket.send(Message::Text(json!({"type":"item", "streamId":id, "value":value}).to_string())).is_err() { return; }
            }
        }
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
            Ok(Message::Text(text)) => {
                if let Ok(open) = serde_json::from_str::<Value>(&text) {
                    if open["type"] == "open" && open["endpoint"] == "$events" {
                        let id = open["streamId"].as_str().unwrap().to_string();
                        if socket.send(Message::Text(json!({"type":"item", "streamId":id, "value":{"type":"ready", "clientId":"ticket13-client"}}).to_string())).is_err() { return; }
                        interaction_stream = Some(id);
                    }
                }
            }
            Ok(Message::Close(_)) => return,
            Err(tokio_tungstenite::tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
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
    trace: Arc<Mutex<Vec<String>>>,
    /// Every HTTP action the bridge called: (action, body).
    calls: Vec<(String, Value)>,
    next_message_id: i64,
    /// Events the WebSocket side still has to deliver.
    queue: VecDeque<Value>,
    /// Opt-in definite rejection of one burst item's text.
    reject_text: Option<String>,
    /// Optional witness at the fake QQ effect boundary (before accepting a send).
    before_send: Option<Arc<dyn Fn(&Value) + Send + Sync>>,
    /// Optional response at the synthetic gateway boundary, outside its state mutex.
    intercept: Option<Arc<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>>,
    /// Accept this text at the gateway, then lose the HTTP response. History stays unverified.
    lose_text: Option<String>,
    /// While set, `get_image` of `ticket24-hold.gif` waits (an image download that stalls).
    hold_images: bool,
}

fn text_seg(text: &str) -> Value {
    json!({"type": "text", "data": {"text": text}})
}

fn group_event(message_id: i64, user_id: i64, nickname: &str, card: &str, segments: Vec<Value>) -> Value {
    json!({
        "time": now_ms() / 1000, "self_id": SELF_ID, "post_type": "message", "message_type": "group",
        "sub_type": "normal", "message_id": message_id, "group_id": GROUP_ID, "user_id": user_id,
        "sender": {"user_id": user_id, "nickname": nickname, "card": card},
        "message": segments, "raw_message": "x", "font": 0
    })
}

fn private_event(message_id: i64, user_id: i64, nickname: &str, segments: Vec<Value>) -> Value {
    json!({
        "time": now_ms() / 1000, "self_id": SELF_ID, "post_type": "message", "message_type": "private",
        "sub_type": "friend", "message_id": message_id, "user_id": user_id,
        "sender": {"user_id": user_id, "nickname": nickname},
        "message": segments, "raw_message": "x", "font": 0
    })
}

fn serve_gateway_ws(listener: TcpListener, gateway: Arc<Mutex<Gateway>>) {
    loop {
        let Ok((stream, _)) = listener.accept() else { return };
        let Ok(mut socket) = accept(stream) else { continue };
        let _ = socket.get_ref().set_read_timeout(Some(Duration::from_millis(50)));
        let end = Instant::now() + Duration::from_secs(600);
        while Instant::now() < end {
            let next = lock(&gateway).queue.pop_front();
            if let Some(ev) = next {
                if socket.send(Message::Text(ev.to_string())).is_err() {
                    break;
                }
            }
            match socket.read() {
                Ok(Message::Close(_)) => break,
                Err(tokio_tungstenite::tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
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
            let action = request.request_line.split_whitespace().nth(1).unwrap_or("").trim_start_matches('/').to_string();
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let intercept = {
                let mut g = lock(&gateway);
                lock(&g.trace).push(action.clone());
                g.calls.push((action.clone(), body.clone()));
                g.intercept.clone()
            };
            if let Some(reply) = intercept.and_then(|f| f(&action, &body)) {
                respond(&mut stream, 200, &reply);
                return;
            }
            if action == "get_image" && body["file"] == "ticket24-hold.gif" {
                wait_for(Duration::from_secs(20), || !lock(&gateway).hold_images);
                use base64::Engine;
                let reply = json!({"status":"ok","retcode":0,"data":{"base64":base64::engine::general_purpose::STANDARD.encode(media_fixtures::gif(2,40,30))}});
                respond(&mut stream, 200, &reply);
                return;
            }
            let reply = {
                let mut g = lock(&gateway);
                match action.as_str() {
                    "send_group_msg" | "send_private_msg" | "send_msg" => {
                        if let Some(witness) = &g.before_send { witness(&body); }
                        let text: String = body["message"].as_array().into_iter().flatten()
                            .filter_map(|s| s["data"]["text"].as_str()).collect();
                        if g.lose_text.as_deref() == Some(text.as_str()) {
                            return; // Close after receipt without a response or history proof.
                        }
                        if g.reject_text.as_deref() == Some(text.as_str()) {
                            json!({"status":"failed","retcode":1400,"wording":"synthetic definite refusal"})
                        } else {
                            g.next_message_id += 1;
                            json!({"status": "ok", "retcode": 0, "data": {"message_id": 880000 + g.next_message_id}})
                        }
                    }
                    "get_image" if body["file"] == "ticket19.gif" => {
                        use base64::Engine;
                        json!({"status":"ok","retcode":0,"data":{"base64":base64::engine::general_purpose::STANDARD.encode(media_fixtures::gif(37,40,30))}})
                    }
                    "get_group_msg_history" => json!({"status": "ok", "retcode": 0, "data": {"messages": []}}),
                    "get_group_list" => json!({"status": "ok", "retcode": 0, "data": [{"group_id": GROUP_ID, "group_name": "测试群"}]}),
                    "get_group_member_info" => match body.get("user_id").and_then(Value::as_i64) {
                        Some(1002) => json!({"status": "ok", "retcode": 0, "data": {"user_id": 1002, "card": "Alice", "nickname": "a"}}),
                        Some(1001) => json!({"status": "ok", "retcode": 0, "data": {"user_id": 1001, "card": "", "nickname": "tester"}}),
                        _ => json!({"status": "failed", "retcode": 1404, "data": null}),
                    },
                    "get_group_member_list" => json!({"status": "ok", "retcode": 0, "data": [{"user_id": 1003, "card": "Bob", "nickname": "b"}]}),
                    "get_msg" => match body.get("message_id").and_then(Value::as_i64) {
                        // a message the bot itself wrote in the group
                        Some(5555) => json!({"status": "ok", "retcode": 0, "data": {"message_id": 5555, "group_id": GROUP_ID, "user_id": SELF_ID,
                            "sender": {"user_id": SELF_ID, "nickname": "bot", "card": ""}, "message": [text_seg("之前机器人说的话")]}}),
                        // a message from another group: must be refused
                        Some(5556) => json!({"status": "ok", "retcode": 0, "data": {"message_id": 5556, "group_id": 999, "user_id": 1003,
                            "sender": {"user_id": 1003, "nickname": "b"}, "message": [text_seg("别的群的话")]}}),
                        _ => json!({"status": "failed", "retcode": 1404, "data": null}),
                    },
                    _ => json!({"status": "ok", "retcode": 0, "data": {"card": "", "nickname": "bot", "user_id": SELF_ID}}),
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
        if self.root.file_name().unwrap().to_string_lossy().contains("reserved-") {
            if let Ok(base)=std::env::var("TICKET15_EVIDENCE") {
                let dst=crate::install_root::development_root(PathBuf::from(base).join(self.root.file_name().unwrap()));
                fs::create_dir_all(&dst).unwrap();
                for name in ["candidate-out.log","candidate-err.log","data/social-v1.json","data/sessions.json","data/turn-ledger.jsonl","data/send-outbox.jsonl","config/config.json"] {
                    if let Ok(bytes)=fs::read(self.root.join(name)){fs::write(dst.join(name.replace('/',"_")),bytes).unwrap();}
                }
                fs::write(dst.join("dsh-prompts.json"),json!(self.dsh.prompts().iter().map(|p|json!({"session":p.session,"text":p.text})).collect::<Vec<_>>()).to_string()).unwrap();
                fs::write(dst.join("gateway.json"),json!(lock(&self.gateway).calls).to_string()).unwrap();
            }
        }
        if self.root.file_name().unwrap().to_string_lossy().contains("hf129") || self.root.file_name().unwrap().to_string_lossy().contains("hf130") {
            if let Ok(base)=std::env::var("HER129_EVIDENCE") {
                let dst=PathBuf::from(base).join(self.root.file_name().unwrap());
                fs::create_dir_all(&dst).unwrap();
                for name in ["candidate-out.log","candidate-err.log","data/social-v2.json","data/logs/rbridge-live.jsonl","data/turn-ledger.jsonl","data/send-outbox.jsonl"] {
                    if let Ok(bytes)=fs::read(self.root.join(name)) { fs::write(dst.join(name.replace('/',"_")),bytes).unwrap(); }
                }
                fs::write(dst.join("prompt-count.json"),json!({"prompts":self.dsh.prompts().len()}).to_string()).unwrap();
            }
        }
        if self.root.file_name().unwrap().to_string_lossy().contains("closed-") {
            if let Ok(base) = std::env::var("TICKET13_EVIDENCE") {
                let dst = crate::install_root::development_root(PathBuf::from(base).join(self.root.file_name().unwrap()));
                fs::create_dir_all(&dst).unwrap();
                for name in ["candidate-out.log", "candidate-err.log", "data/mode.json", "data/sessions.json", "data/turn-ledger.jsonl", "data/send-outbox.jsonl", "data/pending-interactions.json"] {
                    if let Ok(bytes) = fs::read(self.root.join(name)) { fs::write(dst.join(name.replace('/', "_")), bytes).unwrap(); }
                }
                fs::write(dst.join("dsh-rpc.json"), json!(lock(&self.dsh.inner).rpc).to_string()).unwrap();
                fs::write(dst.join("gateway.json"), json!(lock(&self.gateway).calls).to_string()).unwrap();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Seed {
    mode: &'static str,
    /// Merged into the `config.json` top level (objects are merged one level deep).
    config: Value,
    /// `(name, content)` role cards under `resources/roles`.
    roles: Vec<(&'static str, &'static str)>,
    current_role: Option<Value>,
    /// Initial `sessions.json`.
    sessions: Option<Value>,
}

impl Default for Seed {
    fn default() -> Self {
        Seed { mode: "chat", config: json!({}), roles: Vec::new(), current_role: None, sessions: None }
    }
}

impl Rig {
    fn new(name: &str, seed: Seed) -> Self {
        for variable in ["TICKET13_EVIDENCE", "TICKET15_EVIDENCE"] {
            if let Some(path) = std::env::var_os(variable) {
                crate::install_root::development_root(PathBuf::from(path));
            }
        }
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-ticket12-{name}-{}-{}", std::process::id(), now_ms())));
        for dir in ["config", "data/logs", "resources/roles", "user/dsh", "ambient"] {
            fs::create_dir_all(root.join(dir)).expect("create runtime dir");
        }
        // Reserve through fake-listener setup; release immediately before the child starts.
        let reservation = console_fixture::reserve_port();
        let console_port = reservation.local_addr().expect("console addr").port();
        let dsh = Dsh::new();
        let gateway = Arc::new(Mutex::new(Gateway { trace: dsh.trace.clone(), ..Gateway::default() }));
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
            "consoleToken": CONSOLE_TOKEN, "ownerQQ": OWNER,
            "allow": {"groups": [GROUP_ID], "private": [OWNER, STRANGER]},
            "agentPreset": "ticket12-chat",
            "sendDelayMs": 0,
            "dsh": {"baseUrl": dsh_url, "provider": "ticket12-provider", "model": "ticket12-model", "reasoningEffort": "medium", "fallbacks": []},
            "snowluma": {"accessToken": ONEBOT_TOKEN, "httpUrl": ob_url, "wsUrl": ws_url},
            "socialV2": {
                "agentPreset": "ticket12-v2-preset", "turnTimeoutSecs": 60,
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
        fs::write(root.join("data/social-v2.json"), json!({"conversations": {}}).to_string()).unwrap();
        fs::write(root.join("data/mode.json"), json!({"mode": seed.mode}).to_string()).unwrap();
        fs::write(root.join("user/dsh/.credentials.yaml"), SECRET).unwrap();
        fs::write(root.join("dependencies.json"), json!({"dsh": dsh_url, "model": dsh_url, "onebot": ob_url, "onebot_ws": ws_url}).to_string()).unwrap();
        for (name, content) in &seed.roles {
            fs::write(root.join(format!("resources/roles/{name}.md")), content).unwrap();
        }
        if let Some(current) = &seed.current_role {
            fs::write(root.join("data/current-role.json"), current.to_string()).unwrap();
        }
        if let Some(sessions) = &seed.sessions {
            fs::write(root.join("data/sessions.json"), sessions.to_string()).unwrap();
        }
        drop(reservation);
        let console = spawn_console(&root, console_port);
        let mut rig = Self { root, console_port, dsh, gateway, console, candidate: None };
        console_fixture::wait_for_console(&mut rig.console, &mut rig.console_port, &rig.root, CONSOLE_TOKEN, Duration::from_secs(20), |port| spawn_console(&rig.root, port));
        rig
    }

    fn start(&mut self) {
        let log = |name: &str| fs::OpenOptions::new().create(true).append(true).open(self.root.join(name)).unwrap();
        let ambient = self.root.join("ambient");
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .arg("--runtime-root").arg(&self.root)
            .arg("--live").arg("--live-groups").arg(LIVE_GROUPS)
            .arg("--console-port").arg(self.console_port.to_string())
            .env_remove("HER_RUNTIME_ROOT")
            .env("TZ", "UTC")
            .env("HOME", &ambient).env("USERPROFILE", &ambient).env("DSH_HOME", &ambient)
            .env("APPDATA", ambient.join("Roaming")).env("LOCALAPPDATA", ambient.join("Local"))
            .stdout(Stdio::from(log("candidate-out.log")))
            .stderr(Stdio::from(log("candidate-err.log")));
        #[cfg(feature = "deadlock-regression")]
        if self.root.file_name().unwrap().to_string_lossy().contains("closed-delayed-receipt") {
            command.env("HER_DEADLOCK_PROBE", self.root.join("deadlock-probe"));
            command.env("TOKIO_WORKER_THREADS", "4");
        }
        seed_reserved_fixture(&mut command, &self.root);
        hide_window(&mut command);
        self.candidate = Some(command.spawn().expect("spawn candidate"));
        // Wait for the gateway connection to exist before events are pushed.
        assert!(wait_for(LONG, || fs::read_to_string(self.root.join("candidate-out.log")).unwrap_or_default().contains("connected")), "bridge never connected");
    }

    fn push(&self, ev: Value) {
        lock(&self.gateway).queue.push_back(ev);
    }

    fn console(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        http_call(self.console_port, method, path, &[("x-console-token", CONSOLE_TOKEN)], body).unwrap_or((0, Value::Null))
    }

    fn data_json(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.root.join("data").join(name)).unwrap_or_default()).unwrap_or(Value::Null)
    }

    fn data_lines(&self, name: &str) -> Vec<Value> {
        fs::read_to_string(self.root.join("data").join(name))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Texts of every message the bridge sent to QQ, as (target key, text).
    fn sends(&self) -> Vec<(String, String)> {
        lock(&self.gateway)
            .calls
            .iter()
            .filter(|(a, _)| a == "send_group_msg" || a == "send_private_msg")
            .map(|(a, b)| {
                let key = if a == "send_group_msg" {
                    format!("group:{}", b["group_id"])
                } else {
                    format!("private:{}", b["user_id"])
                };
                let text: String = b["message"]
                    .as_array()
                    .map(|segs| segs.iter().filter_map(|s| s["data"]["text"].as_str()).collect())
                    .unwrap_or_default();
                (key, text)
            })
            .collect()
    }

    fn texts_to(&self, key: &str) -> Vec<String> {
        self.sends().into_iter().filter(|(k, _)| k == key).map(|(_, t)| t).collect()
    }

    fn gateway_calls(&self, action: &str) -> usize {
        lock(&self.gateway).calls.iter().filter(|(a, _)| a == action).count()
    }

    fn wait_prompts(&self, count: usize) -> Vec<PromptRec> {
        assert!(wait_for(LONG, || self.dsh.prompts().len() >= count), "prompt {count} never reached DSH: {:?}", self.dsh.prompts());
        self.dsh.prompts()
    }

    fn wait_texts(&self, key: &str, count: usize) -> Vec<String> {
        assert!(wait_for(LONG, || self.texts_to(key).len() >= count), "expected {count} sends to {key}, got {:?}", self.sends());
        self.texts_to(key)
    }

    /// Send one more ordinary message and wait for its reply: everything pushed before it has been
    /// handled by the bridge's event loop by then (frames are consumed in order).
    fn sentinel(&self, key_for_reply: &str, already: usize) -> Vec<String> {
        static NEXT_SENTINEL: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(900_000);
        let id = NEXT_SENTINEL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(rest) = key_for_reply.strip_prefix("private:") {
            let uid: i64 = rest.parse().unwrap();
            self.push(private_event(id, uid, if uid == OWNER { "owner" } else { "stranger" }, vec![text_seg("sentinel")]));
        } else {
            self.push(group_event(id, 1001, "tester", "", vec![text_seg("sentinel")]));
        }
        self.wait_texts(key_for_reply, already + 2)
    }

    fn ledger(&self) -> Vec<Value> {
        self.data_lines("turn-ledger.jsonl")
    }
}

fn seed_reserved_fixture(command: &mut Command, root: &Path) {
    if ["hf129", "hf130"].iter().any(|tag|root.file_name().unwrap().to_string_lossy().contains(tag)) {
        command.env("HER_PARITY_EPOCH_MS", "1760000000000").env("HER_PARITY_RANDOM_SEED", "1");
    } else if root.file_name().unwrap().to_string_lossy().contains("reserved-") {
        // Random draws must repeat, but timestamps cross the console/live process boundary.
        // A fixed epoch starts independently in each child and can timestamp new ingress
        // before an earlier administrator activation. Both children use the host clock.
        command.env_remove("HER_PARITY_EPOCH_MS").env("HER_PARITY_RANDOM_SEED", "1");
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
    seed_reserved_fixture(&mut command, root);
    hide_window(&mut command);
    command.spawn().expect("spawn console")
}

/// The prompt text after the clock line (`【当前时间】…\n\n`).
fn body_of(prompt: &str) -> &str {
    assert!(prompt.starts_with("【当前时间】"), "delivered prompt starts with the clock line:\n{prompt}");
    prompt.split_once("\n\n").map(|(_, rest)| rest).unwrap_or("")
}

fn says(rig: &Rig, key: &str, user: i64, nick: &str, text: &str) {
    static NEXT_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(100_000);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if key.starts_with("private:") {
        rig.push(private_event(id, user, nick, vec![text_seg(text)]));
    } else {
        rig.push(group_event(id, user, nick, "", vec![text_seg(text)]));
    }
}

