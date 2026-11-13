//! ticket 14 external-behaviour acceptance for the QQ core MCP surface: real `her --mcp-serve` and
//! `her --console-serve` processes on a synthetic runtime root and fake OneBot/DSH HTTP endpoints.
//! No real QQ, model or DSH endpoint is contacted. (The comparison with the frozen JS server is
//! the recorded tool schema; these tests pin prompt behavior and the restart,
//! validity-check, concurrency and persisted-identity contracts.)

#[path = "../src/install_root.rs"]
mod install_root;
mod support;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSOLE_TOKEN: &str = "ticket14-console-token-0001";
const ONEBOT_TOKEN: &str = "ticket14-onebot-token-0001";
const GROUP: i64 = 420042;
const GROUP_KEY: &str = "group:420042";
const GROUP_TOKEN: &str = "ticket14-group-agent-token-aaaa";
const OWNER_KEY: &str = "private:8808";
const OWNER_TOKEN: &str = "ticket14-owner-agent-token-bbbb";

#[test]
fn first_message_typing_budget_delays_gateway_and_rechecks_token() {
    let f = fixture("first-gap", json!({"send":{"firstGapEnabled":true,"firstGapBaseMs":200,"firstGapPerCharMs":500,"fillTypingBudget":false,"burstIntervalMinMs":100,"maxGapMs":2000,"duplicateWindowMs":0}}));
    let start = Instant::now();
    let pending = f.mcp.request_nowait("qq_send_message", group(json!({"messages":["🙂"]})));
    thread::sleep(Duration::from_millis(700));
    assert!(f.onebot.actions("send_group_msg").is_empty(), "first item bypassed the UTF-16 typing budget");
    let answer = f.mcp.finish(pending);
    assert!(!is_error(&answer), "{answer}");
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert_eq!(f.onebot.actions("send_group_msg").len(),1);
    // A second first-item delay must capture its epoch before waiting.
    let pending = f.mcp.request_nowait("qq_send_message", group(json!({"messages":["another"]})));
    thread::sleep(Duration::from_millis(900));
    let mut state = f.root.state();
    state["conversations"][GROUP_KEY]["agentToken"] = json!("typing-token-revoked");
    f.root.write_state(&state);
    assert!(is_error(&f.mcp.finish(pending)));
    assert_eq!(f.onebot.actions("send_group_msg").len(),1);
}

#[test]
fn hf119_mcp_private_hold_returns_new_messages_clears_typing_and_tolerates_status_failure() {
    for cancel in [true,false] {
        // Anchor a full typing budget at hold entry, after MCP/admission/fsync work.
        let f=fixture("hf119-mcp-typing",json!({"reply":{"quietMedianMs":200,"quietMinMs":200,"quietMaxMs":200},
            "send":{"firstGapEnabled":true,"fillTypingBudget":false,"firstGapBaseMs":1000,
                "firstGapPerCharMs":0,"burstIntervalMinMs":0,"maxGapMs":1000,"duplicateWindowMs":0}}));
        let key="private:7707";
        let mut cfg=f.root.config();
        cfg["allow"]["private"]=json!([8808,7707]);
        f.root.write_config(&cfg);
        let mut state=f.root.state();
        state["conversations"][key]=json!({"agentToken":OWNER_TOKEN,"lastIncomingAt":now_ms(),"lastUnreadSeq":0,
            "unread":[],"recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,"triggers":{"atMention":true}}});
        f.root.write_state(&state);
        if !cancel {lock(&f.onebot.replies).insert("set_input_status".into(),json!({"status":"failed","retcode":1,"data":null}));}
        let barrier=f.onebot.hold("set_input_status");
        let request=f.mcp.request_nowait("qq_send_private_message",json!({"userId":7707,"message":"ready","token":OWNER_TOKEN}));
        assert!(wait_for(Duration::from_secs(5),|| !f.onebot.actions("set_input_status").is_empty()));
        assert!(f.onebot.actions("send_private_msg").is_empty(),"typing must start before QQ submission");
        if cancel {
            let mut state=f.root.state();
            state["conversations"][key]["lastUnreadSeq"]=json!(1);
            state["conversations"][key]["lastIncomingAt"]=json!(now_ms());
            state["conversations"][key]["lastReplyIncomingAt"]=json!(now_ms());
            state["conversations"][key]["unread"]=json!([{"seq":1,"text":"another message","isSelf":false,"time":now_ms()}]);
            f.root.write_state(&state);
        }
        barrier.send(()).unwrap();
        let answer=f.mcp.finish(request);
        assert!(!is_error(&answer),"a revision/status failure must remain actionable: {answer}");
        let data:Value=serde_json::from_str(&text_of(&answer)).unwrap();
        assert_eq!(data["reviseRequired"]==true,cancel,"{data}");
        if cancel {assert_eq!(data["newMessages"][0]["text"],"another message");}
        let statuses=f.onebot.actions("set_input_status");
        assert_eq!(statuses,vec![json!({"user_id":7707,"event_type":1}),json!({"user_id":7707,"event_type":0})]);
        assert_eq!(f.onebot.actions("send_private_msg").len(),usize::from(!cancel));
    }
}

#[test]
fn hf119_direct_send_filters_quoted_model_copy_and_keeps_outgoing_original() {
    let f = fixture("hf119-direct-model-copy", json!({"context":{"stripFromModelText":["奸"]}}));
    f.onebot.reply("get_msg", json!({"message_id":7003,"group_id":GROUP,
        "sender":{"user_id":10001,"nickname":"奸昵称"},
        "message":[{"type":"text","data":{"text":"奸原话"}}]}));
    let answer = f.mcp.call("qq_reply", group(json!({"groupId":GROUP,
        "replyToMessageId":7003,"message":"奸待发原文"})));
    assert!(!is_error(&answer), "{answer}");
    let data: Value = serde_json::from_str(&text_of(&answer)).unwrap();
    assert_eq!(data["quoted"]["text"], "原话", "{data}");
    assert_eq!(data["quoted"]["sender"], "昵称", "{data}");
    let sends = f.onebot.actions("send_group_msg");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["message"][1]["data"]["text"], "奸待发原文");
    assert!(f.root.state()["conversations"][GROUP_KEY]["recentMessages"]
        .as_array().unwrap().iter().any(|m| m["isSelf"] == true && m["text"] == "奸待发原文"));
}

/// The real `tools/list` of the frozen JS server (`mcp-snowluma-safe.js`) with every switch on, in registration order.
const JS_TOOL_NAMES: [&str; 38] = [
    "qq_status", "qq_list_groups", "qq_get_group_members", "qq_get_group_history", "qq_send_group_message", "qq_reply",
    "qq_send_private_message", "qq_get_prompt", "qq_get_unread_messages", "qq_get_recent_messages", "qq_social_state",
    "qq_mark_read", "qq_set_wake_config", "qq_send_burst", "qq_send_message", "qq_send_poke", "qq_wait_for_messages",
    "qq_report_feedback", "qq_get_my_recent_messages", "qq_get_message_detail", "qq_get_active_members", "qq_memory_append",
    "qq_memory_query", "qq_memory_remove", "qq_memory_clear", "qq_slang_query", "qq_slang_submit", "qq_get_message_images",
    "qq_list_stickers", "qq_get_sticker_image", "qq_send_sticker", "qq_collect_sticker", "qq_send_voice", "qq_list_voices",
    "qq_get_self_image", "qq_sticker_note", "qq_set_sticker_remark", "qq_get_forward_msg",
];

fn hide(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

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
        thread::sleep(Duration::from_millis(50));
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ----- fake OneBot HTTP gateway ------------------------------------------------------------------------------

struct OneBot {
    port: u16,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    delays: Arc<Mutex<HashMap<String, Duration>>>,
    replies: Arc<Mutex<HashMap<String, Value>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    holds: Arc<Mutex<HashMap<String, std::sync::mpsc::Receiver<()>>>>,
}

fn answer(action: &str, params: &Value) -> Value {
    let ok = |data: Value| json!({"status": "ok", "retcode": 0, "data": data});
    match action {
        "set_input_status" if matches!(params["event_type"].as_i64(),Some(0|1)) && params["user_id"].as_i64().is_some() => ok(json!({})),
        "get_login_info" => ok(json!({"user_id": 9001001, "nickname": "测试机器人"})),
        "get_group_member_list" => ok(json!([{"user_id": 10001, "nickname": "小明", "card": ""}])),
        "get_msg" if params["message_id"] == 7003 => ok(json!({
            "message_id": 7003, "group_id": GROUP, "sender": {"user_id": 10001, "nickname": "小明"},
            "message": [{"type": "text", "data": {"text": "被引用的话"}}]
        })),
        "get_msg" => json!({"status": "failed", "retcode": 1200, "data": null}),
        "send_group_msg" | "send_private_msg" => ok(json!({"message_id": 5000 + now_ms() % 1000})),
        _ => ok(Value::Null),
    }
}

impl OneBot {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake gateway");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let delays: Arc<Mutex<HashMap<String, Duration>>> = Arc::new(Mutex::new(HashMap::new()));
        let replies = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holds = Arc::new(Mutex::new(HashMap::new()));
        let (r2, d2, s2, replies2, h2) = (requests.clone(), delays.clone(), stop.clone(), replies.clone(), holds.clone());
        thread::spawn(move || {
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (r3, d3, replies3, h3) = (r2.clone(), d2.clone(), replies2.clone(), h2.clone());
                        thread::spawn(move || serve_onebot(stream, r3, d3, replies3, h3));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self { port, requests, delays, replies, stop, holds }
    }

    fn actions(&self, name: &str) -> Vec<Value> {
        lock(&self.requests).iter().filter(|(a, _)| a == name).map(|(_, p)| p.clone()).collect()
    }

    // A one-shot response barrier keeps state-change tests independent of scheduler/fsync latency.
    fn hold(&self, action: &str) -> std::sync::mpsc::Sender<()> {
        let (release, wait) = std::sync::mpsc::channel();
        assert!(lock(&self.holds).insert(action.to_string(), wait).is_none());
        release
    }

    fn delay(&self, action: &str, by: Duration) {
        lock(&self.delays).insert(action.to_string(), by);
    }

    fn reply(&self, key: &str, data: Value) {
        lock(&self.replies).insert(key.to_string(), json!({"status":"ok", "retcode":0, "data":data}));
    }

    fn image(&self, file: &str, bytes: &[u8]) {
        use base64::Engine;
        self.reply(&format!("get_image:{file}"), json!({"base64":base64::engine::general_purpose::STANDARD.encode(bytes)}));
    }
}

impl Drop for OneBot {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn serve_onebot(mut stream: TcpStream, requests: Arc<Mutex<Vec<(String, Value)>>>, delays: Arc<Mutex<HashMap<String, Duration>>>, replies: Arc<Mutex<HashMap<String, Value>>>, holds: Arc<Mutex<HashMap<String, std::sync::mpsc::Receiver<()>>>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, length) = loop {
        let Ok(n) = stream.read(&mut chunk) else { return };
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..split]).to_string();
            let length = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse::<usize>().ok()))
                .unwrap_or(0);
            if bytes.len() >= split + 4 + length {
                break (split + 4, length);
            }
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
    let action = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").trim_start_matches('/').to_string();
    let params: Value = serde_json::from_slice(&bytes[head_end..head_end + length]).unwrap_or(Value::Null);
    lock(&requests).push((action.clone(), params.clone()));
    let held_response = lock(&holds).remove(&action);
    if let Some(wait) = held_response {
        wait.recv_timeout(Duration::from_secs(10)).expect("release the fake response after the state change commits");
    }
    let delay = lock(&delays).get(&action).copied();
    if let Some(delay) = delay {
        thread::sleep(delay);
    }
    let key = match action.as_str() {
        "get_image" => format!("get_image:{}", params["file"].as_str().unwrap_or("")),
        "get_forward_msg" => format!("get_forward_msg:{}", params["id"].as_str().unwrap_or("")),
        _ => action.clone(),
    };
    let body = lock(&replies).get(&key).cloned().unwrap_or_else(|| answer(&action, &params)).to_string();
    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
}

// ----- runtime root and processes ------------------------------------------------------------------------------

struct Root(PathBuf);

impl Root {
    fn path(&self) -> &Path {
        &self.0
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.0.join("data/social-v2.json")).expect("read state")).expect("state json")
    }
    fn write_state(&self, state: &Value) {
        fs::write(self.0.join("data/social-v2.json"), serde_json::to_vec_pretty(state).expect("state")).expect("write state");
    }
    fn write_config(&self, config: &Value) {
        fs::write(self.0.join("config/config.json"), serde_json::to_vec_pretty(config).expect("config")).expect("write config");
    }
    fn config(&self) -> Value {
        serde_json::from_slice(&fs::read(self.0.join("config/config.json")).expect("read config")).expect("config json")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn message(seq: i64) -> Value {
    json!({
        "seq": seq, "messageId": (7000 + seq).to_string(), "sender": "小明", "userId": "10001", "text": format!("消息{seq}"),
        "plain": format!("消息{seq}"), "tail": format!("消息{seq}"), "quoteTargetIsSelf": false, "isOwner": false, "ownerLabel": "",
        "isSelf": false, "media": [], "hasMedia": false, "forwardIds": [], "hasForward": false, "time": now_ms() - 60_000 + seq
    })
}

fn conversation(token: &str) -> Value {
    let recent: Vec<Value> = (1..=8).map(message).collect();
    let unread: Vec<Value> = (4..=8).map(message).collect();
    json!({
        "wakeConfig": {
            "mode": "diving", "infinite": true, "sleepUntil": null, "batchWindowMs": 3000, "lastWakeAt": 0, "wakeCount": 0, "noActionCount": 0,
            "confirmedAt": 1_760_000_000_000i64, "confirmedBy": "set_wake_config",
            "triggers": {"atMention": true, "nameMention": false, "question": false, "poke": false, "anyMessage": false, "probability": 0, "keywords": [], "speakerIds": []}
        },
        "recentMessages": recent, "unread": unread, "lastWakeReason": "", "lastAiReplyAt": 0, "lastActionAt": 0, "agentToken": token,
        "bootstrapSent": true, "wakeTimes": [], "sendTimes": [], "lastIncomingAt": now_ms() - 600_000, "preSleepWaitSatisfiedAt": 0,
        "preSleepWaitObservedAt": 0, "preSleepWaitAccumMs": 0, "lastUnreadSeq": 8, "lastReadThroughSeq": 3
    })
}

fn make_root(label: &str, onebot_port: u16, console_port: u16, extra_config: Value) -> Root {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let parent = std::env::temp_dir().join("ticket14 mcp fixtures");
    let parent = crate::install_root::development_root(parent);
    fs::create_dir_all(&parent).expect("fixture parent");
    let root = parent.join(format!("{label}-{}-{}-{n}", std::process::id(), now_ms()));
    let root = crate::install_root::development_root(root);
    for dir in ["config", "data", "resources"] {
        fs::create_dir_all(root.join(dir)).expect("fixture dir");
    }
    let mut config = json!({
        "consoleToken": CONSOLE_TOKEN, "consolePort": console_port, "ownerQQ": 8808,
        "allow": {"groups": [GROUP], "private": ["8808"]},
        // A closed poison URL: the selected dependencies.json must win.
        "snowluma": {"httpUrl": "http://127.0.0.1:1", "accessToken": ONEBOT_TOKEN},
        "socialV2": {
            "send": {"mergeMultiIntoOne": true, "maxMessageChars": 500, "maxSendPerMinute": 20, "maxSendPerHour": 100, "burstIntervalMinMs": 0, "maxGapMs": 0, "firstGapEnabled": false},
            "wait": {"minMs": 100, "defaultMs": 1000, "maxMs": 4000, "minQuietAfterNewMs": 0, "defaultQuietMs": 0}
        }
    });
    if let (Some(base), Some(extra)) = (config["socialV2"].as_object_mut(), extra_config.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    let r = Root(root);
    r.write_config(&config);
    r.write_state(&json!({"paused": false, "conversations": {GROUP_KEY: conversation(GROUP_TOKEN), OWNER_KEY: conversation(OWNER_TOKEN)}}));
    fs::write(r.0.join("data/mode.json"), r#"{"mode":"reserved2"}"#).expect("mode");
    fs::write(
        r.0.join("dependencies.json"),
        json!({"dsh": "http://127.0.0.1:1", "model": "http://127.0.0.1:2", "onebot": format!("http://127.0.0.1:{onebot_port}")}).to_string(),
    )
    .expect("dependencies");
    r
}

struct Console {
    child: Child,
    port: u16,
}

impl Console {
    fn start(root: &Root, port: u16) -> Self {
        Self::start_env(root, port, &[])
    }

    fn command(root: &Root, port: u16, envs: &[(&str, String)]) -> Command {
        let executable = if let Some((_, input)) = envs.iter().find(|(key, _)| *key == "TICKET19_CURL_INPUT") {
            // A byte-identical candidate copy puts the external curl fake in Windows'
            // application-directory search position, without changing the shared candidate.
            let copy = Path::new(input).parent().unwrap().join("her.exe");
            fs::copy(env!("CARGO_BIN_EXE_her"), &copy).unwrap();
            copy
        } else { PathBuf::from(env!("CARGO_BIN_EXE_her")) };
        let mut command = Command::new(executable);
        command
            .args(["--runtime-root"])
            .arg(root.path())
            .args(["--console-serve", "--console-port", &port.to_string()])
            .env_remove("HER_RUNTIME_ROOT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in envs { command.env(key, value); }
        if let Some((_, input)) = envs.iter().find(|(key, _)| *key == "TICKET19_CURL_INPUT") {
            // Windows searches the application/current directory before PATH for curl.exe.
            command.current_dir(Path::new(input).parent().unwrap());
        }
        hide(&mut command);
        command
    }

    fn start_env(root: &Root, port: u16, envs: &[(&str, String)]) -> Self {
        let mut console = Self { child: Self::command(root, port, envs).spawn().expect("start console"), port };
        crate::support::ports::wait_owned_health(&mut console.child, port, Duration::from_secs(20))
            .unwrap_or_else(|error| panic!("console did not start: {error}"));
        console
    }

    fn start_fresh(root: &Root) -> (Self, u16) {
        Self::start_fresh_env(root, &[])
    }

    fn start_fresh_env(root: &Root, envs: &[(&str, String)]) -> (Self, u16) {
        let (child, port) = crate::support::ports::start_console(Duration::from_secs(20), |port| {
            // The selected config names the console port the MCP and live children read.
            let mut config = root.config();
            config["consolePort"] = json!(port);
            root.write_config(&config);
            Self::command(root, port, envs).spawn().expect("start console")
        });
        (Self { child, port }, port)
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>) -> Option<(u16, Value)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
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

struct Mcp {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    answers: Arc<(Mutex<Vec<Value>>, std::sync::Condvar)>,
    next_id: Mutex<i64>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    stderr: PathBuf,
}

impl Mcp {
    fn start(root: &Root, console_port: u16, envs: &[(&str, &str)]) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stderr = root.path().join(format!("mcp-{n}.stderr.log"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .arg("--mcp-serve")
            .arg("--runtime-root")
            .arg(root.path())
            .args(["--console-port", &console_port.to_string()])
            .env_remove("HER_RUNTIME_ROOT")
            .env_remove("QQ_SESSION_KEY")
            .env_remove("QQ_SESSION_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&stderr).expect("capture MCP startup errors"));
        for (k, v) in envs {
            command.env(k, v);
        }
        hide(&mut command);
        let mut child = command.spawn().expect("start mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout: ChildStdout = child.stdout.take().expect("stdout");
        let answers: Arc<(Mutex<Vec<Value>>, std::sync::Condvar)> = Arc::new((Mutex::new(Vec::new()), std::sync::Condvar::new()));
        let sink = answers.clone();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = closed.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    lock(&sink.0).push(v);
                    sink.1.notify_all();
                }
            }
            let _queue = lock(&sink.0);
            done.store(true, std::sync::atomic::Ordering::Release);
            sink.1.notify_all();
        });
        Self { child, stdin: Arc::new(Mutex::new(stdin)), answers, next_id: Mutex::new(0), closed, stderr }
    }

    fn send(&self, value: &Value) {
        let mut stdin = lock(&self.stdin);
        writeln!(stdin, "{value}").expect("write to mcp");
        stdin.flush().expect("flush");
    }

    fn send_raw(&self, text: &str) {
        let mut stdin = lock(&self.stdin);
        writeln!(stdin, "{text}").expect("write to mcp");
        stdin.flush().expect("flush");
    }

    /// Sends a request and returns its id without waiting for the answer.
    fn request_nowait(&self, tool: &str, arguments: Value) -> i64 {
        self.send_request("tools/call", json!({"name": tool, "arguments": arguments}))
    }

    fn send_request(&self, method: &str, params: Value) -> i64 {
        let id = {
            let mut n = lock(&self.next_id);
            *n += 1;
            *n
        };
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// The answer to a request sent earlier (other answers stay queued for their own callers).
    fn finish(&self, id: i64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut queue = lock(&self.answers.0);
        loop {
            if let Some(pos) = queue.iter().position(|m| m["id"] == id) {
                return queue.remove(pos);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero() && !self.closed.load(std::sync::atomic::Ordering::Acquire),
                "no answer to request {id} (stdout closed: {}): {}",
                self.closed.load(std::sync::atomic::Ordering::Acquire),
                fs::read_to_string(&self.stderr).unwrap_or_default().chars().take(4096).collect::<String>());
            queue = self.answers.1.wait_timeout(queue, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    fn request(&self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        self.finish(id)
    }

    fn call(&self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }

    fn initialize(&self) {
        let answer = self.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
        assert_eq!(answer["result"]["serverInfo"]["name"], "snowluma-safe");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text_of(answer: &Value) -> String {
    answer["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

fn is_error(answer: &Value) -> bool {
    answer["result"]["isError"] == true
}

fn group(extra: Value) -> Value {
    let mut args = json!({"key": GROUP_KEY, "token": GROUP_TOKEN});
    if let (Some(base), Some(more)) = (args.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    args
}

struct Fixture {
    // Close candidate processes before removing the synthetic root (Windows locks executables/CWD).
    mcp: Mcp,
    _console: Console,
    onebot: OneBot,
    root: Root,
    console_port: u16,
}

fn fixture(label: &str, extra_config: Value) -> Fixture {
    let onebot = OneBot::start();
    let root = make_root(label, onebot.port, 0, extra_config);
    let (console, console_port) = Console::start_fresh(&root);
    let mcp = Mcp::start(&root, console_port, &[]);
    mcp.initialize();
    Fixture { onebot, root, _console: console, console_port, mcp }
}

// ----- tests --------------------------------------------------------------------------------------------------

#[test]
fn strip_model_text_memory_search_and_digest_index_leave_original_memory_readable() {
    let f = fixture("strip-model-memory", json!({"context":{"stripFromModelText":["奸"]}}));
    let raw = "stripmodel82 奸记忆 [引用 奸人：奸话] [图片]";
    let saved = f.mcp.call("memory_write", group(json!({"content":raw,"subject":"奸话题"})));
    assert!(text_of(&saved).contains("已记住"), "{saved}");
    let result = f.mcp.call("memory_search", group(json!({"query":"stripmodel82"})));
    let text = text_of(&result);
    assert!(text.contains("stripmodel82 记忆 [引用 人：话] [图片]"), "{text}");
    assert!(!text.contains('奸'), "{text}");
    let index = text_of(&f.mcp.call("memory_index", group(json!({}))));
    assert!(index.contains("话题"), "{index}");
    assert!(!index.contains('奸'), "{index}");
    let mut cfg = f.root.config();
    cfg["socialV2"]["context"]["stripFromModelText"] = json!([]);
    f.root.write_config(&cfg);
    let original = text_of(&f.mcp.call("memory_search", group(json!({"query":"stripmodel82"}))));
    assert!(original.contains(raw) && original.contains("奸话题"), "original remains stored: {original}");
}

#[test]
fn strip_model_text_mcp_reads_filter_copies_and_reload_config_without_filtering_sends() {
    let f = fixture("strip-model-text", json!({"context":{"stripFromModelText":["奸"]}}));
    let raw = "奸消息 [引用 奸人：奸原话] [图片] [CQ:at,qq=123]";
    let mut state = f.root.state();
    for field in ["unread", "recentMessages"] {
        for m in state["conversations"][GROUP_KEY][field].as_array_mut().unwrap() {
            m["sender"] = json!("奸昵称");
            m["text"] = json!(raw);
            m["plain"] = json!("奸正文");
            m["tail"] = json!("奸尾部");
            m["isSelf"] = json!(m["seq"] == 8);
        }
    }
    f.root.write_state(&state);
    let before = fs::read(f.root.path().join("data/social-v2.json")).unwrap();
    for (tool, args) in [
        ("qq_wait_for_messages", json!({"timeoutMs":100,"quietMs":0})),
        ("qq_get_unread_messages", json!({})),
        ("qq_get_recent_messages", json!({})),
        ("qq_get_my_recent_messages", json!({})),
        ("qq_get_message_detail", json!({"messageId":"7008"})),
    ] {
        let answer = f.mcp.call(tool, group(args));
        assert!(!is_error(&answer), "{tool}: {answer}");
        let text = text_of(&answer);
        assert!(!text.contains('奸'), "{tool}: {text}");
        assert!(text.contains("昵称"), "{tool}: no message returned: {text}");
        assert!(text.contains("[引用 人：原话] [图片] [CQ:at,qq=123]"), "{tool}: {text}");
        let _: Value = serde_json::from_str(&text).expect("MCP result stays valid JSON");
    }
    // Wait admission now persists its budget; every other stored field must remain unchanged.
    let mut after = f.root.state();
    let budget = after["conversations"][GROUP_KEY].as_object_mut().unwrap().remove("waitBudget").expect("wait reservation");
    assert_eq!(budget["used"], 1);
    assert_eq!(after, serde_json::from_slice::<Value>(&before).unwrap());
    let (status, view) = http(f.console_port, "GET", &format!("/api/socialV2/recent?key={GROUP_KEY}"), &[("x-console-token",CONSOLE_TOKEN)], None).unwrap();
    assert_eq!(status, 200);
    assert!(view.to_string().contains('奸'), "console retains raw message");

    let mut cfg = f.root.config();
    cfg["socialV2"]["context"]["stripFromModelText"] = json!([]);
    f.root.write_config(&cfg);
    assert!(text_of(&f.mcp.call("qq_get_recent_messages", group(json!({})))).contains('奸'));
    cfg["socialV2"]["context"]["stripFromModelText"] = json!(["奸"]);
    f.root.write_config(&cfg);
    fs::write(f.root.path().join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
    f.onebot.reply("get_group_msg_history", json!({"messages":[{"message_id":82,
        "sender":{"nickname":"奸昵称","card":"奸名片"},"raw_message":raw,
        "message":[{"type":"text","data":{"text":raw}},{"type":"image","data":{"file":"image.png"}}]}]}));
    let history = f.mcp.call("qq_get_group_history", json!({"groupId":GROUP}));
    assert!(!is_error(&history), "{history}");
    let text = text_of(&history);
    assert!(!text.contains('奸'), "{text}");
    assert!(text.contains("昵称") && text.contains("名片") && text.contains("image.png"), "{text}");
    let _: Value = serde_json::from_str(&text).unwrap();

    fs::write(f.root.path().join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    let outgoing = "奸原样发送 [引用 奸人：奸原话] [图片]";
    let sent = f.mcp.call("qq_send_message", group(json!({"messages":[outgoing]})));
    assert!(!is_error(&sent), "{sent}");
    assert_eq!(f.onebot.actions("send_group_msg")[0]["message"][0]["data"]["text"], outgoing);
}

#[path = "fixtures/media.rs"]
mod media_fixtures;

fn image_blocks(answer: &Value) -> Vec<&Value> {
    answer["result"]["content"].as_array().unwrap().iter().filter(|b| b["type"] == "image").collect()
}

fn set_media(f: &Fixture, id: &str, media: Value, unread_only: bool) {
    let mut state = f.root.state();
    let msg = json!({"seq":19,"messageId":id,"sender":"媒体发送者","text":"[引用 小明：原话][图片]","replyToMessageId":"7003","media":media});
    state["conversations"][GROUP_KEY]["recentMessages"] = if unread_only { json!([]) } else { json!([msg.clone()]) };
    state["conversations"][GROUP_KEY]["unread"] = json!([msg]);
    f.root.write_state(&state);
}

#[test]
fn media_message_formats_are_real_mcp_images_with_metadata_and_order() {
    use base64::Engine;
    let f = fixture("media-message", json!({"media":{"maxBytes":20*1024*1024}}));
    for (file, bytes, count, kind) in [
        ("big.png", media_fixtures::still(image::ImageFormat::Png,4000,3000,false), 1, "image"),
        ("photo.jpg", media_fixtures::still(image::ImageFormat::Jpeg,1500,900,false), 1, "image"),
        ("anim.gif", media_fixtures::gif(37,40,30), 8, "sticker"),
        ("anim.webp", media_fixtures::webp_animation(9), 8, "image"),
    ] {
        f.onebot.image(file, &bytes);
        let media = json!([{"kind":kind,"file":file,"mimeType":"image/incorrect","summary":"保留描述","subType":"0"}]);
        set_media(&f, file, media.clone(), true);
        let before = fs::read(f.root.path().join("data/social-v2.json")).unwrap();
        let (status, response) = http(f.console_port,"GET",&format!("/api/images/message?key={GROUP_KEY}&messageId={file}"),&[("x-console-token",CONSOLE_TOKEN),("x-agent-token",GROUP_TOKEN)],None).unwrap();
        assert_eq!(status,200,"{response}");
        assert_eq!(response["media"],media);
        assert_eq!(response["images"].as_array().unwrap().len(),count);
        assert_eq!(response["images"][0]["file"],file);
        assert_eq!(response["images"][0]["summary"],"保留描述");
        let answer = f.mcp.call("qq_get_message_images",group(json!({"messageId":file})));
        assert!(!is_error(&answer),"{answer}");
        let blocks = image_blocks(&answer);
        assert_eq!(blocks.len(),count,"{answer}");
        for b in &blocks {
            let out = base64::engine::general_purpose::STANDARD.decode(b["data"].as_str().unwrap()).unwrap();
            assert!(out.len() <= 96 * 1024);
            let img = image::load_from_memory(&out).unwrap();
            assert!(img.width() <=1024 && img.height()<=1024);
            assert!(matches!(b["mimeType"].as_str(),Some("image/png"|"image/jpeg"|"image/webp")));
        }
        if file=="anim.gif" {
            assert!(text_of(&answer).contains("动图，共 37 帧，已均匀抽取 8 帧"));
            for (b,expected) in blocks.iter().zip([0u8,5,10,15,20,25,30,36]) {
                let out = base64::engine::general_purpose::STANDARD.decode(b["data"].as_str().unwrap()).unwrap();
                let img = image::load_from_memory(&out).unwrap().to_rgb8();
                assert!(img.get_pixel(0,0)[0].abs_diff(expected)<=4);
            }
        }
        assert_eq!(fs::read(f.root.path().join("data/social-v2.json")).unwrap(),before);
    }
}

#[test]
fn media_failures_limits_and_unsafe_references_keep_js_fallback_without_details() {
    let f = fixture("media-bad",json!({"media":{"maxCount":2,"maxBytes":1,"maxPixels":1}}));
    f.onebot.image("bad.gif",b"GIF89a damaged data................");
    f.onebot.image("many.gif",&media_fixtures::gif(257,1,1));
    let list=json!([
        {"kind":"image","file":"bad.gif","url":"http://127.0.0.1:1/private?secret=hidden"},
        {"kind":"sticker","file":"many.gif"},
        {"kind":"image","file":"not-requested.png"}
    ]);
    set_media(&f,"bad",list,false);
    let answer=f.mcp.call("qq_get_message_images",group(json!({"messageId":"bad"})));
    assert!(!is_error(&answer),"{answer}"); assert!(image_blocks(&answer).is_empty());
    let text=text_of(&answer);
    assert!(text.contains("（图片获取失败）") && text.contains("（表情获取失败）") && text.contains("超过单条上限 2"),"{text}");
    for forbidden in ["127.0.0.1","secret","damaged","curl","C:","panic"] { assert!(!text.contains(forbidden),"{text}"); }
    assert_eq!(f.onebot.actions("get_image").len(),2);
    set_media(&f,"unsafe",json!([
        {"kind":"image","file":"../secret.png"},
        {"kind":"image","file":"C:\\private.png"}
    ]),false);
    let answer=f.mcp.call("qq_get_message_images",group(json!({"messageId":"unsafe"})));
    assert!(image_blocks(&answer).is_empty());
    assert_eq!(f.onebot.actions("get_image").len(),2,"unsafe files reached gateway");
    let revoked=f.mcp.call("qq_get_message_images",json!({"key":GROUP_KEY,"token":"revoked","messageId":"bad"}));
    assert!(is_error(&revoked)); assert_eq!(f.onebot.actions("get_image").len(),2);
    // Even a configured gateway home outside this selected root cannot expose another root.
    let other=make_root("media-other-root",f.onebot.port,1,json!({}));
    let foreign=other.path().join("resources/foreign.png");
    let bytes=media_fixtures::still(image::ImageFormat::Png,24,18,false);
    fs::write(&foreign,&bytes).unwrap();
    let mut cfg=f.root.config();cfg["snowluma"]["homeDir"]=json!(other.path().display().to_string());f.root.write_config(&cfg);
    f.onebot.reply("get_image:foreign.png",json!({"file":foreign.display().to_string()}));
    set_media(&f,"foreign",json!([{"kind":"image","file":"foreign.png"}]),false);
    let answer=f.mcp.call("qq_get_message_images",group(json!({"messageId":"foreign"})));
    assert!(image_blocks(&answer).is_empty());assert!(!text_of(&answer).contains("foreign.png"));
    assert_eq!(fs::read(&foreign).unwrap(),bytes);
    // The same gateway fallback works when both containment checks are satisfied.
    let local=f.root.path().join("resources/local.png");fs::write(&local,&bytes).unwrap();
    cfg["snowluma"]["homeDir"]=json!(f.root.path().display().to_string());f.root.write_config(&cfg);
    f.onebot.reply("get_image:local.png",json!({"file":local.display().to_string()}));
    set_media(&f,"local",json!([{"kind":"image","file":"local.png"}]),false);
    let answer=f.mcp.call("qq_get_message_images",group(json!({"messageId":"local"})));
    assert_eq!(image_blocks(&answer).len(),1,"{answer}");assert_eq!(fs::read(local).unwrap(),bytes);
}

#[test]
fn media_budgets_count_original_bytes_before_compaction_and_clamp_settings() {
    let f=fixture("media-source-budget",json!({"media":{"maxBytes":1,"maxCount":"0x3","maxPixels":1}}));
    let rgb=image::RgbImage::from_fn(300,300,|x,y| {
        let n=(x.wrapping_mul(747796405)^y.wrapping_mul(2891336453)).wrapping_mul(277803737);
        image::Rgb([n as u8,(n>>8) as u8,(n>>16) as u8])
    });
    let mut encoded=std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rgb).write_to(&mut encoded,image::ImageFormat::Jpeg).unwrap();
    let bytes=encoded.into_inner();
    assert!(bytes.len()*2<=256*1024 && bytes.len()*3>256*1024,"fixture {} bytes",bytes.len());
    f.onebot.image("budget.jpg",&bytes);
    set_media(&f,"budget",json!([
        {"kind":"image","file":"budget.jpg"},{"kind":"image","file":"budget.jpg"},
        {"kind":"image","file":"budget.jpg"},{"kind":"image","file":"budget.jpg"}
    ]),false);
    let answer=f.mcp.call("qq_get_message_images",group(json!({"messageId":"budget"})));
    assert_eq!(image_blocks(&answer).len(),2,"{answer}");
    assert!(text_of(&answer).contains("图片总大小超限") && text_of(&answer).contains("超过单条上限 3"));
    assert_eq!(f.onebot.actions("get_image").len(),3);
    f.onebot.image("pixels.gif",&media_fixtures::gif(1,2000,1000));
    set_media(&f,"pixels",json!([{"kind":"image","file":"pixels.gif"}]),false);
    let answer=f.mcp.call("qq_get_message_images",group(json!({"messageId":"pixels"})));
    assert!(image_blocks(&answer).is_empty());assert!(text_of(&answer).contains("（图片获取失败）"));
}

#[test]
fn media_forward_hierarchy_visibility_and_sampled_images_survive_public_mcp() {
    let f=fixture("media-forward",json!({}));
    let mut state=f.root.state();
    state["conversations"][GROUP_KEY]["recentMessages"]=json!([{"seq":1,"messageId":"7001","text":"[转发]","forwardIds":["root"]}]);
    f.root.write_state(&state);
    f.onebot.image("forward.gif",&media_fixtures::gif(10,16,12));
    f.onebot.reply("get_forward_msg:root",json!({"messages":[{"sender":{"nickname":"外层发送者","user_id":10001},"message_id":9001,"time":1760000000,"content":[
        {"type":"reply","data":{"id":"7003"}},
        {"type":"image","data":{"file":"forward.gif"}},
        {"type":"forward","data":{"id":"nested"}}
    ]}]}));
    f.onebot.reply("get_forward_msg:nested",json!({"messages":[{"sender":{"nickname":"内层发送者"},"content":[{"type":"text","data":{"text":"嵌套原话"}},{"type":"forward","data":{"id":"deep"}}]}]}));
    f.onebot.reply("get_forward_msg:deep",json!({"messages":[]}));
    let unknown=f.mcp.call("qq_get_forward_msg",group(json!({"id":"unknown"})));
    assert!(is_error(&unknown)); assert!(f.onebot.actions("get_forward_msg").is_empty());
    let answer=f.mcp.call("qq_get_forward_msg",group(json!({"id":"root"})));
    assert!(!is_error(&answer),"{answer}"); assert_eq!(image_blocks(&answer).len(),8);
    let text=answer["result"]["content"].as_array().unwrap().iter().filter_map(|b|b["text"].as_str()).collect::<Vec<_>>().join("\n");
    for expected in ["外层发送者","内层发送者","嵌套原话","nestedPreviews","9001","[引用]","共 10 帧"] { assert!(text.contains(expected),"missing {expected}: {text}"); }
    assert!(!is_error(&f.mcp.call("qq_get_forward_msg",group(json!({"id":"deep"})))));
    let denied=f.mcp.call("qq_get_forward_msg",json!({"key":OWNER_KEY,"token":OWNER_TOKEN,"id":"deep"}));
    assert!(is_error(&denied),"nested id leaked across conversations");
}

#[test]
fn media_concurrent_results_keep_each_messages_original_bytes() {
    use base64::Engine;
    let f=fixture("media-concurrent",json!({}));
    let a=media_fixtures::still(image::ImageFormat::Png,24,18,false);
    let b=media_fixtures::still(image::ImageFormat::Png,31,19,true);
    f.onebot.image("a.png",&a); f.onebot.image("b.png",&b);
    let mut state=f.root.state();
    state["conversations"][GROUP_KEY]["recentMessages"]=json!([
        {"seq":1,"messageId":"a","media":[{"kind":"image","file":"a.png"}]},
        {"seq":2,"messageId":"b","media":[{"kind":"image","file":"b.png"}]}
    ]); f.root.write_state(&state);
    for _ in 0..4 {
        let ia=f.mcp.request_nowait("qq_get_message_images",group(json!({"messageId":"a"})));
        let ib=f.mcp.request_nowait("qq_get_message_images",group(json!({"messageId":"b"})));
        for (id,expected) in [(ia,&a),(ib,&b)] {
            let answer=f.mcp.finish(id); assert!(!is_error(&answer),"{answer}");
            let blocks=image_blocks(&answer); assert_eq!(blocks.len(),1);
            assert_eq!(&base64::engine::general_purpose::STANDARD.decode(blocks[0]["data"].as_str().unwrap()).unwrap(),expected);
        }
    }
}

#[test]
fn review_raw_inbound_images_and_forwards_reach_real_mcp() {
    use base64::Engine;
    use futures_util::{SinkExt, StreamExt};
    let f = fixture("review-live-media", json!({"proactive":{"enabled":false}}));
    fs::create_dir_all(f.root.path().join("user/dsh")).unwrap();
    fs::write(f.root.path().join("user/dsh/.credentials.yaml"), "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n").unwrap();
    let bytes = media_fixtures::still(image::ImageFormat::Png, 24, 18, false);
    f.onebot.image("live.png", &bytes);
    f.onebot.reply("get_forward_msg:live-forward", json!({"messages":[{
        "sender":{"nickname":"live sender"}, "content":[{"type":"image","data":{"file":"live.png"}}]
    }]}));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let ws_port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let mut dependencies: Value = serde_json::from_slice(&fs::read(f.root.path().join("dependencies.json")).unwrap()).unwrap();
    dependencies["onebot_ws"] = json!(format!("ws://127.0.0.1:{ws_port}"));
    fs::write(f.root.path().join("dependencies.json"), dependencies.to_string()).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({
                "post_type":"message", "message_type":"group", "group_id":GROUP,
                "self_id":401, "user_id":10001, "message_id":9919, "time":1760000000,
                "sender":{"user_id":10001,"nickname":"live sender"},
                "message":[{"type":"image","data":{"file":"live.png"}},
                    {"type":"forward","data":{"id":"live-forward"}}]
            }).to_string())).await.unwrap();
            tokio::pin!(stopped);
            loop { tokio::select! {
                _ = &mut stopped => break,
                frame = ws.next() => if frame.is_none() { break; },
            }}
        });
    });
    struct Live(Child);
    impl Drop for Live { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command.arg("--runtime-root").arg(f.root.path()).args(["--live", "--live-groups", GROUP_KEY, "--console-port", &f.console_port.to_string()])
        .env_remove("HER_RUNTIME_ROOT").stdout(Stdio::from(fs::File::create(f.root.path().join("live-out.log")).unwrap()))
        .stderr(Stdio::from(fs::File::create(f.root.path().join("live-err.log")).unwrap()));
    hide(&mut command);
    let live = Live(command.spawn().unwrap());
    assert!(wait_for(Duration::from_secs(20), || f.root.state()["conversations"][GROUP_KEY]["recentMessages"].as_array().unwrap().iter().any(|m| m["messageId"] == "9919")), "raw event never stored");
    let image = f.mcp.call("qq_get_message_images", group(json!({"messageId":"9919"})));
    let blocks = image_blocks(&image);
    assert_eq!(blocks.len(), 1, "new inbound image lost: {image}");
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(blocks[0]["data"].as_str().unwrap()).unwrap(), bytes);
    let forward = f.mcp.call("qq_get_forward_msg", group(json!({"id":"live-forward"})));
    assert!(!is_error(&forward), "new inbound forward refused: {forward}");
    assert_eq!(image_blocks(&forward).len(), 1, "{forward}");
    drop(live); let _ = stop.send(()); server.join().unwrap();
}

#[test]
fn review_concurrent_gateway_media_is_admitted_before_source_acquisition() {
    let f = fixture("review-source-admission", json!({"media":{"maxBytes":20*1024*1024}}));
    let bytes = media_fixtures::still(image::ImageFormat::Png, 24, 18, false);
    f.onebot.image("admitted.png", &bytes);
    f.onebot.delay("get_image", Duration::from_secs(2));
    set_media(&f, "admission", json!([{"kind":"image","file":"admitted.png"}]), false);
    let ids: Vec<_> = (0..24).map(|_| f.mcp.request_nowait("qq_get_message_images", group(json!({"messageId":"admission"})))).collect();
    let mut successes = 0;
    let mut refused = 0;
    for id in ids {
        let result = f.mcp.finish(id);
        assert!(!is_error(&result), "{result}");
        if image_blocks(&result).is_empty() {
            assert!(text_of(&result).contains("（图片获取失败）"), "overload exposed a different failure: {result}");
            refused += 1;
        } else { successes += 1; }
    }
    let acquired = f.onebot.actions("get_image").len();
    assert!((1..=4).contains(&acquired), "{acquired} gateway base64 responses acquired before admission");
    assert_eq!(successes, acquired);
    assert_eq!(successes + refused, 24);
    assert!(refused >= 20);
    // Every permit is returned, including on overload and decoder failure.
    let again = f.mcp.call("qq_get_message_images", group(json!({"messageId":"admission"})));
    assert_eq!(image_blocks(&again).len(), 1, "{again}");
}

/// Mock the external curl executable only. URL/IP guards, size checks, normalizer,
/// HTTP routes, sticker gateway/cache and MCP process remain the real candidate.
#[cfg(windows)]
#[test]
fn media_sticker_and_face_routes_normalize_downloads_and_hide_bad_data() {
    let onebot=OneBot::start();
    let root=make_root("media-sticker",onebot.port,0,json!({}));
    let dir=root.path().join("resources/fake-curl"); fs::create_dir_all(&dir).unwrap();
    let source=dir.join("curl.cs"); let exe=dir.join("curl.exe"); let input=dir.join("input.bin");
    fs::write(&source,r#"using System; using System.IO;
class Curl { static int Main(string[] args) {
  string output=null, headers=null;
  for(int i=0;i+1<args.Length;i++) { if(args[i]=="-o")output=args[i+1]; if(args[i]=="-D")headers=args[i+1]; }
  string input=Environment.GetEnvironmentVariable("TICKET19_CURL_INPUT");
  File.WriteAllText(Path.Combine(Path.GetDirectoryName(input),"acquire-"+Guid.NewGuid()+".log"),String.Join(" ",args));
  if(File.Exists(input+".delay"))System.Threading.Thread.Sleep(2000);
  File.WriteAllBytes(output,File.ReadAllBytes(input));
  File.WriteAllText(headers,"HTTP/1.1 200 OK\r\nContent-Type: image/gif\r\n\r\n");
  Console.Write("200"); return File.Exists(input+".fail") ? 7 : 0;
} }"#).unwrap();
    let mut csc=Command::new("C:/Windows/Microsoft.NET/Framework64/v4.0.30319/csc.exe");
    csc.args(["/nologo","/target:exe"]).arg(format!("/out:{}",exe.display())).arg(&source);
    hide(&mut csc);
    let compiled=csc.output().unwrap(); assert!(compiled.status.success(),"{}",String::from_utf8_lossy(&compiled.stdout));
    onebot.reply("fetch_custom_face_detail",json!([{"id":"sticker19","url":"http://8.8.8.8/ticket19.gif","desc":"测试贴纸"}]));
    onebot.reply("fetch_face_entity",json!({"url":"http://8.8.8.8/face.gif","q_des":"实体表情"}));
    let bytes=media_fixtures::gif(37,40,30); fs::write(&input,&bytes).unwrap();
    let (console,port)=Console::start_fresh_env(&root,&[
        ("PATH",format!("{};{}",dir.display(),std::env::var("PATH").unwrap_or_default())),
        ("TICKET19_CURL_INPUT",input.display().to_string())
    ]);
    let mcp=Mcp::start(&root,port,&[]); mcp.initialize();
    let f=Fixture{onebot,root,_console:console,console_port:port,mcp};
    let answer=f.mcp.call("qq_get_sticker_image",group(json!({"stickerId":"sticker19"})));
    assert!(!is_error(&answer),"{answer}"); assert_eq!(image_blocks(&answer).len(),8);
    assert!(answer.to_string().contains("动图，共 37 帧，已均匀抽取 8 帧"));
    assert_eq!(fs::read(&input).unwrap(),bytes);
    fs::write(input.with_extension("bin.fail"),b"fail external transport after a partial body").unwrap();
    let interrupted=f.mcp.call("qq_get_sticker_image",group(json!({"stickerId":"sticker19"})));
    assert!(is_error(&interrupted),"failed curl process must not deliver a partial body: {interrupted}");
    fs::remove_file(input.with_extension("bin.fail")).unwrap();
    set_media(&f,"face",json!([{"kind":"face","faceId":"14","summary":"兜底"}]),false);
    let answer=f.mcp.call("qq_get_message_images",group(json!({"messageId":"face"})));
    assert_eq!(image_blocks(&answer).len(),8,"{answer}");
    assert!(text_of(&answer).contains("[表情:实体表情]"));
    // Exercise the same segment extractor as chat/live ingress, then prove the
    // actual URL and emoji CDN acquisition paths using only the external fake.
    let mut state = f.root.state();
    state["conversations"][GROUP_KEY]["recentMessages"].as_array_mut().unwrap().push(json!({"seq":99,"forwardIds":["emoji-fallback"]}));
    f.root.write_state(&state);
    f.onebot.reply("get_forward_msg:emoji-fallback", json!({"messages":[{"content":[
        {"type":"image","data":{"file":"missing.png","sub_type":0,"emoji_id":"aabbccddeeff00112233445566778899","summary":"emoji fallback"}},
        {"type":"mface","data":{"url":"http://8.8.8.8/market.gif","summary":"market"}}
    ]}]}));
    let emoji = f.mcp.call("qq_get_forward_msg", group(json!({"id":"emoji-fallback"})));
    assert_eq!(image_blocks(&emoji).len(),16,"URL/CDN fallback lost: {emoji}");
    let acquired_urls = || fs::read_dir(&dir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with("acquire-")).map(|e| fs::read_to_string(e.path()).unwrap()).collect::<Vec<_>>();
    let trace = acquired_urls().join("\n");
    assert!(trace.contains("https://gxh.vip.qq.com/club/item/parcel/item/aa/aabbccddeeff00112233445566778899/raw300.gif"),"{trace}");
    assert!(trace.contains("http://8.8.8.8/market.gif"),"{trace}");
    // URL acquisition uses the same process-wide source-byte budget as base64.
    let before = acquired_urls().len();
    fs::write(input.with_extension("bin.delay"), b"delay fake download").unwrap();
    let ids: Vec<_> = (0..24).map(|_| f.mcp.request_nowait("qq_get_sticker_image", group(json!({"stickerId":"sticker19"})))).collect();
    let mut refused = 0;
    for id in ids {
        let result = f.mcp.finish(id);
        if image_blocks(&result).is_empty() {
            assert!(is_error(&result) && text_of(&result).contains("（表情获取失败）"), "{result}");
            refused += 1;
        }
    }
    let acquired = acquired_urls().len() - before;
    assert!((1..=4).contains(&acquired), "{acquired} downloads started before admission");
    assert_eq!(acquired + refused,24);
    fs::remove_file(input.with_extension("bin.delay")).unwrap();
    let mut corrupt=bytes.clone();corrupt.truncate(corrupt.len()-8);
    fs::write(&input,corrupt).unwrap();
    let bad=f.mcp.call("qq_get_sticker_image",group(json!({"stickerId":"sticker19"})));
    assert!(is_error(&bad)); assert!(image_blocks(&bad).is_empty());
    assert!(text_of(&bad).contains("（表情获取失败）"),"{bad}");
    assert!(!text_of(&bad).contains("8.8.8.8") && !text_of(&bad).contains("fake-curl"));
    let face=f.mcp.call("qq_get_message_images",group(json!({"messageId":"face"})));
    assert!(image_blocks(&face).is_empty()); assert!(text_of(&face).contains("[表情:实体表情]"));
    fs::write(&input,media_fixtures::still(image::ImageFormat::Png,2000,1000,false)).unwrap();
    let mut cfg=f.root.config();cfg["socialV2"]["media"]=json!({"maxPixels":1_000_000});f.root.write_config(&cfg);
    assert!(is_error(&f.mcp.call("qq_get_sticker_image",group(json!({"stickerId":"sticker19"})))));
}

#[test]
fn media_self_image_is_compact_real_mcp_content_and_keeps_original() {
    use base64::Engine;
    let f = fixture("media-self", json!({}));
    let mut original = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(4000, 3000, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    })).write_to(&mut original, image::ImageFormat::Png).unwrap();
    let path = f.root.path().join("resources/assets/deepseek娘.png");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, original.get_ref()).unwrap();
    let answer = f.mcp.call("qq_get_self_image", group(json!({})));
    assert!(!is_error(&answer), "{answer}");
    let blocks = answer["result"]["content"].as_array().unwrap();
    let images: Vec<_> = blocks.iter().filter(|b| b["type"] == "image").collect();
    assert_eq!(images.len(), 1, "{answer}");
    let bytes = base64::engine::general_purpose::STANDARD.decode(images[0]["data"].as_str().unwrap()).unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1024, 768));
    assert_eq!(images[0]["mimeType"], "image/jpeg");
    assert!(bytes.len() <= 96 * 1024, "{} bytes", bytes.len());
    assert_eq!(fs::read(path).unwrap(), *original.get_ref());
    println!("media self: 4000x3000 PNG {} bytes -> 1024x768 JPEG {} bytes; original unchanged", original.get_ref().len(), bytes.len());
}

#[test]
fn agent_status_dsh_readiness_tracks_authenticated_settings_describe() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    let onebot = OneBot::start();
    // Keep the DSH listener bound before selecting the console port. Otherwise the OS
    // can reuse that released port for DSH and route console health GETs into this fake.
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fake DSH");
    listener.set_nonblocking(true).unwrap();
    let dsh_port = listener.local_addr().unwrap().port();
    let root = make_root("status-dsh-readiness", onebot.port, 0, json!({}));
    let secret = [7u8; 32];
    let credential_dir = root.path().join("user/dsh");
    fs::create_dir_all(&credential_dir).unwrap();
    let credential_file = credential_dir.join(".credentials.yaml");
    let credentials = format!("secret: \"{}\"\n", URL_SAFE_NO_PAD.encode(secret));
    fs::write(&credential_file, &credentials).unwrap();
    fs::write(root.path().join("dependencies.json"), json!({
        "dsh": format!("http://127.0.0.1:{dsh_port}"), "model": "http://127.0.0.1:2",
        "onebot": format!("http://127.0.0.1:{}", onebot.port)
    }).to_string()).unwrap();
    // The selected dependency must win over config, even when config points elsewhere.
    let mut config = root.config();
    config["dsh"] = json!({"baseUrl": "http://127.0.0.1:1"});
    root.write_config(&config);

    let reply = Arc::new(Mutex::new((200, json!({"type": "server-response", "result": {"ok": true, "value": {"namespaces": []}}}).to_string())));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (reply2, calls2, stop2) = (reply.clone(), calls.clone(), stop.clone());
    let server = thread::spawn(move || {
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            // Winsock accept inherits the listener's mode. Request parsing must wait for
            // bytes still in flight; the existing read timeout remains the watchdog.
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" { break; }
                head.push_str(&line);
            }
            let length = head.lines().find_map(|line| line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())).unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let envelope: Value = serde_json::from_slice(&body).unwrap();
            assert!(head.starts_with("POST /api/settings/describe HTTP/1.1"), "{head}");
            assert_eq!(envelope["method"], "settings/describe");
            assert_eq!(envelope["payload"]["args"], json!({}));
            let cookie = head.lines().find_map(|line| line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("cookie"))
                .map(|(_, value)| value.trim())).expect("authenticated probe");
            let (name, value) = cookie.split_once('=').unwrap();
            let authority = format!("127.0.0.1:{dsh_port}");
            assert_eq!(name, format!("dsh-auth-{}", URL_SAFE_NO_PAD.encode(Sha256::digest(authority.as_bytes()))));
            let parts: Vec<_> = value.split('.').collect();
            assert_eq!(parts[0], "v1");
            let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
            mac.update(parts[1].as_bytes());
            mac.verify_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).expect("selected credential signature");
            let payload: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
            assert_eq!(payload["authority"], authority);
            let (status, mut body) = lock(&reply2).clone();
            if let Ok(mut response) = serde_json::from_str::<Value>(&body) {
                if response["type"] == "server-response" && response.get("rpcId").is_none() {
                    response["rpcId"] = envelope["rpcId"].clone();
                    body = response.to_string();
                }
            }
            lock(&calls2).push(envelope);
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let (_console, console_port) = Console::start_fresh(&root);
    assert_ne!(console_port, dsh_port, "independent fake services need distinct ports");
    let check = |expected: bool| {
        for headers in [
            vec![("x-console-token", CONSOLE_TOKEN), ("x-agent-call", "1")],
            vec![("x-console-token", CONSOLE_TOKEN), ("x-agent-token", GROUP_TOKEN)],
        ] {
            let (status, body) = http(console_port, "GET", "/api/status", &headers, None).expect("agent status");
            assert_eq!(status, 200, "{body}");
            assert_eq!(body, json!({"mode": "reserved2", "role": null, "roleMode": "active", "dshReady": expected, "socialV2Paused": false}));
        }
    };
    check(true);
    for response in [
        (200, json!({"type": "server-response", "result": {"ok": false, "error": {"code": "unavailable"}}}).to_string()),
        (401, "{}".to_string()),
        (200, "not JSON".to_string()),
    ] {
        *lock(&reply) = response;
        check(false);
    }
    *lock(&reply) = (200, json!({"type": "server-response", "result": {"ok": true, "value": {"namespaces": []}}}).to_string());
    check(true);
    // Ticket 25: a 401 is answered with one re-authentication and one repeat of the same request, so the two routes make
    // 5 probes each plus one repeat each for the 401 step (12), never more.
    assert_eq!(lock(&calls).len(), 12, "both agent routes must probe actual DSH readiness (the 401 step is repeated once)");
    fs::remove_file(&credential_file).unwrap();
    check(false);
    fs::write(&credential_file, credentials).unwrap();
    check(true);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
    check(false);
}

#[test]
fn get_prompt_recommends_js_proactive_defaults_and_preserves_explicit_config() {
    for (label, proactive, expected) in [
        ("default", None, json!({
            "enabled": true, "checkIntervalMinMs": 1_800_000, "checkIntervalMaxMs": 5_400_000,
            "idleThresholdMs": 900_000, "probability": 0.3
        })),
        ("explicit", Some(json!({"enabled": false, "probability": 0.7})), json!({"enabled": false, "probability": 0.7})),
    ] {
        let mut extra = json!({"sticker": {"enabled": false}, "voice": {"enabled": false}});
        if let Some(proactive) = proactive {
            extra["proactive"] = proactive;
        }
        let f = fixture(&format!("prompt-proactive-{label}"), extra);
        let answer = f.mcp.call("qq_get_prompt", group(json!({})));
        assert!(!is_error(&answer), "{answer}");
        let prompt: Value = serde_json::from_str(&text_of(&answer)).expect("prompt JSON");
        assert_eq!(prompt["recommended"]["proactive"], expected, "{label}");
    }
    let f = fixture("prompt-no-recommendations", json!({"provideRecommendations": false}));
    let answer = f.mcp.call("qq_get_prompt", group(json!({})));
    assert!(!is_error(&answer), "{answer}");
    let prompt: Value = serde_json::from_str(&text_of(&answer)).expect("prompt JSON");
    assert_eq!(prompt["recommended"], Value::Null);
}

#[test]
fn get_prompt_reply_timing_describes_bridge_enforcement_and_override_fallback() {
    for (label, reply, median, min, max) in [
        ("default", json!({}), 10000,4000,25000),
        ("override", json!({"quietMedianMs":6000,"quietMinMs":2000,"quietMaxMs":12000}),6000,2000,12000),
        ("invalid", json!({"quietMedianMs":"bad","quietMinMs":-1,"quietMaxMs":null}),10000,4000,25000),
        ("reversed", json!({"quietMinMs":20000,"quietMaxMs":5000}),10000,4000,25000),
    ] {
        let f=fixture(&format!("prompt-timing-{label}"),json!({"reply":reply}));
        let answer=f.mcp.call("qq_get_prompt",group(json!({})));
        assert!(!is_error(&answer),"{answer}");
        let prompt:Value=serde_json::from_str(&text_of(&answer)).unwrap();
        assert_eq!(prompt["replyTiming"],json!({"enforcedBy":"bridge","quietMedianMs":median,
            "quietMinMs":min,"quietMaxMs":max,"typing":true,"revisionLimit":3}),"{label}");
    }
}

#[test]
fn get_prompt_sticker_total_and_context_belong_only_to_the_selected_root() {
    let a = fixture("prompt-stickers-a", json!({"sticker": {"enabled": true}}));
    let b = fixture("prompt-stickers-b", json!({"sticker": {"enabled": true}}));
    // Different synthetic stores make this fail even on a host with no legacy library.
    let a_bytes = br#"[{"id":"only-a","desc":"alpha fixture"}]"#;
    let b_bytes = br#"[{"id":"only-b-1","desc":"beta fixture"},{"id":"only-b-2","desc":"second beta"}]"#;
    fs::write(a.root.path().join("data/stickers.json"), a_bytes).unwrap();
    fs::write(b.root.path().join("data/stickers.json"), b_bytes).unwrap();
    for (f, total, own, other) in [(&a, 1, "alpha fixture", "beta fixture"), (&b, 2, "beta fixture", "alpha fixture")] {
        let answer = f.mcp.call("qq_get_prompt", group(json!({})));
        assert!(!is_error(&answer), "{answer}");
        let prompt: Value = serde_json::from_str(&text_of(&answer)).expect("prompt JSON");
        assert_eq!(prompt["stickers"]["total"], total);
        assert!(prompt["stickers"]["context"].as_str().unwrap().contains(own));
        assert!(!prompt["stickers"]["context"].as_str().unwrap().contains(other));
    }
    assert_eq!(fs::read(a.root.path().join("data/stickers.json")).unwrap(), a_bytes);
    assert_eq!(fs::read(b.root.path().join("data/stickers.json")).unwrap(), b_bytes);
    fs::remove_file(a.root.path().join("data/stickers.json")).unwrap();
    let answer = a.mcp.call("qq_get_prompt", group(json!({})));
    assert!(!is_error(&answer), "{answer}");
    let prompt: Value = serde_json::from_str(&text_of(&answer)).expect("prompt JSON");
    assert_eq!(prompt["stickers"]["total"], 0, "missing selected store never falls back to legacy data");
    assert_eq!(prompt["stickers"]["context"], "");
}

#[test]
fn get_prompt_sticker_strategy_follows_include_in_prompt_even_when_disabled() {
    // Frozen JS sticker-lib.js::buildStickerStrategyHint, including every line.
    const JS_STRATEGY: &str = "【表情包策略：像真人一样用，不刷屏】
- 合适时机：被戳中笑点/槽点、接梗、怼人、赞同、自嘲、安慰、无语、赢了/输了、告别/晚安、别人发了表情时回一张，都可以自然用。
- 频率：普通闲聊不用每条都配；大约每 3~5 轮来一张就够，热闹/玩梗时可以更密，但不要连续刷屏。
- 选择：优先用备注（desc）和你的记忆（localNote/tags）能准确对上语境的；没有备注/不确定的表情，先 qq_get_sticker_image 看图再决定，不要瞎发。
- 发送：用 qq_send_sticker；一条消息只能是一张表情，不能在同一气泡里附带文字；想说的话先用 qq_send_message / qq_reply 作为单独气泡发出，再单独发表情。需要引用/点名时传 replyToMessageId / atUserId（群聊）。
- 不要：在严肃/正式/敏感话题硬塞表情；不要每次都用同一个；不要一条消息里塞多个表情；不要把文字和表情混在同一个气泡里；不要把表情包当回复的唯一内容（偶尔可以，但别让群友觉得你在敷衍）。
- 学习：看到新表情不确定含义时，先用 qq_get_sticker_image 看图，再用 qq_sticker_note 记下你的理解，下次就能更准地选。";
    for (label, sticker, expected) in [
        ("disabled-included", json!({"enabled": false}), JS_STRATEGY),
        ("disabled-excluded", json!({"enabled": false, "includeInPrompt": false}), ""),
        ("enabled-excluded", json!({"enabled": true, "includeInPrompt": false}), ""),
        ("defaults", json!({}), JS_STRATEGY),
    ] {
        let f = fixture(&format!("prompt-strategy-{label}"), json!({"sticker": sticker}));
        fs::write(f.root.path().join("data/stickers.json"), br#"[{"id":"fixture","desc":"selected fixture"}]"#).unwrap();
        let answer = f.mcp.call("qq_get_prompt", group(json!({})));
        assert!(!is_error(&answer), "{answer}");
        let prompt: Value = serde_json::from_str(&text_of(&answer)).expect("prompt JSON");
        assert_eq!(prompt["stickers"]["strategy"], expected, "{label}");
        assert_eq!(prompt["stickers"]["total"], 1);
        if expected.is_empty() {
            assert_eq!(prompt["stickers"]["context"], "", "{label}");
        } else {
            assert!(prompt["stickers"]["context"].as_str().unwrap().contains("selected fixture"), "{label}");
        }
    }
}

#[test]
fn get_prompt_enabled_tools_match_js_order_and_default_voice_flag() {
    const BASE: [&str; 23] = [
        "qq_get_prompt", "qq_get_unread_messages", "qq_get_recent_messages", "qq_social_state",
        "qq_send_group_message", "qq_send_private_message", "qq_reply", "qq_send_burst", "qq_send_message",
        "qq_wait_for_messages", "qq_report_feedback", "qq_get_my_recent_messages", "qq_get_message_detail",
        "qq_get_active_members", "qq_set_wake_config", "qq_mark_read",
        "qq_memory_append / qq_memory_query / qq_memory_remove / qq_memory_clear",
        "qq_slang_query", "qq_slang_submit", "qq_get_message_images", "qq_get_forward_msg", "qq_send_poke", "qq_get_self_image",
    ];
    for (label, config, with_voice, with_stickers) in [
        ("parity", json!({"sticker": {"enabled": false}, "voice": {"enabled": false}}), false, false),
        ("voice-enabled-default-flag", json!({"sticker": {"enabled": false}, "voice": {"enabled": true}}), false, false),
        ("explicit-voice", json!({"sticker": {"enabled": false}, "voice": {"enabled": false}, "tools": {"sendVoice": true}}), true, false),
        ("explicit-no-voice", json!({"sticker": {"enabled": false}, "voice": {"enabled": true}, "tools": {"sendVoice": false}}), false, false),
        ("sticker-default-flags", json!({"sticker": {"enabled": true}, "voice": {"enabled": false}}), false, true),
    ] {
        let f = fixture(&format!("prompt-tools-{label}"), config);
        let answer = f.mcp.call("qq_get_prompt", group(json!({})));
        assert!(!is_error(&answer), "{answer}");
        let prompt: Value = serde_json::from_str(&text_of(&answer)).expect("prompt JSON");
        let mut expected = BASE.to_vec();
        if with_stickers {
            expected.splice(22..22, ["qq_list_stickers", "qq_get_sticker_image", "qq_send_sticker", "qq_sticker_note", "qq_collect_sticker"]);
        }
        if with_voice {
            expected.push("qq_send_voice / qq_list_voices");
        }
        assert_eq!(prompt["enabledTools"], json!(expected), "{label}");
    }
}

#[test]
fn tool_registry_is_the_js_registration_in_order_and_follows_the_config_switches() {
    let f = fixture("registry", json!({"sticker": {"enabled": true}, "voice": {"enabled": true}}));
    let names: Vec<String> = f.mcp.request("tools/list", json!({}))["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect();
    let qq: Vec<&str> = names.iter().map(String::as_str).filter(|n| n.starts_with("qq_")).collect();
    assert_eq!(qq, JS_TOOL_NAMES, "qq_* tools: same names in the same order as the JS server");
    assert_eq!(names.len(), 38 + 7, "the 7 memory/settings tools are the only extras");
    for tool in f.mcp.request("tools/list", json!({}))["result"]["tools"].as_array().expect("tools") {
        assert!(tool.get("bridge").is_none(), "internal routing must not leak into the tool definition: {}", tool["name"]);
        assert!(tool["inputSchema"].is_object());
    }

    // Registration is decided from the config at start-up, like the JS server.
    let gated = fixture(
        "registry-gated",
        json!({"sticker": {"enabled": false}, "voice": {"enabled": false}, "tools": {"sendPoke": false, "getImages": false, "memory": false}}),
    );
    let gated_names: Vec<String> = gated.mcp.request("tools/list", json!({}))["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect();
    for gone in ["qq_send_poke", "qq_get_message_images", "qq_memory_append", "qq_list_stickers", "qq_send_voice", "qq_set_sticker_remark"] {
        assert!(!gated_names.iter().any(|n| n == gone), "{gone} must not be registered");
    }
    let unknown = gated.mcp.call("qq_send_poke", group(json!({"targetUserId": 10001})));
    assert!(is_error(&unknown));
    assert_eq!(text_of(&unknown), "MCP error -32602: Tool qq_send_poke not found");
}

#[test]
fn invalid_jsonrpc_ids_never_dispatch_tools() {
    let f = fixture("invalid-rpc-id", json!({}));
    for id in [Value::Null, json!(false), json!(true), json!({}), json!([]), json!(1.5)] {
        f.mcp.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "qq_send_message", "arguments": group(json!({"messages": "invalid id must not send"}))
        }}));
    }
    let control = f.mcp.call("qq_send_message", group(json!({"messages": "valid control"})));
    assert!(!is_error(&control), "{control}");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(f.onebot.actions("send_group_msg").len(), 1, "invalid envelopes must have no send effect");
    assert!(lock(&f.mcp.answers.0).is_empty(), "invalid envelopes are dropped by the JS SDK");
    for (raw, expected) in [("0", json!(0)), ("-1", json!(-1)), ("2.0", json!(2.0)), ("\"\"", json!(""))] {
        f.mcp.send_raw(&format!(r#"{{"jsonrpc":"2.0","id":{raw},"method":"ping"}}"#));
        assert!(wait_for(Duration::from_secs(5), || lock(&f.mcp.answers.0).iter().any(|v| v["id"] == expected)), "valid id {raw}");
        lock(&f.mcp.answers.0).clear();
    }
}

#[test]
fn protocol_answers_follow_the_js_server() {
    let f = fixture("protocol", json!({}));
    let m = &f.mcp;
    let init = |version: &str| m.request("initialize", json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    assert_eq!(init("2025-06-18")["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init("1999-01-01")["result"]["protocolVersion"], "2025-11-25", "an unknown version gets the latest");
    assert_eq!(init("2024-11-05")["result"]["capabilities"], json!({"tools": {"listChanged": true}}));
    assert_eq!(m.request("ping", json!({}))["result"], json!({}));
    let unknown = m.request("resources/list", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(unknown["error"]["message"], "Method not found");
    let no_params = m.request("tools/call", Value::Null);
    assert_eq!(no_params["error"]["code"], -32603);

    let missing = m.call("qq_get_prompt", json!({}));
    assert!(is_error(&missing));
    assert_eq!(
        text_of(&missing),
        "MCP error -32602: Input validation error: Invalid arguments for tool qq_get_prompt: Invalid input: expected string, received undefined at key\nInvalid input: expected string, received undefined at token"
    );
    let wrong = m.call("qq_get_unread_messages", group(json!({"limit": "x", "afterSeq": -1})));
    assert_eq!(
        text_of(&wrong),
        "MCP error -32602: Input validation error: Invalid arguments for tool qq_get_unread_messages: Invalid input: expected number, received string at limit\nToo small: expected number to be >=0 at afterSeq"
    );

    // Notifications and unparsable lines get no answer; the next request is still served.
    m.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    m.send_raw("{this is not json");
    m.send(&json!({"id": 99, "method": "ping"}));
    assert_eq!(m.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn mcp_admission_is_bounded_and_recovers_after_slow_requests() {
    let f = fixture("mcp-admission", json!({}));
    fs::write(f.root.path().join("data/mode.json"), r#"{"mode":"closed-agent"}"#).expect("read mode");
    f.onebot.delay("get_group_member_list", Duration::from_secs(3));
    let ids: Vec<_> = (0..80).map(|_| f.mcp.request_nowait("qq_get_group_members", json!({"groupId": GROUP}))).collect();
    let answers: Vec<_> = ids.into_iter().map(|id| f.mcp.finish(id)).collect();
    assert!(answers.iter().filter(|a| a.get("error").is_none()).all(|a| !is_error(a)), "{answers:?}");
    let overloaded = answers.iter().filter(|a| a["error"]["code"] == -32000).count();
    assert!(overloaded >= 16, "at most 64 requests may be in flight; got {overloaded} refusals");
    assert_eq!(f.onebot.actions("get_group_member_list").len(), 80 - overloaded);
    assert_eq!(f.mcp.request("ping", json!({}))["result"], json!({}), "capacity is released after completion");
}

#[test]
fn hf94_wait_cap_survives_console_and_mcp_restart() {
    let mut f = fixture("hf94-cap-restart", json!({
        "wait":{"maxPerTurn":1,"minQuietAfterNewMs":0},
        "wake":{"preSleepWaitEnabled":false}
    }));
    let first = f.mcp.call("qq_wait_for_messages", group(json!({"purpose":"reply","quietMs":0})));
    assert!(!is_error(&first), "{first}");
    let first: Value = serde_json::from_str(&text_of(&first)).unwrap();
    assert_eq!(first["waitCallsUsed"], 1);
    assert_eq!(first["waitLimitReached"], true);
    f._console.child.kill().unwrap();
    f._console.child.wait().unwrap();
    f._console = Console::start(&f.root, f.console_port);
    let restarted = Mcp::start(&f.root, f.console_port, &[]);
    restarted.initialize();
    let answer = restarted.call("qq_wait_for_messages", group(json!({"timeoutMs":30000})));
    assert!(!is_error(&answer), "{answer}");
    let answer: Value = serde_json::from_str(&text_of(&answer)).unwrap();
    assert_eq!(answer["waitLimitReached"], true);
    assert_eq!(answer["waitCallsUsed"], 1);
    assert_eq!(answer["waitedMs"], 0, "restarting either process must not grant another wait");
    assert_eq!(answer["newMessages"], json!([]));
    assert_eq!(answer["nextAction"], "end_turn");
    assert_eq!(answer["reason"], "end this turn now: produce your final answer and make no further tool calls");
}

#[test]
fn a_long_wait_does_not_hold_up_other_tool_calls_and_a_second_wait_supersedes_it() {
    let f = fixture("wait", json!({}));
    // Everything was offered, so the wait has nothing new to return and runs to its budget.
    let offered = f.mcp.call("qq_get_unread_messages", group(json!({})));
    assert!(!is_error(&offered), "{offered}");
    let waiting = f.mcp.request_nowait("qq_wait_for_messages", group(json!({"timeoutMs": 2500, "quietMs": 0})));
    thread::sleep(Duration::from_millis(400));
    let started = Instant::now();
    let state = f.mcp.call("qq_social_state", group(json!({})));
    assert!(!is_error(&state));
    assert!(started.elapsed() < Duration::from_millis(1500), "another call must not queue behind the wait: {:?}", started.elapsed());
    let takeover_started = Instant::now();
    let second = f.mcp.request_nowait("qq_wait_for_messages", group(json!({"timeoutMs": 300, "quietMs": 0})));
    let finished = f.mcp.finish(waiting);
    assert!(takeover_started.elapsed() < Duration::from_millis(1500), "old wait must finish promptly after takeover: {:?}", takeover_started.elapsed());
    assert!(!is_error(&finished), "{finished}");
    let result: Value = serde_json::from_str(&text_of(&finished)).expect("wait result json");
    assert_eq!(result["superseded"], true);
    assert_eq!(result["arrived"], false);
    assert_eq!(result["timeout"], false);
    assert_eq!(result["newMessages"], json!([]));
    // The old handler has finished; dropping its lease must leave the new wait current.
    let second = f.mcp.finish(second);
    assert!(!is_error(&second), "{second}");
    let result: Value = serde_json::from_str(&text_of(&second)).expect("new wait result json");
    assert_ne!(result.get("superseded").and_then(Value::as_bool), Some(true));
    assert_eq!(result["arrived"], false);
    assert_eq!(result["timeout"], true);
    assert_eq!(result["readThroughSeq"], 8);
}

#[test]
fn read_receipts_do_not_survive_a_console_restart_but_the_persisted_mark_does() {
    let onebot = OneBot::start();
    let root = make_root("restart", onebot.port, 0, json!({}));
    let (console, console_port) = Console::start_fresh(&root);
    let mcp = Mcp::start(&root, console_port, &[]);
    mcp.initialize();

    let offered: Value = serde_json::from_str(&text_of(&mcp.call("qq_get_unread_messages", group(json!({"afterSeq": 0}))))).expect("unread json");
    assert_eq!(offered["readThroughSeq"], 8, "all five unread messages were offered");
    let marked: Value = serde_json::from_str(&text_of(&mcp.call("qq_mark_read", group(json!({"throughSeq": 5}))))).expect("mark json");
    assert_eq!(marked["markedCount"], 2);
    assert_eq!(marked["unreadCount"], 3);
    assert_eq!(root.state()["conversations"][GROUP_KEY]["lastReadThroughSeq"], 5);
    for conv in root.state()["conversations"].as_object().expect("conversations").values() {
        assert!(conv.get("modelSeenSeqs").is_none(), "the receipt is process memory, never written to the state file");
    }

    drop(console);
    let console = Console::start(&root, console_port);
    let refused = mcp.call("qq_mark_read", group(json!({"throughSeq": 8})));
    assert!(is_error(&refused), "a receipt from before the restart must not authorise skipping unread messages");
    assert!(text_of(&refused).contains("throughSeq 必须是不超过已展示连续消息水位的非负安全整数"), "{}", text_of(&refused));
    let state = root.state();
    assert_eq!(state["conversations"][GROUP_KEY]["unread"].as_array().expect("unread").len(), 3, "nothing was acknowledged");

    let again: Value = serde_json::from_str(&text_of(&mcp.call("qq_get_unread_messages", group(json!({"afterSeq": 5}))))).expect("unread json");
    assert_eq!(again["readThroughSeq"], 8, "fetching again restores the receipt");
    let done: Value = serde_json::from_str(&text_of(&mcp.call("qq_mark_read", group(json!({"throughSeq": 8}))))).expect("mark json");
    assert_eq!(done["markedCount"], 3);
    assert_eq!(root.state()["conversations"][GROUP_KEY]["lastReadThroughSeq"], 8);
    drop(console);
}

#[test]
fn a_rotated_session_token_voids_the_receipts_it_was_given() {
    let f = fixture("rotation", json!({}));
    f.mcp.call("qq_get_unread_messages", group(json!({"afterSeq": 0})));
    // The conversation gets a new token (cancelled / replaced turn) and the new session reads nothing yet.
    let mut state = f.root.state();
    state["conversations"][GROUP_KEY]["agentToken"] = json!("ticket14-rotated-token-cccc");
    f.root.write_state(&state);
    let refused = f.mcp.call("qq_mark_read", json!({"key": GROUP_KEY, "token": "ticket14-rotated-token-cccc", "throughSeq": 8}));
    assert!(is_error(&refused), "receipts given to the old session must not count for the new one: {refused}");
    let old = f.mcp.call("qq_mark_read", group(json!({"throughSeq": 8})));
    assert!(is_error(&old));
    assert!(text_of(&old).contains("智能体调用携带的会话令牌无效"), "{}", text_of(&old));
}

#[test]
fn the_bot_identity_in_the_prompt_comes_from_the_persisted_identity_file() {
    let f = fixture("identity", json!({}));
    let headers = [("x-console-token", CONSOLE_TOKEN)];
    let path = format!("/api/socialV2/prompt?key={GROUP_KEY}");
    let (_, before) = http(f.console_port, "GET", &path, &headers, None).expect("prompt");
    assert_eq!(before["self"]["nickname"], Value::Null, "no identity before the live bridge persisted one");
    fs::write(f.root.path().join("data/identity.json"), json!({"userId": "401", "nickname": "测试机器人", "updatedAt": now_ms()}).to_string()).expect("identity");
    let (_, after) = http(f.console_port, "GET", &path, &headers, None).expect("prompt");
    assert_eq!(after["self"]["nickname"], "测试机器人");
    assert_eq!(after["self"]["userId"], "401");
}

fn pending_send_stops(change: &str, poke: bool) {
    let f = fixture("pending-disable", json!({"send": {"mergeMultiIntoOne": false, "burstIntervalMinMs": 0, "maxGapMs": 0}}));
    let action = if poke { "get_group_member_list" } else { "send_group_msg" };
    let release = f.onebot.hold(action);
    let pending = if poke {
        f.mcp.request_nowait("qq_send_poke", group(json!({"targetUserId": 10001})))
    } else {
        f.mcp.request_nowait("qq_send_burst", json!({"groupId": GROUP, "token": GROUP_TOKEN, "messages": ["first", "cancelled"]}))
    };
    assert!(wait_for(Duration::from_secs(5), || !f.onebot.actions(action).is_empty()));
    if change == "paused" {
        let (status, _) = http(f._console.port, "POST", "/api/socialV2/activity", &[("x-console-token", CONSOLE_TOKEN)], Some(&json!({"paused": true}))).expect("pause");
        assert_eq!(status, 200);
    } else {
        let mut config = f.root.config();
        if change == "enabled" {
            config["socialV2"]["enabled"] = json!(false);
        } else {
            config["socialV2"]["tools"][if poke { "sendPoke" } else { "sendBurst" }] = json!(false);
        }
        f.root.write_config(&config);
    }
    release.send(()).expect("release the held OneBot response");
    let answer = f.mcp.finish(pending);
    assert!(is_error(&answer), "{change}, poke={poke}: {answer}");
    assert!(text_of(&answer).contains("发送已取消"), "{answer}");
    assert!(f.onebot.actions("group_poke").is_empty());
    assert_eq!(f.onebot.actions("send_group_msg").len(), if poke { 0 } else { 1 });
    assert_eq!(f.root.state()["conversations"][GROUP_KEY]["sendTimes"].as_array().expect("quota").len(), if poke { 0 } else { 1 });
}

#[test]
fn pending_send_burst_paused() { pending_send_stops("paused", false); }
#[test]
fn pending_send_burst_disabled() { pending_send_stops("enabled", false); }
#[test]
fn pending_send_burst_tool_disabled() { pending_send_stops("tool", false); }
#[test]
fn pending_send_poke_paused() { pending_send_stops("paused", true); }
#[test]
fn pending_send_poke_disabled() { pending_send_stops("enabled", true); }
#[test]
fn pending_send_poke_tool_disabled() { pending_send_stops("tool", true); }

#[test]
fn a_poke_is_cancelled_when_the_session_token_changes_during_the_member_check() {
    let f = fixture("poke-guard", json!({}));
    let release = f.onebot.hold("get_group_member_list");
    let pending = f.mcp.request_nowait("qq_send_poke", group(json!({"targetUserId": 10001})));
    assert!(wait_for(Duration::from_secs(5), || !f.onebot.actions("get_group_member_list").is_empty()));
    let mut state = f.root.state();
    state["conversations"][GROUP_KEY]["agentToken"] = json!("ticket14-rotated-token-dddd");
    f.root.write_state(&state);
    release.send(()).expect("release the held OneBot response");
    let answer = f.mcp.finish(pending);
    assert!(is_error(&answer), "{answer}");
    assert!(text_of(&answer).contains("发送已取消"), "{}", text_of(&answer));
    assert!(f.onebot.actions("group_poke").is_empty(), "the cancelled poke must never reach the gateway");
    let sends = f.root.state()["conversations"][GROUP_KEY]["sendTimes"].as_array().expect("sendTimes").len();
    assert_eq!(sends, 0, "the quota reserved for the cancelled poke is given back");
}

#[test]
fn a_quoted_direct_send_is_cancelled_when_the_mode_changes_during_the_quote_lookup() {
    let f = fixture("direct-guard", json!({}));
    let release = f.onebot.hold("get_msg");
    let pending = f.mcp.request_nowait("qq_reply", json!({"groupId": GROUP, "replyToMessageId": 7003, "message": "会被取消", "token": GROUP_TOKEN}));
    assert!(wait_for(Duration::from_secs(5), || !f.onebot.actions("get_msg").is_empty()));
    fs::write(f.root.path().join("data/mode.json"), r#"{"mode":"chat"}"#).expect("mode");
    release.send(()).expect("release the held OneBot response");
    let answer = f.mcp.finish(pending);
    assert!(is_error(&answer), "{answer}");
    assert!(text_of(&answer).contains("发送已取消"), "{}", text_of(&answer));
    assert!(f.onebot.actions("send_group_msg").is_empty(), "no request after the validity check failed");
}

#[test]
fn a_quoted_direct_send_is_cancelled_when_the_allow_list_changes_during_the_quote_lookup() {
    let f = fixture("direct-allow-guard", json!({}));
    let release = f.onebot.hold("get_msg");
    let pending = f.mcp.request_nowait("qq_reply", json!({"groupId": GROUP, "replyToMessageId": 7003, "message": "会被取消", "token": GROUP_TOKEN}));
    assert!(wait_for(Duration::from_secs(5), || !f.onebot.actions("get_msg").is_empty()));
    let mut config = f.root.config();
    config["allow"]["groups"] = json!([420043]);
    f.root.write_config(&config);
    release.send(()).expect("release the held OneBot response");
    let answer = f.mcp.finish(pending);
    assert!(is_error(&answer), "{answer}");
    assert!(text_of(&answer).contains("发送已取消"), "{}", text_of(&answer));
    assert!(f.onebot.actions("send_group_msg").is_empty(), "revoked target must never reach the gateway");
    assert_eq!(f.root.state()["conversations"][GROUP_KEY]["sendTimes"], json!([]), "cancelled quota is released");
}

#[test]
fn revoking_a_target_keeps_the_confirmed_quoted_item_and_cancels_the_rest_of_the_batch() {
    let f = fixture("batch-allow-guard", json!({"send": {"mergeMultiIntoOne": false, "maxMessageChars": 500, "burstIntervalMinMs": 0, "maxGapMs": 0}}));
    let release = f.onebot.hold("send_group_msg");
    let pending = f.mcp.request_nowait("qq_send_message", group(json!({"messages": ["已发送", "应取消"], "replyToMessageId": 3})));
    assert!(wait_for(Duration::from_secs(5), || !f.onebot.actions("send_group_msg").is_empty()));
    let mut config = f.root.config();
    config["allow"]["groups"] = json!([420043]);
    f.root.write_config(&config);
    release.send(()).expect("release the held OneBot response");
    let answer = f.mcp.finish(pending);
    assert!(is_error(&answer), "{answer}");
    assert!(text_of(&answer).contains("发送已取消"), "{}", text_of(&answer));
    let sent = f.onebot.actions("send_group_msg");
    assert_eq!(sent.len(), 1, "only the already submitted item reaches the gateway");
    assert_eq!(sent[0]["message"], json!([{"type": "reply", "data": {"id": "7003"}}, {"type": "text", "data": {"text": "已发送"}}]));
    let state = f.root.state();
    assert_eq!(state["conversations"][GROUP_KEY]["sendTimes"].as_array().expect("quota").len(), 1);
    assert_eq!(state["conversations"][GROUP_KEY]["recentMessages"].as_array().expect("recent").last().expect("sent record")["text"], "已发送");
    assert_eq!(state["conversations"][OWNER_KEY]["sendTimes"], json!([]), "the other conversation is unchanged");
    let outbox = fs::read_to_string(f.root.path().join("data/send-outbox.jsonl")).expect("outbox");
    let records: Vec<Value> = outbox.lines().map(|line| serde_json::from_str(line).expect("outbox json")).collect();
    assert!(records.iter().any(|r| r["k"] == "confirmed" && r["idx"] == 0));
    assert!(records.iter().any(|r| r["k"] == "cancelled" && r["idx"] == 1));
    assert!(!records.iter().any(|r| r["k"] == "attempt" && r["idx"] == 1));
}

#[test]
fn concurrent_send_message_calls_share_one_duplicate_window() {
    let f = fixture("concurrent-duplicate", json!({"send": {"maxMessageChars": 200000, "duplicateWindowMs": 60000, "maxSendPerMinute": 100, "maxSendPerHour": 100}}));
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let calls: Vec<_> = (0..16).map(|_| {
        let barrier = barrier.clone();
        let port = f.console_port;
        thread::spawn(move || {
            let body = json!({"key": GROUP_KEY, "messages": "a".repeat(150000)});
            barrier.wait();
            http(port, "POST", "/api/socialV2/send-message", &[("x-console-token", CONSOLE_TOKEN), ("x-agent-token", GROUP_TOKEN), ("x-agent-call", "1")], Some(&body)).expect("send response")
        })
    }).collect();
    let answers: Vec<_> = calls.into_iter().map(|t| t.join().expect("worker")).collect();
    assert_eq!(answers.iter().filter(|(status, _)| *status == 200).count(), 1, "{answers:?}");
    assert!(answers.iter().all(|(status, _)| *status == 200 || *status == 429), "{answers:?}");
    assert_eq!(f.onebot.actions("send_group_msg").len(), 1, "only one concurrent send reaches the gateway");
    assert_eq!(f.root.state()["conversations"][GROUP_KEY]["sendTimes"].as_array().expect("quota").len(), 1);
}

#[test]
fn burst_merges_into_one_durable_message_and_the_duplicate_window_blocks_a_second_tool_send() {
    let f = fixture("burst", json!({"send": {"mergeMultiIntoOne": true, "maxMessageChars": 500, "maxSendPerMinute": 20, "maxSendPerHour": 100, "burstIntervalMinMs": 0, "maxGapMs": 0, "duplicateWindowMs": 60000}}));
    let burst = f.mcp.call("qq_send_burst", json!({"groupId": GROUP, "token": GROUP_TOKEN, "messages": ["连发一", "连发二"]}));
    assert!(!is_error(&burst), "{burst}");
    let sent = f.onebot.actions("send_group_msg");
    assert_eq!(sent.len(), 1, "two messages are merged into one");
    assert_eq!(sent[0]["message"][0]["data"]["text"], "连发一\n连发二");
    let outbox = fs::read_to_string(f.root.path().join("data/send-outbox.jsonl")).expect("outbox");
    assert!(outbox.contains("\"k\":\"intent\"") && outbox.contains("\"k\":\"attempt\"") && outbox.contains("\"k\":\"confirmed\""), "{outbox}");

    let first = f.mcp.call("qq_send_message", group(json!({"messages": "第一次"})));
    assert!(!is_error(&first), "burst does not open the window: {first}");
    let second = f.mcp.call("qq_send_message", group(json!({"messages": "第二次"})));
    assert!(is_error(&second));
    assert!(text_of(&second).starts_with("发送失败：刚说过话了（"), "{}", text_of(&second));
    assert_eq!(f.onebot.actions("send_group_msg").len(), 2, "the blocked send never reached the gateway");
}

#[test]
fn the_session_defaults_from_the_environment_fill_in_after_validation_only() {
    let onebot = OneBot::start();
    let root = make_root("env-defaults", onebot.port, 0, json!({}));
    let (_console, console_port) = Console::start_fresh(&root);
    let mcp = Mcp::start(&root, console_port, &[("QQ_SESSION_KEY", GROUP_KEY), ("QQ_SESSION_TOKEN", GROUP_TOKEN)]);
    mcp.initialize();
    // key and token are required by the schema, so validation still refuses a call without them ...
    let refused = mcp.call("qq_social_state", json!({}));
    assert!(text_of(&refused).starts_with("MCP error -32602: Input validation error"), "{}", text_of(&refused));
    // ... while a tool with an optional token takes the environment token.
    let sent = mcp.call("qq_send_group_message", json!({"groupId": GROUP, "message": "环境令牌"}));
    assert!(!is_error(&sent), "{sent}");
    assert_eq!(onebot.actions("send_group_msg").len(), 1);
}

#[test]
fn feedback_report_lands_in_the_selected_root_store() {
    // Ticket 23 opened the feedback routes in a selected runtime; the model's tool now reaches them.
    let f = fixture("feedback", json!({}));
    let answer = f.mcp.call("qq_report_feedback", group(json!({"message": "x", "level": "warning"})));
    assert!(!is_error(&answer), "{answer}");
    assert!(text_of(&answer).contains("\"ok\":true"), "{}", text_of(&answer));
    let stored: Value = serde_json::from_slice(&fs::read(f.root.path().join("data/feedback.json")).unwrap()).unwrap();
    assert_eq!(stored.as_array().unwrap().len(), 1);
    assert_eq!((stored[0]["key"].as_str().unwrap(), stored[0]["level"].as_str().unwrap(), stored[0]["message"].as_str().unwrap()), (GROUP_KEY, "warning", "x"));
}

/// Ticket 24 F1: after the console generates its token (`config.json` gets "", the token lives in
/// `data/console-token`), the MCP server process still reaches the console for every tool call
/// (JS `readConsoleToken` falls back to the token file).
#[test]
fn ticket24_a_generated_console_token_still_serves_mcp_tool_calls() {
    let f = fixture("t24-generated-token", json!({}));
    let before = f.mcp.call("qq_get_unread_messages", group(json!({"afterSeq": 0})));
    assert!(!is_error(&before), "{before}");
    let (status, reply) = http(f.console_port, "POST", "/api/console/token", &[("x-console-token", CONSOLE_TOKEN)], Some(&json!({}))).expect("console");
    assert_eq!(status, 200, "{reply}");
    let generated = reply["token"].as_str().expect("token").to_string();
    assert_eq!(reply["generated"], true);
    assert_eq!(f.root.config()["consoleToken"], "", "JS writes \"\" to config.json for a generated token");
    assert_eq!(fs::read_to_string(f.root.path().join("data/console-token")).expect("token file").trim(), generated);
    let answer = f.mcp.call("qq_get_unread_messages", group(json!({"afterSeq": 0})));
    assert!(!is_error(&answer), "an MCP tool call failed after the token was generated: {answer}");
    let unread: Value = serde_json::from_str(&text_of(&answer)).expect("unread json");
    assert_eq!(unread["readThroughSeq"], 8, "{unread}");
    let marked = f.mcp.call("qq_mark_read", group(json!({"throughSeq": 5})));
    assert!(!is_error(&marked), "{marked}");
}

#[test]
fn qq_memory_and_slang_model_schemas_filter_all_text_fields_without_changing_dispatch() {
    let f = fixture(
        "qq-model-schemas",
        json!({"context":{"stripFromModelText":["WORD"]}}),
    );
    let append = f.mcp.call(
        "qq_memory_append",
        group(json!({"category":"activeTopic","content":"WORD memory fact"})),
    );
    assert!(!is_error(&append), "{append}");
    fs::write(f.root.path().join("data/slang.json"), json!([
        {"id":"schema-slang","status":"confirmed","content":"WORD term","meaning":"WORD meaning","usage":"WORD usage","example":"WORD example","count":1},
        {"id":"decoy","status":"confirmed","content":"unrelated term","meaning":"unrelated meaning","count":1}
    ]).to_string()).unwrap();
    let memory = f.mcp.call("qq_memory_query", group(json!({})));
    assert!(!is_error(&memory), "{memory}");
    let packet: Value = serde_json::from_str(&text_of(&memory)).unwrap();
    let formatted = packet["formatted"].as_str().unwrap();
    assert!(formatted.contains("memory fact"));
    assert!(!formatted.contains("WORD"), "{packet}");
    let slang = f.mcp.call("qq_slang_query", group(json!({"q":"WORD"})));
    assert!(!is_error(&slang), "{slang}");
    let packet: Value = serde_json::from_str(&text_of(&slang)).unwrap();
    assert_eq!(
        packet["entries"].as_array().unwrap().len(),
        1,
        "dispatch retains the lookup argument"
    );
    assert!(
        !packet["block"].as_str().unwrap().contains("WORD"),
        "{packet}"
    );
    assert!(packet["block"].as_str().unwrap().contains("meaning"));
    for field in ["meaning", "usage", "example"] {
        assert_eq!(packet["entries"][0][field], format!(" {field}"), "{packet}");
    }
    assert!(
        fs::read_to_string(f.root.path().join("data/social-v2.json"))
            .unwrap()
            .contains("WORD memory fact")
    );
    assert!(fs::read_to_string(f.root.path().join("data/slang.json"))
        .unwrap()
        .contains("WORD meaning"));
}
