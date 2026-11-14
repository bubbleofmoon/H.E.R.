//! ticket 07 外部行为验收：入站先落库再处理、重复事件去重、重启重放与中断标记、
//! 断线缺口按网关历史能力补收，以及管理端 `/api/inbound/gaps` 的可见结果。
//!
//! 全部在选定运行目录内进行，使用本机假 WebSocket 与假 OneBot HTTP。
//! 不连接真实 QQ、模型或 DSH；发送仅由本机假网关记录，DSH 指向已关闭的本机端口。

use support::hide_window as hide_process_window;
mod support;

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::{accept, Message};

const GROUP_ID: i64 = 420042;
const SELF_ID: i64 = 9001;
const OTHER_GROUP: i64 = 520052;
const ONEBOT_TOKEN: &str = "ticket07-onebot-token-0001";
const CONSOLE_TOKEN: &str = "ticket07-console-token-0001";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_millis() as i64
}

type TempRoot = support::RuntimeRoot;

fn write_fixture(root: &Path, ws_url: &str, onebot_url: &str, owner: Option<i64>) {
    fs::write(
        root.join("config/config.json"),
        json!({
            "consoleToken": CONSOLE_TOKEN,
            "ownerQQ": owner,
            "allow": {"groups": [GROUP_ID], "private": []},
            "snowluma": {"accessToken": ONEBOT_TOKEN, "httpUrl": "http://127.0.0.1:9", "wsUrl": "ws://127.0.0.1:9"},
            "socialV2": {
                "wake": {"batchWindowMs": 1000},
                // Keep saved work queued beyond the old redelivery + fixed-sleep bound.
                "reply": {"wakeQuietMs": 6000, "quietMedianMs": 10000, "quietMinMs": 4000, "quietMaxMs": 25000},
                "send": {"maxMessageChars": 800, "gapBaseMs": 0, "gapPerCharMs": 0, "maxGapMs": 0}
            },
            "slang": {"enabled": false},
            "backends": {
                "sessions": {"group:420042": "ticket07-cli"},
                "list": [{
                    "id": "ticket07-cli",
                    "kind": "cli",
                    "cmd": "cmd.exe",
                    "args": ["/c", "exit", "1"]
                }]
            }
        })
        .to_string(),
    )
    .expect("write selected config");
    fs::write(
        root.join("data/social-v2.json"),
        json!({
            "conversations": {
                "group:420042": {
                    "unread": [],
                    "recentMessages": [],
                    "wakeConfig": {"triggers": {"atMention": true}}
                }
            }
        })
        .to_string(),
    )
    .expect("write selected social state");
    fs::write(
        root.join("user/dsh/.credentials.yaml"),
        "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n",
    )
    .expect("write synthetic DSH credential");
    crate::support::write_dependencies(root, "http://127.0.0.1:9", "http://127.0.0.1:9", onebot_url, Some(ws_url));
}

/// 假网关 WebSocket 的脚本步骤。每个连接按顺序执行一组步骤，最后关闭连接以触发 her 重连。
#[derive(Clone)]
enum Step {
    /// 发送一条 @ 本账号的群消息；时间戳在发送时取当前秒。
    Mention { message_id: i64, text: &'static str },
    /// Ordinary non-owner text: the first-contact bootstrap uses the batch window.
    Ordinary { message_id: i64, text: &'static str },
    /// 同一条消息，但文本里带一个未配对的 `\ud83d` 转义（残缺的 emoji），按原样写成帧。
    LoneSurrogateMention { message_id: i64 },
    /// 设置假历史接口的应答方式（下一次补收生效）。
    History(HistoryPlan),
    /// Witness durable admission before starting the scripted outage clock.
    AwaitReceived { message_id: i64 },
    /// Send the next live frame only after the history request is actually held.
    AwaitHistory,
    Wait(u64),
    /// A raw gateway frame, sent exactly as given.
    Frame(Value),
    Drop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HistoryPlan {
    /// 最新页同时包含窗口内的一条缺失消息与窗口起点之前的消息，可证明覆盖。
    Covered,
    /// 网关返回 retcode 1404（不支持该能力）。
    Unsupported,
    /// 网关应答成功但没有任何消息：无法证明窗口已补齐。
    Empty,
    /// Hold the response until the test has witnessed live-frame progress.
    Slow,
}

#[derive(Default)]
struct SlowHistory {
    started: AtomicBool,
    released: AtomicBool,
    responded: AtomicBool,
}

#[derive(Default)]
struct FakeGateway {
    plan: Option<HistoryPlan>,
    last_live_second: i64,
    missed_second: i64,
    paths: Vec<String>,
    requests: Vec<(String, Value)>,
    sends: Vec<(String, Value)>,
    slow_history: Arc<SlowHistory>,
    send_retcode: Option<i64>,
    held_send: Option<Arc<SlowHistory>>,
}

fn mention_event(message_id: i64, text: &str, time_s: i64) -> Value {
    json!({
        "time": time_s,
        "self_id": SELF_ID,
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "message_id": message_id,
        "group_id": GROUP_ID,
        "user_id": 1001,
        "sender": {"nickname": "tester", "card": ""},
        "message": [
            {"type": "at", "data": {"qq": SELF_ID.to_string()}},
            {"type": "text", "data": {"text": format!(" {text}")}}
        ],
        "raw_message": format!("[CQ:at,qq={SELF_ID}] {text}"),
        "font": 0
    })
}

fn group_frame(group: i64, user: i64, message_id: i64, text: &str) -> Value {
    json!({
        "time": now_ms() / 1000,
        "self_id": SELF_ID,
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "message_id": message_id,
        "group_id": group,
        "user_id": user,
        "sender": {"user_id": user, "nickname": "tester", "card": ""},
        "message": [{"type": "text", "data": {"text": text}}],
        "raw_message": text,
        "font": 0
    })
}

fn serve_ws(listener: TcpListener, scripts: Vec<Vec<Step>>, gateway: Arc<Mutex<FakeGateway>>, root: PathBuf) {
    for script in scripts {
        let Ok((stream, _)) = listener.accept() else { return };
        let Ok(mut socket) = accept(stream) else { continue };
        for step in script {
            let ordinary = matches!(&step, Step::Ordinary { .. });
            match step {
                Step::Mention { message_id, text } | Step::Ordinary { message_id, text } => {
                    let now = now_ms();
                    gateway.lock().expect("gateway state").last_live_second = now / 1000;
                    let mut event = mention_event(message_id, text, now / 1000);
                    if ordinary {
                        event["message"] = json!([{"type":"text","data":{"text":text}}]);
                        event["raw_message"] = json!(text);
                    }
                    if socket.send(Message::Text(event.to_string())).is_err() {
                        break;
                    }
                }
                Step::LoneSurrogateMention { message_id } => {
                    let now = now_ms();
                    gateway.lock().expect("gateway state").last_live_second = now / 1000;
                    let frame = mention_event(message_id, "broken LONEMARK emoji", now / 1000)
                        .to_string()
                        .replace("LONEMARK", "\\ud83d");
                    assert!(frame.contains("\\ud83d e"), "the frame carries the unpaired escape");
                    if socket.send(Message::Text(frame)).is_err() {
                        break;
                    }
                }
                Step::History(plan) => {
                    let mut state = gateway.lock().expect("gateway state");
                    state.plan = Some(plan);
                    // Generate the missing message at the scripted outage, after
                    // live ingress. live+1 can precede the actual durable receipt
                    // on a slower host and then correctly falls outside the gap.
                    state.missed_second = now_ms() / 1000;
                }
                Step::AwaitReceived { message_id } => {
                    let saved = wait_for(Duration::from_secs(10), || {
                        let Ok(db) = rusqlite::Connection::open_with_flags(
                            root.join("data/inbound.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                        ) else { return false };
                        db.query_row(
                            "SELECT 1 FROM inbound_events WHERE event_key = ?1",
                            [format!("group:{GROUP_ID}|message:{message_id}")],
                            |row| row.get::<_, i64>(0),
                        ).ok() == Some(1)
                    });
                    assert!(saved, "scripted live message was not durably admitted before the outage");
                }
                Step::AwaitHistory => {
                    let history = gateway.lock().expect("gateway state").slow_history.clone();
                    assert!(
                        wait_for(Duration::from_secs(10), || history.started.load(Ordering::SeqCst)),
                        "history request did not start before the post-reconnect frame"
                    );
                }
                Step::Frame(event) => {
                    if socket.send(Message::Text(event.to_string())).is_err() {
                        break;
                    }
                }
                Step::Wait(ms) => thread::sleep(Duration::from_millis(ms)),
                Step::Drop => break,
            }
        }
        // 脚本结束即丢弃连接：her 视为断线并在 2 秒后重连。
        drop(socket);
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&bytes[..split]).to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if bytes.len() >= split + 4 + length {
            let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
            return Some((path, bytes[split + 4..split + 4 + length].to_vec()));
        }
    }
}

fn write_http_json(stream: &mut TcpStream, status: &str, value: &Value) {
    let body = value.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn serve_onebot(listener: TcpListener, gateway: Arc<Mutex<FakeGateway>>) {
    loop {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let Some((path, body)) = read_http_request(&mut stream) else { continue };
        let plan = {
            let mut state = gateway.lock().expect("gateway state");
            state.paths.push(path.clone());
            state.requests.push((path.clone(), serde_json::from_slice(&body).unwrap_or(Value::Null)));
            if path.contains("send") {
                state.sends.push((path.clone(), serde_json::from_slice(&body).expect("send JSON")));
            }
            state.plan
        };
        if path.contains("send") {
            let gate = gateway.lock().unwrap().held_send.clone();
            if let Some(gate) = gate {
                gate.started.store(true, Ordering::SeqCst);
                assert!(wait_for(Duration::from_secs(60), || gate.released.load(Ordering::SeqCst)), "test did not release held send");
            }
        }
        if path == "/get_group_msg_history" {
            match plan {
                Some(HistoryPlan::Covered) => {
                    let (live, missed) = {
                        let state = gateway.lock().expect("gateway state");
                        (state.last_live_second, state.missed_second)
                    };
                    // 6002 位于断线窗口内；6001 的时间早于窗口起点，证明覆盖。
                    let response = json!({
                        "status": "ok",
                        "retcode": 0,
                        "data": {"messages": [
                            {"message_id": 6002, "time": missed, "message_seq": 12, "raw_message": "missed", "message": []},
                            {"message_id": 6001, "time": live, "message_seq": 11, "raw_message": "live", "message": []}
                        ]}
                    });
                    write_http_json(&mut stream, "200 OK", &response);
                }
                Some(HistoryPlan::Empty) => {
                    write_http_json(
                        &mut stream,
                        "200 OK",
                        &json!({"status": "ok", "retcode": 0, "data": {"messages": []}}),
                    );
                }
                Some(HistoryPlan::Slow) => {
                    let history = gateway.lock().expect("gateway state").slow_history.clone();
                    history.started.store(true, Ordering::SeqCst);
                    assert!(
                        wait_for(Duration::from_secs(60), || history.released.load(Ordering::SeqCst)),
                        "test did not release the held history response"
                    );
                    write_http_json(
                        &mut stream,
                        "200 OK",
                        &json!({"status": "ok", "retcode": 0, "data": {"messages": []}}),
                    );
                }
                Some(HistoryPlan::Unsupported) | None => {
                    write_http_json(
                        &mut stream,
                        "200 OK",
                        &json!({"status": "failed", "retcode": 1404, "data": null}),
                    );
                }
            }
        } else if path.contains("send") && gateway.lock().unwrap().send_retcode.is_some() {
            let code = gateway.lock().unwrap().send_retcode.unwrap();
            write_http_json(&mut stream, "200 OK", &json!({"status":"failed", "retcode":code, "data":null}));
        } else {
            write_http_json(
                &mut stream,
                "200 OK",
                &json!({"status": "ok", "retcode": 0, "data": {"card": "", "nickname": "tester", "user_id": SELF_ID}}),
            );
        }
        if path.contains("send") {
            if let Some(gate) = &gateway.lock().unwrap().held_send {
                gate.responded.store(true, Ordering::SeqCst);
            }
        }
    }
}

fn console_get(port: u16, path: &str) -> Option<Value> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-console-token: {CONSOLE_TOKEN}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (_, body) = text.split_once("\r\n\r\n")?;
    serde_json::from_str(body).ok()
}

fn log_file(root: &Path, name: &str) -> fs::File {
    fs::OpenOptions::new().create(true).append(true).open(root.join(name)).expect("candidate log")
}

/// 候选进程实际执行过的 live 回合数（跨重启累计，日志按追加写入）。
fn turns_executed(root: &Path) -> usize {
    fs::read_to_string(root.join("candidate-out.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("执行 live 回合"))
        .count()
}

fn spawn_candidate(root: &Path, console_port: u16) -> Child {
    let ambient = root.join("ambient");
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--runtime-root")
        .arg(root)
        .arg("--live")
        .arg("--live-groups")
        .arg(format!("group:{GROUP_ID}"))
        .arg("--console-port")
        .arg(console_port.to_string())
        .env_remove("HER_RUNTIME_ROOT")
        .env("HOME", &ambient)
        .env("USERPROFILE", &ambient)
        .env("DSH_HOME", &ambient)
        .env("APPDATA", ambient.join("Roaming"))
        .env("LOCALAPPDATA", ambient.join("Local"))
        .stdout(Stdio::from(log_file(root, "candidate-out.log")))
        .stderr(Stdio::from(log_file(root, "candidate-err.log")));
    hide_process_window(&mut command);
    let child = command.spawn().expect("spawn selected-root candidate");
    eprintln!("candidate launched pid={} launch_ms={}", child.id(), now_ms());
    child
}

/// 管理端控制台是独立进程（`--console-serve`），与桥接进程共用同一运行目录里的入站库。
fn spawn_console(root: &Path, console_port: u16) -> Child {
    let ambient = root.join("ambient");
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--console-serve")
        .arg("--console-port")
        .arg(console_port.to_string())
        .arg("--runtime-root")
        .arg(root)
        .env_remove("HER_RUNTIME_ROOT")
        .env("HOME", &ambient)
        .env("USERPROFILE", &ambient)
        .env("DSH_HOME", &ambient)
        .env("APPDATA", ambient.join("Roaming"))
        .env("LOCALAPPDATA", ambient.join("Local"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_process_window(&mut command);
    command.spawn().expect("spawn selected-root console")
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

fn decisions(root: &Path) -> Vec<Value> {
    let path = root.join("data/logs/rbridge-live.jsonl");
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record.get("shouldWake").is_some())
        .collect()
}

fn history_records(root: &Path) -> Vec<Value> {
    let Ok(entries) = fs::read_dir(root.join("data/history")) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries.filter_map(|entry| entry.ok().map(|e| e.path())).collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .flat_map(|text| text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).collect::<Vec<_>>())
        .collect()
}

fn admin(port: u16) -> Value {
    console_get(port, "/api/inbound/gaps").unwrap_or(Value::Null)
}

fn gaps_of(view: &Value) -> Vec<Value> {
    view["gaps"].as_array().cloned().unwrap_or_default()
}

/// 启动候选进程与两个假服务，返回 (候选进程, 控制台端口, 网关状态)。
struct Harness {
    child: Child,
    console: crate::support::ports::OwnedChild,
    console_port: u16,
    gateway: Arc<Mutex<FakeGateway>>,
    root: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Release even on assertion failure, so the fake HTTP thread cannot stay held.
        self.gateway.lock().expect("gateway state").slow_history.released.store(true, Ordering::SeqCst);
        if let Some(gate) = &self.gateway.lock().unwrap().held_send {
            gate.released.store(true, Ordering::SeqCst);
        }
        if thread::panicking() {
            for name in ["candidate-out.log", "candidate-err.log"] {
                let text = fs::read_to_string(self.root.join(name)).unwrap_or_default();
                eprintln!("---- {name} (tail) ----
{}", text.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("
"));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.console.kill();
        let _ = self.console.wait();
    }
}

fn start(root: &Path, scripts: Vec<Vec<Step>>) -> Harness {
    start_with_owner(root, scripts, Some(8808))
}

fn start_with_owner(root: &Path, scripts: Vec<Vec<Step>>, owner: Option<i64>) -> Harness {
    start_with_scope(root, scripts, owner, &[])
}

fn start_with_scope(root: &Path, scripts: Vec<Vec<Step>>, owner: Option<i64>, privates: &[i64]) -> Harness {
    start_with_config(root, scripts, owner, privates, |_| {})
}

fn start_with_config(root: &Path, scripts: Vec<Vec<Step>>, owner: Option<i64>, privates: &[i64], patch: impl FnOnce(&mut Value)) -> Harness {
    let ws_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake gateway WebSocket");
    let ws_url = format!("ws://127.0.0.1:{}", ws_listener.local_addr().expect("ws address").port());
    let onebot_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake OneBot HTTP");
    let onebot_url = format!("http://127.0.0.1:{}", onebot_listener.local_addr().expect("http address").port());
    write_fixture(root, &ws_url, &onebot_url, owner);
    let path = root.join("config/config.json");
    let mut cfg: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    cfg["allow"]["private"] = json!(privates);
    patch(&mut cfg);
    fs::write(path, cfg.to_string()).unwrap();
    let gateway = Arc::new(Mutex::new(FakeGateway::default()));
    {
        let gateway = gateway.clone();
        let root = root.to_path_buf();
        thread::spawn(move || serve_ws(ws_listener, scripts, gateway, root));
    }
    {
        let gateway = gateway.clone();
        thread::spawn(move || serve_onebot(onebot_listener, gateway));
    }
    let (console, console_port) = crate::support::ports::start_console(Duration::from_secs(20), |port| spawn_console(root, port));
    let console = crate::support::ports::OwnedChild(console);
    let child = spawn_candidate(root, console_port);
    Harness { child, console, console_port, gateway, root: root.to_path_buf() }
}

#[test]
fn duplicate_live_event_is_saved_once_and_processed_once() {
    let root = TempRoot::new("duplicate");
    let harness = start(
        root.path(),
        vec![vec![
            Step::Mention { message_id: 7001, text: "duplicate probe" },
            Step::Mention { message_id: 7001, text: "duplicate probe" },
            Step::Wait(60_000),
        ]],
    );

    let seen = wait_for(Duration::from_secs(30), || {
        admin(harness.console_port)["duplicates"].as_i64().unwrap_or(0) >= 1
            && !decisions(root.path()).is_empty()
    });
    assert!(seen, "duplicate delivery was not observed by the admin view");
    thread::sleep(Duration::from_secs(3));

    let view = admin(harness.console_port);
    eprintln!("ticket07 duplicate admin view: {view}");
    assert_eq!(view["duplicates"], json!(1), "exactly one redelivery must be counted");
    assert_eq!(
        decisions(root.path()).len(),
        1,
        "a redelivered message must not create a second decision or turn"
    );
    let (paths, sends) = {
        let gateway = harness.gateway.lock().expect("gateway state");
        (gateway.paths.clone(), gateway.sends.clone())
    };
    assert!(
        paths.iter().all(|p| !p.contains("send") || p == "/send_private_msg"),
        "only the backend-failure owner alert may be sent: {paths:?}"
    );
    assert_eq!(sends.len(), 1, "a duplicate event must not duplicate the owner alert");
    assert_eq!(sends[0].0, "/send_private_msg");
    assert_eq!(sends[0].1["user_id"], json!(8808));
    let segments = sends[0].1["message"].as_array().expect("message segments");
    assert_eq!(segments.len(), 1, "the owner alert contains only one text segment");
    assert_eq!(segments[0]["type"], "text");
    let alert = segments[0]["data"]["text"].as_str().expect("text alert");
    assert!(alert.contains("[group:420042]") && alert.contains("backend=ticket07-cli") && alert.contains("class=backend"), "{alert}");
    assert!(!alert.contains("duplicate probe"), "chat text must not appear in the owner alert");
}

/// Accepted deviation 18: a gateway frame whose text has an unpaired surrogate escape is processed with
/// U+FFFD in its place (JS keeps the lone unit), not silently dropped.
#[test]
fn frame_with_an_unpaired_surrogate_escape_is_processed_with_a_replacement_character() {
    let root = TempRoot::new("lone-surrogate");
    let harness = start(
        root.path(),
        vec![vec![Step::LoneSurrogateMention { message_id: 7301 }, Step::Wait(60_000)]],
    );
    let processed = wait_for(Duration::from_secs(30), || !decisions(root.path()).is_empty());
    let log = fs::read_to_string(root.path().join("candidate-out.log")).unwrap_or_default();
    assert!(processed, "the frame with a lone surrogate was dropped; candidate log:\n{log}");
    assert_eq!(decisions(root.path()).len(), 1, "exactly one decision for the one message");
    assert!(log.contains("孤立代理项"), "the replacement must be logged:\n{log}");
    // The saved event is parseable again (replay) and carries U+FFFD where the lone unit was.
    let db = rusqlite::Connection::open_with_flags(
        root.path().join("data/inbound.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open inbound.db");
    let raw: String = db
        .query_row("SELECT raw FROM inbound_events WHERE event_key LIKE '%7301%'", [], |row| row.get(0))
        .expect("the event was saved");
    let saved: Value = serde_json::from_str(&raw).expect("saved raw frame is valid JSON");
    assert!(saved["raw_message"].as_str().unwrap_or("").contains("broken \u{fffd} emoji"), "{raw}");
    drop(harness);
}

#[test]
fn restart_replays_saved_event_once_and_marks_started_event_interrupted() {
    let root = TempRoot::new("restart");
    // 模拟两个崩溃点留下的状态：已保存未处理（received）与已开始处理（processing）。
    let db = rusqlite::Connection::open(root.path().join("data/inbound.db")).expect("create inbound db");
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS inbound_events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            event_key TEXT NOT NULL UNIQUE,
            conversation TEXT NOT NULL,
            origin TEXT NOT NULL,
            stage TEXT NOT NULL,
            received_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL,
            raw TEXT NOT NULL
        );",
    )
    .expect("create inbound events table");
    let saved = mention_event(8001, "replay probe", now_ms() / 1000).to_string();
    let started = mention_event(8002, "interrupted probe", now_ms() / 1000).to_string();
    for (key, stage, raw) in [
        ("group:420042|message:8001", "received", saved.as_str()),
        ("group:420042|message:8002", "processing", started.as_str()),
    ] {
        db.execute(
            "INSERT INTO inbound_events (event_key, conversation, origin, stage, received_ms, updated_ms, raw)
             VALUES (?1, 'group:420042', 'live', ?2, ?3, ?3, ?4)",
            rusqlite::params![key, stage, now_ms(), raw],
        )
        .expect("seed crash-state event");
    }
    drop(db);

    let harness = start(root.path(), vec![vec![Step::Wait(60_000)]]);
    let replayed = wait_for(Duration::from_secs(30), || !decisions(root.path()).is_empty());
    assert!(replayed, "saved-but-unprocessed event was not replayed after restart");
    thread::sleep(Duration::from_secs(3));

    let view = admin(harness.console_port);
    eprintln!("ticket07 restart admin view: {view}");
    assert_eq!(decisions(root.path()).len(), 1, "only the saved event may be replayed");
    assert_eq!(
        view["interrupted"],
        json!(["group:420042|message:8002"]),
        "the event that was mid-processing must be reported, not re-run"
    );
    assert_eq!(view["stages"]["received"].as_i64().unwrap_or(0), 0);
}

#[test]
fn disconnect_gap_is_backfilled_without_waking_and_unsupported_gap_stays_open() {
    let root = TempRoot::new("gap");
    // The no-send assertion below needs no owner: Rust's accepted deviation 22 sends a
    // private unresolved-gap notice, whereas frozen JS only logs outages (bridge.js:11658).
    let harness = start_with_owner(
        root.path(),
        vec![
            vec![
                Step::Mention { message_id: 6001, text: "before the drop" },
                Step::AwaitReceived { message_id: 6001 },
                Step::Wait(2500),
                Step::History(HistoryPlan::Covered),
                Step::Drop,
            ],
            vec![Step::Wait(1200), Step::History(HistoryPlan::Unsupported), Step::Drop],
            vec![Step::Wait(1200), Step::Drop],
            vec![Step::Wait(60_000)],
        ],
        None,
    );

    // 第一次连接之后的三次重连各开一段缺口：窗口 1（覆盖）、窗口 2、窗口 3（不支持）。
    let settled = wait_for(Duration::from_secs(45), || gaps_of(&admin(harness.console_port)).len() >= 3);
    assert!(settled, "expected one gap per reconnect window");
    thread::sleep(Duration::from_secs(3));

    let view = admin(harness.console_port);
    eprintln!("ticket07 gap admin view: {view}");
    let gaps = gaps_of(&view);
    let first = &gaps[0];
    assert_eq!(first["conversation"], json!("group:420042"));
    assert_eq!(first["status"], json!("recovered"), "covered history closes the first gap");
    assert_eq!(first["recoveredCount"], json!(1), "only the missed in-window message is filled");
    assert!(
        gaps[1..].iter().all(|g| g["status"] == json!("unrecovered") && g["reason"] == json!("history-unsupported")),
        "unsupported history must stay unrecovered: {gaps:?}"
    );
    assert!(
        view["unresolvedGapCount"].as_i64().unwrap_or(0) >= 2,
        "reconnecting must not clear unresolved gaps"
    );
    assert_eq!(
        decisions(root.path()).len(),
        1,
        "backfilled history must not wake the conversation: {:?}",
        decisions(root.path())
    );
    let paths = harness.gateway.lock().expect("gateway state").paths.clone();
    assert!(paths.iter().all(|p| !p.contains("send")), "no send action may reach the gateway: {paths:?}");
}

#[test]
fn empty_history_is_unknown_and_not_claimed_as_recovered() {
    let root = TempRoot::new("unknown");
    let harness = start(
        root.path(),
        vec![
            vec![Step::Mention { message_id: 5001, text: "before the drop" }, Step::AwaitReceived { message_id: 5001 }, Step::Wait(2500), Step::History(HistoryPlan::Empty), Step::Drop],
            vec![Step::Wait(60_000)],
        ],
    );
    let settled = wait_for(Duration::from_secs(30), || gaps_of(&admin(harness.console_port)).len() >= 1);
    assert!(settled, "expected a gap after the reconnect");
    thread::sleep(Duration::from_secs(2));
    let view = admin(harness.console_port);
    eprintln!("ticket07 unknown admin view: {view}");
    let gaps = gaps_of(&view);
    assert_eq!(gaps[0]["status"], json!("unknown"));
    assert_eq!(gaps[0]["reason"], json!("history-empty"));
    assert_eq!(gaps[0]["recoveredCount"], json!(0));
    assert_eq!(view["unresolvedGapCount"], json!(1));
    assert!(
        wait_for(Duration::from_secs(10), || harness.gateway.lock().expect("gateway state").paths.iter().any(|p| p == "/send_private_msg")),
        "an unresolved gap must still be reported privately when an owner is configured"
    );
}

/// 在 `until` 满足时硬杀候选进程，再用同一运行目录重启并重复投递同一事件。
fn kill_then_restart(name: &str, event: Step, until: fn(&Path, &Value) -> bool) -> (TempRoot, Value) {
    let root = TempRoot::new(name);
    {
        let mut harness = start(
            root.path(),
            vec![vec![event.clone(), Step::Wait(60_000)]],
        );
        let port = harness.console_port;
        assert!(
            wait_for(Duration::from_secs(30), || until(root.path(), &admin(port)["stages"])),
            "first run never reached the planned crash point"
        );
        harness.child.kill().expect("hard-kill first run");
        let _ = harness.child.wait();
    }
    let harness = start(
        root.path(),
        vec![vec![Step::Wait(1500), event, Step::Wait(60_000)]],
    );
    let restarted = Instant::now();
    let config: Value = serde_json::from_slice(&fs::read(root.path().join("config/config.json")).unwrap()).unwrap();
    let reply = &config["socialV2"]["reply"];
    // A nonurgent replay can wait for wake quiet, then for a send hold. Allow
    // five seconds for scheduler ticks and fixture I/O after those configured maxima.
    let replay_bound = Duration::from_millis(
        reply["wakeQuietMs"].as_u64().unwrap() + reply["quietMaxMs"].as_u64().unwrap() + 5000,
    );
    let port = harness.console_port;
    let seen = wait_for(replay_bound, || admin(port)["duplicates"].as_i64().unwrap_or(0) >= 1);
    assert!(seen, "redelivery of the pre-crash event was not recognised as duplicate");
    eprintln!("ticket119 {name} redelivery: elapsedMs={} turns={} stages={}",
        restarted.elapsed().as_millis(), turns_executed(root.path()), admin(port)["stages"]);
    let mut settled_view = Value::Null;
    let settled = wait_for(replay_bound.saturating_sub(restarted.elapsed()), || {
        settled_view = admin(port);
        let stages = &settled_view["stages"];
        stages["received"].as_u64().unwrap_or(0) == 0
            && stages["processing"].as_u64().unwrap_or(0) == 0
            && stages["handled"].as_u64().unwrap_or(0) + stages["interrupted"].as_u64().unwrap_or(0) >= 1
            && turns_executed(root.path()) >= 1
    });
    assert!(settled, "replay did not settle within {replay_bound:?}: turns={} view={settled_view} decisions={:?}",
        turns_executed(root.path()), decisions(root.path()));
    eprintln!("ticket119 {name} settled: elapsedMs={} boundMs={} turns={} stages={}",
        restarted.elapsed().as_millis(), replay_bound.as_millis(), turns_executed(root.path()), settled_view["stages"]);
    // Retain the original post-redelivery observation period and exact counts.
    thread::sleep(Duration::from_secs(3));
    let view = admin(port);
    eprintln!("ticket07 {name} admin view: {view}");
    drop(harness);
    (root, view)
}

#[test]
fn kill_after_save_before_processing_replays_exactly_one_turn() {
    // Nonurgent bootstrap stays queued during the configured wake quiet window.
    // Mentions intentionally dispatch immediately and cannot expose the saved stage.
    let (root, view) = kill_then_restart(
        "kill-saved",
        Step::Ordinary { message_id: 9101, text: "crash probe" },
        |_, s| s["received"].as_i64().unwrap_or(0) >= 1,
    );
    assert_eq!(turns_executed(root.path()), 1, "saved work must run once after restart, not twice");
    assert_eq!(view["stages"]["received"].as_i64().unwrap_or(0), 0);
    assert_eq!(view["duplicates"], json!(1));
}

#[test]
fn kill_after_processing_started_never_reruns_the_turn() {
    let (root, view) = kill_then_restart("kill-started", Step::Mention { message_id: 9102, text: "crash probe" }, |root, s| {
        // Processing is durable before the next tick starts/logs the turn. Observe the
        // same execution witness counted below before killing, including under delayed ticks.
        s["processing"].as_i64().unwrap_or(0) + s["handled"].as_i64().unwrap_or(0) >= 1
            && turns_executed(root) >= 1
    });
    assert_eq!(turns_executed(root.path()), 1, "a turn that already started must not be re-run");
    assert_eq!(view["stages"]["received"].as_i64().unwrap_or(0), 0);
    assert_eq!(view["duplicates"], json!(1));
}

#[test]
fn slow_history_does_not_stop_live_frames_after_reconnect() {
    let root = TempRoot::new("slow-history");
    let harness = start(
        root.path(),
        vec![
            vec![
                Step::Mention { message_id: 4001, text: "before the drop" },
                Step::AwaitReceived { message_id: 4001 },
                Step::Wait(1500),
                Step::History(HistoryPlan::Slow),
                Step::Drop,
            ],
            vec![Step::AwaitHistory, Step::Mention { message_id: 4002, text: "during slow history" }, Step::Wait(60_000)],
        ],
    );
    // The response is held until after this snapshot; elapsed time cannot establish ordering.
    let mut snapshot = Value::Null;
    let handled_live = wait_for(Duration::from_secs(40), || {
        let view = admin(harness.console_port);
        let live_saved = view["stages"]["received"].as_i64().unwrap_or(0)
            + view["stages"]["processing"].as_i64().unwrap_or(0)
            + view["stages"]["handled"].as_i64().unwrap_or(0);
        let ready = live_saved >= 2 && decisions(root.path()).len() >= 2;
        if ready {
            snapshot = view;
        }
        ready
    });
    eprintln!("ticket07h slow-history admin view (history still pending): {snapshot}");
    assert!(handled_live, "live frame was not handled after reconnect");
    let gaps = gaps_of(&snapshot);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0]["status"], json!("open"), "history call must still be in flight: {gaps:?}");
    let history = harness.gateway.lock().expect("gateway state").slow_history.clone();
    assert!(history.started.load(Ordering::SeqCst), "an actual history request must be in flight");
    history.released.store(true, Ordering::SeqCst);
    assert!(
        wait_for(Duration::from_secs(10), || {
            let gaps = gaps_of(&admin(harness.console_port));
            gaps.len() == 1 && gaps[0]["status"] == "unknown" && gaps[0]["reason"] == "history-empty"
        }),
        "released history response must complete recovery without claiming completeness"
    );
}

fn console_post(port: u16, path: &str, body: &Value) -> Option<(u16, Value)> {
    let (status, text) = console_post_raw(port, path, body)?;
    Some((status, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

fn console_post_raw(port: u16, path: &str, body: &Value) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let payload = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-console-token: {CONSOLE_TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let (_, body) = text.split_once("\r\n\r\n")?;
    Some((status, body.to_string()))
}

#[test]
fn admin_can_reset_gap_retries_through_the_console_route() {
    let root = TempRoot::new("manual-retry");
    let harness = start(
        root.path(),
        vec![
            vec![Step::Mention { message_id: 3001, text: "before the drop" }, Step::AwaitReceived { message_id: 3001 }, Step::Wait(2500), Step::History(HistoryPlan::Empty), Step::Drop],
            vec![Step::Wait(60_000)],
        ],
    );
    let port = harness.console_port;
    assert!(
        wait_for(Duration::from_secs(30), || gaps_of(&admin(port)).first().map(|g| g["attempts"].as_i64().unwrap_or(0)) == Some(1)),
        "gap never recorded its first attempt"
    );
    let before = gaps_of(&admin(port));
    assert!(before[0]["nextRetryMs"].as_i64().unwrap_or(0) > 0, "{before:?}");
    let (status, body) = console_post(port, "/api/inbound/gaps/retry", &json!({"conversation": "group:420042"})).expect("retry route");
    assert_eq!((status, body["reset"].clone()), (200, json!(1)), "{body}");
    let after = gaps_of(&admin(port));
    assert_eq!((after[0]["attempts"].clone(), after[0]["nextRetryMs"].clone()), (json!(0), json!(0)));
    assert_eq!(after[0]["status"], json!("unknown"), "manual retry must not change the gap status");
    let (status, _) = console_post(port, "/api/inbound/gaps/retry", &json!({})).expect("retry route");
    assert_eq!(status, 400);
}

#[path = "inbound_durable_e2e/notices.rs"]
mod notices;

#[test]
fn hotfix127_two_restarts_notice_only_new_windows_and_never_retry_private_gaps() {
    let root = TempRoot::new("hotfix127");
    let db = rusqlite::Connection::open(root.path().join("data/inbound.db")).unwrap();
    db.execute_batch("CREATE TABLE inbound_gaps (
        id INTEGER PRIMARY KEY AUTOINCREMENT, conversation TEXT NOT NULL, from_ms INTEGER NOT NULL,
        to_ms INTEGER NOT NULL, status TEXT NOT NULL, reason TEXT NOT NULL, detail TEXT NOT NULL,
        recovered INTEGER NOT NULL DEFAULT 0, opened_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0, next_retry_ms INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE inbound_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);").unwrap();
    for _ in 0..8 {
        db.execute("INSERT INTO inbound_gaps (conversation, from_ms, to_ms, status, reason, detail, opened_ms, updated_ms, attempts, next_retry_ms)
            VALUES ('private:8808', 1, 2, 'unrecovered', 'private-history-unverified', 'legacy detail', 2, 2, 4, 1)", []).unwrap();
    }
    db.execute("INSERT INTO inbound_meta VALUES ('last_seen_ms', ?1)", [now_ms() - 1000]).unwrap();
    drop(db);

    for restart in 1..=2 {
        let harness = start_with_scope(root.path(), vec![vec![Step::Wait(60_000)]], Some(8808), &[8808]);
        assert!(wait_for(Duration::from_secs(30), || !harness.gateway.lock().unwrap().sends.is_empty()), "missing opening notice");
        let sends = harness.gateway.lock().unwrap().sends.clone();
        let private_notices: Vec<_> = sends.iter().filter(|(_, packet)| packet["message"][0]["data"]["text"].as_str().unwrap_or("").contains("private:8808")).collect();
        assert_eq!(private_notices.len(), 1, "one short private notice for this restart");
        assert_eq!(sends[0].0, "/send_private_msg");
        assert_eq!(sends[0].1["user_id"], json!(8808));
        let body = sends[0].1["message"][0]["data"]["text"].as_str().unwrap();
        assert!(body.contains("private:8808：1 个"), "only this restart's new private window: {body}");
        assert!(!body.contains("10 个") && !body.contains("11 个") && !body.contains("12 个"), "historical gaps re-announced: {body}");
        assert!(body.contains("私聊") && body.contains("重发"), "short actionable private notice: {body}");
        assert!(body.chars().count() < 400, "notice should be short: {body}");
        assert!(wait_for(Duration::from_secs(20), || gaps_of(&admin(harness.console_port)).len() == 8 + 2 * restart));
        // Opening rows precede asynchronous history recovery. Await its persisted
        // attempt before asserting retry counts or killing this owner for restart.
        assert!(
            wait_for(Duration::from_secs(20), || gaps_of(&admin(harness.console_port))
                .iter()
                .any(|gap| gap["id"] == 9 && gap["attempts"].as_i64() == Some(restart as i64))),
            "group recovery must persist its attempt before assertions or restart"
        );
        let gaps = gaps_of(&admin(harness.console_port));
        for gap in gaps.iter().filter(|g| g["conversation"] == "private:8808") {
            let expected = if gap["id"].as_i64().unwrap() <= 8 { 4 } else { 1 };
            assert_eq!(gap["attempts"], expected, "private terminal attempts must stay unchanged: {gap}");
            assert_eq!(gap["nextRetryMs"], 0);
            assert_eq!(gap["status"], "unrecovered");
            if gap["id"].as_i64().unwrap() <= 8 {
                assert_eq!(gap["openedMs"], 2);
                assert_eq!(gap["updatedMs"], 2);
                assert_eq!(gap["detail"], "legacy detail");
                assert_eq!(gap["noticeState"], "unknown", "legacy rows have no new notice intent");
            }
        }
        if restart == 2 {
            let old_group = gaps.iter().find(|g| g["id"] == 9).unwrap();
            assert_eq!(old_group["attempts"], 2, "group gaps still retry");
        }
        drop(harness);
        let db = rusqlite::Connection::open(root.path().join("data/inbound.db")).unwrap();
        db.execute("UPDATE inbound_gaps SET next_retry_ms = 0 WHERE conversation LIKE 'group:%'", []).unwrap();
        db.execute("UPDATE inbound_meta SET value = ?1 WHERE key = 'last_seen_ms'", [now_ms() - 1000]).unwrap();
    }
}

#[test]
fn history_log_records_unallowed_groups_and_own_echoes_before_the_allow_filter() {
    let root = TempRoot::new("history-on");
    let harness = start_with_config(
        root.path(),
        vec![vec![
            Step::Frame(group_frame(OTHER_GROUP, 1002, 7201, "别的群在聊天")),
            Step::Frame(group_frame(GROUP_ID, SELF_ID, 7202, "自己的回声")),
            Step::Mention { message_id: 7203, text: "history probe" },
            Step::Wait(60_000),
        ]],
        Some(8808),
        &[],
        |cfg| cfg["history"] = json!({"enabled": true}),
    );
    let logged = wait_for(Duration::from_secs(30), || history_records(root.path()).len() >= 3);
    assert!(logged, "the history log did not record the three frames");
    let records = history_records(root.path());
    let by_mid = |mid: &str| {
        records
            .iter()
            .find(|record| record["mid"] == mid)
            .unwrap_or_else(|| panic!("no history record for mid {mid}: {records:?}"))
    };
    let other = by_mid("7201");
    assert_eq!(other["conv"], "group:520052");
    assert_eq!(other["dir"], "in");
    assert_eq!(by_mid("7202")["dir"], "out");
    assert_eq!(by_mid("7203")["segs"][0], json!({"k": "at", "id": "9001"}));

    let journal = root.path().join("data/inbound.db");
    assert!(wait_for(Duration::from_secs(10), || journal.exists()), "the journal was not created");
    let db = rusqlite::Connection::open_with_flags(&journal, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open journal read-only");
    let journaled: i64 = db
        .query_row("SELECT COUNT(*) FROM inbound_events WHERE conversation = 'group:520052'", [], |row| row.get(0))
        .expect("count journaled events");
    assert_eq!(journaled, 0, "the journal stays allow-gated");
    drop(harness);
}

#[test]
fn history_log_is_off_by_default() {
    let root = TempRoot::new("history-off");
    let harness = start(
        root.path(),
        vec![vec![Step::Mention { message_id: 7301, text: "history default probe" }, Step::Wait(60_000)]],
    );
    assert!(wait_for(Duration::from_secs(30), || !decisions(root.path()).is_empty()), "the mention was not decided");
    assert!(!root.path().join("data/history").exists(), "history is written only when switched on");
    drop(harness);
}

#[test]
fn replayed_saved_event_is_logged_once() {
    let root = TempRoot::new("history-replay");
    let history_on = |cfg: &mut Value| cfg["history"] = json!({"enabled": true});
    {
        let mut harness = start_with_config(
            root.path(),
            vec![vec![Step::Ordinary { message_id: 9201, text: "history replay probe" }, Step::Wait(60_000)]],
            Some(8808),
            &[],
            history_on,
        );
        let port = harness.console_port;
        assert!(
            wait_for(Duration::from_secs(30), || {
                admin(port)["stages"]["received"].as_i64().unwrap_or(0) >= 1
                    && history_records(root.path()).iter().any(|record| record["mid"] == "9201")
            }),
            "the first run never saved the event and logged it"
        );
        harness.child.kill().expect("hard-kill first run");
        let _ = harness.child.wait();
    }
    // No redelivery: only the journaled copy can reach the restarted process.
    let harness = start_with_config(root.path(), vec![vec![Step::Wait(60_000)]], Some(8808), &[], history_on);
    let config: Value = serde_json::from_slice(&fs::read(root.path().join("config/config.json")).unwrap()).unwrap();
    let reply = &config["socialV2"]["reply"];
    let replay_bound = Duration::from_millis(
        reply["wakeQuietMs"].as_u64().unwrap() + reply["quietMaxMs"].as_u64().unwrap() + 5000,
    );
    assert!(
        wait_for(replay_bound, || turns_executed(root.path()) >= 1),
        "the saved event was not replayed after restart"
    );
    let logged: Vec<Value> = history_records(root.path()).into_iter().filter(|record| record["mid"] == "9201").collect();
    assert_eq!(logged.len(), 1, "a replayed event must not be logged again: {logged:?}");
    drop(harness);
}
