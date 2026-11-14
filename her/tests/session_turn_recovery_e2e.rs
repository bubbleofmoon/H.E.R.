//! ticket 10 external-behaviour acceptance: one session mapping shared by the admin console and the
//! turn runner, restart reconciliation of unfinished turns against what DSH holds, pausing of
//! unprovable turns, and refusal of late results from cancelled or replaced turns.
//!
//! Everything runs against real `her` processes (bridge + `--console-serve`) on a synthetic
//! runtime root, a stateful fake DSH (RPC + follow stream, with the real `source.rpcId` on user
//! messages), a fake OneBot WebSocket gateway and a fake OneBot HTTP API. No real QQ, model or DSH
//! endpoint is contacted. The fake DSH models what the installed DSH sources do: `session/cancel`
//! keeps queued prompts, `session/projections` shows the inbox, and the follow snapshot carries the
//! recent events.

#[path = "../src/install_root.rs"]
mod install_root;
#[path = "support/console.rs"]
mod console_fixture;

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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
const ONEBOT_TOKEN: &str = "ticket10-onebot-token-0001";
const CONSOLE_TOKEN: &str = "ticket10-console-token-0001";
const SEED_TOKEN: &str = "ticket10-seed-agent-token-0001";
const SECRET: &str = "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n";

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
    http_call_with_timeout(port, method, path, headers, body, Duration::from_secs(20))
}

fn http_call_with_timeout(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>, timeout: Duration) -> Option<(u16, Value)> {
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
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

// ───────────────────────── fake DSH ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    Refuse,
    /// DSH applies the prompt (message + turn) but the reply never reaches the bridge.
    ApplyThenHang,
    /// DSH receives the request and never applies or answers it.
    HangWithoutApply,
}

/// How the probe endpoints (`session/projections` and the follow stream) fail. None of these is
/// the DSH answer `session-not-found`, so none of them proves that a prompt never ran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProbeFault {
    None,
    /// A proxy answers HTTP 404 "Not Found" (also for the WebSocket handshake).
    Http404,
    /// DSH answers an RPC error whose message is "Method not found".
    MethodNotFound,
    /// Any other error whose free text says "not found".
    NotFoundText,
}

#[derive(Clone)]
struct Plan {
    /// Text the simulated model sends through the public send tool.
    send: Option<&'static str>,
    /// The turn waits (after `tool/call`, before the send) until `release()`.
    hold: bool,
}

#[derive(Default)]
struct Sess {
    inbox: Vec<Value>,
    events: Vec<Value>,
    seq: i64,
    turn_no: i64,
    cancelled: bool,
    gone: bool,
}

#[derive(Clone, Debug)]
struct PromptRec {
    session: String,
    rpc_id: String,
    text: String,
    applied: bool,
}

struct DshInner {
    observation_waits: bool,
    /// With `observation_waits`: the model first sends this reply, then waits.
    engage_send: Option<&'static str>,
    wait_results: Vec<(u16, Value)>,
    chat_preset: bool,
    shutting_down: bool,
    reply_text: &'static str,
    close_turns: bool,
    unclosed_first: bool,
    later_input_tokens: Option<u64>,
    acknowledge_snapshot: bool,
    real_prompt_source: bool,
    input_tokens: u64,
    cached_tokens: u64,
    stream_usage: bool,
    provider: &'static str,
    digest_fault: bool,
    digest_rpc_reject: bool,
    digest_hold: bool,
    digest_text: &'static str,
    digest_requests: Vec<Value>,
    digest_memory_error: bool,
    digest_memory_legacy_error: bool,
    follow_drop: bool,
    follow_history_cap: bool,
    digest_cancel_reject: bool,
    active_digests: HashSet<String>,
    overlapping_prompts: usize,
    archive_fault: bool,
    turn_start_delay_ms: u64,
    sessions: HashMap<String, Sess>,
    next_sid: u32,
    rpc: Vec<(String, Value)>,
    prompts: Vec<PromptRec>,
    plan: Plan,
    fault: Fault,
    down: bool,
    truncate: bool,
    probe: ProbeFault,
    released: bool,
    sends: Vec<(u16, Value)>,
    /// Hold the cancel RPC response after publishing its aborted turn/end.
    cancel_reply_gate: Option<std::sync::mpsc::Receiver<()>>,
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
            inner: Mutex::new(DshInner {
                observation_waits: false,
                engage_send: None,
                wait_results: Vec::new(),
                chat_preset: false,
                shutting_down: false,
                reply_text: "synthetic turn complete",
                close_turns: true,
                unclosed_first: false,
                later_input_tokens: None,
                acknowledge_snapshot: false,
                real_prompt_source: false,
                input_tokens: 7,
                cached_tokens: 0,
                stream_usage: false,
                provider: "ticket10-provider",
                digest_fault: false,
                digest_rpc_reject: false,
                digest_hold: false,
                digest_text: "[]",
                digest_requests: Vec::new(),
                digest_memory_error: false,
                digest_memory_legacy_error: false,
                follow_drop: false,
                follow_history_cap: false,
                digest_cancel_reject: false,
                active_digests: HashSet::new(),
                overlapping_prompts: 0,
                archive_fault: false,
                turn_start_delay_ms: 0,
                sessions: HashMap::new(),
                next_sid: 0,
                rpc: Vec::new(),
                prompts: Vec::new(),
                plan: Plan { send: Some("收到，已回复"), hold: false },
                fault: Fault::None,
                down: false,
                truncate: false,
                probe: ProbeFault::None,
                released: false,
                sends: Vec::new(),
                cancel_reply_gate: None,
            }),
        })
    }

    fn add_session(&self, id: &str) {
        lock(&self.inner).sessions.insert(id.to_string(), Sess::default());
    }
    fn mark_gone(&self, id: &str) {
        lock(&self.inner).sessions.entry(id.to_string()).or_default().gone = true;
    }
    fn set_plan(&self, plan: Plan) {
        let mut inner = lock(&self.inner);
        inner.plan = plan;
        inner.released = false;
    }
    fn set_fault(&self, fault: Fault) {
        lock(&self.inner).fault = fault;
    }
    fn set_down(&self, down: bool) {
        lock(&self.inner).down = down;
    }
    fn set_probe_fault(&self, fault: ProbeFault) {
        lock(&self.inner).probe = fault;
    }
    fn set_truncate(&self, on: bool) {
        lock(&self.inner).truncate = on;
    }
    fn release(&self) {
        lock(&self.inner).released = true;
    }
    fn applied(&self) -> Vec<PromptRec> {
        lock(&self.inner).prompts.iter().filter(|p| p.applied).cloned().collect()
    }
    fn received(&self) -> usize {
        lock(&self.inner).prompts.len()
    }
    fn count(&self, method: &str) -> usize {
        lock(&self.inner).rpc.iter().filter(|(m, _)| m == method).count()
    }
    fn calls(&self, method: &str) -> Vec<Value> {
        lock(&self.inner).rpc.iter().filter(|(m, _)| m == method).map(|(_, a)| a.clone()).collect()
    }
    fn sends(&self) -> Vec<(u16, Value)> {
        lock(&self.inner).sends.clone()
    }
    fn digest_calls(&self) -> usize {
        lock(&self.inner).digest_requests.len()
    }
    /// The relay's chat-completions endpoint answers rotation digests (`dependencies.model` is this server).
    fn model_digest(self: &Arc<Self>, stream: &mut TcpStream, body: &[u8]) {
        let (fault, hold, text) = {
            let mut inner = lock(&self.inner);
            inner.digest_requests.push(serde_json::from_slice(body).unwrap_or(Value::Null));
            (inner.digest_fault, inner.digest_hold, inner.digest_text)
        };
        if hold {
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                { let inner = lock(&self.inner); if inner.released || inner.shutting_down { break; } }
                thread::sleep(Duration::from_millis(50));
            }
        }
        if fault {
            respond(stream, 500, &json!({"error":{"message":"synthetic-private-error-text"}}));
            return;
        }
        respond(stream, 200, &json!({"id":"fixture","object":"chat.completion","created":1,"model":"ticket10-digest",
            "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}));
    }

    /// The old turn finally calls its send tool with the token it was given.
    fn late_send(&self, prompt_text: &str) -> Option<(u16, Value)> {
        relay_send(self.console_port.load(Ordering::Relaxed), prompt_text, "迟到的发送")
    }

    fn rpc(self: &Arc<Self>, method: &str, args: &Value) -> Result<Value, (String, String)> {
        let mut inner = lock(&self.inner);
        inner.rpc.push((method.to_string(), args.clone()));
        let sid = args.pointer("/request/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        let gone = |inner: &DshInner| inner.sessions.get(&sid).map(|s| s.gone).unwrap_or(false);
        let missing = |inner: &DshInner| !sid.is_empty() && !inner.sessions.contains_key(&sid);
        match method {
            "workspace/create" => Ok(json!({"workspace": {"workspaceId": "ws-ticket10"}, "created": false})),
            "agentPresets/list" if inner.chat_preset => Ok(json!({"presets":[{"id":"qq-chat"}]})),
            "workspace/rename" | "agentPresets/list" => Ok(json!({})),
            "session/create" => {
                inner.next_sid += 1;
                let id = format!("fake-session-{}", inner.next_sid);
                inner.sessions.insert(id.clone(), Sess::default());
                Ok(json!({"sessionId": id}))
            }
            "session/selectModel" => {
                if gone(&inner) || missing(&inner) {
                    return Err(("session-not-found".into(), format!("session {sid} not found")));
                }
                Ok(json!({}))
            }
            "session/projections" => {
                if gone(&inner) || missing(&inner) {
                    return Err(("session-not-found".into(), format!("session {sid} not found")));
                }
                Ok(json!({"asOfSeq": inner.sessions[&sid].seq, "values": {"inbox": {"next-turn": inner.sessions[&sid].inbox, "next-step": []}}}))
            }
            "session/updateQueue" => {
                let id = args.pointer("/request/itemId");
                inner.sessions.get_mut(&sid).unwrap().inbox.retain(|item| Some(&item["id"]) != id);
                Ok(json!({"accepted": true}))
            }
            "session/cancel" if inner.digest_cancel_reject && sid == "js-origin-session" => Err(("invalid-request".into(), "synthetic cancel refusal".into())),
            "session/cancel" => {
                if let Some(s) = inner.sessions.get_mut(&sid) {
                    s.cancelled = true;
                }
                Ok(json!({"accepted": true}))
            }
            "workspace/archiveSession" if inner.archive_fault => Err(("synthetic-archive-error".into(),"synthetic archive refusal".into())),
            "workspace/archiveSession" => Ok(json!({})),
            _ => Err(("unexpected".into(), format!("unexpected fake DSH method {method}"))),
        }
    }

    /// `session/prompt`: returns `None` when the reply must be withheld (fault).
    fn prompt(self: &Arc<Self>, args: &Value, rpc_id: &str) -> Option<Result<Value, (String, String)>> {
        let sid = args.pointer("/request/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        let text = args.pointer("/request/content/0/text").and_then(Value::as_str).unwrap_or("").to_string();
        let digest = text.starts_with("【记忆整理】");
        let (plan, fault) = {
            let mut inner = lock(&self.inner);
            inner.rpc.push(("session/prompt".into(), args.clone()));
            if !digest && inner.active_digests.contains(&sid) {inner.overlapping_prompts+=1;}
            if inner.sessions.get(&sid).map(|s| s.gone).unwrap_or(true) {
                inner.prompts.push(PromptRec { session: sid.clone(), rpc_id: rpc_id.into(), text, applied: false });
                return Some(Err(("session-not-found".into(), "session not found".into())));
            }
            if digest && inner.digest_rpc_reject {
                inner.prompts.push(PromptRec { session: sid.clone(), rpc_id: rpc_id.into(), text, applied: false });
                return Some(Err(("invalid-request".into(), "synthetic digest RPC rejection".into())));
            }
            let fault = if digest && inner.digest_fault { Fault::Refuse } else { inner.fault };
            if fault == Fault::Refuse {
                inner.prompts.push(PromptRec { session:sid.clone(),rpc_id:rpc_id.into(),text:text.clone(),applied:false });
                return Some(Ok(json!({"accepted":false})));
            }
            let applied = fault != Fault::HangWithoutApply;
            inner.prompts.push(PromptRec { session: sid.clone(), rpc_id: rpc_id.into(), text: text.clone(), applied });
            (if digest { Plan {send:None,hold:inner.digest_hold} } else {inner.plan.clone()}, fault)
        };
        if fault == Fault::HangWithoutApply {
            return None;
        }
        {
            let mut inner = lock(&self.inner);
            let real_prompt_source = inner.real_prompt_source;
            let sess = inner.sessions.get_mut(&sid).expect("session exists");
            sess.cancelled = false;
            if real_prompt_source {
                // Real DSH echoes the prompt requestId, not the RPC envelope id.
                let mut observed: Value = serde_json::from_str(include_str!("fixtures/dsh-0.2.1-rc.2-user-message.json")).unwrap();
                sess.seq += 1;
                observed["seq"] = json!(sess.seq);
                observed["time"] = json!(now_ms());
                observed["data"]["content"] = args["request"]["content"].clone();
                observed["data"]["source"]["rpcId"] = args["request"]["requestId"].clone();
                sess.events.push(observed);
            } else { event(
                sess,
                "user/message",
                json!({"role": "user", "id": format!("msg-{rpc_id}"), "content": [{"type": "text", "text": text}],
                       "source": {"kind": "user", "rpcId": rpc_id}}),
            ); }
        }
        let world = self.clone();
        let rpc = rpc_id.to_string();
        thread::spawn(move || world.run_turn(&sid, &rpc, &text, plan));
        if fault == Fault::ApplyThenHang {
            return None;
        }
        Some(Ok(json!({"accepted": true})))
    }

    fn run_turn(self: &Arc<Self>, sid: &str, _rpc_id: &str, prompt: &str, plan: Plan) {
        let delay=lock(&self.inner).turn_start_delay_ms;
        if delay>0 { thread::sleep(Duration::from_millis(delay)); }
        let turn = {
            let mut inner = lock(&self.inner);
            if prompt.starts_with("【记忆整理】") {inner.active_digests.insert(sid.into());}
            let sess = inner.sessions.get_mut(sid).expect("session");
            sess.turn_no += 1;
            let turn = sess.turn_no;
            event(sess, "turn/start", json!({"turn": turn}));
            if plan.send.is_some() {
                event(
                    sess,
                    "tool/call",
                    json!({"turn": turn, "step": 0, "callId": format!("c{turn}"), "name": "mcp__snowluma__qq_send_message", "arguments": "{}"}),
                );
            }
            turn
        };
        if lock(&self.inner).observation_waits && !prompt.starts_with("【记忆整理】") {
            let token = prompt.lines().find_map(|line| line.strip_prefix("【会话令牌】"))
                .and_then(|line| line.split_once('（').map(|(token, _)| token.trim())).unwrap();
            let engage = lock(&self.inner).engage_send;
            if let Some(text) = engage {
                let sent = relay_send(self.console_port.load(Ordering::Relaxed), prompt, text).unwrap_or((0, Value::Null));
                lock(&self.inner).sends.push(sent);
            }
            for _ in 0..100 {
                {
                    let mut inner = lock(&self.inner);
                    if inner.shutting_down || inner.sessions[sid].cancelled { break; }
                    event(inner.sessions.get_mut(sid).unwrap(), "tool/call", json!({"turn":turn,"step":0,
                        "callId":"hf136-wait","name":"mcp__snowluma__qq_wait_for_messages","arguments":"{}"}));
                }
                let result = http_call_with_timeout(self.console_port.load(Ordering::Relaxed), "POST", "/api/socialV2/wait",
                    &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", token)],
                    Some(&json!({"key":GROUP_KEY,"timeoutMs":120000})), Duration::from_secs(190)).expect("fake model wait result");
                let end = result.0 != 200 || result.1["nextAction"] == "end_turn";
                lock(&self.inner).wait_results.push(result);
                if end { break; }
            }
        }
        if plan.hold {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                {
                    let inner = lock(&self.inner);
                    if inner.released || inner.sessions.get(sid).map(|s| s.cancelled).unwrap_or(true) {
                        break;
                    }
                }
                if Instant::now() > deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        if lock(&self.inner).shutting_down {return;}
        let cancelled = lock(&self.inner).sessions.get(sid).map(|s| s.cancelled).unwrap_or(true);
        if cancelled {
            let mut inner = lock(&self.inner);
            inner.active_digests.remove(sid);
            if let Some(sess) = inner.sessions.get_mut(sid) {
                event(sess, "turn/end", json!({"turn": turn, "reason": {"kind": "aborted"}}));
            }
            return;
        }
        // Most lifecycle cases model an agent that closes its turn without acknowledging any
        // unread work. The dedicated missing-close case opts out and checks JS's reminders.
        let close = {let inner=lock(&self.inner); inner.close_turns && !(inner.unclosed_first && turn==1)};
        if !prompt.starts_with("【记忆整理】") && close {
            let token = prompt.lines().find_map(|line| line.strip_prefix("【会话令牌】"))
                .and_then(|line| line.split_once('（').map(|(token, _)| token.trim()));
            if let Some(token) = token {
                let offered=if lock(&self.inner).acknowledge_snapshot {
                    prompt.split("【本次消息快照】\n").nth(1).and_then(|tail|tail.lines().next())
                        .and_then(|packet|serde_json::from_str::<Value>(packet).ok()).and_then(|packet|packet["readThroughSeq"].as_u64()).unwrap_or(0)
                } else {0};
                let mut response = http_call(self.console_port.load(Ordering::Relaxed), "POST", "/api/socialV2/mark-read",
                    &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", token)],
                    Some(&json!({"key":prompt_key(prompt).unwrap_or(GROUP_KEY),"throughSeq":offered})));
                // A freshly reset diving conversation has not completed its observation wait.
                // The synthetic agent stays awake instead of bypassing that real tool guard.
                if response.as_ref().is_some_and(|(status, body)| *status == 400 && body.get("preSleepWaitRemainingMs").is_some()) {
                    response = http_call(self.console_port.load(Ordering::Relaxed), "POST", "/api/socialV2/wake-config",
                        &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", token)],
                        Some(&json!({"key":prompt_key(prompt).unwrap_or(GROUP_KEY),"config":{"mode":"active"}})));
                }
                if response.is_none() && lock(&self.inner).shutting_down {return;}
                assert_eq!(response.as_ref().map(|r|r.0), Some(200), "synthetic closing tool: {response:?}");
            }
        }
        if let Some(text) = plan.send {
            let result = relay_send(self.console_port.load(Ordering::Relaxed), prompt, text);
            let mut inner = lock(&self.inner);
            let (status, body) = result.unwrap_or((0, Value::Null));
            inner.sends.push((status, body));
            let is_error = status != 200;
            let sess = inner.sessions.get_mut(sid).expect("session");
            event(
                sess,
                "tool/result",
                json!({"turn": turn, "step": 0, "message": {"isError": is_error, "source": {"callId": format!("c{turn}")}, "content": [{"type": "text", "text": "ok"}]}}),
            );
        }
        let mut inner = lock(&self.inner);
        inner.active_digests.remove(sid);
        let input_tokens = if turn>1 {inner.later_input_tokens.unwrap_or(inner.input_tokens)} else {inner.input_tokens};
        let cached_tokens = inner.cached_tokens;
        let stream_usage = inner.stream_usage;
        let provider = inner.provider;
        let digest = prompt.starts_with("【记忆整理】");
        let final_text = if digest { inner.digest_text } else { inner.reply_text };
        let memory_error = inner.digest_memory_error;
        let legacy_memory_error = inner.digest_memory_legacy_error;
        let sess = inner.sessions.get_mut(sid).expect("session");
        if digest {
            event(sess, "tool/call", json!({"turn":turn,"step":0,"callId":"digest-memory","name":"memory_write","arguments":"{}"}));
            event(sess, "tool/result", json!({"turn":turn,"step":0,"message":{"isError":memory_error && !legacy_memory_error,"source":{"callId":"digest-memory"},"content":[{"type":"text","text":if memory_error && legacy_memory_error {"写入失败：synthetic-private-error-text"} else {"ok"}}]}}));
        }
        event(
            sess,
            "assistant/message",
            json!({"turn": turn, "step": 1, "provider": provider,
                   "model":"ticket10-model", "usage": {"inputTokens": input_tokens, "cacheReadTokens":cached_tokens, "outputTokens": 3},
                   "message": {"content": [{"type": "text", "text": final_text}]}}),
        );
        if stream_usage {
            let data=&mut sess.events.last_mut().unwrap()["data"];
            let usage=data.as_object_mut().unwrap().remove("usage").unwrap();
            data["stream"]=json!([{"usage":usage}]);
        }
        event(sess, "turn/end", json!({"turn": turn, "reason": {"kind": "completed"}}));
    }
}

/// Post the public `qq_send_message` route exactly as the DSH MCP relay would: with the key and
/// agent token found in the wake prompt.
fn prompt_key(prompt:&str)->Option<&str> {
    prompt
        .split_once("【唤醒】")
        .and_then(|(_, rest)| rest.lines().next())
        .or_else(|| {
            prompt
                .split_once("【引导唤醒】你已接入 QQ 会话 ")
                .and_then(|(_, rest)| rest.lines().next())
                .map(|line| line.trim_end_matches('。'))
        })
        .map(str::trim)

}

fn relay_send(console_port: u16, prompt: &str, text: &str) -> Option<(u16, Value)> {
    // An ordinary wake names the conversation after 【唤醒】; a first wake (also the first one after a
    // session reset) is a bootstrap wake and names it in 【引导唤醒】.
    let key=prompt_key(prompt)?;
    let token = prompt
        .lines()
        .find_map(|line| line.strip_prefix("【会话令牌】"))
        .and_then(|line| line.split_once('（').map(|(token, _)| token.trim().to_string()))
        .filter(|t| !t.is_empty())?;
    http_call(
        console_port,
        "POST",
        "/api/socialV2/send-message",
        &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", &token)],
        Some(&json!({"key": key, "messages": [text]})),
    )
}

fn dsh_connection(world: Arc<Dsh>, mut stream: TcpStream) {
    let mut peek = [0u8; 64];
    let Ok(n) = stream.peek(&mut peek) else { return };
    let is_ws = String::from_utf8_lossy(&peek[..n]).starts_with("GET /api/remote.mux");
    if lock(&world.inner).down {
        return; // connection dropped: DSH is unreachable
    }
    if is_ws {
        if lock(&world.inner).probe == ProbeFault::Http404 {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found");
            return;
        }
        dsh_follow(world, stream);
        return;
    }
    let Some(request) = read_request(&mut stream) else { return };
    if request.request_line.split_whitespace().nth(1) == Some("/v1/chat/completions") {
        world.model_digest(&mut stream, &request.body);
        return;
    }
    let Ok(envelope) = serde_json::from_slice::<Value>(&request.body) else { return };
    let method = envelope.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let rpc_id = envelope.get("rpcId").and_then(Value::as_str).unwrap_or("").to_string();
    let probe_fault = lock(&world.inner).probe;
    if method == "session/projections" && probe_fault != ProbeFault::None {
        match probe_fault {
            ProbeFault::Http404 => {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found");
            }
            fault => {
                let message = if fault == ProbeFault::MethodNotFound { "Method not found" } else { "widget not found in registry" };
                respond(
                    &mut stream,
                    200,
                    &json!({"type": "server-response", "rpcId": rpc_id,
                            "result": {"ok": false, "error": {"code": "internal", "message": message}}}),
                );
            }
        }
        return;
    }
    let args = envelope.pointer("/payload/args").cloned().unwrap_or(Value::Null);
    let outcome = if method == "session/prompt" {
        match world.prompt(&args, &rpc_id) {
            Some(result) => result,
            None => {
                // withheld reply: hold the connection until the bridge goes away
                thread::sleep(Duration::from_secs(45));
                return;
            }
        }
    } else {
        world.rpc(&method, &args)
    };
    if method == "session/cancel" {
        let gate = lock(&world.inner).cancel_reply_gate.take();
        if let Some(gate) = gate {
            gate.recv_timeout(Duration::from_secs(20)).expect("release cancel response");
        }
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
    let stream_id = frame.get("streamId").and_then(Value::as_str).unwrap_or("").to_string();
    let sid = frame.pointer("/payload/args/request/address/sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let probe_fault = lock(&world.inner).probe;
    if probe_fault != ProbeFault::None {
        let code = if probe_fault == ProbeFault::MethodNotFound { "method-not-found" } else { "resource not found" };
        let _ = socket.send(Message::Text(json!({"type": "error", "streamId": stream_id, "error": {"code": code}}).to_string()));
        return;
    }
    let (events, truncate, gone) = {
        let inner = lock(&world.inner);
        match inner.sessions.get(&sid) {
            Some(sess) => (sess.events.clone(), inner.truncate, sess.gone),
            None => (Vec::new(), false, true),
        }
    };
    if gone {
        let _ = socket.send(Message::Text(
            json!({"type": "error", "streamId": stream_id, "error": {"code": "session-not-found"}}).to_string(),
        ));
        return;
    }
    let mut sent = events.last().and_then(|e| e["seq"].as_i64()).unwrap_or(0);
    let max = if lock(&world.inner).follow_history_cap {
        frame.pointer("/payload/args/request/maxMessages").and_then(Value::as_u64).unwrap_or(200) as usize
    } else { events.len() };
    let start = events.iter().enumerate().rev()
        .filter(|(_, e)| matches!(e["type"].as_str(), Some("user/message" | "assistant/message")))
        .nth(max.saturating_sub(1)).map(|(index, _)| index).unwrap_or(0);
    let visible = if truncate { &events[events.len().saturating_sub(2)..] } else { &events[start..] };
    let has_more = truncate || visible.len() < events.len();
    let records: Vec<Value> = visible.iter().map(|e| json!({"type": "event", "event": e})).collect();
    let snapshot = json!({
        "type": "item", "streamId": stream_id,
        "value": {"type": "snapshot", "records": records, "hasMore": has_more,
                  "projections": {"values": {"inbox": {"next-turn": [], "next-step": []}}}}
    });
    if socket.send(Message::Text(snapshot.to_string())).is_err() {
        return;
    }
    if lock(&world.inner).follow_drop && sid == "js-origin-session" {
        let _ = socket.close(None);
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

// ───────────────────────── fake OneBot (HTTP + WebSocket gateway) ─────────────────────────

#[derive(Default)]
struct Gateway {
    reject_send: bool,
    requests: Vec<(String,Value)>,
    sends: Vec<Value>,
    paths: Vec<String>,
    /// Accept a send request and close without answering (the receipt is lost).
    drop_send_reply: bool,
    next_message_id: i64,
    scripts: Vec<Vec<Step>>,
    frames: Vec<Value>,
}

#[derive(Clone)]
enum Step {
    Mention { message_id: i64, text: &'static str },
    Frame(Value),
    Wait(u64),
    WaitForTurnStart(Arc<Dsh>),
    WaitForSends(usize),
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
                Step::Frame(value) => {
                    if socket.send(Message::Text(value.to_string())).is_err() { break; }
                }
                Step::Mention { message_id, text } => {
                    if socket.send(Message::Text(mention_event(message_id, text).to_string())).is_err() {
                        break;
                    }
                }
                Step::Wait(ms) => thread::sleep(Duration::from_millis(ms)),
                Step::WaitForSends(count) => {
                    assert!(wait_for(LONG, || lock(&gateway).sends.len() >= count),
                        "script did not observe {count} gateway sends");
                }
                Step::WaitForTurnStart(dsh) => {
                    assert!(wait_for(LONG, || lock(&dsh.inner).sessions.values().any(|session| session.events.iter().any(|event| event["type"] == "turn/start"))), "script did not observe a running DSH turn");
                }
            }
        }
        // hold the connection open until the bridge goes away
        let _ = socket.get_ref().set_read_timeout(Some(Duration::from_millis(200)));
        let end = Instant::now() + Duration::from_secs(300);
        while Instant::now() < end {
            for frame in std::mem::take(&mut lock(&gateway).frames) {
                if socket.send(Message::Text(frame.to_string())).is_err() {break;}
            }
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
                g.paths.push(path.clone());
                g.requests.push((path.clone(),body.clone()));
                match path.as_str() {
                    "/get_msg" => json!({"status":"ok","retcode":0,"data":{"group_id":GROUP_ID,"sender":{"user_id":SELF_ID.to_string()}}}),
                    "/send_group_msg" | "/send_private_msg" | "/send_msg" => {
                        if g.reject_send {
                            respond(&mut stream,200,&json!({"status":"failed","retcode":1404,"wording":"synthetic refusal"}));
                            return;
                        }
                        g.sends.push(body.clone());
                        if g.drop_send_reply {
                            return; // close without a reply
                        }
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
    epoch: Option<i64>,
    clock_advance_file: Option<PathBuf>,
    root: PathBuf,
    console_port: u16,
    dsh: Arc<Dsh>,
    gateway: Arc<Mutex<Gateway>>,
    console: Child,
    candidate: Option<Child>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        lock(&self.dsh.inner).shutting_down=true;
        if thread::panicking() {
            for name in ["candidate-out.log", "candidate-err.log", "console-out.log", "console-err.log"] {
                let text = fs::read_to_string(self.root.join(name)).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(60).collect();
                eprintln!("---- {name} (tail) ----\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
            }
            eprintln!("---- turn ledger ----\n{}", fs::read_to_string(self.root.join("data/turn-ledger.jsonl")).unwrap_or_default());
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

struct Opts {
    seed_session: Option<&'static str>,
}

impl Rig {
    fn new(name: &str, opts: Opts) -> Self {
        Self::new_with_console_hook(name, opts, |_| {})
    }

    // The hook runs in the reservation-to-child-bind interval, only in this fixture.
    fn new_with_console_hook(name: &str, opts: Opts, mut after_release: impl FnMut(u16)) -> Self {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-ticket10-{name}-{}-{}", std::process::id(), now_ms())));
        for dir in ["config", "data/logs", "resources", "user/dsh", "ambient"] {
            fs::create_dir_all(root.join(dir)).expect("create runtime dir");
        }
        // Keep the port reserved while the fake listeners bind: the ephemeral allocator may
        // otherwise give it to OneBot, whose generic 200 response can impersonate readiness.
        let console_reservation = console_fixture::reserve_port();
        let console_port = console_reservation.local_addr().expect("console addr").port();
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

        fs::write(
            root.join("config/config.json"),
            json!({
                "consoleToken": CONSOLE_TOKEN, "ownerQQ": 8808,
                "allow": {"groups": [GROUP_ID], "private": []},
                "dsh": {"baseUrl": dsh_url, "provider": "ticket10-provider", "model": "ticket10-model", "reasoningEffort": "medium", "fallbacks": []},
                "snowluma": {"accessToken": ONEBOT_TOKEN, "httpUrl": ob_url, "wsUrl": ws_url},
                "socialV2": {
                    "agentPreset": "ticket10-preset",
                    "wake": {"batchWindowMs": 1000, "maxWakePerMinute": 30, "maxWakePerHour": 100},
                    "social": {"skipProbability": 0.0},
                    "tools": {"sendMessage": true, "getPrompt": true},
                    "send": {"mergeMultiIntoOne": true, "maxMessageChars": 800, "maxSendPerMinute": 0, "maxSendPerHour": 0, "burstIntervalMinMs": 0, "maxGapMs": 0, "gapBaseMs": 0, "gapPerCharMs": 0},
                    "recovery": {"waitSecs": 30}
                },
                "slang": {"enabled": false},
                "harness": {"model": "ticket10-digest"},
                "rollout": {"enabled": false, "rustGroups": []}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            root.join("data/social-v2.json"),
            json!({"conversations": {GROUP_KEY: {
                "agentToken": SEED_TOKEN, "unread": [], "recentMessages": [], "bootstrapSent": true,
                "wakeConfig": {"mode": "active", "infinite": true,
                    "triggers": {"atMention": true, "nameMention": false, "question": false, "poke": false, "anyMessage": false, "probability": 0.0},
                    "batchWindowMs": 1000}
            }}})
            .to_string(),
        )
        .unwrap();
        fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        fs::write(root.join("user/dsh/.credentials.yaml"), SECRET).unwrap();
        fs::write(root.join("config/harness-api-key"), "ticket10-digest-key").unwrap();
        fs::write(
            root.join("dependencies.json"),
            json!({"dsh": dsh_url, "model": dsh_url, "onebot": ob_url, "onebot_ws": ws_url}).to_string(),
        )
        .unwrap();
        if let Some(session) = opts.seed_session {
            dsh.add_session(session);
            fs::write(
                root.join("data/sessions.json"),
                json!({
                    "sessions": {GROUP_KEY: session},
                    "sessionPolicies": {GROUP_KEY: "[\"reserved2\",\"ticket10-preset\",null]"},
                    "jsOnly": {"keep": true}
                })
                .to_string(),
            )
            .unwrap();
        }
        drop(console_reservation);
        after_release(console_port);
        let console = spawn_console(&root, console_port);
        let mut rig = Self { root, console_port, dsh, gateway, console, candidate: None, epoch:None, clock_advance_file:None };
        rig.wait_for_console_with_hook(Duration::from_secs(20), &mut after_release);
        rig
    }

    fn start_candidate(&mut self, script: Vec<Step>) {
        self.start_candidate_groups(script, GROUP_KEY);
    }

    fn start_candidate_groups(&mut self, script: Vec<Step>, groups: &str) {
        lock(&self.gateway).scripts.push(script);
        let log = |name: &str| fs::OpenOptions::new().create(true).append(true).open(self.root.join(name)).unwrap();
        let ambient = self.root.join("ambient");
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        if let Some(epoch) = self.epoch { command.env("HER_PARITY_EPOCH_MS", epoch.to_string()); }
        command.env_remove("HER_PARITY_ADVANCE_FILE");
        if let Some(path) = &self.clock_advance_file { command.env("HER_PARITY_ADVANCE_FILE", path); }
        command
            .arg("--runtime-root").arg(&self.root)
            .arg("--live").arg("--live-groups").arg(groups)
            .arg("--console-port").arg(self.console_port.to_string())
            .env_remove("HER_RUNTIME_ROOT")
            .env("HOME", &ambient).env("USERPROFILE", &ambient).env("DSH_HOME", &ambient)
            .env("APPDATA", ambient.join("Roaming")).env("LOCALAPPDATA", ambient.join("Local"))
            .stdout(Stdio::from(log("candidate-out.log")))
            .stderr(Stdio::from(log("candidate-err.log")));
        hide_window(&mut command);
        self.candidate = Some(command.spawn().expect("spawn candidate"));
    }

    fn patch_config(&self, change: impl FnOnce(&mut Value)) {
        let path = self.root.join("config/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        change(&mut config);
        fs::write(path, config.to_string()).unwrap();
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

    fn restart_console_at_epoch(&mut self) {
        self.kill_candidate();
        self.console.kill().unwrap();
        self.console.wait().unwrap();
        self.console=spawn_console_at_epoch(&self.root,self.console_port,self.epoch,self.clock_advance_file.as_deref());
        self.wait_for_console(LONG);
    }

    fn wait_for_console(&mut self, timeout: Duration) {
        self.wait_for_console_with_hook(timeout, &mut |_| {});
    }

    fn wait_for_console_with_hook(&mut self, timeout: Duration, after_release: &mut impl FnMut(u16)) {
        console_fixture::wait_for_console(&mut self.console, &mut self.console_port, &self.root, CONSOLE_TOKEN, timeout, |port| {
            self.dsh.console_port.store(port, Ordering::Relaxed);
            after_release(port);
            spawn_console_at_epoch(&self.root, port, self.epoch, self.clock_advance_file.as_deref())
        });
    }

    fn console(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        http_call(self.console_port, method, path, &[("x-console-token", CONSOLE_TOKEN)], body)
            .unwrap_or((0, Value::Null))
    }

    fn turns(&self) -> Vec<Value> {
        self.console("GET", "/api/turns", None).1["turns"].as_array().cloned().unwrap_or_default()
    }

    fn view(&self) -> Value {
        self.console("GET", "/api/turns", None).1
    }

    fn turn_state(&self, index: usize) -> String {
        self.turns().get(index).and_then(|t| t["state"].as_str()).unwrap_or("").to_string()
    }

    fn inbound(&self) -> Value {
        self.console("GET", "/api/inbound/gaps", None).1
    }

    fn sessions_file(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.join("data/sessions.json")).unwrap_or_default()).unwrap_or(Value::Null)
    }

    fn onebot_sends(&self) -> Vec<Value> {
        lock(&self.gateway).sends.iter().filter(|body|body["group_id"]==GROUP_ID).cloned().collect()
    }

    fn agent_token(&self) -> String {
        let state: Value = serde_json::from_slice(&fs::read(self.root.join("data/social-v2.json")).unwrap()).unwrap();
        state["conversations"][GROUP_KEY]["agentToken"].as_str().unwrap_or("").to_string()
    }
}

fn spawn_console(root: &Path, port: u16) -> Child {
    spawn_console_at_epoch(root,port,None,None)
}

fn spawn_console_at_epoch(root: &Path, port: u16, epoch: Option<i64>, advance_file: Option<&Path>) -> Child {
    let ambient = root.join("ambient");
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    if let Some(epoch)=epoch {command.env("HER_PARITY_EPOCH_MS",epoch.to_string());}
    command.env_remove("HER_PARITY_ADVANCE_FILE");
    if let Some(path) = advance_file { command.env("HER_PARITY_ADVANCE_FILE", path); }
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

const LONG: Duration = Duration::from_secs(45);

#[test]
fn fixture_console_retries_port_taken_after_reservation_release() {
    let mut collision = None;
    let mut attempts = Vec::new();
    let mut rig = Rig::new_with_console_hook("console-bind-collision", Opts { seed_session: Some("js-origin-session") }, |port| {
        attempts.push(port);
        if collision.is_none() {
            // Synchronously take the released reservation before Command::spawn can bind.
            collision = Some(TcpListener::bind(("127.0.0.1", port)).expect("occupy released console port"));
        }
    });
    assert_eq!(attempts.len(), 2, "one forced collision must cause one retry");
    assert_ne!(rig.console_port, collision.as_ref().unwrap().local_addr().unwrap().port());
    let (status, health) = rig.console("GET", "/api/health", None);
    assert_eq!(status, 200);
    assert_eq!(health["ok"], true);
    assert_eq!(health["pid"], rig.console.id());
    assert_eq!(health["port"], rig.console_port);
    // Fake DSH tools must follow the replacement port too.
    rig.start_candidate(mention(142001, "wake after console bind collision"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    assert_eq!(rig.onebot_sends().len(), 1);
}

#[test]
fn scheduling_unclosed_wake_gets_one_reminder_then_resets_its_counter() {
    let mut rig = Rig::new("unclosed-wake", Opts { seed_session: Some("js-origin-session") });
    lock(&rig.dsh.inner).close_turns = false;
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.start_candidate(mention(16104, "wake without closing tool"));
    assert!(wait_for(Duration::from_secs(8), || rig.dsh.applied().len() == 2), "an unclosed wake never received its closing reminder");
    assert!(wait_for(LONG, || rig.turn_state(1) == "completed"));
    assert!(wait_for(Duration::from_secs(2), || {
        let state: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
        state["conversations"][GROUP_KEY]["wakeConfig"]["wakeCount"] == 0
    }), "scheduler did not reset the completed unclosed wake");
    assert!(rig.dsh.applied()[1].text.contains("【提醒】你还没有完成回合收尾"));
    assert!(!rig.dsh.applied()[1].text.contains("【本次消息快照】"), "JS reminders do not offer a wake snapshot");
    let state: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(state["conversations"][GROUP_KEY]["wakeConfig"]["wakeCount"], 0);
    assert_eq!(state["conversations"][GROUP_KEY]["wakeTimes"].as_array().unwrap().len(), 1, "a reminder is not another wake");
    thread::sleep(Duration::from_secs(1));
    assert_eq!(rig.dsh.applied().len(), 2, "reminders must be bounded");
}

#[test]
fn scheduling_wake_snapshot_grants_only_offered_receipts_in_the_console_process() {
    let mut rig = Rig::new("wake-receipts", Opts { seed_session: Some("js-origin-session") });
    rig.patch_config(|cfg| cfg["socialV2"]["context"] = json!({"wakeMessageLimit":1}));
    rig.dsh.set_plan(Plan { send: None, hold: true });
    rig.start_candidate(vec![Step::Wait(500), Step::Mention { message_id:16103, text:"offered in wake snapshot" }, Step::Wait(100), Step::Mention { message_id:16105, text:"outside the snapshot budget" }]);
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1));
    assert!(wait_for(LONG, || {
        let state: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap_or_default()).unwrap_or(Value::Null);
        state["conversations"][GROUP_KEY]["recentMessages"].as_array().is_some_and(|messages| messages.iter().any(|message| message["messageId"] == "16105"))
    }), "the second message never reached persisted ingress state");
    let response = rig.console("POST", "/api/socialV2/wake-config", Some(&json!({"key":GROUP_KEY,"config":{"mode":"active"}})));
    assert_eq!(response.0, 200, "{response:?}");
    assert_eq!(response.1["readThroughSeq"], 1, "wake snapshot was delivered but its process receipts never reached the console");
    assert_eq!(response.1["markedCount"], 0);
    assert_eq!(response.1["unreadCount"], 2);
    assert!(rig.dsh.applied()[0].text.contains("【本次消息快照】"));
    let disk = fs::read_to_string(rig.root.join("data/social-v2.json")).unwrap();
    assert!(!disk.contains("modelSeenSeqs"), "prompt receipts must not survive a process restart");
}

#[test]
fn scheduling_typing_budget_starts_at_dsh_turn_start_and_excludes_queue_time() {
    let mut rig=Rig::new("queued-typing",Opts{seed_session:Some("js-origin-session")});
    rig.patch_config(|c|c["socialV2"]["send"]=json!({"firstGapEnabled":true,"fillTypingBudget":true,"firstGapBaseMs":1500,"firstGapPerCharMs":10,"maxGapMs":2000,"burstIntervalMinMs":100}));
    lock(&rig.dsh.inner).turn_start_delay_ms=3500;
    rig.dsh.set_plan(Plan{send:Some("queued budget"),hold:false});
    rig.start_candidate(mention(16909,"DSH queues before starting"));
    assert!(wait_for(LONG,||lock(&rig.dsh.inner).sessions["js-origin-session"].events.iter().any(|e|e["type"]=="turn/start")));
    thread::sleep(Duration::from_millis(700));
    assert!(rig.onebot_sends().is_empty(),"DSH queue time incorrectly filled the typing budget");
    assert!(wait_for(Duration::from_secs(4),||rig.onebot_sends().len()==1));
}

#[test]
fn scheduling_first_typing_budget_subtracts_time_already_spent_in_the_turn() {
    let mut rig=Rig::new("filled-typing",Opts{seed_session:Some("js-origin-session")});
    rig.patch_config(|c|c["socialV2"]["send"]=json!({"firstGapEnabled":true,"fillTypingBudget":true,"firstGapBaseMs":3000,"firstGapPerCharMs":10,"maxGapMs":4000,"burstIntervalMinMs":100,"duplicateWindowMs":0}));
    rig.dsh.set_plan(Plan{send:Some("filled budget"),hold:true});
    rig.start_candidate(mention(16908,"model has already spent time"));
    assert!(wait_for(LONG,||rig.dsh.applied().len()==1));
    let mut started_at = None;
    assert!(wait_for(LONG, || {
        started_at = fs::read_to_string(rig.root.join("data/turn-ledger.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["k"] == "started")
            .filter_map(|record| record["t"].as_i64())
            .max();
        started_at.is_some()
    }), "the held turn never published its durable start");
    let started_at = started_at.unwrap();
    let state: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
    let incoming_at = state["conversations"][GROUP_KEY]["lastIncomingAt"].as_i64().unwrap();
    // Thinking fills the entire jittered typing budget, independently of the urgent quiet hold.
    assert!(wait_for(LONG, || now_ms() >= started_at + 4000 && now_ms() >= incoming_at + 6000),
        "the held turn did not reach both timing deadlines");
    assert!(rig.onebot_sends().is_empty(), "a held DSH turn must not send before release");
    let released=Instant::now();
    rig.dsh.release();
    assert!(wait_for(Duration::from_millis(1500),||rig.onebot_sends().len()==1),"typing budget was charged again after four seconds of model time");
    assert!(released.elapsed()<Duration::from_millis(1500));
}

#[test]
fn scheduling_refused_dispatch_restores_finite_sleep_and_retries_without_ingress() {
    let mut rig=Rig::new("refused-dispatch",Opts{seed_session:Some("js-origin-session")});
    let path=rig.root.join("data/social-v2.json");
    let mut state:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeConfig"]["infinite"]=json!(false);
    state["conversations"][GROUP_KEY]["wakeConfig"]["sleepUntil"]=json!("2020-01-01T00:00:00.000Z");
    fs::write(&path,state.to_string()).unwrap();
    rig.dsh.set_fault(Fault::Refuse);
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(vec![]);
    assert!(wait_for(Duration::from_secs(5),||rig.turn_state(0)=="failed"));
    assert!(wait_for(Duration::from_secs(2),||serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["conversations"][GROUP_KEY]["wakeConfig"]["sleepUntil"]=="2020-01-01T00:00:00.000Z"),"refused delivery consumed the finite deadline");
    assert!(rig.dsh.applied().is_empty());
    rig.dsh.set_fault(Fault::None);
    assert!(wait_for(Duration::from_secs(9),||rig.dsh.applied().len()==1),"dispatch lease prevented the safe retry");
}

#[test]
fn scheduling_failed_send_does_not_advance_read_or_reply_state() {
    let mut rig=Rig::new("send-refused",Opts{seed_session:Some("js-origin-session")});
    rig.patch_config(|c| {c["socialV2"]["autoReplyCheckMs"]=json!(1000);c["socialV2"]["send"]["firstGapEnabled"]=json!(false);});
    lock(&rig.gateway).reject_send=true;
    rig.dsh.set_plan(Plan{send:Some("refused text"),hold:false});
    rig.start_candidate(mention(16907,"send will fail"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    assert_eq!(rig.dsh.sends()[0].0,500);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.applied().len(),1);
    let state:Value=serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(state["conversations"][GROUP_KEY]["lastAiReplyAt"].as_i64().unwrap_or(0),0);
    assert_eq!(state["conversations"][GROUP_KEY]["lastReadThroughSeq"].as_i64().unwrap_or(0),0);
    assert_eq!(state["conversations"][GROUP_KEY]["unread"].as_array().unwrap().len(),1);
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn scheduling_explicit_read_mark_consumes_busy_work_without_replaying_it() {
    let mut rig=Rig::new("read-busy",Opts{seed_session:Some("js-origin-session")});
    rig.dsh.set_plan(Plan{send:None,hold:true});
    rig.start_candidate(vec![Step::Wait(500),Step::Mention{message_id:16905,text:"first"},Step::Wait(2500),Step::Mention{message_id:16906,text:"busy but handled"}]);
    assert!(wait_for(LONG,||rig.dsh.applied().len()==1));
    assert!(wait_for(Duration::from_secs(5),||serde_json::from_slice::<Value>(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap()["conversations"][GROUP_KEY]["lastUnreadSeq"]==2));
    // A console cursor is not model-delivery evidence: the fake model first reads the messages.
    let token=rig.agent_token();
    let (status,packet)=http_call(rig.console_port,"GET",&format!("/api/socialV2/unread?key={GROUP_KEY}"),
        &[("x-console-token",CONSOLE_TOKEN),("x-agent-token",&token)],None).unwrap();
    assert_eq!(status,200,"{packet}");
    assert!(packet["messages"].as_array().unwrap().iter().any(|m|m["seq"]==2),"{packet}");
    let marked=rig.console("POST","/api/socialV2/mark-read",Some(&json!({"key":GROUP_KEY,"throughSeq":2})));
    assert_eq!(marked.0,200,"{marked:?}");
    assert_eq!(marked.1["markedCount"],2);
    rig.dsh.release();
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.applied().len(),1);
    let state:Value=serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(state["conversations"][GROUP_KEY]["lastReadThroughSeq"],2);
    assert!(state["conversations"][GROUP_KEY]["unread"].as_array().unwrap().is_empty());
}

#[test]
fn scheduling_console_wake_setting_changes_next_message_and_delivered_prompt() {
    let mut rig=Rig::new("console-wake",Opts{seed_session:Some("js-origin-session")});
    let mut first=mention_event(16903,"first quiet message");
    first["message"]=json!([{"type":"text","data":{"text":"first quiet message"}}]);
    let mut second=first.clone(); second["message_id"]=json!(16904);
    second["message"]=json!([{"type":"text","data":{"text":"next active message"}}]);
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(vec![Step::Wait(500),Step::Frame(first),Step::Wait(2500),Step::Frame(second)]);
    assert!(wait_for(Duration::from_secs(4),||fs::read_to_string(rig.root.join("data/social-v2.json")).unwrap().contains("first quiet message")));
    assert!(rig.dsh.applied().is_empty());
    let configured=rig.console("POST","/api/socialV2/wake-config",Some(&json!({"key":GROUP_KEY,"config":{"mode":"active","infinite":true}})));
    assert_eq!(configured.0,200,"{configured:?}");
    assert!(wait_for(Duration::from_secs(7),||rig.dsh.applied().len()==1));
    let prompt=&rig.dsh.applied()[0].text;
    assert!(prompt.contains("【当前唤醒】活跃，无限"));
    assert!(prompt.contains("任意消息"));
    assert!(prompt.contains("next active message"));
}

#[test]
fn scheduling_silent_turn_does_not_invent_a_send_or_reply_check() {
    let mut rig=Rig::new("silent-outcome",Opts{seed_session:Some("js-origin-session")});
    rig.patch_config(|c|c["socialV2"]["autoReplyCheckMs"]=json!(1000));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(16902,"model chooses silence"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.applied().len(),1,"model text created an unsolicited reply check");
    let state:Value=serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(state["conversations"][GROUP_KEY]["lastAiReplyAt"].as_i64().unwrap_or(0),0);
    assert_eq!(state["conversations"][GROUP_KEY]["unread"].as_array().unwrap().len(),1);
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn timing_records_distinguish_normal_and_cancelled_dsh_endings() {
    for cancelled in [false, true] {
        let mut rig = Rig::new("timing-end-kind", Opts { seed_session: Some("js-origin-session") });
        rig.dsh.set_plan(Plan { send:None, hold:cancelled });
        if cancelled {
            rig.patch_config(|c| c["socialV2"]["wake"]["preemptAfterMs"] = json!(1000));
            rig.start_candidate(vec![Step::Wait(500), Step::Mention {message_id:29901,text:"held for timing"},
                Step::WaitForTurnStart(rig.dsh.clone()), Step::Wait(1200), Step::Mention {message_id:29902,text:"preempt timing"}]);
        } else {
            rig.start_candidate(mention(29901,"normal timing"));
        }
        let read_timing = || -> Vec<Value> {
            fs::read_to_string(rig.root.join("data/logs/turn-timing.jsonl")).unwrap_or_default()
                .lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
        };
        assert!(wait_for(LONG, || !read_timing().is_empty()), "turn timing missing");
        let timing = read_timing().remove(0);
        assert_eq!(timing["endKind"], if cancelled {"aborted"} else {"completed"});
        assert_eq!(timing["cancelled"], cancelled);
        if cancelled { assert!(timing["cancelReason"].as_str().is_some_and(|s| !s.is_empty())); }
        else { assert!(timing["cancelReason"].is_null()); }
        let turn_id = timing["turnId"].as_str().expect("timing needs the bridge turn identity");
        let rpc_id = timing["attemptRpcId"].as_str().expect("timing needs its submitted attempt");
        let ledger: Vec<Value> = fs::read_to_string(rig.root.join("data/turn-ledger.jsonl")).unwrap()
            .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(ledger.iter().any(|row| row["k"] == "begun" && row["turn"] == turn_id));
        assert!(ledger.iter().any(|row| row["k"] == "attempt" && row["turn"] == turn_id && row["rpcId"] == rpc_id));
        assert!(!timing.to_string().contains(SEED_TOKEN), "timing leaked an agent token");
    }
}

#[test]
fn scheduling_signed_quote_with_string_sender_identity_wakes_actual_conversation() {
    let mut rig=Rig::new("signed-quote",Opts{seed_session:Some("js-origin-session")});
    let mut frame=mention_event(16901,"signed quote");
    frame["self_id"]=json!(SELF_ID.to_string());
    frame["group_id"]=json!(GROUP_ID.to_string());
    frame["user_id"]=json!("1001");
    frame["message"]=json!([{"type":"reply","data":{"id":-1234}},{"type":"text","data":{"text":"signed quote"}}]);
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(vec![Step::Wait(500),Step::Frame(frame)]);
    assert!(wait_for(Duration::from_secs(6),||rig.dsh.applied().len()==1));
    assert_eq!(lock(&rig.gateway).requests.iter().find(|(p,_)|p=="/get_msg").unwrap().1["message_id"],-1234);
}

#[test]
fn scheduling_timer_held_by_recovery_is_released_without_new_ingress() {
    let mut rig=Rig::new("timer-held",Opts{seed_session:Some("js-origin-session")});
    fs::write(rig.root.join("data/turn-ledger.jsonl"), format!("{}\n{}\n",
        json!({"k":"begun","turn":"paused-before-wake","key":GROUP_KEY,"t":now_ms(),"backend":"dsh","epoch":"old","inbound":[]}),
        json!({"k":"unknown","turn":"paused-before-wake","t":now_ms(),"reason":"test-unproven","detail":"synthetic paused recovery"}))).unwrap();
    let path=rig.root.join("data/social-v2.json");
    let mut state:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeConfig"]["infinite"]=json!(false);
    state["conversations"][GROUP_KEY]["wakeConfig"]["sleepUntil"]=json!("2020-01-01T00:00:00.000Z");
    fs::write(path,state.to_string()).unwrap();
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(vec![]);
    assert!(wait_for(Duration::from_secs(6),||fs::read_to_string(rig.root.join("data/turn-ledger.jsonl")).unwrap().contains("\"k\":\"held\"")));
    assert!(rig.dsh.applied().is_empty());
    let resolved=rig.console("POST","/api/turns/resolve",Some(&json!({"turnId":"paused-before-wake","action":"abandon"})));
    assert_eq!(resolved.0,200,"{resolved:?}");
    assert!(wait_for(Duration::from_secs(8),||rig.dsh.applied().len()==1),"timer wake lost behind recovery dispatch lease");
}

#[test]
fn scheduling_restart_keeps_rate_limit_and_console_config_changes_next_dispatch() {
    let mut rig=Rig::new("rate-restart",Opts{seed_session:Some("js-origin-session")});
    let path=rig.root.join("data/social-v2.json");
    let mut state:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeTimes"]=json!([now_ms()]);
    fs::write(&path,state.to_string()).unwrap();
    rig.patch_config(|c|c["socialV2"]["wake"]["maxWakePerMinute"]=json!(1));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(16801,"rate limited after restart"));
    thread::sleep(Duration::from_secs(4));
    assert!(rig.dsh.applied().is_empty(),"restart reset the persisted cooldown");
    rig.patch_config(|c|c["socialV2"]["wake"]["maxWakePerMinute"]=json!(0));
    assert!(wait_for(Duration::from_secs(4),||rig.dsh.applied().len()==1),"new wake rate settings did not release the queued retry");
}

#[test]
fn scheduling_elapsed_finite_wake_is_consumed_once_and_recorded() {
    let mut rig = Rig::new("finite-once", Opts {seed_session:Some("js-origin-session")});
    let path = rig.root.join("data/social-v2.json");
    let mut state:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeConfig"]["infinite"]=json!(false);
    state["conversations"][GROUP_KEY]["wakeConfig"]["sleepUntil"]=json!("2020-01-01T00:00:00.000Z");
    fs::write(&path,state.to_string()).unwrap();
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(vec![]);
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.applied().len(),1,"expired deadline replayed at every turn end");
    let state:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(state["conversations"][GROUP_KEY]["wakeConfig"]["sleepUntil"].is_null());
    assert_eq!(state["conversations"][GROUP_KEY]["lastWakeReason"],"timeout");
    assert_eq!(state["conversations"][GROUP_KEY]["wakeConfig"]["wakeCount"],1);
}

#[test]
fn scheduling_paused_batch_is_not_dispatched_and_does_not_return_on_resume() {
    let mut rig = Rig::new("pause-batch", Opts { seed_session: Some("js-origin-session") });
    rig.patch_config(|c|c["socialV2"]["reply"]=json!({"wakeQuietMs":3000}));
    let path = rig.root.join("data/social-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"]["anyMessage"] = json!(true);
    fs::write(&path,state.to_string()).unwrap();
    let mut incoming=mention_event(16701,"pause queued work");
    incoming["message"]=json!([{"type":"text","data":{"text":"pause queued work"}}]);
    rig.start_candidate(vec![Step::Wait(500),Step::Frame(incoming)]);
    assert!(wait_for(Duration::from_secs(5), || fs::read_to_string(&path).unwrap_or_default().contains("pause queued work")));
    let paused = rig.console("POST","/api/socialV2/activity",Some(&json!({"paused":true})));
    assert_eq!(paused.0,200,"{paused:?}");
    thread::sleep(Duration::from_secs(4));
    assert!(rig.dsh.applied().is_empty(),"paused timer reached DSH");
    assert_eq!(rig.console("POST","/api/socialV2/activity",Some(&json!({"paused":false}))).0,200);
    thread::sleep(Duration::from_secs(2));
    assert!(rig.dsh.applied().is_empty(),"cancelled batch came back on resume");
}

#[test]
fn scheduling_successful_tool_send_arms_reply_check_for_later_nontrigger_message() {
    let mut rig = Rig::new("reply-check", Opts { seed_session: Some("js-origin-session") });
    rig.patch_config(|c| { c["socialV2"]["autoReplyCheckMs"] = json!(3000); c["socialV2"]["send"]["firstGapEnabled"] = json!(false); });
    let mut follow = mention_event(16601,"nontrigger follow up");
    follow["message"] = json!([{"type":"text","data":{"text":"nontrigger follow up"}}]);
    rig.dsh.set_plan(Plan {send:None,hold:false});
    rig.start_candidate(vec![Step::WaitForSends(1),Step::Frame(follow)]);
    assert!(wait_for(Duration::from_secs(5), || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("connected")));
    thread::sleep(Duration::from_millis(750)); // allow the initial driver synchronization before the tool call
    let send = relay_send(rig.console_port, &format!("【唤醒】{GROUP_KEY}\n【会话令牌】{SEED_TOKEN}（test）"),"sent first").unwrap();
    assert_eq!(send.0,200,"{send:?}");
    assert_eq!(send.1["sent"],1,"HTTP 200 must mean delivery, not reviseRequired: {send:?}");
    assert_ne!(send.1["reviseRequired"],true,"{send:?}");
    assert_eq!(rig.onebot_sends().len(),1,"initial gateway delivery must precede the follow-up");
    assert!(wait_for(Duration::from_secs(8), || rig.dsh.applied().len() == 1), "successful tool send never armed scheduleReplyCheckV2");
    let wakes = fs::read_to_string(rig.root.join("data/logs/rbridge-live-wakes.jsonl")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(wakes.lines().next().unwrap()).unwrap()["reason"], "replyCheck");
    assert!(rig.dsh.applied()[0].text.contains("nontrigger follow up"));
}

#[test]
fn scheduling_busy_reply_check_is_suppressed_after_turn_answers_and_marks_read() {
    let mut rig = Rig::new("busy-reply-check", Opts { seed_session: Some("js-origin-session") });
    rig.patch_config(|c| {
        c["socialV2"]["autoReplyCheckMs"] = json!(5000);
        c["socialV2"]["proactive"]["enabled"] = json!(false);
        c["socialV2"]["send"]["firstGapEnabled"] = json!(false);
    });
    rig.dsh.set_plan(Plan { send: Some("answered busy input"), hold: true });
    let mut follow = mention_event(16603, "new nontrigger input during reply check");
    follow["message"] = json!([{"type":"text","data":{"text":"new nontrigger input during reply check"}}]);
    rig.start_candidate(vec![Step::Frame(mention_event(16602, "start the busy turn")), Step::WaitForSends(1), Step::Frame(follow)]);
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1));
    let send = relay_send(rig.console_port, &format!("【唤醒】{GROUP_KEY}\n【会话令牌】{SEED_TOKEN}（test）"), "first successful send").unwrap();
    assert_eq!(send.0, 200, "{send:?}");
    assert_eq!(send.1["sent"], 1, "HTTP 200 must mean delivery, not reviseRequired: {send:?}");
    assert_ne!(send.1["reviseRequired"], true, "{send:?}");
    assert_eq!(rig.onebot_sends().len(), 1, "initial gateway delivery must precede the follow-up");
    assert!(wait_for(LONG, || serde_json::from_slice::<Value>(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap()["conversations"][GROUP_KEY]["lastUnreadSeq"] == 2));
    thread::sleep(Duration::from_secs(6)); // the first send's check expires while DSH holds the turn
    assert_eq!(rig.dsh.applied().len(), 1);
    let marked = rig.console("POST", "/api/socialV2/mark-read", Some(&json!({"key":GROUP_KEY,"throughSeq":2})));
    assert_eq!(marked.0, 200, "{marked:?}");
    assert_eq!(marked.1["markedCount"], 2);
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    thread::sleep(Duration::from_secs(7)); // cover both immediate replay and the rearmed check
    assert_eq!(rig.dsh.applied().len(), 1, "a busy check must re-read the reply state instead of starting an extra turn");
    assert_eq!(rig.onebot_sends().len(), 2, "only the initial send and the actual answer are sent");
    let state: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(state["conversations"][GROUP_KEY]["unread"], json!([]));
    assert_eq!(state["conversations"][GROUP_KEY]["wakeConfig"]["infinite"], true);
}

#[test]
fn scheduling_persisted_conversation_gets_proactive_opportunity_without_ingress() {
    let mut rig = Rig::new("proactive", Opts { seed_session: Some("js-origin-session") });
    rig.epoch = Some(1793001600000); // 2026-10-26 08:00 UTC, daytime in the bridge's supported zone.
    rig.patch_config(|c| c["socialV2"]["proactive"] = json!({"enabled":true,"checkIntervalMinMs":60000,"checkIntervalMaxMs":60000,"idleThresholdMs":1,"probability":1}));
    rig.dsh.set_plan(Plan { send:None, hold:false });
    rig.start_candidate(vec![]);
    assert!(wait_for(Duration::from_secs(66), || rig.dsh.applied().len() == 1), "no proactive turn without ingress");
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    let wakes: Vec<Value> = fs::read_to_string(rig.root.join("data/logs/rbridge-live-wakes.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(wakes[0]["reason"], "proactiveCheck");
    assert_eq!(rig.dsh.count("session/create"), 0);
}

#[test]
fn scheduling_mode_roundtrip_invalidates_batched_work_and_old_token() {
    let mut rig = Rig::new("mode-queue", Opts { seed_session: Some("js-origin-session") });
    rig.patch_config(|c|c["socialV2"]["reply"]=json!({"wakeQuietMs":4000}));
    let path = rig.root.join("data/social-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"]["anyMessage"] = json!(true);
    fs::write(&path, state.to_string()).unwrap();
    let mut incoming=mention_event(16501,"queued before mode switch");
    incoming["message"]=json!([{"type":"text","data":{"text":"queued before mode switch"}}]);
    rig.start_candidate(vec![Step::Wait(500),Step::Frame(incoming)]);
    assert!(wait_for(Duration::from_secs(5), || fs::read_to_string(&path).unwrap_or_default().contains("queued before mode switch")));
    assert_eq!(rig.console("POST","/api/mode",Some(&json!({"mode":"chat"}))).0, 200);
    // Round trip inside a single synchronization tick must also revoke queued work.
    assert_eq!(rig.console("POST","/api/mode",Some(&json!({"mode":"reserved2"}))).0, 200);
    thread::sleep(Duration::from_secs(5));
    assert!(rig.dsh.applied().is_empty(), "old batch survived mode round trip");
    assert_ne!(rig.agent_token(), SEED_TOKEN);
    let late = relay_send(rig.console_port, &format!("【唤醒】{GROUP_KEY}\n【会话令牌】{SEED_TOKEN}（old）"), "late").unwrap();
    assert!(matches!(late.0,401|403));
}

#[test]
fn scheduling_preemption_cancels_pipeline_and_refuses_late_tool_calls() {
    let mut rig = Rig::new("preempt", Opts { seed_session: Some("js-origin-session") });
    rig.patch_config(|c| c["socialV2"]["wake"]["preemptAfterMs"] = json!(1000));
    rig.dsh.set_plan(Plan { send:None, hold:true });
    rig.start_candidate(vec![Step::Wait(500), Step::Mention {message_id:16401,text:"held"}, Step::WaitForTurnStart(rig.dsh.clone()), Step::Wait(1200), Step::Mention {message_id:16402,text:"preempt me"}]);
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1));
    let old = rig.dsh.applied()[0].text.clone();
    lock(&rig.dsh.inner).sessions.get_mut("js-origin-session").unwrap().inbox.push(json!({"id":"queued-obsolete-prompt"}));
    // The fake records the RPC before replying. Pipeline::cancel_with_reason still has to
    // revoke the token and publish the terminal ledger record after that reply arrives.
    assert!(wait_for(Duration::from_secs(6), || rig.dsh.count("session/cancel") > 0 && rig.turn_state(0) == "cancelled"),
        "Pipeline::cancel did not finish: {}", rig.view());
    assert!(rig.dsh.count("session/updateQueue") > 0);
    assert!(lock(&rig.dsh.inner).sessions["js-origin-session"].inbox.is_empty());
    assert!(wait_for(Duration::from_secs(6),||rig.agent_token()!=SEED_TOKEN),"cancel did not durably revoke the old epoch");
    let late = rig.dsh.late_send(&old).unwrap();
    assert!(matches!(late.0, 401 | 403), "{late:?}");
    assert!(rig.onebot_sends().is_empty());
    rig.dsh.set_plan(Plan { send:None, hold:false });
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 2));
    assert_eq!(rig.dsh.count("session/create"), 0);
}

#[test]
fn scheduling_busy_ingress_remains_unread_and_replays_once_after_turn_end() {
    let mut rig = Rig::new("busy-replay", Opts { seed_session: Some("js-origin-session") });
    rig.dsh.set_plan(Plan { send: None, hold: true });
    rig.start_candidate(vec![Step::Wait(500), Step::Mention {message_id:16301,text:"first"}, Step::Wait(3000), Step::Mention {message_id:16302,text:"busy second"}, Step::Mention {message_id:16303,text:"busy third"}]);
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1));
    let state = || -> Value { serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap_or(Value::Null) };
    assert!(wait_for(Duration::from_secs(6), || state()["conversations"][GROUP_KEY]["unread"].as_array().is_some_and(|a| a.len() == 3)), "busy ingress was not received while DSH held the turn");
    assert_eq!(rig.dsh.applied().len(), 1, "one active turn per conversation");
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 2), "busy triggers must coalesce and replay");
    assert!(wait_for(LONG, || rig.turn_state(1) == "completed"));
    assert!(rig.dsh.applied()[1].text.contains("busy third"));
    assert_eq!(state()["conversations"][GROUP_KEY]["unread"].as_array().unwrap().len(), 3, "silence does not acknowledge messages");
}

#[test]
fn scheduling_priority_upgrade_flushes_quiet_on_mention() {
    let mut rig = Rig::new("batch-upgrade", Opts { seed_session: Some("js-origin-session") });
    let path = rig.root.join("data/social-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"]["keywords"] = json!(["lower"]);
    fs::write(path, state.to_string()).unwrap();
    let mut first = mention_event(16201, "first lower priority");
    first["message"] = json!([{"type":"text","data":{"text":"first lower priority"}}]);
    rig.start_candidate(vec![Step::Wait(500), Step::Frame(first), Step::Wait(700), Step::Mention { message_id:16202, text:"upgrade" }]);
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    let records = |file: &str| -> Vec<Value> { fs::read_to_string(rig.root.join(file)).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect() };
    let decisions = records("data/logs/rbridge-live.jsonl");
    let urgent = decisions.iter().find(|v| v["key"] == GROUP_KEY && v["reason"] == "atMention").unwrap()["ts"].as_i64().unwrap();
    let wakes = records("data/logs/rbridge-live-wakes.jsonl");
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0]["reason"], "atMention");
    let elapsed = wakes[0]["ts"].as_i64().unwrap() - urgent;
    assert!((0..=1000).contains(&elapsed), "mention did not flush quiet within the hub tick: {elapsed}");
}

#[test]
fn scheduling_private_poke_uses_actual_peer_and_not_group_zero() {
    let mut rig = Rig::new("private-poke", Opts { seed_session: None });
    rig.patch_config(|c| c["allow"]["private"] = json!([1001]));
    let path = rig.root.join("data/social-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"]["private:1001"] = state["conversations"][GROUP_KEY].clone();
    state["conversations"]["private:1001"]["wakeConfig"]["triggers"]["poke"] = json!(true);
    fs::write(path, state.to_string()).unwrap();
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.start_candidate_groups(vec![Step::Wait(500), Step::Frame(json!({
        "post_type":"notice", "sub_type":"poke", "self_id":"9001", "user_id":"1001", "target_id":"9001"
    }))], "private:1001");
    assert!(wait_for(Duration::from_secs(6), || rig.dsh.applied().len() == 1));
    assert!(rig.dsh.applied()[0].text.contains("private:1001"));
}

#[test]
fn scheduling_ingress_denied_and_unknown_routes_do_not_lookup_quotes() {
    let mut rig = Rig::new("route-gate", Opts { seed_session: Some("js-origin-session") });
    let mut denied = mention_event(16101, "denied");
    denied["group_id"] = json!(420043);
    denied["message"] = json!([{"type":"reply","data":{"id": "-1234"}}]);
    let mut unknown = denied.clone();
    unknown["group_id"] = json!(GROUP_ID);
    unknown["message_type"] = json!("unknown");
    rig.start_candidate(vec![Step::Wait(500), Step::Frame(denied), Step::Frame(unknown), Step::Wait(1000), Step::Mention { message_id:16102, text:"allowed" }]);
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    assert_eq!(rig.dsh.applied().len(), 1);
    assert_eq!(lock(&rig.gateway).paths.iter().filter(|p| *p == "/get_msg").count(), 0);
}

#[test]
fn scheduling_ingress_self_echo_is_filtered_before_quote_lookup_or_turn() {
    let mut rig = Rig::new("self-echo", Opts { seed_session: Some("js-origin-session") });
    let mut own = mention_event(16001, "own echo");
    own["user_id"] = json!(SELF_ID);
    own["message"].as_array_mut().unwrap().push(json!({"type":"reply","data":{"id":"-1234"}}));
    rig.start_candidate(vec![Step::Wait(500), Step::Frame(own), Step::Wait(2500), Step::Mention { message_id:16002, text:"real peer" }]);
    assert!(wait_for(LONG, || !rig.dsh.applied().is_empty()));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    assert_eq!(rig.dsh.applied().len(), 1);
    assert!(!rig.dsh.applied()[0].text.contains("own echo"));
    assert_eq!(lock(&rig.gateway).paths.iter().filter(|p| *p == "/get_msg").count(), 0);
}


// ───────────────────────── AC1: one session mapping ─────────────────────────

#[test]
fn a_session_written_by_js_is_reused_across_turns_and_restarts() {
    let mut rig = Rig::new("reuse", Opts { seed_session: Some("js-origin-session") });
    rig.restart(mention(1001, "第一句"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "first turn never reached DSH");
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "turn not completed: {:?}", rig.turns());

    // The admin console lists the very mapping the runner used.
    let (status, listing) = rig.console("GET", "/api/sessions", None);
    assert_eq!(status, 200, "{listing}");
    assert_eq!(listing["sessions"][0]["sessionId"], json!("js-origin-session"));

    rig.restart(mention(1002, "第二句"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 2), "second turn after restart never reached DSH");
    let applied = rig.dsh.applied();
    assert!(applied.iter().all(|p| p.session == "js-origin-session"), "turns must stay in the original session: {applied:?}");
    assert_eq!(rig.dsh.count("session/create"), 0, "no new DSH session may be created for a mapped conversation");
    assert_eq!(rig.dsh.count("workspace/create"), 0);
    let file = rig.sessions_file();
    assert_eq!(file["sessions"][GROUP_KEY], json!("js-origin-session"));
    assert_eq!(file["jsOnly"], json!({"keep": true}), "fields written by the JS bridge must survive");
}

#[test]
fn admin_reset_revokes_the_mapping_stops_dsh_work_and_refuses_the_old_turns_late_send() {
    let mut rig = Rig::new("reset", Opts { seed_session: Some("js-origin-session") });
    rig.dsh.set_plan(Plan { send: Some("旧回合的回复"), hold: true });
    rig.restart(mention(2001, "触发"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "turn never reached DSH");
    assert!(wait_for(LONG, || rig.turn_state(0) == "running"), "{:?}", rig.turns());
    let old_prompt = rig.dsh.applied()[0].text.clone();

    let (status, body) = rig.console("POST", "/api/session/reset", Some(&json!({"key": GROUP_KEY})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["archived"], json!("js-origin-session"));
    assert!(wait_for(LONG, || rig.dsh.count("session/cancel") >= 1 && rig.dsh.count("workspace/archiveSession") >= 1),
        "reset must stop and archive the old DSH session");
    assert!(rig.sessions_file()["sessions"].get(GROUP_KEY).is_none(), "mapping must be gone");
    assert!(wait_for(LONG, || rig.turn_state(0) == "cancelled"), "old turn must be cancelled: {:?}", rig.turns());
    assert_eq!(rig.turns()[0]["outcomeReason"], json!("session-reset"));

    // The old turn's model finally tries to send with the token it was given.
    let late = rig.dsh.late_send(&old_prompt).expect("late send reached the console");
    assert!(matches!(late.0, 401 | 403 | 404 | 409), "late send must be refused, got {late:?}");
    assert!(rig.onebot_sends().is_empty(), "nothing from the cancelled turn may reach QQ");

    // A new wake starts a fresh session with a fresh token and works normally.
    rig.dsh.set_plan(Plan { send: Some("新回合的回复"), hold: false });
    rig.restart(mention(2002, "重置之后"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 2), "turn after reset never reached DSH");
    assert_eq!(rig.dsh.count("session/create"), 1, "reset session must be rebuilt exactly once");
    assert!(wait_for(LONG, || rig.turn_state(1) == "completed"), "{:?}", rig.turns());
    assert_eq!(rig.onebot_sends().len(), 1, "only the new turn's reply is delivered");
    let new_session = rig.sessions_file()["sessions"][GROUP_KEY].as_str().unwrap_or("").to_string();
    assert_ne!(new_session, "js-origin-session");
    assert_ne!(rig.agent_token(), SEED_TOKEN, "the reset conversation must get a new token");
}

#[test]
fn admin_reset_with_a_busy_finalization_gate_still_stops_and_archives_the_dsh_session() {
    let mut rig = Rig::new("reset-busy-gate", Opts { seed_session: Some("js-origin-session") });
    rig.dsh.set_plan(Plan { send: Some("旧回合的回复"), hold: true });
    rig.restart(mention(2101, "触发"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "turn never reached DSH");
    assert!(wait_for(LONG, || rig.turn_state(0) == "running"), "{:?}", rig.turns());

    // Another process holds the runtime-wide finalization gate for longer than the 2 s budget.
    let gate_path = rig.root.join("data/locks/turn-ledger-finalize.lock");
    fs::create_dir_all(gate_path.parent().unwrap()).unwrap();
    let gate = fs::OpenOptions::new().read(true).write(true).create(true).open(&gate_path).unwrap();
    gate.try_lock().expect("the finalization gate must be free before the reset");
    let started = Instant::now();
    let (status, body) = rig.console("POST", "/api/session/reset", Some(&json!({"key": GROUP_KEY})));
    let waited = started.elapsed();
    drop(gate);

    assert_ne!(status, 200, "a reset that could not settle its turn must not report success: {body}");
    assert!(waited >= Duration::from_millis(1500), "the reset must wait for the busy gate before giving up: {waited:?}");
    assert!(wait_for(LONG, || rig.dsh.count("session/cancel") >= 1 && rig.dsh.count("workspace/archiveSession") >= 1),
        "a busy gate must not leave the old DSH session running unarchived");
    assert!(rig.sessions_file()["sessions"].get(GROUP_KEY).is_none(), "mapping must stay revoked");
}

#[test]
fn a_session_dsh_no_longer_has_is_replaced_and_recorded() {
    let mut rig = Rig::new("replace", Opts { seed_session: Some("stale-session") });
    rig.dsh.mark_gone("stale-session");
    rig.restart(mention(3001, "会话失效"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "turn never reached DSH");
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "{:?}", rig.turns());
    assert_eq!(rig.dsh.count("session/create"), 1);
    let new_session = rig.sessions_file()["sessions"][GROUP_KEY].as_str().unwrap_or("").to_string();
    assert!(new_session.starts_with("fake-session-"), "mapping must point at the replacement: {new_session}");
    assert_eq!(rig.dsh.applied()[0].session, new_session);
    let replacements = rig.view()["sessionReplacements"].clone();
    assert_eq!(replacements[0]["oldSession"], json!("stale-session"));
    assert_eq!(replacements[0]["newSession"], json!(new_session));
    assert_eq!(replacements[0]["reason"], json!("dsh-not-found"));
    // The old session's token is dead; the prompt of the replacement carries a new one.
    assert_ne!(rig.agent_token(), SEED_TOKEN);
    let old_token_use = http_call(
        rig.console_port, "POST", "/api/socialV2/send-message",
        &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", SEED_TOKEN)],
        Some(&json!({"key": GROUP_KEY, "messages": ["旧令牌"]})),
    ).expect("console answered");
    assert!(old_token_use.0 == 401 || old_token_use.0 == 403, "{old_token_use:?}");
}

#[test]
#[cfg(feature = "parity-clock")]
fn hf136_observation_budget_sleeps_once_and_restart_does_not_reopen() {
    use sha2::{Digest, Sha256};
    for forced_cap in [false, true] {
        let mut rig = Rig::new("hf136-observation", Opts { seed_session: None });
        rig.patch_config(|c| {
            c["socialV2"]["wake"]["preSleepWaitMs"] = json!(1);
            c["socialV2"]["wake"]["turnHardCapMs"] = json!(180000);
            c["socialV2"]["wait"] = json!({"minMs":100,"maxMs":120000,"minQuietAfterNewMs":0,
                "batchWindowMs":100,"batchMaxMs":100,"maxPerTurn":1000});
        });
        let origin = now_ms();
        // Deliberate clock skew forces the cap to interrupt an actual pending HTTP wait,
        // exercising the fallback even though normal synchronized clocks clip it first.
        let advance_file = rig.root.join("clock-advance-ms");
        if !forced_cap { rig.clock_advance_file = Some(advance_file.clone()); }
        rig.epoch = Some(origin - if forced_cap {60000} else {0});
        rig.restart_console_at_epoch();
        rig.epoch = Some(origin);
        rig.clock_advance_file = Some(advance_file.clone());
        {
            let mut inner = lock(&rig.dsh.inner);
            inner.observation_waits = true;
            inner.close_turns = false;
            inner.plan = Plan { send:None, hold:false };
        }
        rig.start_candidate(mention(13601, "observation budget"));
        assert!(wait_for(LONG, || !rig.dsh.applied().is_empty()));
        // Prompt/tool delivery is too early: admission and ledger reads precede lease
        // publication. Witness the same request's held OS lock before crossing the cap.
        assert!(wait_for(Duration::from_secs(10), || {
            let state: Value = fs::read(rig.root.join("data/social-v2.json")).ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(Value::Null);
            let active = &state["conversations"][GROUP_KEY]["activeWait"];
            let (Some(console), Some(request), Some(turn)) =
                (active["console"].as_str(), active["id"].as_str(), active["turn"].as_str()) else { return false };
            if active["purpose"] != "messages" || rig.turns().first().is_none_or(|t| t["turnId"] != turn) { return false; }
            let digest = Sha256::digest(format!("{console}:{request}").as_bytes());
            let path = rig.root.join(format!("data/locks/observation-wait-{digest:x}.lock"));
            fs::OpenOptions::new().read(true).write(true).open(path).ok()
                .is_some_and(|file| matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock)))
        }), "forced={forced_cap}: observation wait never became live");
        assert!(lock(&rig.dsh.inner).wait_results.is_empty(), "the first observation must still be pending");
        // Publish once by rename so neither process can read a partially written offset.
        // Synchronized clocks hit the closing margin; only the live clock jumps in the
        // forced case, keeping the witnessed console wait pending across the cap tick.
        let staged = rig.root.join("clock-advance-ms.tmp");
        fs::write(&staged, if forced_cap { "180001" } else { "160001" }).unwrap();
        fs::rename(staged, advance_file).unwrap();
        assert!(wait_for(Duration::from_secs(45), || rig.turn_state(0) == "completed"),
            "forced={forced_cap}: {:?}", rig.turns());
        assert!(wait_for(Duration::from_secs(5), || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap()
            .contains("observation budget ended; keeping wake config")), "hub did not collect the sleeping turn");
        assert!(wait_for(Duration::from_secs(5), || rig.inbound()["stages"]["handled"].as_i64() == Some(1)),
            "restart must witness durable ingress settlement");
        let ends = |root: &Path| fs::read_to_string(root.join("data/turn-ledger.jsonl")).unwrap().lines()
            .filter_map(|l|serde_json::from_str::<Value>(l).ok()).filter(|r|r["k"]=="ended").collect::<Vec<_>>();
        assert!(wait_for(Duration::from_secs(5), || !lock(&rig.dsh.inner).wait_results.is_empty()));
        let results = lock(&rig.dsh.inner).wait_results.clone();
        assert_eq!(results.len(), 1, "one witnessed observation must cover the budget exit");
        if forced_cap {
            assert_eq!(results[0].0, 403, "the cap must revoke the pending wait's authority");
        } else {
            assert_eq!(results[0].0, 200);
            assert_eq!(results[0].1["nextAction"], "end_turn");
            assert_eq!(results[0].1["turnBudgetExhausted"], true);
            assert_eq!(results[0].1["preSleepWaitSatisfied"], false);
        }
        assert_eq!(ends(&rig.root).len(), 1, "forced={forced_cap}");
        assert_eq!(ends(&rig.root)[0]["reason"], "observationSleep");
        assert_eq!(rig.dsh.count("session/cancel"), usize::from(forced_cap));
        let state: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
        let wake = state["conversations"][GROUP_KEY]["wakeConfig"].clone();
        assert_eq!(state["conversations"][GROUP_KEY]["lastReadThroughSeq"].as_i64().unwrap_or(0), 0);
        assert_eq!(state["conversations"][GROUP_KEY]["unread"].as_array().unwrap().len(), 1);
        assert!(rig.onebot_sends().is_empty());
        rig.restart(vec![]);
        thread::sleep(Duration::from_secs(3));
        assert_eq!(rig.turn_state(0), "completed");
        assert_eq!(rig.dsh.applied().len(), 1, "recovery reopened the sleeping turn");
        assert_eq!(ends(&rig.root).len(), 1);
        let after: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
        assert_eq!(after["conversations"][GROUP_KEY]["wakeConfig"], wake, "sleep must retain the wake config");
        assert!(fs::read_to_string(rig.root.join("candidate-out.log")).unwrap()
            .contains("observation budget ended; keeping wake config"));
    }
}

fn chatter_event(message_id: i64, text: &str) -> Value {
    json!({
        "time": now_ms() / 1000, "self_id": SELF_ID, "post_type": "message", "message_type": "group",
        "sub_type": "normal", "message_id": message_id, "group_id": GROUP_ID, "user_id": 1002,
        "sender": {"nickname": "chatter", "card": ""},
        "message": [{"type": "text", "data": {"text": text}}], "raw_message": text, "font": 0
    })
}

/// Engagement window: after her send, chatter returns one message at a time (not for her) and
/// never extends the window; its end is a clean sleep with exactly one terminal record.
#[test]
fn engage_window_end_is_a_clean_sleep_with_one_terminal_record() {
    let mut rig = Rig::new("engage-window", Opts { seed_session: None });
    rig.patch_config(|c| {
        c["socialV2"]["wake"]["preSleepWaitMs"] = json!(8000);
        c["socialV2"]["wait"] = json!({"minMs":100,"maxMs":120000,"minQuietAfterNewMs":0,
            "batchWindowMs":100,"batchMaxMs":100});
        c["socialV2"]["reply"] = json!({"quietMedianMs":1,"quietMinMs":1,"quietMaxMs":1});
        c["socialV2"]["send"]["firstGapEnabled"] = json!(false);
    });
    {
        let mut inner = lock(&rig.dsh.inner);
        inner.observation_waits = true;
        inner.engage_send = Some("我来啦");
        inner.close_turns = false;
        inner.plan = Plan { send: None, hold: false };
    }
    rig.start_candidate(mention(13701, "engage window"));
    assert!(wait_for(LONG, || !rig.onebot_sends().is_empty()), "her reply never went out");
    lock(&rig.gateway).frames.push(chatter_event(13702, "随便聊聊天气"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "{:?}", rig.turns());
    assert!(wait_for(Duration::from_secs(5), || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap()
        .contains("observation budget ended; keeping wake config")), "hub did not collect the sleeping turn");
    let sends = lock(&rig.dsh.inner).sends.clone();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, 200, "{sends:?}");
    let results = lock(&rig.dsh.inner).wait_results.clone();
    let last = &results.last().unwrap().1;
    assert_eq!(last["engagementEnded"], true, "{results:?}");
    // Chatter does not end an engaged wait; it returns with the window end, marked not for her.
    assert!(last["newMessages"].as_array().is_some_and(|m| m.len() == 1 && m[0]["forYou"] == false
        && m[0]["text"].as_str().is_some_and(|t| t.contains("随便聊聊天气"))),
        "chatter returns with the window end, marked not for her: {results:?}");
    assert!(results.iter().all(|(_, body)| body["waitLimitReached"] != true), "{results:?}");
    assert_eq!(last["nextAction"], "end_turn");
    let records: Vec<Value> = fs::read_to_string(rig.root.join("data/turn-ledger.jsonl")).unwrap().lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect();
    let ends: Vec<&Value> = records.iter().filter(|r| r["k"] == "ended").collect();
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["reason"], "observationSleep");
    let budget = records.iter().find(|r| r["k"] == "budget").expect("budget record");
    assert!(budget["capMs"].as_i64().is_some_and(|ms| ms > 0), "{budget}");
    assert!(budget["ceiling"].as_i64().is_some_and(|at| at >= budget["deadline"].as_i64().unwrap() + 1_000_000), "{budget}");
    assert_eq!(rig.dsh.count("session/cancel"), 0, "window end is never a cancellation");
    let state: Value = serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(state["conversations"][GROUP_KEY]["wakeConfig"]["mode"], "active", "wake config kept");
}

#[test]
fn a_capped_turn_is_cancelled_and_its_late_send_is_refused() {
    let mut rig = Rig::new("timeout", Opts { seed_session: None });
    rig.patch_config(|c| {
        c["socialV2"]["wake"]["preSleepWaitEnabled"] = json!(false);
        // The smallest valid cap: observation 1 + quiet extension (100 + 5000) + 30000 = 35101 ms.
        c["socialV2"]["wake"]["preSleepWaitMs"] = json!(1);
        c["socialV2"]["wait"] = json!({"minMs": 100, "maxMs": 100, "minQuietAfterNewMs": 0,
            "batchWindowMs":100,"batchMaxMs":100});
        c["socialV2"]["wake"]["turnHardCapMs"] = json!(36000);
    });
    rig.dsh.set_plan(Plan { send: Some("太迟了的回复"), hold: true });
    rig.restart(mention(11001, "会超时的回合"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "turn never reached DSH");
    let old_prompt = rig.dsh.applied()[0].text.clone();
    // The bridge gives up after the timeout: DSH work is stopped and the turn's token is revoked.
    assert!(wait_for(LONG, || rig.dsh.count("session/cancel") >= 1), "a timed-out turn must be cancelled in DSH");
    assert!(wait_for(LONG, || rig.turn_state(0) == "cancelled"), "{:?}", rig.turns());
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("data/logs/turn-timing.jsonl"))
        .unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|v| v["endKind"] == "aborted" && v["cancelled"] == true && v["cancelReason"] == "turnTimeout")),
        "hard-cap timing must identify the bridge cancellation");
    assert_eq!(rig.dsh.count("session/cancel"),1,"one hard cap must cause exactly one cancellation");
    assert_ne!(rig.agent_token(), SEED_TOKEN, "the abandoned attempt's token must be revoked");
    let late = rig.dsh.late_send(&old_prompt).expect("late send reached the console");
    assert!(matches!(late.0, 401 | 403), "the abandoned turn's late send must be refused: {late:?}");
    assert!(rig.onebot_sends().is_empty(), "nothing from the abandoned turn may reach QQ");
    assert_eq!(rig.dsh.applied().len(), 1, "the timed-out prompt is not submitted again");
    // The conversation keeps its session and works again with the new token.
    rig.dsh.set_plan(Plan { send: Some("之后的回复"), hold: false });
    rig.restart(mention(11002, "超时之后"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 2), "the next wake must run");
    assert!(wait_for(LONG, || rig.onebot_sends().len() == 1), "the next turn's reply is delivered");
    assert_eq!(rig.dsh.count("session/create"), 1, "context continues in the same session");
}

#[test]
fn a_capped_turn_ending_before_the_cancel_rpc_returns_is_cancelled_once() {
    let mut rig = Rig::new("cancel-end-interleaving", Opts { seed_session: None });
    rig.patch_config(|c| {
        c["socialV2"]["wake"]["preSleepWaitMs"] = json!(1);
        c["socialV2"]["wait"] = json!({"minMs": 100, "maxMs": 100, "minQuietAfterNewMs": 0,
            "batchWindowMs":100,"batchMaxMs":100});
        c["socialV2"]["wake"]["turnHardCapMs"] = json!(36000);
    });
    let (release, gate) = std::sync::mpsc::channel();
    lock(&rig.dsh.inner).cancel_reply_gate = Some(gate);
    rig.dsh.set_plan(Plan { send: Some("late reply"), hold: true });
    rig.restart(mention(11003, "cancel interleaving"));
    assert!(wait_for(LONG, || rig.dsh.count("session/cancel") == 1));
    // The collector must consume the aborted end while cancel_with_reason is still
    // waiting for its RPC response, before it can rotate authority and end the turn.
    let collected = wait_for(Duration::from_secs(10), || {
        fs::read_to_string(rig.root.join("data/logs/turn-timing.jsonl"))
            .unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|row| row["endKind"] == "aborted" && row["cancelReason"] == "turnTimeout")
    });
    let ledger = rig.root.join("data/turn-ledger.jsonl");
    let before_reply = fs::read_to_string(&ledger).unwrap();
    release.send(()).unwrap();
    assert!(collected, "collector did not reach the forced interleaving");
    assert!(!before_reply.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| row["k"] == "ended"), "cancellation must revoke authority before finalizing: {before_reply}");
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log"))
        .unwrap_or_default().contains("[reserved2] group:420042 turn ended:")));
    let records: Vec<Value> = fs::read_to_string(&ledger).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    let ends: Vec<_> = records.iter().filter(|row| row["k"] == "ended").collect();
    assert_eq!(ends.len(), 1, "{records:?}");
    assert_eq!(ends[0]["outcome"], "cancelled");
    assert_eq!(ends[0]["reason"], "turnTimeout");
    assert_eq!(rig.turn_state(0), "cancelled");
    let output = fs::read_to_string(rig.root.join("candidate-out.log")).unwrap();
    assert!(output.contains("turn ended: Err("), "{output}");
    let late = rig.dsh.late_send(&rig.dsh.applied()[0].text).unwrap();
    assert!(matches!(late.0, 401 | 403), "{late:?}");
    assert!(rig.onebot_sends().is_empty());
}

// ───────────────────────── AC2/AC3/AC5: restart reconciliation ─────────────────────────

#[test]
fn a_completed_end_observed_before_the_cap_keeps_completion_while_bookkeeping_waits() {
    let mut rig = Rig::new("completed-before-cap", Opts { seed_session: None });
    rig.patch_config(|c| {
        c["socialV2"]["wake"]["preSleepWaitMs"] = json!(1);
        c["socialV2"]["wait"] = json!({"minMs": 100, "maxMs": 100, "minQuietAfterNewMs": 0,
            "batchWindowMs":100,"batchMaxMs":100});
        c["socialV2"]["wake"]["turnHardCapMs"] = json!(36000);
    });
    let hold = rig.root.join("data/fault-hold-collector-bookkeeping");
    fs::write(&hold, "hold").unwrap();
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.restart(mention(11005, "complete before the cap"));
    assert!(wait_for(LONG, || rig.root.join("data/collector-bookkeeping-ready").exists()),
        "collector did not observe the completed end");
    let ledger = rig.root.join("data/turn-ledger.jsonl");
    let before_cap = fs::read_to_string(&ledger).unwrap();
    assert!(!before_cap.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| row["k"] == "ended" || row["verdict"] == "cancel-requested"), "{before_cap}");
    assert_eq!(rig.dsh.count("session/cancel"), 0, "completion was observed before cancellation intent");
    assert!(wait_for(LONG, || rig.dsh.count("session/cancel") == 1), "the cap never fired");
    // Token rotation proves the independent cap handler has passed its intent and RPC.
    assert!(wait_for(LONG, || rig.agent_token() != SEED_TOKEN));
    let while_held = fs::read_to_string(&ledger).unwrap();
    fs::remove_file(hold).unwrap();
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log"))
        .unwrap_or_default().contains("[reserved2] group:420042 turn ended:")));
    assert!(!while_held.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| row["k"] == "ended"), "completion's terminal write is still pending: {while_held}");
    let records: Vec<Value> = fs::read_to_string(&ledger).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    let ends: Vec<_> = records.iter().filter(|row| row["k"] == "ended").collect();
    assert_eq!(ends.len(), 1, "{records:?}");
    assert_eq!(ends[0]["outcome"], "completed", "{records:?}");
    assert_eq!(rig.turn_state(0), "completed");
    let timing = fs::read_to_string(rig.root.join("data/logs/turn-timing.jsonl")).unwrap();
    assert!(timing.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| row["endKind"] == "completed" && row["cancelled"] == false && row["ok"] == true), "{timing}");
    let output = fs::read_to_string(rig.root.join("candidate-out.log")).unwrap();
    assert!(output.contains("turn ended: Ok("), "{output}");
    assert_eq!(rig.dsh.applied().len(), 1);
}

#[test]
fn a_completed_end_keeps_its_outcome_when_the_bookkeeping_barrier_path_is_invalid() {
    let mut rig = Rig::new("completed-invalid-bookkeeping", Opts { seed_session: None });
    // A directory at the optional file's location deterministically triggers the same
    // path error as removing the hold file between its metadata check and read.
    fs::create_dir(rig.root.join("data/fault-hold-collector-bookkeeping")).unwrap();
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.restart(mention(11006, "completed with an unavailable bookkeeping barrier"));
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log"))
        .unwrap_or_default().contains("[reserved2] group:420042 turn ended:")));
    let records: Vec<Value> = fs::read_to_string(rig.root.join("data/turn-ledger.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(records.iter().any(|row| row["verdict"] == "completion-observed"), "{records:?}");
    let ends: Vec<_> = records.iter().filter(|row| row["k"] == "ended").collect();
    assert_eq!(ends.len(), 1, "{records:?}");
    assert_eq!(ends[0]["outcome"], "completed", "{records:?}");
    assert_eq!(rig.turn_state(0), "completed");
    let timing = fs::read_to_string(rig.root.join("data/logs/turn-timing.jsonl")).unwrap();
    assert!(timing.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| row["endKind"] == "completed" && row["cancelled"] == false && row["ok"] == true), "{timing}");
    let output = fs::read_to_string(rig.root.join("candidate-out.log")).unwrap();
    assert!(output.contains("turn ended: Ok("), "{output}");
    let diagnostic = fs::read_to_string(rig.root.join("candidate-err.log")).unwrap();
    assert!(diagnostic.lines().any(|line| line == "[collector] bookkeeping barrier unavailable; continuing completion classification"), "{diagnostic}");
    assert!(!diagnostic.contains(rig.root.to_str().unwrap()), "{diagnostic}");
    assert!(!diagnostic.contains(SEED_TOKEN), "{diagnostic}");
    assert_eq!(rig.dsh.applied().len(), 1);
    assert!(rig.onebot_sends().is_empty());
}

fn restart_with_pending_cancellation(queued: bool, unreachable: bool) {
    let mut rig = Rig::new("pending-cancel-restart", Opts { seed_session: Some("js-origin-session") });
    if queued {
        rig.dsh.set_fault(Fault::HangWithoutApply);
        rig.restart(mention(11004, "queued cancellation"));
        assert!(wait_for(LONG, || rig.dsh.received() == 1));
        let prompt = lock(&rig.dsh.inner).prompts[0].clone();
        lock(&rig.dsh.inner).sessions.get_mut(&prompt.session).unwrap().inbox.push(
            json!({"id":"pending-prompt", "source":{"rpcId":prompt.rpc_id}}));
    } else {
        running_turn(&mut rig, 11004);
    }
    rig.kill_candidate();
    let prompt = lock(&rig.dsh.inner).prompts[0].clone();
    let ledger = rig.root.join("data/turn-ledger.jsonl");
    let records: Vec<Value> = fs::read_to_string(&ledger).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    let turn = records.iter().find(|row| row["k"] == "begun").unwrap()["turn"].clone();
    // Crash image: intent is durable, but the cancelling process never rotated the token.
    writeln!(fs::OpenOptions::new().append(true).open(&ledger).unwrap(), "{}",
        json!({"k":"verified", "turn":turn, "t":now_ms(), "verdict":"cancel-requested", "detail":"turnTimeout"})).unwrap();
    assert_eq!(rig.agent_token(), SEED_TOKEN);
    rig.dsh.set_down(unreachable);
    rig.start_candidate(vec![]);
    assert!(wait_for(LONG, || {
        let turns = rig.turns();
        turns.first().is_some_and(|t| if unreachable {
            t["state"] == "paused" && t["pauseReason"] == "dsh-unreachable"
        } else { t["reattaching"] == true })
    }), "recovery did not reach the probe's wait path: {}", rig.view());
    assert_ne!(rig.agent_token(), SEED_TOKEN, "pending cancellation must revoke authority before waiting");
    let late = rig.dsh.late_send(&prompt.text).unwrap();
    assert!(matches!(late.0, 401 | 403), "{late:?}");
    let records: Vec<Value> = fs::read_to_string(&ledger).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(!records.iter().any(|row| row["k"] == "ended"), "unproven work must stay open: {records:?}");
    assert_eq!(rig.dsh.received(), 1, "the cancelled input must not be replayed");
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn pending_cancellation_revokes_authority_before_running_reattach() {
    restart_with_pending_cancellation(false, false);
}

#[test]
fn pending_cancellation_revokes_authority_before_queued_reattach() {
    restart_with_pending_cancellation(true, false);
}

#[test]
fn pending_cancellation_revokes_authority_before_unreachable_defer() {
    restart_with_pending_cancellation(false, true);
}

fn running_turn(rig: &mut Rig, id: i64) {
    rig.dsh.set_plan(Plan { send: Some("回复一次"), hold: true });
    rig.restart(mention(id, "会被打断的回合"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "turn never reached DSH");
    assert!(wait_for(LONG, || rig.turn_state(0) == "running"), "{:?}", rig.turns());
}

#[test]
fn crash_after_dsh_finished_the_turn_does_not_redo_it() {
    let mut rig = Rig::new("crash-done", Opts { seed_session: None });
    running_turn(&mut rig, 4001);
    rig.kill_candidate();
    // DSH finishes the turn while the bridge is down: the model sends through the public route.
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.onebot_sends().len() == 1), "the model's send did not reach QQ");
    rig.restart(vec![]);
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "recovery must close the turn: {:?}", rig.turns());
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.applied().len(), 1, "a finished turn must not be submitted again");
    assert_eq!(rig.onebot_sends().len(), 1, "nothing may be sent twice");
    let turn = &rig.turns()[0];
    assert_eq!(turn["sends"][0]["sent"], json!(1), "send results must be linked to the turn: {turn}");
    assert_eq!(turn["tools"][0]["name"], json!("mcp__snowluma__qq_send_message"));
    let stages = rig.inbound();
    assert_eq!(stages["interrupted"], json!([]), "the interrupted event is settled: {stages}");
    assert_eq!(stages["stages"]["handled"].as_i64(), Some(1), "{stages}");
}

#[test]
fn crash_while_dsh_is_still_running_reattaches_and_holds_new_wakes() {
    let mut rig = Rig::new("crash-running", Opts { seed_session: None });
    running_turn(&mut rig, 5001);
    rig.kill_candidate();
    rig.start_candidate(mention(5002, "恢复期间的新消息"));
    assert!(
        wait_for(LONG, || rig.turns().first().map(|t| t["reattaching"] == json!(true)).unwrap_or(false)),
        "recovery must re-attach to the running turn: {:?}",
        rig.turns()
    );
    // While recovering, the conversation starts no second turn.
    thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.dsh.applied().len(), 1, "no new prompt while the old turn is still in DSH");
    assert_eq!(rig.dsh.count("session/create"), 1, "the DSH session is never rebuilt");
    // DSH finishes; recovery closes the turn without resubmitting it, then the held wake runs.
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "{:?}", rig.turns());
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 2), "the held wake must run after recovery");
    let applied = rig.dsh.applied();
    assert_ne!(applied[0].rpc_id, applied[1].rpc_id);
    assert_eq!(applied[0].session, applied[1].session, "context continues in the same session");
    assert_eq!(rig.dsh.count("session/create"), 1);
}

#[test]
fn crash_before_dsh_applied_the_prompt_resumes_exactly_once() {
    let mut rig = Rig::new("crash-unapplied", Opts { seed_session: None });
    rig.dsh.set_fault(Fault::HangWithoutApply);
    rig.restart(mention(6001, "没送到的提交"));
    assert!(wait_for(LONG, || rig.dsh.received() == 1), "prompt request never reached DSH");
    rig.kill_candidate();
    rig.dsh.set_fault(Fault::None);
    rig.dsh.set_plan(Plan { send: Some("恢复后的回复"), hold: false });
    // Prompt acceptance precedes execution; exercise a start slower than the old four-second wait.
    lock(&rig.dsh.inner).turn_start_delay_ms = 6000;
    rig.start_candidate(vec![]);
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "the never-applied prompt must be run after restart");
    assert!(wait_for(LONG, || !rig.onebot_sends().is_empty()), "the resumed turn's reply never reached QQ");
    assert!(wait_for(LONG, || rig.turn_state(1) == "completed"), "the resumed turn did not settle: {:?}", rig.turns());
    assert!(!wait_for(Duration::from_secs(4), || rig.onebot_sends().len() > 1), "nothing may be sent twice after settlement");
    assert_eq!(rig.dsh.applied().len(), 1, "exactly one prompt may be applied in total");
    assert_eq!(rig.onebot_sends().len(), 1);
    let turns = rig.turns();
    assert_eq!(turns[0]["state"], json!("resumed"), "{turns:?}");
    assert_eq!(turns[1]["state"], json!("completed"), "{turns:?}");
}

#[test]
fn crash_after_dsh_applied_but_the_reply_was_lost_is_found_by_rpc_id() {
    let mut rig = Rig::new("crash-lost-reply", Opts { seed_session: None });
    rig.dsh.set_fault(Fault::ApplyThenHang);
    rig.restart(mention(7001, "回复丢失"));
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "prompt never reached DSH");
    assert!(wait_for(LONG, || rig.onebot_sends().len() == 1), "the simulated model should have sent once");
    rig.kill_candidate();
    rig.dsh.set_fault(Fault::None);
    rig.start_candidate(vec![]);
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "{:?}", rig.turns());
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.applied().len(), 1, "the applied prompt must not be resubmitted");
    assert_eq!(rig.onebot_sends().len(), 1);
}

#[test]
fn an_unreachable_dsh_pauses_the_turn_and_recovery_continues_when_it_returns() {
    let mut rig = Rig::new("unreachable", Opts { seed_session: None });
    running_turn(&mut rig, 8001);
    rig.kill_candidate();
    rig.dsh.set_down(true);
    rig.start_candidate(vec![]);
    assert!(
        wait_for(LONG, || rig.view()["paused"].as_array().map(|p| p.iter().any(|t| t["reason"] == json!("dsh-unreachable"))).unwrap_or(false)),
        "an unverifiable turn must be reported as paused with its reason: {}",
        rig.view()
    );
    assert_eq!(rig.dsh.applied().len(), 1, "nothing is resubmitted while DSH cannot be asked");
    rig.dsh.set_down(false);
    rig.dsh.release();
    assert!(wait_for(Duration::from_secs(60), || rig.turn_state(0) == "completed"), "recovery must verify and close: {:?}", rig.turns());
    assert_eq!(rig.dsh.applied().len(), 1);
    assert_eq!(rig.onebot_sends().len(), 1);
}

fn paused_turn(rig: &mut Rig, id: i64) -> String {
    rig.dsh.set_fault(Fault::HangWithoutApply);
    rig.restart(mention(id, "无法核实"));
    assert!(wait_for(LONG, || rig.dsh.received() == 1), "prompt request never reached DSH");
    rig.kill_candidate();
    rig.dsh.set_fault(Fault::None);
    rig.dsh.set_truncate(true); // the DSH log is cut off: absence cannot be proven
    rig.dsh.set_plan(Plan { send: Some("管理员决定之后"), hold: false });
    rig.start_candidate(mention(id + 1, "暂停期间到来"));
    assert!(
        wait_for(LONG, || rig.view()["pausedCount"].as_i64() == Some(1)),
        "an unprovable turn must pause: {}",
        rig.view()
    );
    rig.view()["paused"][0]["turnId"].as_str().unwrap_or("").to_string()
}

fn seed_paused(rig: &Rig, key: &str, reason: &str, at: i64) {
    let records = [
        json!({"k":"begun","turn":"turn-hf84-synthetic","t":at-600_000,"key":key,"epoch":"synthetic","inbound":[]}),
        json!({"k":"unknown","turn":"turn-hf84-synthetic","t":at,"reason":reason,"detail":"synthetic pause"}),
    ];
    fs::write(
        rig.root.join("data/turn-ledger.jsonl"),
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
}

fn owner_messages(rig: &Rig) -> Vec<String> {
    lock(&rig.gateway)
        .requests
        .iter()
        .filter(|(path, body)| path == "/send_private_msg" && body["user_id"] == json!(8808))
        .map(|(_, body)| body["message"].to_string())
        .collect()
}

fn turn_messages(rig: &Rig, code: &str) -> Vec<String> {
    owner_messages(rig)
        .into_iter()
        .filter(|message| message.contains(code))
        .collect()
}

fn private_message(user: i64, id: i64, text: &str) -> Value {
    json!({"time":now_ms()/1000,"self_id":SELF_ID,"post_type":"message","message_type":"private",
        "sub_type":"friend","message_id":id,"user_id":user,"sender":{"user_id":user,"nickname":"tester"},
        "message":[{"type":"text","data":{"text":text}}],"raw_message":text})
}

fn restart_with_owner_route(rig: &mut Rig, script: Vec<Step>) {
    rig.kill_candidate();
    rig.start_candidate_groups(script, "group:420042,private:8808");
}

#[test]
fn hf84_owner_private_commands_resolve_before_model_dispatch() {
    for (index, command) in ["放弃", "重试", "abandon", "retry"].iter().enumerate() {
        let mut rig = Rig::new(&format!("hf84-owner-{index}"), Opts { seed_session: None });
        rig.patch_config(|c| {
            c["socialV2"]["pausedTurnAutoAbandonMinutes"] = json!(0);
            c["allow"]["private"] = json!([8808]);
        });
        let id = paused_turn(&mut rig, 84_100 + index as i64 * 10);
        assert!(
            wait_for(LONG, || rig.inbound()["stages"]["handled"]
                .as_i64()
                .unwrap_or(0)
                >= 1),
            "paused input must be durably held before restart"
        );
        let original = rig.turns().into_iter().find(|t| t["turnId"] == id).unwrap();
        let code = original["code"].as_str().unwrap();
        let input = format!("{command} {code}");
        restart_with_owner_route(
            &mut rig,
            vec![Step::Frame(private_message(8808, 84_500, &input))],
        );
        assert!(
            wait_for(Duration::from_secs(10), || rig.turn_state(0) == "resolved"),
            "owner command was not consumed: {input}, {}",
            rig.view()
        );
        let action = if matches!(*command, "重试" | "retry") {
            "retry"
        } else {
            "abandon"
        };
        assert_eq!(rig.turns()[0]["resolution"], action);
        assert_eq!(rig.turns()[0]["resolutionBy"], "chat");
        assert!(
            wait_for(LONG, || !rig.dsh.applied().is_empty()),
            "held/retried input was not released"
        );
        assert!(
            wait_for(Duration::from_secs(6), || turn_messages(&rig, code)
                .iter()
                .any(|m| m.contains("已处理") && m.contains("chat"))),
            "chat resolution notice missing"
        );
        thread::sleep(Duration::from_secs(2));
        let turns = rig.turns();
        assert!(
            turns.iter().all(|t| t["key"] != "private:8808"),
            "the command must start no owner model turn"
        );
        let original_input = original["inbound"].as_array().unwrap();
        let retried = turns
            .iter()
            .skip(1)
            .filter(|t| {
                t["inbound"]
                    .as_array()
                    .is_some_and(|events| events.iter().any(|event| original_input.contains(event)))
            })
            .count();
        assert_eq!(
            retried,
            usize::from(action == "retry"),
            "only explicit retry returns the old input"
        );
        assert!(
            rig.dsh.applied().iter().all(|p| !p.text.contains(&input)),
            "owner command leaked into a prompt"
        );
        assert_eq!(
            turn_messages(&rig, code).len(),
            2,
            "one pause alert and one chat resolution notice"
        );
    }
}

#[test]
fn hf84_unknown_send_retry_requires_confirmation_and_stale_codes_stay_out_of_models() {
    for (index, confirmation) in ["确认", "confirm"].iter().enumerate() {
        let mut rig = Rig::new(
            &format!("hf84-confirm-{index}"),
            Opts { seed_session: None },
        );
        rig.patch_config(|c| {
            c["socialV2"]["pausedTurnAutoAbandonMinutes"] = json!(0);
            c["allow"]["private"] = json!([8808]);
        });
        seed_paused(&rig, GROUP_KEY, "send-unresolved", now_ms());
        let code = rig.turns()[0]["code"].as_str().unwrap().to_string();
        let retry = format!("重试 {code}");
        rig.start_candidate_groups(
            vec![Step::Frame(private_message(8808, 84_600, &retry))],
            "group:420042,private:8808",
        );
        let request = format!("重试 {code} 确认");
        assert!(
            wait_for(Duration::from_secs(10), || owner_messages(&rig)
                .iter()
                .any(|m| m.contains(&request) && m.contains("重复"))),
            "explicit duplicate-send confirmation request missing: {:?}",
            owner_messages(&rig)
        );
        assert_eq!(rig.turn_state(0), "paused");
        assert_eq!(
            rig.agent_token(),
            SEED_TOKEN,
            "an unconfirmed retry must not change authority"
        );
        restart_with_owner_route(
            &mut rig,
            vec![Step::Frame(private_message(
                8808,
                84_601,
                &format!("retry {code} {confirmation}"),
            ))],
        );
        assert!(
            wait_for(Duration::from_secs(10), || rig.turn_state(0) == "resolved"),
            "confirmed retry was not resolved"
        );
        assert_eq!(rig.turns()[0]["resolution"], "retry");
        assert_eq!(rig.turns()[0]["resolutionBy"], "chat");
        assert!(wait_for(Duration::from_secs(6), || turn_messages(
            &rig, &code
        )
        .iter()
        .any(|m| m.contains("已处理") && m.contains("chat"))));
        let before = owner_messages(&rig).len();
        restart_with_owner_route(
            &mut rig,
            vec![Step::Frame(private_message(
                8808,
                84_602,
                &format!("放弃 {code}"),
            ))],
        );
        assert!(
            wait_for(Duration::from_secs(6), || owner_messages(&rig)
                .iter()
                .skip(before)
                .any(|m| m.contains("已失效"))),
            "stale code needs a short reply"
        );
        let before = owner_messages(&rig).len();
        restart_with_owner_route(
            &mut rig,
            vec![Step::Frame(private_message(
                8808,
                84_603,
                "abandon 0badc0de",
            ))],
        );
        assert!(
            wait_for(Duration::from_secs(6), || owner_messages(&rig)
                .iter()
                .skip(before)
                .any(|m| m.contains("回合码不存在"))),
            "unknown code needs a short reply"
        );
        assert_eq!(
            rig.dsh.received(),
            0,
            "commands and confirmation must never reach a model"
        );
        assert!(rig.turns().iter().all(|t| t["key"] != "private:8808"));
        let (status, state) = rig.console("GET", "/api/socialV2/state?key=private:8808", None);
        assert_eq!(status, 200, "{state}");
        assert_eq!(
            state["recentCount"], 0,
            "command text must not enter chat history"
        );
        assert_eq!(
            state["unreadCount"], 0,
            "command text must not enter model-readable unread"
        );
        assert_eq!(
            turn_messages(&rig, &code)
                .iter()
                .filter(|m| m.contains("回合暂停"))
                .count(),
            1
        );
        assert_eq!(
            turn_messages(&rig, &code)
                .iter()
                .filter(|m| m.contains("已处理"))
                .count(),
            1
        );
    }
}

#[test]
fn hf84_auto_abandon_waits_until_timeout_revokes_unknown_sends_and_notifies_once() {
    let mut rig = Rig::new("hf84-auto", Opts { seed_session: None });
    let origin = now_ms();
    rig.patch_config(|c| c["socialV2"]["pausedTurnAutoAbandonMinutes"] = json!(1));
    seed_paused(&rig, GROUP_KEY, "send-unresolved", origin);
    let mut ledger = fs::OpenOptions::new()
        .append(true)
        .open(rig.root.join("data/turn-ledger.jsonl"))
        .unwrap();
    writeln!(ledger, "{}", json!({"k":"unknown","turn":"turn-hf84-synthetic","t":origin+49_000,"reason":"send-unresolved","detail":"probe repeated"})).unwrap();
    drop(ledger);
    rig.epoch = Some(origin + 50_000);
    rig.start_candidate(vec![]);
    assert!(
        wait_for(Duration::from_secs(5), || owner_messages(&rig)
            .iter()
            .any(|m| m.contains("放弃") && m.contains("重试"))),
        "pause alert missing"
    );
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        rig.turn_state(0),
        "paused",
        "must not abandon before one minute since pause"
    );
    assert_eq!(rig.agent_token(), SEED_TOKEN, "no revocation before expiry");
    assert!(
        wait_for(Duration::from_secs(12), || rig.turn_state(0) == "resolved"),
        "timeout must abandon: {}",
        rig.view()
    );
    assert_ne!(
        rig.agent_token(),
        SEED_TOKEN,
        "auto abandon revokes send-unresolved authority"
    );
    assert_eq!(rig.turns()[0]["resolution"], "abandon");
    assert_eq!(rig.turns()[0]["resolutionBy"], "auto");
    let turn = rig.turns().remove(0);
    let code = turn["code"].as_str().expect("short turn code");
    assert!(
        wait_for(Duration::from_secs(5), || turn_messages(&rig, code).len()
            == 2),
        "resolution notice missing: {:?}",
        owner_messages(&rig)
    );
    let messages = turn_messages(&rig, code);
    assert_eq!(code.len(), 8);
    assert_eq!(
        turn["pausedAt"],
        json!(origin),
        "repeated probes preserve first pause time"
    );
    assert!(messages[0].contains(code));
    assert!(
        regex::Regex::new(r"\d{4}/\d{1,2}/\d{1,2} \d{2}:\d{2}:\d{2}")
            .unwrap()
            .is_match(&messages[0]),
        "human-readable expiry time: {}",
        messages[0]
    );
    assert!(messages[0].contains(GROUP_KEY) && messages[0].contains("send-unresolved"));
    assert!(messages[0].contains("自动放弃") && messages[1].contains("auto"));
    rig.restart(vec![]);
    thread::sleep(Duration::from_secs(4));
    assert_eq!(
        turn_messages(&rig, code).len(),
        2,
        "restart must not repeat either notice"
    );
    assert_eq!(rig.dsh.received(), 0, "expiry never retries");
}

#[test]
fn hf84_default_expiry_is_ten_minutes_from_the_pause() {
    let mut rig = Rig::new("hf84-default", Opts { seed_session: None });
    let origin = now_ms();
    seed_paused(&rig, GROUP_KEY, "dsh-history-incomplete", origin);
    let code = rig.turns()[0]["code"].as_str().unwrap().to_string();
    rig.epoch = Some(origin + 590_000);
    rig.start_candidate(vec![]);
    assert!(wait_for(Duration::from_secs(5), || !turn_messages(
        &rig, &code
    )
    .is_empty()));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        rig.turn_state(0),
        "paused",
        "default must not expire before ten minutes"
    );
    assert!(
        wait_for(Duration::from_secs(12), || rig.turn_state(0) == "resolved"),
        "default ten-minute expiry missing"
    );
    assert_eq!(rig.turns()[0]["resolution"], "abandon");
    assert_eq!(rig.turns()[0]["resolutionBy"], "auto");
    assert_eq!(rig.dsh.received(), 0, "expiry must never retry");
}

#[test]
fn hf84_zero_disables_expiry_and_console_resolution_is_notified_once() {
    let mut rig = Rig::new("hf84-disabled", Opts { seed_session: None });
    let origin = now_ms();
    rig.patch_config(|c| c["socialV2"]["pausedTurnAutoAbandonMinutes"] = json!(0));
    seed_paused(&rig, GROUP_KEY, "send-unresolved", origin);
    let code = rig.turns()[0]["code"].as_str().unwrap().to_string();
    rig.epoch = Some(origin + 3_600_000);
    rig.start_candidate(vec![]);
    assert!(wait_for(Duration::from_secs(5), || turn_messages(
        &rig, &code
    )
    .iter()
    .any(|m| m.contains("自动放弃已禁用"))));
    thread::sleep(Duration::from_secs(4));
    assert_eq!(
        rig.turn_state(0),
        "paused",
        "zero must disable expiry even after an hour"
    );
    assert_eq!(rig.agent_token(), SEED_TOKEN);
    let (status, body) = rig.console(
        "POST",
        "/api/turns/resolve",
        Some(&json!({"turnId":"turn-hf84-synthetic", "action":"abandon"})),
    );
    assert_eq!(status, 200, "{body}");
    assert_ne!(
        rig.agent_token(),
        SEED_TOKEN,
        "console abandon still revokes unknown-send authority"
    );
    assert_eq!(rig.turns()[0]["resolutionBy"], "console");
    assert!(wait_for(Duration::from_secs(6), || turn_messages(
        &rig, &code
    )
    .iter()
    .any(|m| m.contains("已处理") && m.contains("console"))));
    rig.restart(vec![]);
    thread::sleep(Duration::from_secs(4));
    assert_eq!(
        turn_messages(&rig, &code).len(),
        2,
        "console resolution must not repeat notices after restart"
    );
    assert_eq!(rig.dsh.received(), 0);
}

#[test]
fn hf84_commands_bypass_a_paused_owner_conversation_in_every_mode() {
    for mode in ["chat", "closed-agent", "reserved", "reserved2"] {
        let mut rig = Rig::new(&format!("hf84-mode-{mode}"), Opts { seed_session: None });
        rig.patch_config(|c| {
            c["socialV2"]["pausedTurnAutoAbandonMinutes"] = json!(0);
            c["allow"]["private"] = json!([8808]);
        });
        fs::write(
            rig.root.join("data/mode.json"),
            json!({"mode":mode}).to_string(),
        )
        .unwrap();
        seed_paused(&rig, "private:8808", "dsh-history-incomplete", now_ms());
        let code = rig.turns()[0]["code"].as_str().unwrap().to_string();
        let command = private_message(8808, 84_700, &format!("abandon {code}"));
        rig.start_candidate_groups(
            vec![Step::Frame(command.clone())],
            "group:420042,private:8808",
        );
        assert!(
            wait_for(Duration::from_secs(10), || rig.turn_state(0) == "resolved"),
            "paused owner command failed in {mode}"
        );
        assert_eq!(rig.turns()[0]["resolutionBy"], "chat");
        assert!(
            wait_for(Duration::from_secs(6), || turn_messages(&rig, &code).len()
                == 2),
            "notice missing in {mode}"
        );
        restart_with_owner_route(&mut rig, vec![Step::Frame(command)]);
        thread::sleep(Duration::from_secs(3));
        assert_eq!(
            turn_messages(&rig, &code).len(),
            2,
            "a duplicate command must stay consumed in {mode}"
        );
        assert_eq!(
            rig.turns().len(),
            1,
            "command created a model turn in {mode}"
        );
        assert_eq!(rig.dsh.received(), 0, "command reached DSH in {mode}");
        let (status, state) = rig.console("GET", "/api/socialV2/state?key=private:8808", None);
        assert_eq!(status, 200, "{state}");
        assert_eq!(state["recentCount"], 0, "command entered history in {mode}");
        assert_eq!(state["unreadCount"], 0, "command entered unread in {mode}");
    }
}

#[test]
fn hf84_non_owner_private_and_owner_group_commands_are_ordinary_model_input() {
    let mut rig = Rig::new("hf84-ordinary", Opts { seed_session: None });
    lock(&rig.dsh.inner).chat_preset = true;
    rig.patch_config(|c| {
        c["socialV2"]["pausedTurnAutoAbandonMinutes"] = json!(0);
        c["allow"]["private"] = json!([7707]);
        c["allow"]["groups"] = json!([GROUP_ID, GROUP_ID + 1]);
    });
    fs::write(
        rig.root.join("data/mode.json"),
        json!({"mode":"chat"}).to_string(),
    )
    .unwrap();
    seed_paused(&rig, GROUP_KEY, "dsh-history-incomplete", now_ms());
    let code = rig.turns()[0]["code"].as_str().unwrap().to_string();
    let input = format!("放弃 {code}");
    let mut group = mention_event(84_801, &input);
    group["group_id"] = json!(GROUP_ID + 1);
    group["user_id"] = json!(8808);
    group["sender"] = json!({"user_id":8808,"nickname":"owner"});
    rig.dsh.set_plan(Plan {
        send: None,
        hold: false,
    });
    rig.start_candidate_groups(
        vec![
            Step::Frame(private_message(7707, 84_800, &input)),
            Step::Frame(group),
        ],
        "group:420042,group:420043,private:7707",
    );
    assert!(
        wait_for(Duration::from_secs(12), || rig
            .dsh
            .applied()
            .iter()
            .filter(|p| p.text.contains(&input))
            .count()
            == 2),
        "ordinary command text did not reach both model sessions: {:?}",
        rig.dsh.applied()
    );
    assert_eq!(
        rig.turn_state(0),
        "paused",
        "neither non-owner nor owner-in-group may resolve the turn"
    );
    assert!(rig.turns().iter().any(|t| t["key"] == "private:7707"));
    assert!(rig.turns().iter().any(|t| t["key"] == "group:420043"));
    assert!(!turn_messages(&rig, &code)
        .iter()
        .any(|m| m.contains("已处理")));
}

#[test]
fn owner_private_text_that_only_starts_with_a_recovery_verb_is_ordinary_model_input() {
    let mut rig = Rig::new("hf84-verb-words", Opts { seed_session: None });
    lock(&rig.dsh.inner).chat_preset = true;
    rig.patch_config(|c| {
        c["socialV2"]["pausedTurnAutoAbandonMinutes"] = json!(0);
        c["allow"]["private"] = json!([8808]);
    });
    fs::write(
        rig.root.join("data/mode.json"),
        json!({"mode":"chat"}).to_string(),
    )
    .unwrap();
    seed_paused(&rig, GROUP_KEY, "dsh-history-incomplete", now_ms());
    let code = rig.turns()[0]["code"].as_str().unwrap().to_string();
    let inputs = [
        "retry later please".to_string(),
        "放弃 吧".to_string(),
        "重试 一下".to_string(),
        "abandon unknown84".to_string(),
        "重试 20261109 的任务".to_string(),
        format!("retry {code} 吗"),
        format!("放弃 {code} 吧"),
    ];
    rig.dsh.set_plan(Plan {
        send: None,
        hold: false,
    });
    rig.start_candidate_groups(
        inputs
            .iter()
            .enumerate()
            .map(|(i, text)| Step::Frame(private_message(8808, 84_900 + i as i64, text)))
            .collect(),
        "group:420042,private:8808",
    );
    assert!(
        wait_for(Duration::from_secs(20), || {
            let applied = rig.dsh.applied();
            inputs
                .iter()
                .all(|input| applied.iter().any(|p| p.text.contains(input.as_str())))
        }),
        "ordinary owner text did not reach the model: {:?}",
        rig.dsh.applied()
    );
    assert_eq!(rig.turn_state(0), "paused", "no recovery command ran");
    assert!(!owner_messages(&rig)
        .iter()
        .any(|m| m.contains("已处理") || m.contains("不存在") || m.contains("已失效") || m.contains("用法")));
    assert!(!turn_messages(&rig, &code).iter().any(|m| m.contains("已处理")));
}

#[test]
fn an_unprovable_turn_is_paused_explained_and_its_late_results_are_refused() {
    let mut rig = Rig::new("paused", Opts { seed_session: None });
    let first_token = SEED_TOKEN;
    let id = paused_turn(&mut rig, 9001);
    let view = rig.view();
    assert_eq!(view["paused"][0]["reason"], json!("dsh-history-incomplete"));
    assert!(!view["paused"][0]["detail"].as_str().unwrap_or("").is_empty(), "the administrator needs a reason");
    thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.dsh.applied().len(), 0, "an unknown turn is never resubmitted, nor is the conversation rebuilt");
    assert_eq!(rig.dsh.count("session/create"), 1);
    assert_eq!(rig.inbound()["interrupted"].as_array().map(Vec::len), Some(1), "the event stays reported: {}", rig.inbound());
    // The paused turn's token is dead: whatever it still tries to do is refused.
    assert_ne!(rig.agent_token(), first_token);
    let late = http_call(
        rig.console_port, "POST", "/api/socialV2/send-message",
        &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", first_token)],
        Some(&json!({"key": GROUP_KEY, "messages": ["迟到"]})),
    ).expect("answered");
    assert!(late.0 == 401 || late.0 == 403, "{late:?}");
    assert!(rig.onebot_sends().is_empty());
    // A wake arriving meanwhile is held, not run.
    assert_eq!(rig.dsh.received(), 1);
    let (status, _) = rig.console("POST", "/api/turns/resolve", Some(&json!({"turnId": id, "action": "abandon"})));
    assert_eq!(status, 200);
    // Abandon closes the old event; the held wake then runs once in the same session.
    assert!(wait_for(LONG, || rig.dsh.applied().len() == 1), "held wake must run after the decision: {}", rig.view());
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.applied().len(), 1, "abandon must not re-run the old turn");
}

#[test]
fn admin_retry_of_a_paused_turn_runs_it_once() {
    let mut rig = Rig::new("retry", Opts { seed_session: None });
    let id = paused_turn(&mut rig, 9101);
    let (status, _) = rig.console("POST", "/api/turns/resolve", Some(&json!({"turnId": id, "action": "retry"})));
    assert_eq!(status, 200);
    assert!(wait_for(LONG, || rig.dsh.applied().len() >= 1), "retry must run the turn: {}", rig.view());
    thread::sleep(Duration::from_secs(5));
    let applied = rig.dsh.applied();
    let texts: HashSet<&str> = applied.iter().map(|p| p.text.as_str()).collect();
    assert!(applied.len() <= 2, "the retried and the held wake run at most once each: {applied:?}");
    assert!(!texts.is_empty());
    let (again, _) = rig.console("POST", "/api/turns/resolve", Some(&json!({"turnId": id, "action": "retry"})));
    assert_eq!(again, 409, "a settled turn cannot be resolved twice");
}

#[test]
fn an_unknown_send_result_pauses_the_finished_turn_and_is_never_resent() {
    let mut rig = Rig::new("send-unknown", Opts { seed_session: None });
    running_turn(&mut rig, 10001);
    rig.kill_candidate();
    // DSH finishes the turn while the bridge is down, but the gateway loses the send receipt.
    lock(&rig.gateway).drop_send_reply = true;
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.onebot_sends().len() == 1), "the send never reached the gateway");
    rig.start_candidate(vec![]);
    assert!(
        wait_for(LONG, || rig.view()["paused"].as_array().map(|p| p.iter().any(|t| t["reason"] == json!("send-unresolved"))).unwrap_or(false)),
        "an unresolved send must pause the turn: {}",
        rig.view()
    );
    let detail = rig.view()["paused"][0]["detail"].as_str().unwrap_or("").to_string();
    assert!(detail.contains("resumeBatchId"), "the administrator is pointed at the batch: {detail}");
    let turn = rig.turns()[0].clone();
    assert_eq!(turn["sends"][0]["unknown"], json!([0]), "the lost receipt is linked to the turn: {turn}");
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.onebot_sends().len(), 1, "an unknown send is never resent by recovery");
    assert_eq!(rig.dsh.applied().len(), 1);
}

/// A damaged send-ledger line (not a truncated prefix of a record, which the reader skips) leaves the send
/// state unknown, which recovery must treat like any other unknown send: pause as `send-unresolved` and rotate
/// the token, never close the turn.
#[test]
fn an_unreadable_send_ledger_pauses_the_finished_turn_and_rotates_the_token() {
    let mut rig = Rig::new("send-ledger-torn", Opts { seed_session: None });
    running_turn(&mut rig, 10101);
    rig.kill_candidate();
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.onebot_sends().len() == 1), "the model's send did not reach QQ");
    let before = rig.agent_token();
    let outbox = rig.root.join("data/send-outbox.jsonl");
    let mut file = fs::OpenOptions::new().append(true).open(&outbox).expect("the send ledger exists");
    std::io::Write::write_all(&mut file, b"{\"k\":\"attempt\",\"batch\":\"b\"}}\n").expect("damage the last line");
    drop(file);
    rig.start_candidate(vec![]);
    // The console's own turn view reads the same ledger and refuses while it is torn, so observe the verdict
    // recovery recorded in the turn ledger.
    let ledger = rig.root.join("data/turn-ledger.jsonl");
    let records = || -> Vec<Value> {
        fs::read_to_string(&ledger).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    };
    assert!(
        wait_for(LONG, || records().iter().any(|r| r["k"] == json!("unknown") && r["reason"] == json!("send-unresolved"))),
        "an unreadable send ledger must pause the turn instead of closing it: {:?}",
        records()
    );
    assert!(records().iter().all(|r| r["k"] != json!("ended")), "the turn must not be closed: {:?}", records());
    assert!(wait_for(LONG, || rig.agent_token() != before), "the agent token must be rotated when the send state is unknown");
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.onebot_sends().len(), 1, "nothing is sent again");
    assert_eq!(rig.dsh.applied().len(), 1);
}

// ───────────────────────── review B1/B3: a not-found text is not a proof ─────────────────────────

/// A prompt whose request was in flight when the bridge died, and a DSH that cannot be asked
/// properly afterwards: whatever "not found" text the probe gets back, the turn is unproven.
fn probe_failure_never_resumes(name: &str, fault: ProbeFault) {
    let mut rig = Rig::new(name, Opts { seed_session: None });
    rig.dsh.set_fault(Fault::HangWithoutApply);
    rig.restart(mention(12001, "探针失败"));
    assert!(wait_for(LONG, || rig.dsh.received() == 1), "prompt request never reached DSH");
    rig.kill_candidate();
    rig.dsh.set_fault(Fault::None);
    rig.dsh.set_probe_fault(fault);
    rig.start_candidate(vec![]);
    assert!(
        wait_for(LONG, || rig.view()["pausedCount"].as_i64() == Some(1)),
        "an unverifiable turn must be reported as paused: {}",
        rig.view()
    );
    thread::sleep(Duration::from_secs(5));
    let view = rig.view();
    assert_eq!(view["paused"][0]["reason"], json!("dsh-unreachable"), "{view}");
    assert_eq!(rig.dsh.received(), 1, "an unproven turn must never be resubmitted ({fault:?})");
    assert!(rig.dsh.applied().is_empty());
    assert_eq!(rig.turn_state(0), "paused", "{:?}", rig.turns());
    assert_eq!(rig.turns().len(), 1, "no second turn may have been started: {:?}", rig.turns());
    assert_eq!(rig.inbound()["interrupted"].as_array().map(Vec::len), Some(1), "{}", rig.inbound());
    // Once DSH answers properly (the log shows no such prompt), the turn is proven unexecuted and runs once.
    rig.dsh.set_probe_fault(ProbeFault::None);
    rig.dsh.set_plan(Plan { send: Some("核实之后"), hold: false });
    assert!(wait_for(Duration::from_secs(60), || rig.dsh.applied().len() == 1), "proven unexecuted work must resume once: {}", rig.view());
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.applied().len(), 1);
}

#[test]
fn an_http_404_from_the_probe_is_not_a_proof_that_the_prompt_never_ran() {
    probe_failure_never_resumes("probe-404", ProbeFault::Http404);
}

#[test]
fn a_method_not_found_rpc_error_is_not_a_proof_that_the_prompt_never_ran() {
    probe_failure_never_resumes("probe-method", ProbeFault::MethodNotFound);
}

#[test]
fn any_other_not_found_text_is_not_a_proof_that_the_prompt_never_ran() {
    probe_failure_never_resumes("probe-text", ProbeFault::NotFoundText);
}

/// Makes appends to `path` fail (or succeed again). The read-only attribute is enough on Windows and for
/// unprivileged Unix users; root ignores it, so root uses the immutable flag instead.
fn set_file_writable(path: &Path, writable: bool) {
    #[cfg(unix)]
    {
        let root = Command::new("id").arg("-u").output().map(|out| out.stdout == b"0\n").unwrap_or(false);
        if root {
            let status = Command::new("chattr")
                .arg(if writable { "-i" } else { "+i" })
                .arg(path)
                .status()
                .expect("run chattr");
            assert!(status.success(), "chattr must be able to toggle the immutable flag as root");
            return;
        }
    }
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(!writable);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn a_failed_ledger_write_stops_the_replay_and_recovery_completes_once_the_ledger_is_writable() {
    let mut rig = Rig::new("ledger-fail", Opts { seed_session: None });
    rig.dsh.set_fault(Fault::HangWithoutApply);
    rig.restart(mention(13001, "账本写不进"));
    assert!(wait_for(LONG, || rig.dsh.received() == 1), "prompt request never reached DSH");
    rig.kill_candidate();
    rig.dsh.set_fault(Fault::None);
    rig.dsh.set_plan(Plan { send: Some("账本恢复之后"), hold: false });
    let ledger = rig.root.join("data/turn-ledger.jsonl");
    set_file_writable(&ledger, false);
    rig.start_candidate(vec![]);
    thread::sleep(Duration::from_secs(8));
    // The turn is provably unexecuted, but it cannot be recorded as resumed: nothing may be replayed.
    assert!(rig.dsh.applied().is_empty(), "no replay while the verdict cannot be recorded");
    assert_eq!(rig.dsh.received(), 1);
    assert_eq!(rig.inbound()["interrupted"].as_array().map(Vec::len), Some(1), "{}", rig.inbound());
    set_file_writable(&ledger, true);
    assert!(wait_for(Duration::from_secs(60), || rig.dsh.applied().len() == 1), "recovery must complete once the ledger is writable");
    thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.dsh.applied().len(), 1, "exactly one replay in total");
    assert_eq!(rig.onebot_sends().len(), 1);
}

/// Ticket 22: a turn that DSH finished while the bridge was down has its usage only in the DSH snapshot.
/// Recovery must write that usage to the Rust ledger exactly once, attributed to the conversation.
#[test]
fn recovered_finished_turn_usage_is_written_once_to_the_rust_ledger() {
    let mut rig = Rig::new("crash-done-usage", Opts { seed_session: None });
    running_turn(&mut rig, 4101);
    rig.kill_candidate();
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.onebot_sends().len() == 1), "the model's send did not reach QQ");
    rig.restart(vec![]);
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "recovery must close the turn: {:?}", rig.turns());
    thread::sleep(Duration::from_secs(3));
    let ledger = fs::read_to_string(rig.root.join("data/token-usage-rust.jsonl")).unwrap_or_default();
    let rows: Vec<Value> = ledger
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("ledger line is JSON"))
        .collect();
    let usage: Vec<&Value> = rows.iter().filter(|r| r["k"] == "turn" && r["b"].is_object()).collect();
    assert_eq!(usage.len(), 1, "the finished turn's usage is written exactly once: {ledger}");
    assert_eq!(usage[0]["b"]["cacheMiss"].as_f64(), Some(7.0), "{ledger}");
    assert_eq!(usage[0]["b"]["output"].as_f64(), Some(3.0), "{ledger}");
    assert_eq!(usage[0]["provider"], json!("ticket10-provider"), "{ledger}");
    assert_eq!(usage[0]["key"], json!(GROUP_KEY), "{ledger}");
}

/// Ticket 22: a turn still running in DSH when the bridge went down is re-attached; its usage arrives on the
/// re-attached stream and must be written to the Rust ledger exactly once.
#[test]
fn reattached_running_turn_usage_is_written_once_to_the_rust_ledger() {
    let mut rig = Rig::new("crash-running-usage", Opts { seed_session: None });
    running_turn(&mut rig, 5101);
    rig.kill_candidate();
    rig.start_candidate(vec![]);
    assert!(
        wait_for(LONG, || rig.turns().first().map(|t| t["reattaching"] == json!(true)).unwrap_or(false)),
        "recovery must re-attach to the running turn: {:?}",
        rig.turns()
    );
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"), "{:?}", rig.turns());
    thread::sleep(Duration::from_secs(3));
    let ledger = fs::read_to_string(rig.root.join("data/token-usage-rust.jsonl")).unwrap_or_default();
    let usage: Vec<Value> = ledger
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("ledger line is JSON"))
        .filter(|r| r["k"] == "turn" && r["b"].is_object())
        .collect();
    assert_eq!(usage.len(), 1, "the re-attached turn's usage is written exactly once: {ledger}");
    assert_eq!(usage[0]["b"]["cacheMiss"].as_f64(), Some(7.0), "{ledger}");
    assert_eq!(usage[0]["b"]["output"].as_f64(), Some(3.0), "{ledger}");
    assert_eq!(usage[0]["key"], json!(GROUP_KEY), "{ledger}");
}

// #85 seam 1: observe the actual DSH user input and explicit QQ sends, never real services.
#[test]
fn hf85_briefing_once_then_twenty_four_fresh_deltas_and_restart() {
    let mut rig = Rig::new("hf85-deltas", Opts { seed_session: Some("js-origin-session") });
    lock(&rig.dsh.inner).acknowledge_snapshot=true;
    fs::write(rig.root.join("data/initial-prompts.json"), json!({GROUP_KEY:{"text":"operator-briefing-85","createdAt":1,"updatedAt":1}}).to_string()).unwrap();
    rig.start_candidate(mention(85000, "first fresh snapshot 85"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    let first = rig.dsh.applied()[0].text.clone();
    assert!(first.contains("operator-briefing-85"), "a non-bootstrap first hydration lost operator background");
    assert!(first.contains("【简报 revision="));
    assert!(first.contains("【反AI味】"));
    assert!(first.contains("【简报节 sending】\n【发送规则】") && first.contains("文本输出不会发送到 QQ"), "first non-bootstrap briefing lost tool-only delivery rules");
    for i in 1..25 {
        lock(&rig.gateway).frames.push(mention_event(85000+i as i64,"fresh delta message"));
        assert!(wait_for(LONG, || rig.turn_state(i) == "completed"), "wake {i} did not finish");
        let text = rig.dsh.applied()[i].text.clone();
        assert!(text.contains("【唤醒增量】"), "wake {i}: {text}");
        assert!(!text.contains("【反AI味】"), "wake {i} repeated unchanged rules");
        assert!(!text.contains("operator-briefing-85"));
        assert!(!text.contains("【首回复节奏】先处理") && !text.contains("【沉睡前强制等待】这是"));
        assert!(text.contains("【会话令牌】"));
        assert!(text.contains("【本轮发送】") && text.contains("文本输出不会发送到 QQ"), "wake {i} lost current-turn tool-only delivery instruction");
        assert!(text.contains("【本次消息快照】"));
        assert!(text.contains("readThroughSeq") && text.contains("throughSeq"));
        let packet:Value=serde_json::from_str(text.split("【本次消息快照】\n").nth(1).unwrap().lines().next().unwrap()).unwrap();
        let expected=(85000+i).to_string();
        assert!(packet["messages"].as_array().unwrap().iter().chain(packet["recent"].as_array().unwrap()).any(|m|m["messageId"]==expected),"wake {i} omitted its new packet message ID");
    }
    assert_eq!(rig.onebot_sends().len(), 25, "only explicit synthetic send tools deliver replies");
    let inputs=rig.dsh.applied();
    let units=|s:&str|s.encode_utf16().count();
    let packet=|s:&str|s.split("【本次消息快照】\n").nth(1).unwrap().lines().next().unwrap().to_string();
    let mut deltas:Vec<usize>=inputs[1..].iter().map(|p|units(&p.text)).collect();
    deltas.sort_unstable();
    println!("hf85 UTF16: first body={}, first briefing={}, delta median body={}, first packet={}, delta packet={}; tools stay session-owned (none in prompt RPC)",
        units(&inputs[0].text),units(inputs[0].text.split("【会话令牌】").next().unwrap()),deltas[deltas.len()/2],
        units(&packet(&inputs[0].text)),units(&packet(&inputs[24].text)));
    let timing=fs::read_to_string(rig.root.join("data/logs/turn-timing.jsonl")).unwrap();
    let rows:Vec<Value>=timing.lines().map(|l|serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.last().unwrap()["promptChars"],inputs[24].text.chars().count(),"timing counts actual delivery");

    rig.restart(vec![]);
    assert_eq!(rig.console("POST", "/api/socialV2/wake", Some(&json!({"key":GROUP_KEY}))).0, 200);
    assert!(wait_for(LONG, || rig.turn_state(25) == "completed"));
    assert!(!rig.dsh.applied()[25].text.contains("【反AI味】"), "durable hydration lost on restart");
    assert!(!rig.dsh.applied()[25].text.contains("【替换简报节 identity】"),"restart replaced the known bot identity");
    assert_eq!(rig.dsh.count("session/create"), 0);
}

#[test]
fn hf85_briefing_changes_replace_remove_and_clock_does_not_resend_memory() {
    let mut rig = Rig::new("hf85-changes", Opts { seed_session: Some("js-origin-session") });
    rig.dsh.set_plan(Plan { send: None, hold: false });
    assert_eq!(rig.console("POST","/api/socialV2/memory-append",Some(&json!({"key":GROUP_KEY,"category":"activeTopic","content":"stable-memory-85"}))).0,200);
    fs::write(rig.root.join("data/initial-prompts.json"),json!({GROUP_KEY:{"text":"old-operator-85"}}).to_string()).unwrap();
    rig.start_candidate(mention(85100,"a single message"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    assert!(rig.dsh.applied()[0].text.contains("stable-memory-85"));
    // Advancing only the clock must not turn relative topic ages into new briefing content.
    rig.epoch=Some(now_ms()+65_000);
    rig.dsh.set_truncate(true);
    rig.restart(vec![]);
    assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
    assert!(wait_for(LONG,||rig.turn_state(1)=="completed"));
    assert!(!rig.dsh.applied()[1].text.contains("stable-memory-85"),"clock-only change resent memory");
    let inputs=rig.dsh.applied();
    let revision=|s:&str|s.split("revision=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    assert_eq!(revision(&inputs[0].text),revision(&inputs[1].text),"time-only changes churned the briefing revision");
    fs::write(rig.root.join("data/initial-prompts.json"),json!({GROUP_KEY:{"text":"new-operator-85"}}).to_string()).unwrap();
    for (category,content,extra) in [("pendingThought","new-thought-85",json!({})),("memberImpression","new-impression-85",json!({"target":"tester"}))] {
        assert_eq!(rig.console("POST","/api/socialV2/memory-append",Some(&json!({"key":GROUP_KEY,"category":category,"content":content,"extra":extra}))).0,200);
    }
    fs::write(rig.root.join("data/stickers.json"),json!([{"id":"synthetic-sticker-85","desc":"new-sticker-85"}]).to_string()).unwrap();
    rig.patch_config(|c|c["socialV2"]["send"]["recommendedHint"]=json!("new-send-rule-85"));
    assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
    assert!(wait_for(LONG,||rig.turn_state(2)=="completed"));
    let changed=rig.dsh.applied()[2].text.clone();
    assert!(changed.contains("【替换简报节 operator】") && changed.contains("new-operator-85"));
    assert!(!changed.contains("old-operator-85") && !changed.contains("【反AI味】"));
    assert!(changed.contains("【替换简报节 memory】") && changed.contains("new-thought-85") && changed.contains("new-impression-85"));
    assert!(changed.contains("【替换简报节 stickers】") && changed.contains("new-sticker-85"));
    assert!(changed.contains("【替换简报节 sending】") && changed.contains("new-send-rule-85"));
    fs::write(rig.root.join("data/initial-prompts.json"),"{}").unwrap();
    assert_eq!(rig.console("POST","/api/socialV2/memory-clear",Some(&json!({"key":GROUP_KEY}))).0,200);
    fs::write(rig.root.join("data/stickers.json"),"[]").unwrap();
    rig.patch_config(|c|{c["socialV2"]["sticker"]=json!({"enabled":false});c["socialV2"]["send"]["recommendedHint"]=json!("");});
    assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
    assert!(wait_for(LONG,||rig.turn_state(3)=="completed"));
    let removed=rig.dsh.applied()[3].text.clone();
    assert!(removed.contains("【删除简报节 operator】") && removed.contains("【删除简报节 memory】"),"{removed}");
    assert!(removed.contains("【删除简报节 stickers】"));
    assert!(removed.contains("【替换简报节 sending】") && removed.contains("文本输出不会发送到 QQ"), "removing the optional hint must retain delivery rules");
    assert!(rig.onebot_sends().is_empty(),"final model text was forwarded");
}


#[test]
fn hf85_completed_compaction_refreshes_once_and_ignores_failed_prune_offload_replay() {
    let mut rig=Rig::new("hf85-compaction",Opts{seed_session:Some("js-origin-session")});
    rig.dsh.set_plan(Plan{send:None,hold:false});
    fs::write(rig.root.join("data/initial-prompts.json"),json!({GROUP_KEY:{"text":"😀".repeat(150)}}).to_string()).unwrap();
    rig.patch_config(|c|c["socialV2"]["initialPromptMaxChars"]=json!(100));
    rig.start_candidate(mention(85200,"compaction fixture"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    {
        let mut inner=lock(&rig.dsh.inner);
        let sess=inner.sessions.get_mut("js-origin-session").unwrap();
        event(sess,"compaction/start",json!({"compactionId":"failed"}));
        event(sess,"compaction/end",json!({"compactionId":"failed","error":"summary failed"}));
        event(sess,"compaction/prune",json!({"compactionId":"prune"}));
        event(sess,"user/message",json!({"surfaceOp":"replace","source":{"kind":"image-offload"}}));
    }
    let wake=|rig:&Rig,index|{
        assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
        assert!(wait_for(LONG,||rig.turn_state(index)=="completed"));
    };
    wake(&rig,1);
    assert!(rig.dsh.applied()[1].text.contains("【唤醒增量】"));
    {
        let mut inner=lock(&rig.dsh.inner);
        let sess=inner.sessions.get_mut("js-origin-session").unwrap();
        event(sess,"compaction/start",json!({"compactionId":"success"}));
        event(sess,"compaction/summary",json!({"compactionId":"success","summary":"synthetic summary"}));
        event(sess,"user/message",json!({"source":{"kind":"compact-checkpoint","compactionId":"success"}}));
        let checkpoint=sess.events.last_mut().unwrap();
        checkpoint["surfaceOp"]=json!({"op":"replace","start":1,"end":2});
        checkpoint["sourceEventSeqs"]=json!([sess.seq-1]);
        event(sess,"compaction/end",json!({"compactionId":"success"}));
    }
    wake(&rig,2);
    let refreshed=rig.dsh.applied()[2].text.clone();
    assert!(refreshed.contains("【简报 revision="),"completed compaction did not refresh exact notes");
    assert_eq!(refreshed.matches('😀').count(),50,"operator delivery cap must remain UTF-16");
    wake(&rig,3);
    assert!(rig.dsh.applied()[3].text.contains("【唤醒增量】"),"snapshot replay caused another refresh");
    assert_eq!(rig.dsh.count("session/create"),0,"compaction must retain the session");
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn hf85_replacement_reset_and_refused_admission_keep_full_briefing_pending() {
    let mut rig=Rig::new("hf85-authority",Opts{seed_session:Some("js-origin-session")});
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.dsh.mark_gone("js-origin-session");
    rig.dsh.set_fault(Fault::Refuse);
    rig.start_candidate(mention(85300,"replacement fixture"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="failed"));
    assert!(rig.dsh.applied().is_empty());
    rig.dsh.set_fault(Fault::None);
    assert!(lock(&rig.dsh.inner).prompts.last().unwrap().text.contains("【简报 revision="),"the dispatching non-bootstrap replacement did not receive full notes");
    assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
    assert!(wait_for(LONG,||!rig.dsh.applied().is_empty()));
    assert!(wait_for(LONG,||rig.turns().iter().any(|t|t["state"]=="completed")));
    let first=rig.dsh.applied()[0].clone();
    assert!(first.text.contains("【简报 revision="));
    assert_ne!(first.session,"js-origin-session");
    assert_eq!(rig.console("POST","/api/session/reset",Some(&json!({"key":GROUP_KEY}))).0,200);
    rig.restart(mention(85301,"after explicit reset"));
    assert!(wait_for(LONG,||rig.dsh.applied().len()>=2));
    let second=rig.dsh.applied()[1].clone();
    assert_ne!(first.session,second.session);
    assert!(second.text.contains("【简报 revision="));
    assert!(rig.dsh.calls("workspace/archiveSession").len()>=2);
}

// Cache-life timing is exercised with the existing process parity clock, never real minutes.
#[test]
fn hf85_idle_rotation_digest_archive_and_next_wake_fresh_briefing() {
    let mut rig=Rig::new("hf85-rotate",Opts{seed_session:Some("js-origin-session")});
    lock(&rig.dsh.inner).input_tokens=500_000;
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":5}));
    rig.patch_config(|c|c["socialV2"]["wake"]["noActionLimit"]=json!(1));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(85400,"preserve this recent message"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.kill_candidate();
    // Below the cap, even an expired cache must leave this valid session alone.
    rig.patch_config(|c|c["dsh"]["rotation"]=json!({"caps":{"ticket10-model":500_001}}));
    rig.epoch=Some(now_ms()+245_000);
    rig.start_candidate(vec![]);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.count("workspace/archiveSession"),0,"under-cap session rotated");
    let sent_before=lock(&rig.gateway).sends.len();
    // Hot reload the cap at equality: one digest, then archive, no summary call or QQ send.
    rig.patch_config(|c|c["dsh"]["rotation"]["caps"]["ticket10-model"]=json!(500_000));
    assert!(wait_for(LONG,||rig.dsh.count("workspace/archiveSession")==1),"eligible sleeping session was not rotated");
    let calls=rig.dsh.calls("session/prompt");
    assert_eq!(calls.len(),1,"one wake; the digest goes to the relay");
    assert_eq!(rig.dsh.digest_calls(),1);
    assert!(lock(&rig.dsh.inner).digest_requests[0].to_string().contains("preserve this recent message"));
    assert!(rig.onebot_sends().is_empty());
    assert!(wait_for(LONG,||fs::read_to_string(rig.root.join("data/qq-activity.log")).unwrap_or_default().contains("session rotation")));
    let activity=fs::read_to_string(rig.root.join("data/qq-activity.log")).unwrap();
    assert_eq!(activity.matches("session rotation").count(),1);
    assert_eq!(lock(&rig.gateway).sends.len(),sent_before,"successful rotation sent a QQ message");
    assert!(activity.contains("500000") && activity.contains("digest=yes"));
    assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
    assert!(wait_for(LONG,||rig.dsh.applied().len()==2));
    let next=rig.dsh.applied()[1].clone();
    assert_ne!(next.session,"js-origin-session");
    assert!(next.text.contains("【简报 revision=") && next.text.contains("preserve this recent message"));
    assert!(next.text.contains("【简报节 sending】\n【发送规则】") && next.text.contains("文本输出不会发送到 QQ"), "rotated session lost delivery rules");
}


#[test]
fn hf85_rotation_digest_failure_alerts_once_and_still_archives() {
    let mut rig=Rig::new("hf85-digest-fail",Opts{seed_session:Some("js-origin-session")});
    lock(&rig.dsh.inner).input_tokens=500_000;
    lock(&rig.dsh.inner).digest_fault=true;
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":5}));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(85500,"digest failure fixture"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.epoch=Some(now_ms()+245_000);
    rig.restart(vec![]);
    assert!(wait_for(LONG,||rig.dsh.count("workspace/archiveSession")==1),"failed digest blocked rotation");
    assert!(wait_for(LONG,||lock(&rig.gateway).requests.iter().filter(|(_,b)|b["user_id"].to_string().trim_matches('"')=="8808" && b["message"].to_string().contains("session rotation")).count()==1));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(lock(&rig.gateway).requests.iter().filter(|(_,b)|b["user_id"].to_string().trim_matches('"')=="8808" && b["message"].to_string().contains("session rotation")).count(),1,"owner alert was not deduplicated");
    assert_eq!(rig.dsh.count("workspace/archiveSession"),1);
    assert_eq!(rig.dsh.received(),1);
    assert_eq!(rig.dsh.digest_calls(),1,"failed digest was resent");
}

#[test]
fn hf85_rotation_incoming_during_digest_cancels_retirement_and_replies_once() {
    let mut rig=Rig::new("hf85-rotation-ingress",Opts{seed_session:Some("js-origin-session")});
    lock(&rig.dsh.inner).input_tokens=500_000;
    lock(&rig.dsh.inner).digest_hold=true;
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":5}));
    rig.start_candidate(mention(85600,"before rotation"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.epoch=Some(now_ms()+245_000);
    // Ingress and held sends must use the same synthetic wall clock in both processes.
    rig.restart_console_at_epoch();
    rig.restart(vec![]);
    assert!(wait_for(LONG,||rig.dsh.digest_calls()==1),"digest never started");
    lock(&rig.gateway).frames.push(mention_event(85601,"message during digest"));
    assert!(wait_for(LONG,||rig.dsh.applied().iter().any(|p|p.text.contains("message during digest"))),"queued incoming wake did not resume");
    assert!(wait_for(LONG,||rig.onebot_sends().len()==2));
    assert_eq!(rig.dsh.count("workspace/archiveSession"),0,"incoming message did not cancel rotation");
    assert_eq!(rig.onebot_sends().len(),2,"incoming wake replied more than once");
}


#[test]
fn hf85_rotation_owner_private_uses_total_input_and_provider_cache_life() {
    const KEY:&str="private:8808";
    let mut rig=Rig::new("hf85-owner-rotation",Opts{seed_session:Some("js-origin-session")});
    {
        let mut inner=lock(&rig.dsh.inner);
        inner.input_tokens=400_000;
        inner.cached_tokens=100_000;
        inner.stream_usage=true;
        inner.provider="opencode-go";
        inner.plan=Plan{send:None,hold:false};
    }
    rig.patch_config(|c|{c["allow"]["private"]=json!([8808]);c["dsh"]["provider"]=json!("opencode-go");});
    let path=rig.root.join("data/social-v2.json");
    let mut state:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][KEY]=state["conversations"][GROUP_KEY].take();
    fs::write(path,state.to_string()).unwrap();
    fs::write(rig.root.join("data/sessions.json"),json!({"sessions":{KEY:"js-origin-session"},"sessionPolicies":{KEY:"[\"reserved2\",\"ticket10-preset\",null]"}}).to_string()).unwrap();
    let frame=json!({"post_type":"message","message_type":"private","user_id":8808,"message_id":85700,"message":[{"type":"text","data":{"text":"private owner cache fixture"}}],"sender":{"nickname":"owner"}});
    rig.start_candidate_groups(vec![Step::Frame(frame)],KEY);
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.kill_candidate();
    // opencode-go's default 120-minute cache life gives a 119-minute idle boundary.
    rig.epoch=Some(now_ms()+118*60_000+5000);
    rig.start_candidate_groups(vec![],KEY);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.count("workspace/archiveSession"),0,"private session rotated before the provider boundary");
    let sent_before=lock(&rig.gateway).sends.len();
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"opencode-go":119}));
    assert!(wait_for(LONG,||rig.dsh.count("workspace/archiveSession")==1),"private chat did not follow the hot-reloaded provider cache life");
    assert_eq!(lock(&rig.gateway).sends.len(),sent_before,"successful owner-private rotation sent a QQ message");
}


#[test]
fn hf85_rotation_digest_interrupted_by_restart_writes_nothing_and_runs_once_more() {
    let mut rig=Rig::new("hf85-digest-restart",Opts{seed_session:Some("js-origin-session")});
    lock(&rig.dsh.inner).input_tokens=500_000;
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":5}));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(85800,"before interrupted digest"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.epoch=Some(now_ms()+245_000);
    lock(&rig.dsh.inner).digest_hold=true;
    rig.restart(vec![]);
    assert!(wait_for(LONG,||rig.dsh.digest_calls()==1),"digest never reached the fake relay");
    rig.kill_candidate();
    lock(&rig.dsh.inner).digest_hold=false;
    rig.dsh.release();
    rig.start_candidate(vec![]);
    assert!(wait_for(LONG,||rig.dsh.count("workspace/archiveSession")==1),"interrupted digest blocked rotation");
    assert_eq!(rig.dsh.digest_calls(),2,"an interrupted digest wrote nothing and runs once more");
    let record=rig.sessions_file()["briefings"][GROUP_KEY].clone();
    assert_eq!(record["digestedThrough"],record["lastTurnSeq"]);
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn hf85_rotation_never_runs_during_a_turn_even_after_cache_expiry() {
    let mut rig=Rig::new("hf85-active-turn",Opts{seed_session:Some("js-origin-session")});
    lock(&rig.dsh.inner).input_tokens=500_000;
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":5}));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(85900,"prior high input call"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.dsh.set_plan(Plan{send:None,hold:true});
    rig.epoch=Some(now_ms()+245_000);
    rig.restart(mention(85901,"active turn prevents rotation"));
    assert!(wait_for(LONG,||rig.turn_state(1)=="running"));
    let before=rig.dsh.received();
    assert_eq!(rig.dsh.digest_calls(),0,"normal turn overlaps a digest");
    assert_eq!(lock(&rig.dsh.inner).overlapping_prompts,0);
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":1.01}));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.received(),before,"maintenance submitted a prompt during a running turn");
    assert_eq!(rig.dsh.count("workspace/archiveSession"),0);
    rig.dsh.release();
    assert!(wait_for(LONG,||rig.turn_state(1)=="completed"));
}


#[test]
fn hf85_rotation_archive_failure_alerts_once_without_a_success_log() {
    let mut rig=Rig::new("hf85-archive-failure",Opts{seed_session:Some("js-origin-session")});
    lock(&rig.dsh.inner).input_tokens=500_000;
    lock(&rig.dsh.inner).archive_fault=true;
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":5}));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(86000,"archive failure fixture"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.epoch=Some(now_ms()+245_000);
    rig.restart(vec![]);
    assert!(wait_for(LONG,||rig.dsh.count("workspace/archiveSession")==1));
    assert!(wait_for(LONG,||lock(&rig.gateway).requests.iter().filter(|(_,b)|b["user_id"].to_string().trim_matches('"')=="8808" && b["message"].to_string().contains("session rotation")).count()==1));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(lock(&rig.gateway).requests.iter().filter(|(_,b)|b["user_id"].to_string().trim_matches('"')=="8808" && b["message"].to_string().contains("session rotation")).count(),1);
    let activity=fs::read_to_string(rig.root.join("data/qq-activity.log")).unwrap_or_default();
    assert!(!activity.contains("session rotation"),"failed archive was logged as a success");
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn hf85_rotation_one_minute_cache_rotates_after_the_turn_closes() {
    let mut rig=Rig::new("hf85-short-cache",Opts{seed_session:Some("js-origin-session")});
    lock(&rig.dsh.inner).input_tokens=500_000;
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":1}));
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(86100,"one-minute provider cache"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    assert!(wait_for(LONG,||rig.dsh.count("workspace/archiveSession")==1),"cache life minus one minute should rotate immediately after closing");
    assert_eq!(rig.dsh.applied().len(),1);
    assert_eq!(rig.dsh.digest_calls(),1);
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn hf85_partial_snapshot_with_a_forward_gap_refreshes_once_without_reset() {
    let mut rig=Rig::new("hf85-history-gap",Opts{seed_session:Some("js-origin-session")});
    rig.dsh.set_plan(Plan{send:None,hold:false});
    rig.start_candidate(mention(86200,"before lost follow coverage"));
    assert!(wait_for(LONG,||rig.turn_state(0)=="completed"));
    rig.kill_candidate();
    {
        let mut inner=lock(&rig.dsh.inner);
        let sess=inner.sessions.get_mut("js-origin-session").unwrap();
        event(sess,"compaction/start",json!({"compactionId":"unseen"}));
        event(sess,"compaction/summary",json!({"compactionId":"unseen"}));
        event(sess,"user/message",json!({"source":{"kind":"compact-checkpoint"}}));
        event(sess,"compaction/end",json!({"compactionId":"unseen"}));
        // The bounded follow tail no longer exposes the compaction marker or installed RPC.
        event(sess,"diagnostic",json!({}));
        event(sess,"diagnostic",json!({}));
    }
    rig.dsh.set_truncate(true);
    rig.start_candidate(vec![]);
    assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
    assert!(wait_for(LONG,||rig.turn_state(1)=="completed"));
    assert!(rig.dsh.applied()[1].text.contains("【简报 revision="),"forward history gap incorrectly reused exact briefing evidence");
    assert_eq!(rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY}))).0,200);
    assert!(wait_for(LONG,||rig.turn_state(2)=="completed"));
    assert!(rig.dsh.applied()[2].text.contains("【唤醒增量】"),"the same partial tail scheduled another full refresh");
    assert_eq!(rig.dsh.count("session/create"),0);
    assert_eq!(rig.dsh.count("workspace/archiveSession"),0);
}


#[test]
fn hf85_rotation_uses_the_last_reminder_calls_input_and_closing_evidence() {
    let mut rig=Rig::new("hf85-reminder-usage",Opts{seed_session:Some("js-origin-session")});
    {
        let mut inner=lock(&rig.dsh.inner);
        inner.input_tokens=499_999;
        inner.later_input_tokens=Some(500_000);
        inner.unclosed_first=true;
        inner.plan=Plan{send:None,hold:false};
    }
    rig.patch_config(|c|c["dsh"]["cacheTtlMinutes"]=json!({"ticket10-provider":5}));
    rig.start_candidate(mention(86300,"requires a closing reminder"));
    assert!(wait_for(LONG,||rig.turn_state(1)=="completed"));
    assert!(rig.dsh.applied()[1].text.contains("【提醒】"));
    rig.epoch=Some(now_ms()+245_000);
    rig.restart(vec![]);
    assert!(wait_for(LONG,||rig.dsh.count("workspace/archiveSession")==1),"last call was a closed reminder at the cap; it must be eligible for rotation");
    assert_eq!(rig.dsh.digest_calls(),1);
}


fn hf85_digest_terminal_case(name: &str, text: &'static str, recover: bool, success: bool) {
    let mut rig = Rig::new(name, Opts { seed_session: Some("js-origin-session") });
    {
        let mut inner = lock(&rig.dsh.inner);
        inner.input_tokens = 500_000;
        inner.digest_text = text;
        inner.digest_hold = recover;
        inner.plan = Plan { send: None, hold: false };
    }
    rig.patch_config(|c| c["dsh"]["cacheTtlMinutes"] = json!({"ticket10-provider":5}));
    rig.start_candidate(mention(86500, "digest terminal classification"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    rig.epoch = Some(now_ms() + 245_000);
    rig.restart(vec![]);
    if recover {
        assert!(wait_for(LONG, || rig.dsh.digest_calls() == 1));
        rig.kill_candidate();
        rig.dsh.release();
        rig.start_candidate(vec![]);
    }
    assert!(wait_for(LONG, || rig.dsh.count("workspace/archiveSession") == 1), "digest completion blocked rotation: {name}, recover={recover}");
    let record = rig.sessions_file()["briefings"][GROUP_KEY].clone();
    if success {
        assert_eq!(record["digestedThrough"], record["lastTurnSeq"], "empty final text after memory tools is valid");
    } else {
        assert!(record["digestedThrough"].as_i64().unwrap_or(0) < record["lastTurnSeq"].as_i64().unwrap(), "completed refusal/error advanced the digest cursor: {name}, recover={recover}");
    }
    let alerts = |rig: &Rig| lock(&rig.gateway).requests.iter().filter(|(_, b)| b["user_id"].to_string().trim_matches('"') == "8808" && b["message"].to_string().contains("session rotation")).count();
    if !success { assert!(wait_for(LONG, || alerts(&rig) == 1), "failed digest needs an owner alert"); }
    assert_eq!(alerts(&rig), if success { 0 } else { 1 });
    rig.restart(vec![]);
    thread::sleep(Duration::from_secs(1));
    assert_eq!(alerts(&rig), if success { 0 } else { 1 }, "restart duplicated the owner alert");
    assert_eq!(rig.dsh.received(), 1);
    assert_eq!(rig.dsh.digest_calls(), if recover { 2 } else { 1 }, "digest was resent");
    assert!(rig.onebot_sends().is_empty());
}

#[test]
fn hf85_fix1_digest_completed_refusal_fails_live_and_after_restart() {
    for recover in [false, true] {
        hf85_digest_terminal_case("hf85-completed-refusal", "I cannot assist with this request.", recover, false);
    }
}

#[test]
fn hf85_fix1_digest_completed_error_fails_live_and_after_restart() {
    for recover in [false, true] {
        hf85_digest_terminal_case("hf85-completed-error", "Error: upstream request failed (HTTP 500).", recover, false);
    }
}

#[test]
fn hf85_fix1_digest_empty_array_succeeds_live_and_after_restart() {
    for recover in [false, true] {
        hf85_digest_terminal_case("hf85-empty-digest", "[]", recover, true);
    }
}

#[test]
fn hf85_fix1_explicit_reset_revokes_string_and_object_mapping_authority() {
    for mapping in [json!("js-origin-session"), json!({"sessionId":"js-origin-session"})] {
        let rig = Rig::new("hf85-reset-shapes", Opts { seed_session: Some("js-origin-session") });
        let mut sessions = rig.sessions_file();
        sessions["sessions"][GROUP_KEY] = mapping.clone();
        fs::write(rig.root.join("data/sessions.json"), sessions.to_string()).unwrap();
        let old_token = rig.agent_token();
        let (status, body) = rig.console("POST", "/api/session/reset", Some(&json!({"key":GROUP_KEY})));
        assert_eq!(status, 200, "reset of {mapping}: {body}");
        assert_eq!(body["archived"], "js-origin-session", "reset resolved the wrong old session: {mapping}");
        assert!(rig.sessions_file()["sessions"].get(GROUP_KEY).is_none(), "reset did not retire {mapping}");
        assert_ne!(rig.agent_token(), old_token, "reset did not revoke authority for {mapping}");
        assert_eq!(rig.dsh.calls("workspace/archiveSession")[0]["request"]["sessionId"], "js-origin-session");
        let late = http_call(rig.console_port, "POST", "/api/socialV2/wake-config",
            &[("x-console-token",CONSOLE_TOKEN),("x-agent-token",&old_token)],
            Some(&json!({"key":GROUP_KEY,"config":{"mode":"active"}}))).unwrap();
        assert_eq!(late.0, 403, "retired token retained authority after resetting {mapping}");
    }
}

#[test]
fn hf85_fix1_explicit_reset_without_a_resolvable_session_reports_non_success() {
    let rig = Rig::new("hf85-reset-unresolved", Opts { seed_session: Some("js-origin-session") });
    let mut sessions = rig.sessions_file();
    sessions["sessions"][GROUP_KEY] = json!({});
    fs::write(rig.root.join("data/sessions.json"), sessions.to_string()).unwrap();
    let old_token = rig.agent_token();
    let (status, body) = rig.console("POST", "/api/session/reset", Some(&json!({"key":GROUP_KEY})));
    assert_eq!(status, 404, "unapplied reset must not answer success: {body}");
    assert_eq!(rig.sessions_file(), sessions);
    assert_eq!(rig.agent_token(), old_token);
    assert_eq!(rig.dsh.count("workspace/archiveSession"), 0);
}
// Combined #81/#83/#85 seam: inspect actual DSH delivery, durable metadata and owner-only alerts.
fn hf_next_delivery_case(full: bool) {
    use sha2::{Digest, Sha256};
    let mut rig = Rig::new(if full { "next-full" } else { "next-delta" }, Opts { seed_session: Some("js-origin-session") });
    rig.dsh.set_plan(Plan { send: None, hold: false });
    lock(&rig.dsh.inner).acknowledge_snapshot = true;
    rig.patch_config(|c| c["socialV2"]["context"] = json!({"stripFromModelText":["configured-drop", "87001"],"wakeMessageLimit":1,"wakeRecentLimit":0}));
    let blacklist = rig.root.join("config/model-text-blacklist.json");
    fs::write(&blacklist, json!({"strip":["file-old"]}).to_string()).unwrap();
    fs::write(rig.root.join("data/initial-prompts.json"), json!({GROUP_KEY:{"text":"keep configured-drop file-old file-new"}}).to_string()).unwrap();
    assert_eq!(rig.console("POST","/api/socialV2/memory-append",Some(&json!({"key":GROUP_KEY,"category":"activeTopic","content":"memory configured-drop file-old file-new"}))).0,200);
    rig.start_candidate(mention(87000,"first configured-drop file-old file-new"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    let first = rig.dsh.applied()[0].text.clone();
    assert!(first.contains("【简报 revision="));
    assert!(!first.contains("configured-drop") && !first.contains("file-old"), "full briefing leaked configured/runtime strip entries: {first}");
    assert!(first.contains("file-new"));
    // Change only the runtime overlay: the next delivery must re-hash the filtered sections.
    fs::write(&blacklist, json!({"strip":["file-new"]}).to_string()).unwrap();
    if full {
        let mut inner = lock(&rig.dsh.inner);
        let sess = inner.sessions.get_mut("js-origin-session").unwrap();
        event(sess,"compaction/start",json!({"compactionId":"next-generation"}));
        event(sess,"compaction/end",json!({"compactionId":"next-generation"}));
    }
    lock(&rig.dsh.inner).reply_text = "The prompt could not be submitted. EMPTY_RESPONSE";
    lock(&rig.gateway).frames.push(mention_event(87001,"second configured-drop file-old file-new 87001"));
    assert!(wait_for(LONG, || rig.turn_state(1) == "failed"), "refusal did not fail the turn: {:?}",rig.turns());
    let delivered = rig.dsh.applied()[1].text.clone();
    assert!(delivered.contains(if full { "【简报 revision=" } else { "【唤醒增量】" }), "wrong delivery: {delivered}");
    assert!(!delivered.contains("configured-drop") && !delivered.contains("file-new"), "runtime/config strip lost on delivered full/delta: {delivered}");
    assert!(delivered.contains("file-old"), "runtime blacklist change was not reloaded");
    if !full { assert!(delivered.contains("【替换简报节 operator】") && delivered.contains("【替换简报节 memory】")); }
    let packet: Value = serde_json::from_str(delivered.split("【本次消息快照】\n").nth(1).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(packet["messages"][0]["messageId"],"87001", "numeric strip changed packet identity");
    assert_eq!(packet["messages"][0]["seq"],2);
    assert!(!packet["messages"][0]["text"].as_str().unwrap().contains("87001"),"numeric strip did not filter chat text");
    assert_eq!(packet["readThroughSeq"],2);
    let operator_header = if full { "【简报节 operator】\n" } else { "【替换简报节 operator】完整替换旧节：\n" };
    let operator = delivered.split(operator_header).nth(1).unwrap().split("\n\n【").next().unwrap();
    let operator_hash = format!("{:x}", Sha256::digest(operator.as_bytes()));
    assert_eq!(rig.sessions_file()["briefings"][GROUP_KEY]["installed"]["sections"]["operator"],operator_hash,"stored section hash differs from delivered filtered content");
    let rows = || fs::read_to_string(rig.root.join("data/logs/model-refusals.jsonl")).unwrap_or_default().lines().map(|l|serde_json::from_str::<Value>(l).unwrap()).collect::<Vec<_>>();
    assert!(wait_for(LONG, || rows().len() == 1), "refusal metadata not recorded");
    let row = rows().remove(0);
    assert_eq!(row["errorClass"],"refusal");
    assert_eq!(row["messages"],json!([{"messageId":"87001","seq":2}]),"refusal provenance must match the delivered packet, not the discarded full recent block");
    let alerts = || lock(&rig.gateway).requests.iter().filter(|(_,b)| b["user_id"] == 8808 && b["message"].to_string().contains("class=refusal")).count();
    assert!(wait_for(LONG, || alerts() == 1),"refusal owner alert missing");
    assert!(rig.onebot_sends().is_empty(), "refusal reached the group");
    let stored = fs::read_to_string(rig.root.join("data/social-v2.json")).unwrap();
    assert!(stored.contains("configured-drop") && stored.contains("file-new"),"model filtering changed stored chat");
    assert!(!rows().iter().any(|r|r.to_string().contains("file-old")),"refusal record retained chat text");
    let timing = fs::read_to_string(rig.root.join("data/logs/turn-timing.jsonl")).unwrap();
    let last: Value = serde_json::from_str(timing.lines().last().unwrap()).unwrap();
    assert_eq!(last["promptChars"],delivered.chars().count());
}

#[test]
fn hf_next_full_briefing_blacklist_reload_and_refusal_provenance() { hf_next_delivery_case(true); }

#[test]
fn hf_next_delta_blacklist_reload_replacements_and_refusal_provenance() { hf_next_delivery_case(false); }

#[test]
fn hf_next_digest_shared_refusal_classifier_live_and_recovered() {
    for recover in [false,true] {
        hf85_digest_terminal_case("next-digest-shared", "Provider returned empty candidates", recover, false);
    }
}

#[test]
fn hf_next_digest_quota_terminal_fails_live_and_recovered() {
    for recover in [false, true] {
        hf85_digest_terminal_case("next-digest-quota", "HTTP 429 quota exceeded", recover, false);
    }
}

#[test]
fn hf_next_completed_failure_records_a_single_terminal_outcome() {
    for (name, text) in [
        ("completed-refusal", "I cannot assist with this request."),
        ("completed-error", "Error: upstream request failed (HTTP 500)."),
    ] {
        let mut rig = Rig::new(name, Opts { seed_session: Some("js-origin-session") });
        {
            let mut inner = lock(&rig.dsh.inner);
            inner.reply_text = text;
            inner.plan = Plan { send: None, hold: false };
        }
        rig.start_candidate(mention(87_100, "one completed backend failure"));
        // Task completion comes after the collector's terminal write attempt. This barrier
        // distinguishes a rejected finalization from bookkeeping that is still in progress.
        assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log"))
            .unwrap_or_default().contains(&format!("[reserved2] {GROUP_KEY} turn ended: Err("))));
        assert_eq!(rig.turn_state(0), "failed", "completed failure remained open: {}", rig.view());
        let records: Vec<Value> = fs::read_to_string(rig.root.join("data/turn-ledger.jsonl"))
            .unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(records.iter().filter(|row| row["k"] == "ended").count(), 1);
        assert!(rig.onebot_sends().is_empty());
    }
}

// Fix-round seam 1: inspect delivered full/delta text.
#[test]
fn hf85_port_fix1_wake_keywords_are_filtered_on_full_and_delta() {
    let mut rig = Rig::new("port-fix1-keywords", Opts { seed_session: Some("js-origin-session") });
    rig.epoch = Some(now_ms());
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.patch_config(|c| c["socialV2"]["context"]["stripFromModelText"] = json!(["configured-keyword"]));
    fs::write(rig.root.join("config/model-text-blacklist.json"), json!({"strip":["runtime-keyword"]}).to_string()).unwrap();
    let wake = json!({"mode":"active","triggers":{"keywords":["keep-keyword","configured-keyword","runtime-keyword"]}});
    assert_eq!(rig.console("POST", "/api/socialV2/wake-config", Some(&json!({"key":GROUP_KEY,"config":wake}))).0, 200);
    rig.start_candidate(mention(87100, "first filtered wake"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    let first = rig.dsh.applied()[0].text.clone();
    assert!(first.contains("【简报 revision=") && first.contains("keep-keyword"));
    assert!(!first.contains("configured-keyword") && !first.contains("runtime-keyword"), "full wake leaked keywords: {first}");
    // The fake model replaces its wake config at turn end; reinstall the keyword config.
    assert_eq!(rig.console("POST", "/api/socialV2/wake-config", Some(&json!({"key":GROUP_KEY,"config":wake}))).0, 200);
    lock(&rig.gateway).frames.push(mention_event(87101, "second filtered wake"));
    assert!(wait_for(LONG, || rig.turn_state(1) == "completed"));
    let delta = rig.dsh.applied()[1].text.clone();
    assert!(delta.contains("【唤醒增量】") && delta.contains("keep-keyword"));
    assert!(!delta.contains("configured-keyword") && !delta.contains("runtime-keyword"), "delta leaked keywords: {delta}");
}

#[test]
fn hf85_port_fix2_digest_mixed_case_errors_fail_live_and_after_restart() {
    for text in [r#"{"errorMessage":"upstream unavailable"}"#, r#"{"isError":true}"#] {
        for recover in [false, true] {
            hf85_digest_terminal_case("port-fix2-json-error", text, recover, false);
        }
    }
}

#[test]
fn hf85_port_fix3_restart_after_settled_digest_rotates_once_without_redigest() {
    for success in [false, true] {
        let mut rig = Rig::new("port-fix3-settled", Opts { seed_session: Some("js-origin-session") });
        {
            let mut inner = lock(&rig.dsh.inner);
            inner.input_tokens = 500_000;
            inner.digest_hold = true;
            inner.digest_text = if success { "[]" } else { r#"{"errorMessage":"upstream unavailable"}"# };
            inner.plan = Plan { send: None, hold: false };
        }
        rig.patch_config(|c| c["dsh"]["cacheTtlMinutes"] = json!({"ticket10-provider":5}));
        rig.start_candidate(mention(87200, "before settled digest restart"));
        assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
        rig.epoch = Some(now_ms() + 245_000);
        rig.restart(vec![]);
        assert!(wait_for(LONG, || rig.dsh.digest_calls() == 1));
        // Hold the existing OS reset gate: digest persistence can finish, but the hub cannot retire.
        let gate = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(rig.root.join("data/locks/closed-agent.lock")).unwrap();
        assert!(wait_for(Duration::from_secs(2), || gate.try_lock().is_ok()));
        rig.dsh.release();
        assert!(wait_for(Duration::from_secs(3), || {
            let record = rig.sessions_file();
            matches!(record["briefings"][GROUP_KEY]["rotation"]["phase"].as_str(), Some("done" | "rotate"))
        }), "digest never settled while reset was fenced");
        let alerts = |rig: &Rig| lock(&rig.gateway).requests.iter().filter(|(_, b)| b["user_id"].to_string().trim_matches('"') == "8808" && b["message"].to_string().contains("session rotation")).count();
        if !success { assert!(wait_for(Duration::from_secs(1), || alerts(&rig) == 1)); }
        assert_eq!(rig.dsh.count("workspace/archiveSession"), 0);
        assert_eq!(rig.sessions_file()["sessions"][GROUP_KEY], "js-origin-session");
        rig.kill_candidate();
        drop(gate);
        rig.start_candidate(vec![]);
        assert!(wait_for(LONG, || rig.dsh.count("workspace/archiveSession") == 1), "restart lost the rotation after digest success={success}");
        assert_eq!(rig.dsh.received(), 1); assert_eq!(rig.dsh.digest_calls(), 1, "restart re-ran a settled digest");
        let record = rig.sessions_file()["briefings"][GROUP_KEY].clone();
        assert_eq!(record["digestedThrough"] == record["lastTurnSeq"], success);
        assert!(rig.sessions_file()["sessions"].get(GROUP_KEY).is_none());
        rig.restart(vec![]);
        thread::sleep(Duration::from_secs(1));
        assert_eq!(rig.dsh.count("workspace/archiveSession"), 1, "restart repeated retirement");
        assert_eq!(rig.dsh.received(), 1); assert_eq!(rig.dsh.digest_calls(), 1, "restart re-ran a settled digest");
        assert_eq!(alerts(&rig), if success { 0 } else { 1 });
        assert!(rig.onebot_sends().is_empty());
    }
}

#[test]
fn hf85_port_ack_real_dsh_prompt_identity_installs_briefing_then_sends_delta() {
    let mut rig = Rig::new("port-ack-real-source", Opts { seed_session: Some("js-origin-session") });
    rig.epoch = Some(now_ms());
    lock(&rig.dsh.inner).real_prompt_source = true;
    rig.start_candidate(mention(87300, "synthetic first wake"));
    assert!(wait_for(Duration::from_secs(4), || rig.sessions_file()["briefings"][GROUP_KEY]["installed"].is_object()),
        "real DSH delivery did not install briefing: {}", rig.sessions_file());
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    assert!(rig.sessions_file()["briefings"][GROUP_KEY]["pending"].is_null());
    assert!(rig.dsh.applied()[0].text.contains("【简报 revision="));
    lock(&rig.gateway).frames.push(mention_event(87301, "synthetic second wake"));
    assert!(wait_for(LONG, || rig.turn_state(1) == "completed"));
    let prompts = rig.dsh.applied();
    assert_eq!(prompts[0].session, prompts[1].session);
    assert!(prompts[1].text.contains("【唤醒增量】"));
    assert!(!prompts[1].text.contains("【简报 revision="));
}

#[test]
fn hf129_fix1_recovered_running_turn_admits_arrivals_and_covers_model_reads() {
    let mut rig = Rig::new("hf129-fix1-recovered", Opts { seed_session: None });
    rig.patch_config(|c| {
        c["socialV2"]["wake"]["maxWakeConfigReminders"] = json!(1);
        c["socialV2"]["proactive"]["enabled"] = json!(false);
    });
    running_turn(&mut rig, 129301);
    rig.kill_candidate();
    let path = rig.root.join("data/social-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"] = json!({"atMention":true,"question":true,"probability":1.0});
    fs::write(path,state.to_string()).unwrap();
    rig.start_candidate(vec![]);
    assert!(wait_for(LONG, || rig.turns().first().is_some_and(|t|t["reattaching"]==true)));
    let ordinary = |id,text| {
        let mut frame=mention_event(id,text);
        frame["message"]=json!([{"type":"text","data":{"text":text}}]);
        frame["raw_message"]=json!(text);
        frame
    };
    lock(&rig.gateway).frames.extend([
        ordinary(129302,"ordinary busy arrival"), ordinary(129303,"你有问题吗？"), mention_event(129304,"urgent read by recovered model")
    ]);
    assert!(wait_for(LONG, || {
        let state:Value=serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap_or_default();
        state["conversations"][GROUP_KEY]["lastUnreadSeq"].as_i64().unwrap_or(0)>=4
    }));
    let token=rig.agent_token();
    let (status,packet)=http_call(rig.console_port,"GET",&format!("/api/socialV2/unread?key={GROUP_KEY}"),
        &[("x-console-token",CONSOLE_TOKEN),("x-agent-token",&token)],None).unwrap();
    assert_eq!(status,200,"{packet}");
    assert!(packet["messages"].as_array().unwrap().iter().any(|m|m["seq"]==4),"{packet}");
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.turn_state(0)=="completed"));
    thread::sleep(Duration::from_secs(3));
    let decisions:Vec<Value>=fs::read_to_string(rig.root.join("data/logs/rbridge-live.jsonl")).unwrap().lines()
        .filter_map(|l|serde_json::from_str::<Value>(l).ok()).filter(|v|v["shouldWake"].is_boolean()).collect();
    assert_eq!(decisions[1]["reason"],"in-turn","{decisions:?}");
    assert_eq!(decisions[2]["reason"],"in-turn","{decisions:?}");
    assert_eq!(rig.dsh.applied().len(),1,"model reads during recovery cover the urgent wake: {:?}",rig.turns());
}

#[test]
fn hf129_fix3_restart_keeps_the_base_urgent_followup_once() {
    let mut rig = Rig::new("hf129-fix3-restart", Opts { seed_session: None });
    rig.patch_config(|c| {
        c["socialV2"]["wake"]["maxWakeConfigReminders"] = json!(1);
        c["socialV2"]["proactive"]["enabled"] = json!(false);
    });
    running_turn(&mut rig, 129530);
    rig.kill_candidate();
    rig.start_candidate(vec![]);
    assert!(wait_for(LONG, || rig.turns().first().is_some_and(|t|t["reattaching"]==true)));
    lock(&rig.gateway).frames.push(mention_event(129531,"unseen after restart"));
    assert!(wait_for(LONG, || {
        let state:Value=serde_json::from_slice(&fs::read(rig.root.join("data/social-v2.json")).unwrap()).unwrap_or_default();
        state["conversations"][GROUP_KEY]["lastUnreadSeq"]==2
    }));
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.dsh.release();
    assert!(wait_for(LONG, || rig.dsh.applied().len()==2), "base urgent followup lost: {:?}",rig.turns());
    assert!(wait_for(LONG, || rig.turn_state(1)=="completed"));
    rig.kill_candidate();
    rig.start_candidate(vec![]);
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.applied().len(),2,"one completed base followup stays settled across restart");
}

/// Total search hits of the fixture memory database (the live bridge holds it open in WAL mode).
fn memory_hits(rig: &Rig) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(rig.root.join("data/memory.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open memory.db");
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.query_row("SELECT COALESCE(SUM(hits), 0) FROM memory", [], |row| row.get(0)).expect("sum hits")
}

#[test]
fn recall_hits_count_once_per_accepted_delivery_and_not_for_a_refused_attempt() {
    let mut rig = Rig::new("recall-hits", Opts { seed_session: Some("js-origin-session") });
    let written = http_call(rig.console_port, "POST", "/api/memory/write", &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", SEED_TOKEN)],
        Some(&json!({"key": GROUP_KEY, "kind": "fact", "subject": "测试", "content": "zebra148kw 的回忆内容"}))).expect("memory write reply");
    assert_eq!(written.0, 200, "{:?}", written.1);
    assert_eq!(memory_hits(&rig), 0);
    rig.dsh.set_fault(Fault::Refuse);
    rig.dsh.set_plan(Plan { send: None, hold: false });
    rig.start_candidate(mention(14801, "zebra148kw 你还记得吗"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "failed"), "the refused dispatch did not fail: {:?}", rig.turns());
    assert!(rig.dsh.applied().is_empty());
    assert_eq!(memory_hits(&rig), 0, "a refused submission delivered no recall, so it counts no hit");
    rig.dsh.set_fault(Fault::None);
    assert!(wait_for(Duration::from_secs(20), || !rig.dsh.applied().is_empty()), "the retry was never accepted");
    assert!(wait_for(LONG, || rig.turns().iter().any(|t| t["state"] == "completed")), "{:?}", rig.turns());
    let delivered = rig.dsh.applied();
    assert_eq!(delivered.len(), 1, "one accepted delivery");
    assert!(delivered[0].text.contains("zebra148kw 的回忆内容"), "the accepted wake carries the recall: {}", delivered[0].text);
    assert_eq!(memory_hits(&rig), 1, "the one accepted delivery counts the one recalled memory once");
}

// #135: the durable pending intent belongs to maintenance, not turn-ledger recovery.
fn hf135_pending_fixture(name: &str) -> Rig {
    let mut rig = Rig::new(name, Opts { seed_session: Some("js-origin-session") });
    rig.dsh.set_plan(Plan { send: None, hold: false });
    lock(&rig.dsh.inner).input_tokens = 500_000;
    rig.patch_config(|c| c["dsh"]["cacheTtlMinutes"] = json!({"ticket10-provider":5}));
    rig.start_candidate(mention(13500, "synthetic rotation source"));
    assert!(wait_for(LONG, || rig.turn_state(0) == "completed"));
    rig.kill_candidate();
    let mut doc = rig.sessions_file();
    let r = &mut doc["briefings"][GROUP_KEY];
    r["rotation"] = json!({"phase":"pending","rpcId":"hf135-before-restart","turnSeq":r["lastTurnSeq"]});
    fs::write(rig.root.join("data/sessions.json"), doc.to_string()).unwrap();
    rig.epoch = Some(now_ms() + 245_000);
    rig
}



#[test]
fn hf135_pending_dsh_digest_from_an_earlier_release_runs_the_digest_once() {
    let mut rig = hf135_pending_fixture("hf135-legacy-pending");
    rig.start_candidate(vec![]);
    assert!(wait_for(Duration::from_secs(15), || rig.dsh.count("workspace/archiveSession") == 1));
    assert_eq!(rig.dsh.received(), 1, "no DSH digest prompt is ever submitted");
    assert_eq!(rig.dsh.digest_calls(), 1);
    let record = rig.sessions_file()["briefings"][GROUP_KEY].clone();
    assert_eq!(record["digestedThrough"], record["lastTurnSeq"]);
}












