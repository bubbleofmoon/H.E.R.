#[path = "../src/install_root.rs"]
mod install_root;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSOLE_A: &str = "fixture-console-token-A-abcdefghij";
const CONSOLE_B: &str = "fixture-console-token-B-abcdefghij";
const AGENT_A: &str = "fixture-agent-token-A";
const AGENT_B: &str = "fixture-agent-token-B";
const ONEBOT_A: &str = "fixture-onebot-token-A";
const ONEBOT_B: &str = "fixture-onebot-token-B";

fn hide_process_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

struct FixtureRoot(PathBuf);

impl Deref for FixtureRoot {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_root(
    label: &str,
    console_token: &str,
    agent_token: &str,
    onebot_token: &str,
    onebot_url: &str,
    key: &str,
    group_ids: &[i64],
    private_ids: &[&str],
    max_per_minute: i64,
    recent_messages: Value,
) -> FixtureRoot {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".scratch")
        .join("ticket03 send-message fixtures 中文");
    let parent = crate::install_root::development_root(parent);
    fs::create_dir_all(&parent).expect("create worktree-local fixture parent");
    let root = parent.join(format!("{label}-{}-{stamp}-{sequence}", std::process::id()));
    let root = crate::install_root::development_root(root);
    fs::create_dir_all(root.join("config")).expect("create selected config directory");
    fs::create_dir_all(root.join("data")).expect("create selected data directory");
    fs::create_dir_all(root.join("resources")).expect("create selected resources directory");

    let config = json!({
        "consoleToken": console_token,
        "ownerQQ": 9001,
        "allow": { "groups": group_ids, "private": private_ids },
        "snowluma": {
            // This closed poison URL detects accidental fallback from dependencies.json.
            "httpUrl": "http://127.0.0.1:1",
            "accessToken": onebot_token
        },
        "socialV2": {
            // These tests count OneBot contact; the startup sticker warm-up is not their subject.
            "sticker": { "enabled": false },
            "tools": { "sendMessage": true },
            "send": {
                "burstEnabled": true,
                "mergeMultiIntoOne": false,
                "burstMaxMessages": 8,
                "maxMessageChars": 500,
                "burstIntervalMinMs": 0,
                "maxGapMs": 100,
                "gapBaseMs": 5,
                "gapPerCharMs": 2,
                "maxSendPerMinute": max_per_minute,
                "maxSendPerHour": 20
            }
        }
    });
    fs::write(
        root.join("config/config.json"),
        serde_json::to_vec_pretty(&config).expect("serialize selected config"),
    )
    .expect("write selected config");

    let conversation = json!({
        "agentToken": agent_token,
        "sendTimes": [],
        "recentMessages": recent_messages
    });
    let mut conversations = serde_json::Map::new();
    conversations.insert(key.to_string(), conversation);
    let state = json!({ "paused": false, "conversations": Value::Object(conversations) });
    fs::write(
        root.join("data/social-v2.json"),
        serde_json::to_vec_pretty(&state).expect("serialize selected state"),
    )
    .expect("write selected state");
    fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).expect("write selected mode");
    fs::write(
        root.join("dependencies.json"),
        json!({
            "dsh": "http://127.0.0.1:1",
            "model": "http://127.0.0.1:2",
            "onebot": onebot_url
        })
        .to_string(),
    )
    .expect("write selected dependencies");
    FixtureRoot(root)
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind temporary port reservation");
    listener.local_addr().expect("read temporary port").port()
}

struct ConsoleProcess {
    child: Child,
    port: u16,
}

impl ConsoleProcess {
    fn start(root: &Path) -> Self {
        let port = free_port();
        let stdout = fs::File::create(root.join("console.stdout.log"))
            .expect("create synthetic candidate stdout log");
        let stderr = fs::File::create(root.join("console.stderr.log"))
            .expect("create synthetic candidate stderr log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .arg("--console-serve")
            .arg("--runtime-root")
            .arg(root)
            .arg("--console-port")
            .arg(port.to_string())
            .env_remove("HER_RUNTIME_ROOT")
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        hide_process_window(&mut command);
        let mut process = Self {
            child: command.spawn().expect("start isolated console candidate"),
            port,
        };
        process.wait_for_health();
        process
    }

    fn wait_for_health(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if self
                .child
                .try_wait()
                .expect("check candidate process")
                .is_some()
            {
                panic!("isolated console exited before its public health endpoint became ready");
            }
            if let Ok(response) =
                http_request(self.port, "GET", "/api/health", None, None, &Value::Null)
            {
                if response.status == 200 {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "isolated console health endpoint did not become ready"
            );
            thread::sleep(Duration::from_millis(40));
        }
    }
}

impl Drop for ConsoleProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

struct HttpResponse {
    status: u16,
    body: Value,
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    console_token: Option<&str>,
    agent_token: Option<&str>,
    body: &Value,
) -> Result<HttpResponse, String> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect console: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .map_err(|e| format!("set HTTP timeout: {e}"))?;
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let mut headers = String::new();
    if let Some(value) = console_token {
        headers.push_str(&format!("X-Console-Token: {value}\r\n"));
    }
    if let Some(value) = agent_token {
        headers.push_str(&format!("X-Agent-Token: {value}\r\n"));
    }
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
        body.as_bytes().len()
    )
    .map_err(|e| format!("write HTTP request: {e}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read HTTP response: {e}"))?;
    let response = String::from_utf8(response).map_err(|_| "response was not UTF-8".to_string())?;
    let (header, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "response had no header separator".to_string())?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| "response status was invalid".to_string())?;
    let body =
        serde_json::from_str(body).map_err(|e| format!("response body was not JSON: {e}"))?;
    Ok(HttpResponse { status, body })
}

fn http_request_with_declared_length(
    port: u16,
    console_token: Option<&str>,
    agent_token: Option<&str>,
    declared_length: usize,
) -> Result<HttpResponse, String> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect console: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .map_err(|e| format!("set HTTP timeout: {e}"))?;
    let mut headers = String::new();
    if let Some(value) = console_token {
        headers.push_str(&format!("X-Console-Token: {value}\r\n"));
    }
    if let Some(value) = agent_token {
        headers.push_str(&format!("X-Agent-Token: {value}\r\n"));
    }
    write!(
        stream,
        "POST /api/socialV2/send-message HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {declared_length}\r\n{headers}Connection: close\r\n\r\n"
    )
    .map_err(|e| format!("write HTTP headers: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("finish declared-length request: {e}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read HTTP response: {e}"))?;
    let response = String::from_utf8(response).map_err(|_| "response was not UTF-8".to_string())?;
    let (header, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "response had no header separator".to_string())?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| "response status was invalid".to_string())?;
    let body =
        serde_json::from_str(body).map_err(|e| format!("response body was not JSON: {e}"))?;
    Ok(HttpResponse { status, body })
}

fn http_request_with_chunked_oversize_body(
    port: u16,
    console_token: Option<&str>,
    agent_token: Option<&str>,
    key: &str,
    max_body_bytes: usize,
) -> Result<HttpResponse, String> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect console: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .map_err(|e| format!("set HTTP timeout: {e}"))?;
    let mut headers = String::new();
    if let Some(value) = console_token {
        headers.push_str(&format!("X-Console-Token: {value}\r\n"));
    }
    if let Some(value) = agent_token {
        headers.push_str(&format!("X-Agent-Token: {value}\r\n"));
    }
    write!(
        stream,
        "POST /api/socialV2/send-message HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n{headers}Connection: close\r\n\r\n{max_body_bytes:x}\r\n"
    )
    .map_err(|e| format!("write chunked request headers: {e}"))?;
    let valid_json = json!({ "key": key, "messages": ["synthetic oversize fixture"] }).to_string();
    if valid_json.len() >= max_body_bytes {
        return Err("valid send-message fixture prefix exceeds the body cap".to_string());
    }
    let mut first_chunk = vec![b' '; max_body_bytes];
    first_chunk[..valid_json.len()].copy_from_slice(valid_json.as_bytes());
    stream
        .write_all(&first_chunk)
        .map_err(|e| format!("write first chunk body: {e}"))?;
    stream
        .write_all(b"\r\n1\r\n \r\n0\r\n\r\n")
        .map_err(|e| format!("write over-limit follow-up chunk: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("finish chunked request: {e}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read HTTP response: {e}"))?;
    let response = String::from_utf8(response).map_err(|_| "response was not UTF-8".to_string())?;
    let (header, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "response had no header separator".to_string())?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| "response status was invalid".to_string())?;
    let body =
        serde_json::from_str(body).map_err(|e| format!("response body was not JSON: {e}"))?;
    Ok(HttpResponse { status, body })
}

#[derive(Debug)]
struct OneBotRequest {
    path: String,
    authorization: String,
    body: Value,
}

struct FakeOneBot {
    url: String,
    stop: Option<mpsc::Sender<()>>,
    request_count: Arc<std::sync::atomic::AtomicUsize>,
    private_request_count: Arc<std::sync::atomic::AtomicUsize>,
    join: Option<thread::JoinHandle<Vec<OneBotRequest>>>,
}

impl FakeOneBot {
    fn start(quote_group: i64, reject_send: bool) -> Self {
        Self::start_with(quote_group, Behavior::plain(reject_send))
    }

    /// Start a fake whose send outcomes follow `behavior` (scripted steps, history, quote misses).
    fn start_with(quote_group: i64, behavior: Behavior) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind synthetic OneBot");
        Self::from_listener(listener, quote_group, behavior)
            .expect("prepare synthetic OneBot listener")
    }

    fn start_ipv6(quote_group: i64, reject_send: bool) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("::1", 0))?;
        Self::from_listener(listener, quote_group, Behavior::plain(reject_send))
    }

    fn from_listener(
        listener: TcpListener,
        quote_group: i64,
        mut behavior: Behavior,
    ) -> std::io::Result<Self> {
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let url = format!("http://{address}");
        let (stop, stopped) = mpsc::channel();
        let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_count = request_count.clone();
        let private_request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_private_count = private_request_count.clone();
        let join = thread::spawn(move || {
            let mut requests = Vec::new();
            loop {
                match stopped.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // An accepted socket inherits the listener's non-blocking mode on Windows.
                        let _ = stream.set_nonblocking(false);
                        // A connection that fails mid-request must not take the fixture down.
                        if let Some(request) =
                            handle_onebot(stream, quote_group, &mut behavior, &observed_count)
                        {
                            let private = request.path == "/send_private_msg";
                            requests.push(request);
                            if private {
                                observed_private_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(10035) =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            requests
        });
        Ok(Self {
            url,
            stop: Some(stop),
            request_count,
            private_request_count,
            join: Some(join),
        })
    }

    fn finish(mut self) -> Vec<OneBotRequest> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.join
            .take()
            .expect("fake listener worker exists")
            .join()
            .expect("fake listener worker exits")
    }
}

impl Drop for FakeOneBot {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// One scripted outcome for the next send request.
#[derive(Clone)]
enum Step {
    /// Accept and answer with a message id.
    Ok,
    /// Explicit OneBot refusal: definitely not delivered.
    Reject,
    /// Read the request and close without answering: the bridge cannot know the outcome.
    Drop,
    /// Answer a failed OneBot envelope with this retcode (HTTP 200).
    Failed(i64),
    /// Answer with this HTTP status and an empty JSON failure envelope.
    HttpStatus(u16),
    /// Deliver (id assigned) but answer only after this many milliseconds.
    OkDelayed(u64),
    /// Accept, then rewrite the conversation's agent token in the selected state file, whose path
    /// the test fills in after the fixture exists.
    OkRotateToken { target: RotateTarget, key: String, token: String },
}

type RotateTarget = Arc<Mutex<Option<PathBuf>>>;

/// Scripted fake OneBot behavior. Sends beyond `steps` are Reject when `reject_send`, else Ok.
#[derive(Default)]
struct Behavior {
    reject_send: bool,
    quote_missing: bool,
    /// The `message` of the quoted `get_msg` answer (default: one text segment `引用文本`).
    quote_message: Option<Value>,
    steps: std::collections::VecDeque<Step>,
    history: Vec<Value>,
    /// The first N history requests answer with an empty history (evidence appears later).
    history_hidden_calls: usize,
    /// Per `get_login_info` call: milliseconds to wait before answering (missing = 0).
    login_delays: std::collections::VecDeque<u64>,
    sends: i64,
}

impl Behavior {
    fn plain(reject_send: bool) -> Self {
        Self { reject_send, ..Self::default() }
    }
}

fn handle_onebot(
    stream: TcpStream,
    quote_group: i64,
    behavior: &mut Behavior,
    request_count: &std::sync::atomic::AtomicUsize,
) -> Option<OneBotRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut authorization = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_string();
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok()?;
            }
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).ok()?;
    let body: Value = serde_json::from_slice(&body).ok()?;
    request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut http_status = 200u16;
    let response: Option<String> = if path == "/set_input_status" {
        Some(json!({"status":"ok","retcode":0,"data":{}}).to_string())
    } else if path == "/get_msg" {
        Some(
            if behavior.quote_missing {
                json!({ "status": "failed", "retcode": 100, "wording": "fixture missing" })
            } else {
                json!({
                    "status": "ok",
                    "retcode": 0,
                    "data": {
                        "group_id": quote_group,
                        "user_id": 711,
                        "message_id": 90042,
                        "message_seq": 41,
                        "message": behavior.quote_message.clone().unwrap_or_else(|| json!([{"type":"text","data":{"text":"引用文本"}}])),
                        "sender": {"user_id": 711, "card": "fixture-card"}
                    }
                })
            }
            .to_string(),
        )
    } else if path == "/get_group_member_info" {
        Some(json!({ "status": "ok", "retcode": 0, "data": { "user_id": 822, "card": "被艾特的名片", "nickname": "nick" } }).to_string())
    } else if path == "/get_login_info" {
        if let Some(delay) = behavior.login_delays.pop_front() {
            thread::sleep(Duration::from_millis(delay));
        }
        Some(json!({ "status": "ok", "retcode": 0, "data": { "user_id": 9001 } }).to_string())
    } else if path == "/get_group_msg_history" || path == "/get_friend_msg_history" {
        {
        let messages = if behavior.history_hidden_calls > 0 {
            behavior.history_hidden_calls -= 1;
            Vec::new()
        } else {
            behavior.history.clone()
        };
        Some(json!({ "status": "ok", "retcode": 0, "data": { "messages": messages } }).to_string())
    }
    } else {
        behavior.sends += 1;
        let step = behavior.steps.pop_front().unwrap_or(if behavior.reject_send {
            Step::Reject
        } else {
            Step::Ok
        });
        let accepted =
            json!({ "status": "ok", "retcode": 0, "data": { "message_id": 91000 + behavior.sends } }).to_string();
        match step {
            Step::Ok => Some(accepted),
            Step::OkDelayed(ms) => {
                thread::sleep(Duration::from_millis(ms));
                Some(accepted)
            }
            Step::Failed(retcode) => Some(
                json!({ "status": "failed", "retcode": retcode, "wording": "fixture failure" }).to_string(),
            ),
            Step::HttpStatus(code) => {
                http_status = code;
                Some(json!({ "status": "failed", "retcode": 1400 }).to_string())
            }
            Step::OkRotateToken { target, key, token } => {
                let state_file = target.lock().unwrap().clone().expect("rotation target is set");
                let mut state: Value =
                    serde_json::from_slice(&fs::read(&state_file).expect("read state")).expect("state json");
                state["conversations"][&key]["agentToken"] = json!(token);
                fs::write(&state_file, state.to_string()).expect("rotate agent token");
                Some(accepted)
            }
            Step::Reject => Some(
                json!({ "status": "failed", "retcode": 1400, "wording": "fixture rejected" }).to_string(),
            ),
            Step::Drop => None,
        }
    };
    if let Some(response) = response {
        let stream = reader.get_mut();
        write!(
            stream,
            "HTTP/1.1 {http_status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.as_bytes().len(),
            response
        )
        .ok()?;
        stream.flush().ok()?;
    }
    Some(OneBotRequest {
        path,
        authorization,
        body,
    })
}

fn send_message(
    console: &ConsoleProcess,
    console_token: &str,
    agent_token: &str,
    payload: Value,
) -> HttpResponse {
    http_request(
        console.port,
        "POST",
        "/api/socialV2/send-message",
        Some(console_token),
        Some(agent_token),
        &payload,
    )
    .expect("send public HTTP request")
}

#[test]
fn send_message_keeps_two_selected_roots_and_complete_success_contract() {
    let onebot_a = FakeOneBot::start(70001, false);
    let onebot_b = FakeOneBot::start(70002, false);
    let root_a = fixture_root(
        "A",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot_a.url,
        "group:70001",
        &[70001],
        &[],
        20,
        json!([{ "seq": 42, "messageId": 90042 }]),
    );
    let root_b = fixture_root(
        "B",
        CONSOLE_B,
        AGENT_B,
        ONEBOT_B,
        &onebot_b.url,
        "private:9002",
        &[],
        &["9002"],
        2,
        json!([]),
    );
    // Both B bubbles deliberately hold, independent of disk time spent settling the first.
    let mut b_config: Value =
        serde_json::from_slice(&fs::read(root_b.join("config/config.json")).unwrap()).unwrap();
    b_config["socialV2"]["send"]["fillTypingBudget"] = json!(false);
    fs::write(root_b.join("config/config.json"), b_config.to_string()).unwrap();
    let console_a = ConsoleProcess::start(&root_a);
    let console_b = ConsoleProcess::start(&root_b);

    let response_a = send_message(
        &console_a,
        CONSOLE_A,
        AGENT_A,
        json!({
            "key": "group:70001",
            "messages": ["我们 在", "继续", "马上完成"],
            "gapMode": "fixed",
            "gaps": [0, 0],
            "replyToMessageId": "42",
            "atUserId": 711
        }),
    );
    assert_eq!(
        response_a.status, 200,
        "send-message response: {}",
        response_a.body
    );
    assert_eq!(response_a.body["ok"], true);
    assert_eq!(response_a.body["key"], "group:70001");
    assert_eq!(response_a.body["sent"], 3);
    assert_eq!(response_a.body["failed"], 0);
    assert_eq!(response_a.body["delays"], json!([0, 0]));
    assert_eq!(response_a.body["quoted"]["sender"], "fixture-card");
    assert_eq!(response_a.body["quoted"]["text"], "引用文本");
    // JS `resolveReplyInfo` returns exactly `{sender, text, userId}` for a verified quote; no `messageId`.
    assert_eq!(
        response_a.body["quoted"],
        json!({ "sender": "fixture-card", "text": "引用文本", "userId": "711" })
    );
    assert!(response_a.body["hint"].as_str().is_some());
    assert!(response_a.body["warn"]
        .as_str()
        .unwrap()
        .contains("中文空格"));
    assert!(response_a.body["splitWarn"]
        .as_str()
        .unwrap()
        .contains("同一句话"));
    let a_state_after_initial: Value =
        serde_json::from_slice(&fs::read(root_a.join("data/social-v2.json")).unwrap()).unwrap();

    let response_b = send_message(
        &console_b,
        CONSOLE_B,
        AGENT_B,
        json!({
            "key": "private:9002",
            "messages": ["长度", "短"],
            "gapMode": "byLength"
        }),
    );
    assert_eq!(
        response_b.status, 200,
        "send-message response: {}",
        response_b.body
    );
    assert_eq!(response_b.body["ok"], true);
    assert_eq!(response_b.body["key"], "private:9002");
    assert_eq!(response_b.body["sent"], 2);
    assert_eq!(response_b.body["failed"], 0);
    assert_eq!(response_b.body["delays"], json!([9]));
    assert!(response_b.body.get("hint").is_none());

    let b_calls_before_limit = onebot_b.request_count.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        b_calls_before_limit, 6,
        "two private sends and their typing pairs"
    );
    // A second B request exceeds the selected B minute quota; A's larger quota is independent.
    let limited_b = send_message(
        &console_b,
        CONSOLE_B,
        AGENT_B,
        json!({ "key": "private:9002", "messages": ["限频"] }),
    );
    assert_eq!(
        limited_b.status, 429,
        "rate-limit response: {}",
        limited_b.body
    );

    assert_eq!(
        onebot_b.request_count.load(std::sync::atomic::Ordering::SeqCst),
        b_calls_before_limit,
        "the rate-limited request must make no OneBot call, including typing"
    );

    let foreign_console = http_request(
        console_a.port,
        "POST",
        "/api/socialV2/send-message",
        Some(CONSOLE_B),
        Some(AGENT_A),
        &json!({ "key": "group:70001", "messages": ["不能复用根B的控制台令牌"] }),
    )
    .unwrap();
    assert_eq!(
        foreign_console.status, 401,
        "cross-root console-token response: {}",
        foreign_console.body
    );

    let foreign_agent = send_message(
        &console_a,
        CONSOLE_A,
        AGENT_B,
        json!({ "key": "group:70001", "messages": ["不能复用根B的会话令牌"] }),
    );
    assert_eq!(
        foreign_agent.status, 403,
        "cross-root token response: {}",
        foreign_agent.body
    );

    let foreign_allowlist = send_message(
        &console_b,
        CONSOLE_B,
        AGENT_B,
        json!({ "key": "group:70001", "messages": ["根B不允许这个群"] }),
    );
    assert_eq!(
        foreign_allowlist.status, 403,
        "selected allow-list response: {}",
        foreign_allowlist.body
    );

    let b_state_before_mode: Value =
        serde_json::from_slice(&fs::read(root_b.join("data/social-v2.json")).unwrap()).unwrap();
    fs::write(root_b.join("data/mode.json"), r#"{"mode":"reserved"}"#)
        .expect("change only synthetic root B's selected mode");
    let wrong_mode_b = send_message(
        &console_b,
        CONSOLE_B,
        AGENT_B,
        json!({ "key": "private:9002", "messages": ["根B模式拒绝"] }),
    );
    assert_eq!(
        wrong_mode_b.status, 403,
        "selected B mode response: {}",
        wrong_mode_b.body
    );

    // The two server processes keep their roots across interleaved calls and B's mode change.
    let a_state_before_last_send: Value =
        serde_json::from_slice(&fs::read(root_a.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(
        a_state_before_last_send, a_state_after_initial,
        "B's failures must not mutate root A"
    );
    let still_working_a = send_message(
        &console_a,
        CONSOLE_A,
        AGENT_A,
        json!({ "key": "group:70001", "messages": ["根A继续工作"] }),
    );
    assert_eq!(
        still_working_a.status, 200,
        "root A after root B mode change: {}",
        still_working_a.body
    );

    let requests_a = onebot_a.finish();
    let requests_b = onebot_b.finish();
    assert_eq!(
        requests_a.len(),
        5,
        "one quote lookup and four A sends: {requests_a:?}"
    );
    assert_eq!(requests_a[0].path, "/get_msg");
    assert_eq!(requests_a[0].body["message_id"], 90042);
    assert_eq!(requests_a[0].authorization, format!("Bearer {ONEBOT_A}"));
    assert_eq!(requests_a[1].path, "/send_group_msg");
    assert_eq!(requests_a[1].body["group_id"], 70001);
    assert_eq!(requests_a[1].body["message"][0]["type"], "reply");
    assert_eq!(requests_a[1].body["message"][0]["data"]["id"], "90042");
    assert_eq!(requests_a[1].body["message"][1]["type"], "at");
    assert_eq!(requests_a[1].body["message"][1]["data"]["qq"], "711");
    assert_eq!(requests_a[1].body["message"][2]["data"]["text"], "我们 在");
    assert_eq!(requests_a[2].body["message"].as_array().unwrap().len(), 1);
    assert_eq!(requests_a[3].body["message"].as_array().unwrap().len(), 1);
    assert_eq!(
        requests_a[4].body["message"][0]["data"]["text"],
        "根A继续工作"
    );
    assert!(
        requests_a.iter().all(|request| request.path != "/set_input_status"),
        "groups must never publish typing status"
    );
    assert_eq!(
        requests_b.len(), 6,
        "two B sends and four private typing calls only"
    );
    let b_sends: Vec<_> = requests_b
        .iter()
        .filter(|request| request.path == "/send_private_msg")
        .collect();
    assert_eq!(b_sends.len(), 2, "exactly two B sends");
    let b_typing: Vec<_> = requests_b
        .iter()
        .filter(|request| request.path == "/set_input_status")
        .map(|request| request.body.clone())
        .collect();
    assert_eq!(
        b_typing,
        vec![
            json!({"user_id":9002,"event_type":1}),
            json!({"user_id":9002,"event_type":0}),
            json!({"user_id":9002,"event_type":1}),
            json!({"user_id":9002,"event_type":0}),
        ]
    );
    assert_eq!(
        requests_b.iter().map(|request| request.path.as_str()).collect::<Vec<_>>(),
        vec![
            "/set_input_status", "/set_input_status", "/send_private_msg",
            "/set_input_status", "/set_input_status", "/send_private_msg",
        ]
    );
    assert!(requests_b
        .iter()
        .all(|request| request.authorization == format!("Bearer {ONEBOT_B}")));
    assert_eq!(b_sends[0].body["user_id"], 9002);
    assert_eq!(b_sends[0].body["message"][0]["data"]["text"], "长度");
    assert_eq!(b_sends[1].body["message"][0]["data"]["text"], "短");

    let state_a: Value =
        serde_json::from_slice(&fs::read(root_a.join("data/social-v2.json")).unwrap()).unwrap();
    let state_b: Value =
        serde_json::from_slice(&fs::read(root_b.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(
        state_a["conversations"]["group:70001"]["sendTimes"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        state_b["conversations"]["private:9002"]["sendTimes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(state_a["conversations"]["group:70001"]
        .get("lastAiReplyAt")
        .is_some());
    assert!(state_b["conversations"]["private:9002"]
        .get("lastAiReplyAt")
        .is_some());
    assert!(state_a["conversations"].get("private:9002").is_none());
    assert!(state_b["conversations"].get("group:70001").is_none());
    assert_eq!(
        state_b, b_state_before_mode,
        "rejected B calls must not write its selected state"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(root_b.join("data/mode.json")).unwrap()).unwrap()
            ["mode"],
        "reserved"
    );
}

#[test]
fn send_message_rejects_bad_auth_mode_allow_and_disabled_tool_without_onebot_calls() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root(
        "denials",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        20,
        json!([]),
    );
    let console = ConsoleProcess::start(&root);
    let payload = json!({ "key": "group:70001", "messages": ["fixture"] });

    let bad_console = http_request(
        console.port,
        "POST",
        "/api/socialV2/send-message",
        Some("wrong-console-token"),
        Some(AGENT_A),
        &payload,
    )
    .unwrap();
    assert_eq!(
        bad_console.status, 401,
        "console authorization response: {}",
        bad_console.body
    );

    let bad_agent = send_message(&console, CONSOLE_A, "wrong-agent-token", payload.clone());
    assert_eq!(
        bad_agent.status, 403,
        "agent authorization response: {}",
        bad_agent.body
    );

    let denied_key = send_message(
        &console,
        CONSOLE_A,
        AGENT_A,
        json!({ "key": "group:70099", "messages": ["fixture"] }),
    );
    assert_eq!(
        denied_key.status, 403,
        "allow-list response: {}",
        denied_key.body
    );

    fs::write(root.join("data/mode.json"), r#"{"mode":"reserved"}"#)
        .expect("change only the synthetic selected mode");
    let wrong_mode = send_message(&console, CONSOLE_A, AGENT_A, payload.clone());
    assert_eq!(
        wrong_mode.status, 403,
        "selected mode response: {}",
        wrong_mode.body
    );

    fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#)
        .expect("restore synthetic selected mode");
    let mut config: Value =
        serde_json::from_slice(&fs::read(root.join("config/config.json")).unwrap()).unwrap();
    config["socialV2"]["tools"]["sendMessage"] = json!(false);
    fs::write(
        root.join("config/config.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .expect("disable only the synthetic selected tool");
    let disabled_tool = send_message(&console, CONSOLE_A, AGENT_A, payload);
    assert_eq!(
        disabled_tool.status, 403,
        "tool policy response: {}",
        disabled_tool.body
    );

    assert!(
        onebot.finish().is_empty(),
        "denied requests must not reach OneBot"
    );
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap();
    assert!(state["conversations"]["group:70001"]["sendTimes"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn send_message_merges_messages_when_selected_config_enables_merging() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root(
        "merge",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        20,
        json!([]),
    );
    let mut config: Value =
        serde_json::from_slice(&fs::read(root.join("config/config.json")).unwrap()).unwrap();
    config["socialV2"]["send"]["mergeMultiIntoOne"] = json!(true);
    fs::write(
        root.join("config/config.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .expect("enable merging only in the selected fixture");
    let console = ConsoleProcess::start(&root);
    let response = send_message(
        &console,
        CONSOLE_A,
        AGENT_A,
        json!({ "key": "group:70001", "messages": ["第一段", "第二段"], "gapMode": "fixed", "gaps": [0] }),
    );
    assert_eq!(
        response.status, 200,
        "merged send response: {}",
        response.body
    );
    assert_eq!(response.body["sent"], 1);
    assert_eq!(response.body["failed"], 0);
    assert_eq!(response.body["delays"], json!([]));
    let requests = onebot.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/send_group_msg");
    assert_eq!(requests[0].body["message"].as_array().unwrap().len(), 1);
    assert_eq!(
        requests[0].body["message"][0]["data"]["text"],
        "第一段\n第二段"
    );
}

#[test]
fn send_message_quote_owned_by_another_group_falls_back_to_local_info_without_foreign_content() {
    // Frozen JS `resolveReplyTargetV2`: a reference that hits a local `seq` with a real messageId is
    // resolved by that messageId only; when OneBot says the message belongs to another conversation
    // the local record is used (`{sender, text, userId: null, messageId, seq}`) and the send goes on.
    // Only a reference with no local record is refused with 400 (see `UNRESOLVED_QUOTE`).
    // The fake deliberately returns another group's message for the selected local reference.
    let onebot = FakeOneBot::start(70099, false);
    let root = fixture_root(
        "foreign-quote",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        20,
        json!([{ "seq": 42, "messageId": 90042 }]),
    );
    let console = ConsoleProcess::start(&root);
    let response = send_message(
        &console,
        CONSOLE_A,
        AGENT_A,
        json!({ "key": "group:70001", "messages": ["不能跨群引用"], "replyToMessageId": "42" }),
    );
    assert_eq!(
        response.status, 200,
        "foreign quote response: {}",
        response.body
    );
    assert_eq!(response.body["ok"], true);
    assert_eq!(response.body["sent"], 1);
    assert_eq!(
        response.body["quoted"],
        json!({ "sender": "", "text": "", "userId": null, "messageId": "90042", "seq": 42 }),
        "the foreign group's sender and text must not be shown as the quote"
    );
    let requests = onebot.finish();
    assert_eq!(
        requests.len(),
        2,
        "resolve the local message id once, then send: {requests:?}"
    );
    assert_eq!(requests[0].path, "/get_msg");
    assert_eq!(requests[0].body["message_id"], 90042);
    assert_eq!(requests[1].path, "/send_group_msg");
    assert_eq!(requests[1].body["group_id"], 70001);
    assert_eq!(requests[1].body["message"][0]["type"], "reply");
    assert_eq!(requests[1].body["message"][0]["data"]["id"], "90042");
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap();
    assert_eq!(
        state["conversations"]["group:70001"]["sendTimes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn send_message_onebot_failure_returns_error_without_committing_success_state() {
    let onebot = FakeOneBot::start(70001, true);
    let root = fixture_root(
        "onebot-failure",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        20,
        json!([]),
    );
    let console = ConsoleProcess::start(&root);
    let response = send_message(
        &console,
        CONSOLE_A,
        AGENT_A,
        json!({ "key": "group:70001", "messages": ["fixture"] }),
    );
    assert_eq!(
        response.status, 500,
        "send error response: {}",
        response.body
    );
    assert_eq!(response.body["ok"], false);
    assert!(response.body["error"]
        .as_str()
        .unwrap()
        .contains("已发 0 条"));

    let requests = onebot.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/send_group_msg");
    assert_eq!(requests[0].authorization, format!("Bearer {ONEBOT_A}"));
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap();
    assert!(state["conversations"]["group:70001"]["sendTimes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(state["conversations"]["group:70001"]
        .get("lastAiReplyAt")
        .is_none());
}

#[test]
fn send_message_rejects_oversized_declared_body_after_auth_without_side_effects() {
    const MAX_BODY_BYTES: usize = 1_000_000;
    let onebot_a = FakeOneBot::start(70001, false);
    let onebot_b = FakeOneBot::start(70002, false);
    let root_a = fixture_root(
        "oversized-body-a",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot_a.url,
        "group:70001",
        &[70001],
        &[],
        20,
        json!([]),
    );
    let root_b = fixture_root(
        "oversized-body-b",
        CONSOLE_B,
        AGENT_B,
        ONEBOT_B,
        &onebot_b.url,
        "group:70002",
        &[70002],
        &[],
        20,
        json!([]),
    );
    let state_a = root_a.join("data/social-v2.json");
    let state_b = root_b.join("data/social-v2.json");
    let state_a_before = fs::read(&state_a).expect("snapshot selected A state");
    let state_b_before = fs::read(&state_b).expect("snapshot selected B state");
    let console_a = ConsoleProcess::start(&root_a);
    let console_b = ConsoleProcess::start(&root_b);

    let unauthorized = http_request_with_declared_length(
        console_a.port,
        Some("wrong-console-token"),
        Some(AGENT_A),
        MAX_BODY_BYTES + 1,
    )
    .expect("read the auth response for an oversized request");
    assert_eq!(unauthorized.status, 401, "console auth must precede body limits");
    assert_eq!(
        unauthorized.body["error"],
        "未授权：请提供控制台访问令牌"
    );

    for (console, console_token) in [(&console_a, CONSOLE_A), (&console_b, CONSOLE_B)] {
        let response = http_request_with_declared_length(
            console.port,
            Some(console_token),
            Some(if console.port == console_a.port { AGENT_A } else { AGENT_B }),
            MAX_BODY_BYTES + 1,
        )
        .expect("read the selected send-message body-limit response");
        assert_eq!(response.status, 413, "oversized selected send must be rejected");
        assert_eq!(response.body["error"], "请求体过大（超过 1MB）");
    }

    assert_eq!(fs::read(&state_a).unwrap(), state_a_before);
    assert_eq!(fs::read(&state_b).unwrap(), state_b_before);
    assert!(onebot_a.finish().is_empty(), "A must not contact OneBot");
    assert!(onebot_b.finish().is_empty(), "B must not contact OneBot");
}

#[test]
fn send_message_rejects_oversized_chunked_body_after_auth_without_side_effects() {
    const MAX_BODY_BYTES: usize = 1_000_000;
    let onebot_a = FakeOneBot::start(70001, false);
    let onebot_b = FakeOneBot::start(70002, false);
    let root_a = fixture_root(
        "chunked-oversized-body-a",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot_a.url,
        "group:70001",
        &[70001],
        &[],
        20,
        json!([]),
    );
    let root_b = fixture_root(
        "chunked-oversized-body-b",
        CONSOLE_B,
        AGENT_B,
        ONEBOT_B,
        &onebot_b.url,
        "group:70002",
        &[70002],
        &[],
        20,
        json!([]),
    );
    let state_a = root_a.join("data/social-v2.json");
    let state_b = root_b.join("data/social-v2.json");
    let config_a = root_a.join("config/config.json");
    let config_b = root_b.join("config/config.json");
    let mode_a = root_a.join("data/mode.json");
    let mode_b = root_b.join("data/mode.json");
    let dependencies_a = root_a.join("dependencies.json");
    let dependencies_b = root_b.join("dependencies.json");
    let state_a_before = fs::read(&state_a).expect("snapshot selected A state");
    let state_b_before = fs::read(&state_b).expect("snapshot selected B state");
    let config_a_before = fs::read(&config_a).expect("snapshot selected A config");
    let config_b_before = fs::read(&config_b).expect("snapshot selected B config");
    let mode_a_before = fs::read(&mode_a).expect("snapshot selected A mode");
    let mode_b_before = fs::read(&mode_b).expect("snapshot selected B mode");
    let dependencies_a_before = fs::read(&dependencies_a).expect("snapshot selected A dependencies");
    let dependencies_b_before = fs::read(&dependencies_b).expect("snapshot selected B dependencies");
    let console_a = ConsoleProcess::start(&root_a);
    let console_b = ConsoleProcess::start(&root_b);

    let unauthorized = http_request_with_chunked_oversize_body(
        console_a.port,
        Some("wrong-console-token"),
        Some(AGENT_A),
        "group:70001",
        MAX_BODY_BYTES,
    )
    .expect("read the auth response for an oversized chunked request");
    assert_eq!(unauthorized.status, 401, "console auth must precede body limits");
    assert_eq!(
        unauthorized.body["error"],
        "未授权：请提供控制台访问令牌"
    );

    let empty_agent = http_request_with_chunked_oversize_body(
        console_a.port,
        Some(CONSOLE_A),
        Some(""),
        "group:70001",
        MAX_BODY_BYTES,
    )
    .expect("read the global agent-gate response before the body limit");
    assert_eq!(empty_agent.status, 403, "agent gate must precede body limits");
    assert_eq!(empty_agent.body["error"], "agent token 不能为空");

    let paused_state = json!({
        "paused": true,
        "conversations": { "group:70001": { "agentToken": AGENT_A, "sendTimes": [] } }
    });
    fs::write(&state_a, serde_json::to_vec(&paused_state).unwrap()).unwrap();
    let paused_before = fs::read(&state_a).unwrap();
    let paused = http_request_with_chunked_oversize_body(
        console_a.port,
        Some(CONSOLE_A),
        Some(AGENT_A),
        "group:70001",
        MAX_BODY_BYTES,
    )
    .expect("read the paused gate response before the body limit");
    assert_eq!(paused.status, 403, "paused gate must precede body limits");
    assert_eq!(paused.body["error"], "AI 已暂停，当前不允许执行 v2 工具");
    assert_eq!(fs::read(&state_a).unwrap(), paused_before);
    fs::write(&state_a, &state_a_before).unwrap();

    let mut disabled_config: Value = serde_json::from_slice(&config_a_before).unwrap();
    disabled_config["socialV2"]["enabled"] = json!(false);
    fs::write(&config_a, serde_json::to_vec(&disabled_config).unwrap()).unwrap();
    let disabled_before = fs::read(&config_a).unwrap();
    let disabled = http_request_with_chunked_oversize_body(
        console_a.port,
        Some(CONSOLE_A),
        Some(AGENT_A),
        "group:70001",
        MAX_BODY_BYTES,
    )
    .expect("read the disabled-v2 gate response before the body limit");
    assert_eq!(disabled.status, 403, "v2 enabled gate must precede body limits");
    assert_eq!(disabled.body["error"], "二代模式已关闭，当前不允许执行 v2 工具");
    assert_eq!(fs::read(&config_a).unwrap(), disabled_before);
    fs::write(&config_a, &config_a_before).unwrap();

    fs::write(&mode_a, br#"{"mode":"chat"}"#).unwrap();
    let mode_chat_before = fs::read(&mode_a).unwrap();
    let wrong_mode = http_request_with_chunked_oversize_body(
        console_a.port,
        Some(CONSOLE_A),
        Some(AGENT_A),
        "group:70001",
        MAX_BODY_BYTES,
    )
    .expect("read the mode gate response before the body limit");
    assert_eq!(wrong_mode.status, 403, "mode gate must precede body limits");
    assert_eq!(wrong_mode.body["error"], "该接口仅 reserved2 模式可用");
    assert_eq!(fs::read(&mode_a).unwrap(), mode_chat_before);
    fs::write(&mode_a, &mode_a_before).unwrap();

    for (console, console_token, agent_token, key) in [
        (&console_a, CONSOLE_A, AGENT_A, "group:70001"),
        (&console_b, CONSOLE_B, AGENT_B, "group:70002"),
    ] {
        let response = http_request_with_chunked_oversize_body(
            console.port,
            Some(console_token),
            Some(agent_token),
            key,
            MAX_BODY_BYTES,
        )
        .expect("read the selected send-message chunked body-limit response");
        assert_eq!(response.status, 413, "oversized selected send must be rejected");
        assert_eq!(response.body["error"], "请求体过大（超过 1MB）");
    }

    assert_eq!(fs::read(&state_a).unwrap(), state_a_before);
    assert_eq!(fs::read(&state_b).unwrap(), state_b_before);
    assert_eq!(fs::read(&config_a).unwrap(), config_a_before);
    assert_eq!(fs::read(&config_b).unwrap(), config_b_before);
    assert_eq!(fs::read(&mode_a).unwrap(), mode_a_before);
    assert_eq!(fs::read(&mode_b).unwrap(), mode_b_before);
    assert_eq!(fs::read(&dependencies_a).unwrap(), dependencies_a_before);
    assert_eq!(fs::read(&dependencies_b).unwrap(), dependencies_b_before);
    assert!(onebot_a.finish().is_empty(), "A must not contact OneBot");
    assert!(onebot_b.finish().is_empty(), "B must not contact OneBot");
}

fn probe_ipv6_loopback() -> Result<(), (&'static str, std::io::Error)> {
    let listener = TcpListener::bind(("::1", 0)).map_err(|error| ("bind", error))?;
    let address = listener
        .local_addr()
        .map_err(|error| ("local_addr", error))?;
    let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|error| ("connect", error))?;
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| ("client timeout", error))?;
    let (mut accepted, _) = listener.accept().map_err(|error| ("accept", error))?;
    client
        .write_all(b"ipv6-loopback-probe")
        .map_err(|error| ("client write", error))?;
    let mut request = [0u8; 19];
    accepted
        .read_exact(&mut request)
        .map_err(|error| ("server read", error))?;
    assert_eq!(&request, b"ipv6-loopback-probe");
    accepted
        .write_all(b"ipv6-loopback-ok")
        .map_err(|error| ("server write", error))?;
    let mut response = [0u8; 16];
    client
        .read_exact(&mut response)
        .map_err(|error| ("client read", error))?;
    assert_eq!(&response, b"ipv6-loopback-ok");
    Ok(())
}

#[test]
fn send_message_uses_bracketed_selected_ipv6_onebot_endpoint_when_available() {
    match probe_ipv6_loopback() {
        Ok(()) => {}
        Err(("connect", error)) if error.raw_os_error() == Some(10013) => {
            eprintln!("SKIP: independent IPv6 loopback connect probe denied by Windows (10013)");
            return;
        }
        Err((stage, error)) => panic!("independent IPv6 loopback {stage} probe failed: {error}"),
    }
    let onebot = FakeOneBot::start_ipv6(70001, false)
        .expect("bind synthetic IPv6 OneBot after independent loopback probe passed");
    let root = fixture_root(
        "ipv6",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        10,
        json!([]),
    );
    let console = ConsoleProcess::start(&root);
    let response = send_message(
        &console,
        CONSOLE_A,
        AGENT_A,
        json!({ "key": "group:70001", "messages": ["IPv6 fixture"] }),
    );
    assert_eq!(
        response.status, 200,
        "IPv6 send response: {}",
        response.body
    );
    let requests = onebot.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/send_group_msg");
    assert_eq!(requests[0].authorization, format!("Bearer {ONEBOT_A}"));
    assert_eq!(requests[0].body["group_id"], 70001);
}

// ── Ticket 08: durable outgoing sends ─────────────────────────────────────────

fn read_selected_state(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap()
}

fn read_outbox(root: &Path) -> Vec<Value> {
    fs::read_to_string(root.join("data/send-outbox.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("outbox line is JSON"))
        .collect()
}

fn send_paths(requests: &[OneBotRequest]) -> Vec<&OneBotRequest> {
    requests
        .iter()
        .filter(|r| r.path == "/send_group_msg" || r.path == "/send_private_msg")
        .collect()
}

fn segment_text(request: &OneBotRequest) -> String {
    request.body["message"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["type"] == "text")
        .map(|s| s["data"]["text"].as_str().unwrap().to_string())
        .collect()
}

fn post_send(console: &ConsoleProcess, agent_token: &str, payload: Value) -> HttpResponse {
    send_message(console, CONSOLE_A, agent_token, payload)
}

#[test]
fn durable_partial_success_resumes_without_resending_confirmed_items() {
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Ok, Step::Reject].into_iter().collect(),
            ..Behavior::default()
        },
    );
    let root = fixture_root("durable-partial", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);

    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["一", "二", "三"], "gapMode": "fixed", "gaps": [0, 0] }));
    assert_eq!(first.status, 500, "partial batch body: {}", first.body);
    assert_eq!(first.body["ok"], false);
    assert_eq!(first.body["sent"], 2);
    assert_eq!(first.body["failed"], 1);
    assert_eq!(first.body["pending"], json!([1]));
    assert_eq!(first.body["notCommitted"], json!([1]));
    assert!(first.body["error"].as_str().unwrap().contains("已发 2 条"));
    let batch_id = first.body["batchId"].as_str().unwrap().to_string();

    let state = read_selected_state(&root);
    let conv = &state["conversations"]["group:70001"];
    assert_eq!(conv["sendTimes"].as_array().unwrap().len(), 2, "refused item gives its reservation back");
    let recent: Vec<&str> = conv["recentMessages"].as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap()).collect();
    assert_eq!(recent, vec!["一", "三"], "only delivered texts become the bot's recent messages");
    assert!(conv.get("lastAiReplyAt").is_none(), "a partial batch is not a completed reply");

    let resumed = post_send(&console, AGENT_A, json!({ "key": "group:70001", "resumeBatchId": batch_id }));
    assert_eq!(resumed.status, 200, "resume body: {}", resumed.body);
    assert_eq!(resumed.body["ok"], true);
    assert_eq!(resumed.body["sent"], 3);
    assert_eq!(resumed.body["pending"], json!([]));

    let requests = onebot.finish();
    let texts: Vec<String> = send_paths(&requests).into_iter().map(segment_text).collect();
    assert_eq!(texts, vec!["一", "二", "三", "二"], "resume sends only the refused item again");

    let view = http_request(console.port, "GET", "/api/socialV2/send-outbox?key=group:70001", Some(CONSOLE_A), None, &Value::Null).unwrap();
    assert_eq!(view.status, 200);
    let batch = &view.body["batches"][0];
    assert_eq!(batch["batchId"], json!(batch_id));
    let states: Vec<&str> = batch["items"].as_array().unwrap().iter().map(|i| i["state"].as_str().unwrap()).collect();
    assert_eq!(states, vec!["confirmed", "confirmed", "confirmed"]);

    assert!(read_outbox(&root).iter().any(|r| r["k"] == "not_committed" && r["idx"] == 1));
}

#[test]
fn unknown_result_is_recorded_and_never_resent_without_evidence() {
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Drop].into_iter().collect(),
            ..Behavior::default()
        },
    );
    let root = fixture_root("durable-unknown", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);

    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["未知结果"] }));
    assert_eq!(first.status, 500, "unknown body: {}", first.body);
    assert_eq!(first.body["sent"], 0);
    assert_eq!(first.body["unknown"], json!([0]));
    let batch_id = first.body["batchId"].as_str().unwrap().to_string();
    let reserved = read_selected_state(&root)["conversations"]["group:70001"]["sendTimes"].as_array().unwrap().len();
    assert_eq!(reserved, 1, "an unknown item keeps its reservation");

    let resumed = post_send(&console, AGENT_A, json!({ "key": "group:70001", "resumeBatchId": batch_id }));
    assert_eq!(resumed.status, 409, "resume must refuse: {}", resumed.body);
    assert_eq!(resumed.body["unknown"], json!([0]));

    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 1, "no automatic resend of an unverified item");
    assert!(read_outbox(&root).iter().any(|r| r["k"] == "unknown" && r["idx"] == 0));
}

#[test]
fn unknown_result_is_confirmed_when_history_shows_our_own_message() {
    let now_s = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Drop].into_iter().collect(),
            history: vec![json!({
                "message_id": 77001,
                "message_type": "group",
                "time": now_s + 5,
                "sender": { "user_id": 9001 },
                "message": [{ "type": "text", "data": { "text": "已核实" } }]
            })],
            ..Behavior::default()
        },
    );
    let root = fixture_root("durable-verified", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);

    let response = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["已核实"] }));
    assert_eq!(response.status, 200, "verified body: {}", response.body);
    assert_eq!(response.body["sent"], 1);
    assert_eq!(response.body["failed"], 0);

    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 1);
    let confirmed = read_outbox(&root)
        .into_iter()
        .find(|r| r["k"] == "confirmed")
        .expect("verified outcome is recorded");
    assert_eq!(confirmed["messageId"], 77001);
    assert_eq!(confirmed["verified"], true);
}

#[test]
fn concurrent_sends_share_one_persisted_quota_reservation() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("durable-quota", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 2, json!([]));
    let console = ConsoleProcess::start(&root);
    let port = console.port;

    let statuses: Vec<u16> = thread::scope(|scope| {
        let handles: Vec<_> = ["甲", "乙", "丙"]
            .into_iter()
            .map(|text| {
                scope.spawn(move || {
                    http_request(
                        port,
                        "POST",
                        "/api/socialV2/send-message",
                        Some(CONSOLE_A),
                        Some(AGENT_A),
                        &json!({ "key": "group:70001", "messages": [text] }),
                    )
                    .expect("send public HTTP request")
                    .status
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut sorted = statuses.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![200, 200, 429], "two sends fit the per-minute quota, the third is refused");

    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 2, "the refused send never reaches OneBot");
}

#[test]
fn concurrent_sends_wait_out_a_stalled_state_gate_holder_instead_of_refusing() {
    // A holder that keeps `locks/social-v2.lock` for far longer than the old 300 ms wait (a slow
    // fsync or a descheduled writer) must delay the sends, not turn one of them into a 503.
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("durable-quota-stall", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 2, json!([]));
    let console = ConsoleProcess::start(&root);
    let port = console.port;

    fs::create_dir_all(root.join("data/locks")).unwrap();
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(root.join("data/locks/social-v2.lock"))
        .unwrap();
    holder.try_lock().expect("test takes the state gate first");

    let statuses: Vec<u16> = thread::scope(|scope| {
        let handles: Vec<_> = ["甲", "乙", "丙"]
            .into_iter()
            .map(|text| {
                scope.spawn(move || {
                    http_request(
                        port,
                        "POST",
                        "/api/socialV2/send-message",
                        Some(CONSOLE_A),
                        Some(AGENT_A),
                        &json!({ "key": "group:70001", "messages": [text] }),
                    )
                    .expect("send public HTTP request")
                    .status
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(900));
        drop(holder);
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut sorted = statuses.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![200, 200, 429], "a stalled gate holder delays the sends; it must not refuse them");

    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 2, "the refused send never reaches OneBot");
}

/// Gap batch small-parity item 2: the quote preview is the quoted message's own words rendered by
/// `segmentsToText` (`@name` through the member lookup, media labels), not the old `[媒体]` form.
#[test]
fn quote_preview_renders_the_quoted_message_like_segments_to_text() {
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            quote_message: Some(json!([
                {"type":"reply","data":{"id":"1"}},
                {"type":"text","data":{"text":"看 "}},
                {"type":"at","data":{"qq":"822"}},
                {"type":"image","data":{"file":"a.png"}},
                {"type":"face","data":{"id":14}},
                {"type":"mface","data":{"summary":"给你糖吃"}},
                {"type":"json","data":{}},
                {"type":5}
            ])),
            ..Behavior::default()
        },
    );
    let root = fixture_root("quote-preview", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let response = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["引用"], "replyToMessageId": "90042" }));
    assert_eq!(response.status, 200, "send body: {}", response.body);
    assert_eq!(
        response.body["quoted"]["text"],
        "看 @被艾特的名片[图片][表情14][表情:给你糖吃][卡片消息][5]"
    );
    let _ = onebot.finish();
}

#[test]
fn quote_failure_gives_back_the_reservation_without_any_request() {
    let onebot = FakeOneBot::start_with(70001, Behavior { quote_missing: true, ..Behavior::default() });
    let root = fixture_root("durable-quote-miss", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);

    let response = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["引用"], "replyToMessageId": "90042" }));
    assert_eq!(response.status, 400, "quote body: {}", response.body);
    let state = read_selected_state(&root);
    assert!(state["conversations"]["group:70001"]["sendTimes"].as_array().unwrap().is_empty());

    let requests = onebot.finish();
    assert!(send_paths(&requests).is_empty());
}

#[test]
fn stale_turn_batch_is_refused_after_the_agent_token_changes() {
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Ok, Step::Reject].into_iter().collect(),
            ..Behavior::default()
        },
    );
    let root = fixture_root("durable-stale", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["甲", "乙"], "gapMode": "fixed", "gaps": [0] }));
    assert_eq!(first.status, 500);
    let batch_id = first.body["batchId"].as_str().unwrap().to_string();

    let mut state = read_selected_state(&root);
    state["conversations"]["group:70001"]["agentToken"] = json!("fixture-agent-token-rotated");
    fs::write(root.join("data/social-v2.json"), state.to_string()).unwrap();

    let resumed = post_send(&console, "fixture-agent-token-rotated", json!({ "key": "group:70001", "resumeBatchId": batch_id }));
    assert_eq!(resumed.status, 409, "old-turn batch must be refused: {}", resumed.body);

    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 2, "refused resume sends nothing");
}

#[test]
fn token_rotation_during_a_batch_cancels_the_remaining_items() {
    let rotate_target: RotateTarget = Default::default();
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::OkRotateToken {
                target: rotate_target.clone(),
                key: "group:70001".to_string(),
                token: "fixture-agent-token-rotated".to_string(),
            }]
            .into_iter()
            .collect(),
            ..Behavior::default()
        },
    );
    let root = fixture_root("durable-cancel", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    *rotate_target.lock().unwrap() = Some(root.join("data/social-v2.json"));
    let console = ConsoleProcess::start(&root);

    let response = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["甲", "乙", "丙"], "gapMode": "fixed", "gaps": [0, 0] }));
    assert_eq!(response.status, 500, "cancel body: {}", response.body);
    assert_eq!(response.body["sent"], 1);
    assert_eq!(response.body["cancelled"], json!([1, 2]));
    assert!(response.body["error"].as_str().unwrap().contains("发送已取消"));

    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 1, "cancelled items never reach OneBot");
    assert_eq!(read_selected_state(&root)["conversations"]["group:70001"]["sendTimes"].as_array().unwrap().len(), 1);
}

#[test]
fn silent_mode_blocks_non_owner_sends_before_any_request() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("durable-silent", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    fs::write(root.join("data/current-role.json"), r#"{"role":null,"mode":"silent"}"#).unwrap();
    let console = ConsoleProcess::start(&root);

    let response = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["静默"] }));
    assert_eq!(response.status, 403, "silent body: {}", response.body);
    let requests = onebot.finish();
    assert!(send_paths(&requests).is_empty());
}

#[test]
fn quote_and_at_stay_on_the_first_segment_in_order() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("durable-segments", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);

    let response = post_send(
        &console,
        AGENT_A,
        json!({ "key": "group:70001", "messages": ["第一", "第二"], "replyToMessageId": "90042", "atUserId": 711 }),
    );
    assert_eq!(response.status, 200, "segments body: {}", response.body);

    let requests = onebot.finish();
    let sends = send_paths(&requests);
    assert_eq!(sends.len(), 2);
    let first_types: Vec<&str> = sends[0].body["message"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
    assert_eq!(first_types, vec!["reply", "at", "text"]);
    assert_eq!(sends[0].body["message"][0]["data"]["id"], "90042");
    let second_types: Vec<&str> = sends[1].body["message"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
    assert_eq!(second_types, vec!["text"]);
}

#[test]
fn crash_between_attempt_and_outcome_restarts_as_unknown_and_resume_verifies_before_any_resend() {
    let now_s = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Drop].into_iter().collect(),
            history_hidden_calls: 1,
            history: vec![json!({
                "message_id": 77002,
                "message_type": "group",
                "time": now_s + 5,
                "sender": { "user_id": 9001 },
                "message": [{ "type": "text", "data": { "text": "重启前发出" } }]
            })],
            ..Behavior::default()
        },
    );
    let root = fixture_root("durable-restart", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["重启前发出"] }));
    assert_eq!(first.status, 500, "unknown body: {}", first.body);
    let batch_id = first.body["batchId"].as_str().unwrap().to_string();
    drop(console);

    // Simulate a crash right after the request: the durable file ends at the attempt record.
    let path = root.join("data/send-outbox.jsonl");
    let kept: Vec<String> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|line| !line.contains("\"k\":\"unknown\""))
        .map(str::to_string)
        .collect();
    assert_eq!(kept.len(), 2, "intent + attempt remain: {kept:?}");
    fs::write(&path, kept.join("
") + "
").unwrap();

    let console = ConsoleProcess::start(&root);
    let view = http_request(console.port, "GET", "/api/socialV2/send-outbox?key=group:70001", Some(CONSOLE_A), None, &Value::Null).unwrap();
    assert_eq!(view.body["batches"][0]["items"][0]["state"], "unknown", "attempt without outcome is unknown after restart");
    assert_eq!(view.body["batches"][0]["sent"], 0);

    let resumed = post_send(&console, AGENT_A, json!({ "key": "group:70001", "resumeBatchId": batch_id }));
    assert_eq!(resumed.status, 200, "resume body: {}", resumed.body);
    assert_eq!(resumed.body["sent"], 1);

    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 1, "history evidence confirms the item; nothing is resent");
    let confirmed = read_outbox(&root).into_iter().rev().find(|r| r["k"] == "confirmed").unwrap();
    assert_eq!(confirmed["messageId"], 77002);
    assert_eq!(confirmed["verified"], true);
}

#[test]
fn unreachable_gateway_is_recorded_as_not_committed_and_resumable() {
    let onebot = FakeOneBot::start(70001, false);
    let dead_port = free_port();
    let dead_url = format!("http://127.0.0.1:{dead_port}");
    let root = fixture_root("durable-dead", CONSOLE_A, AGENT_A, ONEBOT_A, &dead_url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["连不上"] }));
    assert_eq!(first.status, 500, "dead gateway body: {}", first.body);
    assert_eq!(first.body["notCommitted"], json!([0]));
    assert_eq!(first.body["unknown"], json!([]));
    assert!(read_selected_state(&root)["conversations"]["group:70001"]["sendTimes"].as_array().unwrap().is_empty());
    drop(onebot);
}


fn history_message(message_id: i64, time: i64, text: &str) -> Value {
    json!({
        "message_id": message_id,
        "message_type": "group",
        "time": time,
        "sender": { "user_id": 9001 },
        "message": [{ "type": "text", "data": { "text": text } }]
    })
}

fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn send_texts(requests: &[OneBotRequest]) -> Vec<String> {
    send_paths(requests).into_iter().map(segment_text).collect()
}

fn resume(console: &ConsoleProcess, batch_id: &str) -> HttpResponse {
    post_send(console, AGENT_A, json!({ "key": "group:70001", "resumeBatchId": batch_id }))
}

// ---- Ticket 08 hardening -------------------------------------------------------------------

#[test]
fn concurrent_resumes_of_one_batch_never_send_an_item_twice() {
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Drop, Step::Reject].into_iter().collect(),
            // Call 1 is the verification of the original run; call 2 holds the first resume
            // before its send so the second resume overlaps it.
            login_delays: [0u64, 800].into_iter().collect(),
            ..Behavior::default()
        },
    );
    let root = fixture_root("harden-resume-race", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["一", "二", "三"], "gapMode": "fixed", "gaps": [0, 0] }));
    assert_eq!(first.status, 500, "setup body: {}", first.body);
    assert_eq!(first.body["unknown"], json!([0]));
    assert_eq!(first.body["notCommitted"], json!([1]));
    let batch_id = first.body["batchId"].as_str().unwrap().to_string();

    let port = console.port;
    let id_a = batch_id.clone();
    let worker = thread::spawn(move || {
        http_request(port, "POST", "/api/socialV2/send-message", Some(CONSOLE_A), Some(AGENT_A),
            &json!({ "key": "group:70001", "resumeBatchId": id_a })).unwrap()
    });
    thread::sleep(Duration::from_millis(250));
    let second = resume(&console, &batch_id);
    let first_resume = worker.join().unwrap();

    assert_eq!(second.status, 409, "overlapping resume must be refused: {}", second.body);
    assert!(second.body.to_string().contains("进行中"), "refusal explains why: {}", second.body);
    assert_eq!(first_resume.status, 500, "first resume still has the unknown item: {}", first_resume.body);
    let requests = onebot.finish();
    assert_eq!(send_texts(&requests), vec!["一", "二", "三", "二"], "item 二 is resent exactly once");
}

#[test]
fn resume_racing_the_original_send_is_refused_and_never_duplicates() {
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::OkDelayed(900)].into_iter().collect(),
            ..Behavior::default()
        },
    );
    let root = fixture_root("harden-original-race", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let port = console.port;
    let original = thread::spawn(move || {
        http_request(port, "POST", "/api/socialV2/send-message", Some(CONSOLE_A), Some(AGENT_A),
            &json!({ "key": "group:70001", "messages": ["一", "二"], "gapMode": "fixed", "gaps": [0] })).unwrap()
    });
    // Find the running batch through the durable view once its attempt is recorded.
    let deadline = Instant::now() + Duration::from_secs(5);
    let batch_id = loop {
        let view = http_request(console.port, "GET", "/api/socialV2/send-outbox?key=group:70001", Some(CONSOLE_A), None, &Value::Null).unwrap();
        if let Some(id) = view.body["batches"][0]["batchId"].as_str() {
            if read_outbox(&root).iter().any(|r| r["k"] == "attempt") {
                break id.to_string();
            }
        }
        assert!(Instant::now() < deadline, "batch never appeared in the outbox");
        thread::sleep(Duration::from_millis(40));
    };
    let racing = resume(&console, &batch_id);
    let original = original.join().unwrap();
    assert_eq!(racing.status, 409, "resume of a running batch must be refused: {}", racing.body);
    assert_eq!(original.status, 200, "original send completes: {}", original.body);
    let requests = onebot.finish();
    assert_eq!(send_texts(&requests), vec!["一", "二"], "no item is sent twice");
}

#[test]
fn failure_retcodes_that_may_have_delivered_are_unknown_and_clear_refusals_stay_not_committed() {
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Failed(1403), Step::HttpStatus(401), Step::Failed(200), Step::Failed(100)].into_iter().collect(),
            ..Behavior::default()
        },
    );
    let root = fixture_root("harden-retcode", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["甲", "乙", "丙", "丁"], "gapMode": "fixed", "gaps": [0, 0, 0] }));
    assert_eq!(first.status, 500, "body: {}", first.body);
    assert_eq!(first.body["notCommitted"], json!([0, 1]), "1403 and HTTP 401 are refusals before processing: {}", first.body);
    assert_eq!(first.body["unknown"], json!([2, 3]), "other failure retcodes may have delivered: {}", first.body);
    let batch_id = first.body["batchId"].as_str().unwrap().to_string();
    let reserved = read_selected_state(&root)["conversations"]["group:70001"]["sendTimes"].as_array().unwrap().len();
    assert_eq!(reserved, 2, "unknown items keep their reservation, refusals give theirs back");

    let resumed = resume(&console, &batch_id);
    assert_eq!(resumed.status, 500, "unknown items remain after resume: {}", resumed.body);
    assert_eq!(resumed.body["unknown"], json!([2, 3]));
    let requests = onebot.finish();
    assert_eq!(send_texts(&requests), vec!["甲", "乙", "丙", "丁", "甲", "乙"], "unknown items are never resent");
}

#[test]
fn identical_message_from_before_the_attempt_does_not_confirm_an_unknown_item() {
    let now_s = now_seconds();
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Drop].into_iter().collect(),
            // Same text, our own id, 10 s before the attempt: inside the old 30 s look-back.
            history: vec![history_message(77100, now_s - 10, "重复文本")],
            ..Behavior::default()
        },
    );
    let root = fixture_root("harden-verify-old", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["重复文本"] }));
    assert_eq!(first.status, 500, "an old identical message is not evidence: {}", first.body);
    assert_eq!(first.body["sent"], 0);
    assert_eq!(first.body["unknown"], json!([0]));
    assert!(!read_outbox(&root).iter().any(|r| r["k"] == "confirmed"));
    drop(onebot);
}

#[test]
fn one_history_message_confirms_only_one_of_two_identical_unknown_items_on_resume() {
    let now_s = now_seconds();
    let onebot = FakeOneBot::start_with(
        70001,
        Behavior {
            steps: [Step::Drop, Step::Drop].into_iter().collect(),
            history_hidden_calls: 2,
            history: vec![history_message(77200, now_s + 5, "同文")],
            ..Behavior::default()
        },
    );
    let root = fixture_root("harden-verify-twice", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url, "group:70001", &[70001], &[], 20, json!([]));
    let console = ConsoleProcess::start(&root);
    let first = post_send(&console, AGENT_A, json!({ "key": "group:70001", "messages": ["同文", "同文"], "gapMode": "fixed", "gaps": [0] }));
    assert_eq!(first.body["unknown"], json!([0, 1]), "setup body: {}", first.body);
    let batch_id = first.body["batchId"].as_str().unwrap().to_string();
    drop(console);
    let path = root.join("data/send-outbox.jsonl");
    let kept: Vec<String> = fs::read_to_string(&path).unwrap().lines()
        .filter(|line| !line.contains("\"k\":\"unknown\"")).map(str::to_string).collect();
    assert_eq!(kept.len(), 3, "intent + two attempts remain: {kept:?}");
    fs::write(&path, kept.join("\n") + "\n").unwrap();

    let console = ConsoleProcess::start(&root);
    let resumed = resume(&console, &batch_id);
    assert_eq!(resumed.body["sent"], 1, "one message cannot confirm two items: {}", resumed.body);
    assert_eq!(resumed.body["unknown"], json!([1]));
    assert_eq!(resumed.status, 409);
    let requests = onebot.finish();
    assert_eq!(send_paths(&requests).len(), 2, "nothing resent");
}

#[test]
fn hf124_text_face_and_face_only_reach_onebot_as_segments() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf124", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    let console = ConsoleProcess::start(&root);
    for text in ["前[表情28]后", "[表情14]"] {
        let response = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":[text]}));
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(response.body["sent"], 1);
    }
    let requests = onebot.finish();
    let sends: Vec<_> = requests.iter().filter(|r| r.path == "/send_group_msg").collect();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].body["message"], json!([
        {"type":"text","data":{"text":"前"}}, {"type":"face","data":{"id":"28"}},
        {"type":"text","data":{"text":"后"}}]));
    assert_eq!(sends[1].body["message"], json!([{"type":"face","data":{"id":"14"}}]));
}

fn hf123_config(root: &Path, update: impl FnOnce(&mut Value)) {
    let path = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["socialV2"]["send"]["firstGapEnabled"] = json!(false);
    config["socialV2"]["send"]["maxSendPerHour"] = json!(100);
    update(&mut config);
    fs::write(path, config.to_string()).unwrap();
}

#[test]
fn hf124_fix1_nested_and_malformed_notation_has_identical_wire_and_outbox_text() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root(
        "hf124-fix1-canonical",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        100,
        json!([]),
    );
    hf123_config(&root, |_| {});
    let console = ConsoleProcess::start(&root);
    let cases = [
        (
            "a[表情[表情]]b",
            "ab",
            json!([{"type":"text","data":{"text":"ab"}}]),
        ),
        (
            "前[表情0[表情]28]后",
            "前[表情28]后",
            json!([
            {"type":"text","data":{"text":"前"}}, {"type":"face","data":{"id":"28"}},
            {"type":"text","data":{"text":"后"}}]),
        ),
        (
            "a[表[表情]情]b",
            "ab",
            json!([{"type":"text","data":{"text":"ab"}}]),
        ),
        (
            "a[表情:[表情]]b",
            "a]b",
            json!([{"type":"text","data":{"text":"a]b"}}]),
        ),
        (
            "[表情包:图][表情999999][表情x][表情-1]",
            "[表情包:图][表情999999][表情x][表情-1]",
            json!([{"type":"text","data":{"text":"[表情包:图][表情999999][表情x][表情-1]"}}]),
        ),
    ];
    for (input, _, _) in &cases {
        let response = post_send(
            &console,
            AGENT_A,
            json!({"key":"group:70001","messages":[input]}),
        );
        assert_eq!(response.status, 200, "{}", response.body);
    }
    let requests = onebot.finish();
    let sends: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/send_group_msg")
        .collect();
    let intents: Vec<Value> = fs::read_to_string(root.join("data/send-outbox.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|r| r["k"] == "intent")
        .collect();
    assert_eq!(sends.len(), cases.len());
    assert_eq!(intents.len(), cases.len());
    for (idx, (input, canonical, payload)) in cases.iter().enumerate() {
        assert_eq!(
            sends[idx].body["message"], *payload,
            "wire text for {input}"
        );
        assert_eq!(
            intents[idx]["messages"],
            json!([canonical]),
            "outbox text for {input}"
        );
    }
}

#[test]
fn hf124_fix1_third_identical_nested_message_is_refused() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root(
        "hf124-fix1-repeat",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        100,
        json!([]),
    );
    hf123_config(&root, |_| {});
    let console = ConsoleProcess::start(&root);
    let statuses: Vec<_> = (0..3)
        .map(|_| {
            post_send(
                &console,
                AGENT_A,
                json!({"key":"group:70001","messages":["a[表情[表情]]b"]}),
            )
            .status
        })
        .collect();
    let requests = onebot.finish();
    assert_eq!(
        statuses,
        [200, 200, 429],
        "identical delivered canonical text must hit the repeat limit"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == "/send_group_msg")
            .count(),
        2
    );
}

#[test]
fn hf123_fix1_tool_sentinel_checks_canonical_text_before_any_submission() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root(
        "hf123-fix1-sentinel",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        100,
        json!([]),
    );
    hf123_config(&root, |cfg| {
        cfg["socialV2"]["send"]["repeatLimit"] = json!(100);
        cfg["socialV2"]["send"]["maxPerTurn"] = json!(100);
    });
    let console = ConsoleProcess::start(&root);
    let mut statuses = Vec::new();
    for text in [
        "/ro[表情]ot/fixture.txt",
        "/ro[表情:摘要]ot/fixture.txt",
        "/ro[表情[表情]]ot/fixture.txt",
    ] {
        for (path, body) in [
            (
                "/api/socialV2/send-message",
                json!({"key":"group:70001","messages":[text]}),
            ),
            (
                "/api/socialV2/send-burst",
                json!({"key":"group:70001","messages":[text]}),
            ),
            (
                "/api/send/group",
                json!({"groupId":70001,"token":AGENT_A,"message":text}),
            ),
        ] {
            let response = http_request(
                console.port,
                "POST",
                path,
                Some(CONSOLE_A),
                Some(AGENT_A),
                &body,
            )
            .unwrap();
            statuses.push((path, response.status, response.body));
        }
    }
    let requests = onebot.finish();
    assert!(
        statuses.iter().all(|(_, status, _)| *status == 403),
        "canonical paths must be refused: {statuses:?}"
    );
    assert!(
        send_paths(&requests).is_empty(),
        "no OneBot submission after a sentinel refusal: {requests:?}"
    );
    assert!(
        !root.join("data/send-outbox.jsonl").exists(),
        "refused text must not create an intent"
    );
}

#[test]
fn hf123_fix1_resumed_legacy_text_is_audited_before_submission() {
    use sha2::{Digest, Sha256};
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root(
        "hf123-fix1-resume-sentinel",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        100,
        json!([]),
    );
    hf123_config(&root, |_| {});
    let epoch: String = Sha256::digest(AGENT_A.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let outbox = root.join("data/send-outbox.jsonl");
    fs::write(
        &outbox,
        format!(
            "{}\n",
            json!({
                "k":"intent", "batch":"legacy-sensitive", "key":"group:70001", "epoch":epoch,
                "messages":["/ro[表情[表情]]ot/fixture.txt"], "delays":[], "t":1001
            })
        ),
    )
    .unwrap();
    let console = ConsoleProcess::start(&root);
    let response = post_send(
        &console,
        AGENT_A,
        json!({"key":"group:70001","resumeBatchId":"legacy-sensitive"}),
    );
    let requests = onebot.finish();
    assert_eq!(
        response.status, 403,
        "canonical legacy text must be refused: {}",
        response.body
    );
    assert!(
        send_paths(&requests).is_empty(),
        "refused legacy text must never reach OneBot: {requests:?}"
    );
    assert_eq!(
        fs::read_to_string(outbox).unwrap().lines().count(),
        1,
        "no attempt after the original legacy intent"
    );
}

#[test]
fn hf123_third_identical_send_is_refused_before_hold_and_notifies_owner_once() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf123-repeat", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    hf123_config(&root, |_| {});
    let console = ConsoleProcess::start(&root);
    for (index, expected) in [200, 200, 429, 429].into_iter().enumerate() {
        if index == 2 {
            // A wrongly admitted third batch would enter a 25-second hold (HTTP budget: 4 s).
            hf123_config(&root, |cfg| cfg["socialV2"]["reply"] = json!({"quietMedianMs":25000,"quietMinMs":25000,"quietMaxMs":25000}));
            let path = root.join("data/social-v2.json");
            let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
            state["conversations"]["group:70001"]["lastIncomingAt"] = json!(now);
            state["conversations"]["group:70001"]["lastReplyIncomingAt"] = json!(now);
            fs::write(path, state.to_string()).unwrap();
        }
        let response = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["REPEAT_SECRET_123"]}));
        assert_eq!(response.status, expected, "{}", response.body);
        if expected == 429 { assert!(!response.body.to_string().contains("REPEAT_SECRET_123")); }
    }
    // Notifications are asynchronous; wait for the one private request without shutting down its fake.
    let deadline = Instant::now() + Duration::from_secs(3);
    while onebot.request_count.load(std::sync::atomic::Ordering::Relaxed) < 3 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let requests = onebot.finish();
    assert_eq!(requests.iter().filter(|r|r.path == "/send_group_msg").count(), 2);
    assert_eq!(requests.iter().filter(|r|r.path == "/send_private_msg").count(), 1);
    let ledger = fs::read_to_string(root.join("data/send-outbox.jsonl")).unwrap();
    assert_eq!(ledger.lines().filter(|l|serde_json::from_str::<Value>(l).unwrap()["k"] == "intent").count(), 2);
    let log = fs::read_to_string(root.join("console.stdout.log")).unwrap();
    let refusals: Vec<_> = log.lines().filter(|l| l.contains("[send-guard]")).collect();
    assert_eq!(refusals.len(), 2, "{log}");
    assert!(refusals.iter().all(|l| l.contains("key=group:70001") && l.contains("turn=") && l.contains("reason=repeat") && l.contains("count=") && !l.contains("REPEAT_SECRET_123")));
    let state: Value = serde_json::from_slice(&fs::read(root.join("data/social-v2.json")).unwrap()).unwrap();
    assert!(state["conversations"]["group:70001"]["replyRevisionBudget"].is_null());
}

#[test]
fn hf124_unknown_tokens_stay_text_summaries_are_stripped_and_inbound_face_roundtrips() {
    let onebot = FakeOneBot::start_with(70001, Behavior {
        quote_message: Some(json!([{"type":"face","data":{"id":28}}])), ..Behavior::default()
    });
    let root = fixture_root("hf124-normalize", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    let console = ConsoleProcess::start(&root);
    let quoted = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["引用"],"replyToMessageId":"90042"}));
    assert_eq!(quoted.status, 200);
    assert_eq!(quoted.body["quoted"]["text"], "[表情28]");
    for text in [quoted.body["quoted"]["text"].as_str().unwrap(), "[表情999999][表情x][表情-1]", "前[表情:给糖][表情]后", "[表情028]"] {
        let response = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":[text]}));
        assert_eq!(response.status, 200, "{}", response.body);
    }
    let empty = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情:摘要][表情]"]}));
    assert_eq!(empty.status, 400, "stripped-only messages must never create empty sends: {}", empty.body);
    let requests = onebot.finish();
    let sends: Vec<_> = requests.iter().filter(|r|r.path == "/send_group_msg").collect();
    assert_eq!(sends.len(), 5);
    assert_eq!(sends[1].body["message"], json!([{"type":"face","data":{"id":"28"}}]));
    assert_eq!(sends[2].body["message"], json!([{"type":"text","data":{"text":"[表情999999][表情x][表情-1]"}}]));
    assert_eq!(sends[3].body["message"], json!([{"type":"text","data":{"text":"前后"}}]));
    assert_eq!(sends[4].body["message"], json!([{"type":"face","data":{"id":"28"}}]));
    let intents: Vec<Value> = fs::read_to_string(root.join("data/send-outbox.jsonl")).unwrap().lines()
        .map(|l|serde_json::from_str::<Value>(l).unwrap()).filter(|r|r["k"] == "intent").collect();
    assert_eq!(intents[3]["messages"], json!(["前后"]));
    assert_eq!(intents[4]["messages"], json!(["[表情28]"]));
}

#[test]
fn hf123_turn_cap_counts_bubbles_atomically_and_varied_sends_are_allowed() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf123-cap", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    // This case pins its own cap; the default is covered by the engagement-window case below.
    hf123_config(&root, |cfg| cfg["socialV2"]["send"]["maxPerTurn"] = json!(8));
    let console = ConsoleProcess::start(&root);
    for n in 0..7 {
        assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":[format!("varied-{n}")]})).status, 200);
    }
    let before = fs::read_to_string(root.join("data/send-outbox.jsonl")).unwrap();
    let refused = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["overflow-a","overflow-b"]}));
    assert_eq!(refused.status, 429, "whole batch must be refused: {}", refused.body);
    // A refusal record may be appended, but no new intent, attempt or hold is admitted.
    let intents = |s: &str|s.lines().filter(|l| serde_json::from_str::<Value>(l).unwrap()["k"] == "intent").count();
    assert_eq!(intents(&fs::read_to_string(root.join("data/send-outbox.jsonl")).unwrap()), intents(&before));
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["eighth"]})).status, 200);
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["ninth"]})).status, 429);
    // Settings are read on the next request; increasing the cap requires no restart.
    hf123_config(&root, |cfg|cfg["socialV2"]["send"]["maxPerTurn"] = json!(9));
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["new-cap"]})).status, 200);
    let requests = onebot.finish();
    assert_eq!(requests.iter().filter(|r|r.path == "/send_group_msg").count(), 9);
}

#[test]
fn engage_default_turn_send_cap_is_twenty_and_repeats_are_still_refused() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("engage-cap", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    hf123_config(&root, |cfg| cfg["socialV2"]["send"]["maxSendPerMinute"] = json!(100));
    let console = ConsoleProcess::start(&root);
    for n in 0..20 {
        let sent = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":[format!("long chat {n}")]}));
        assert_eq!(sent.status, 200, "send {n} is within the default cap of 20: {}", sent.body);
    }
    let over = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["twenty-first"]}));
    assert_eq!(over.status, 429, "the 21st bubble exceeds the default cap: {}", over.body);
    let requests = onebot.finish();
    assert_eq!(requests.iter().filter(|r|r.path == "/send_group_msg").count(), 20);

    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("engage-repeat", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    hf123_config(&root, |_| {});
    let console = ConsoleProcess::start(&root);
    for expected in [200, 200, 429] {
        let response = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["same words"]}));
        assert_eq!(response.status, expected, "the repeat guard is unchanged: {}", response.body);
    }
    assert_eq!(onebot.finish().iter().filter(|r|r.path == "/send_group_msg").count(), 2);
}

fn hf123_turn(root: &Path, id: &str, previous: Option<&str>) {
    use sha2::{Digest, Sha256};
    let epoch: String = Sha256::digest(AGENT_A.as_bytes())[..8].iter().map(|b|format!("{b:02x}")).collect();
    let mut log = fs::OpenOptions::new().create(true).append(true).open(root.join("data/turn-ledger.jsonl")).unwrap();
    if let Some(prev) = previous {
        writeln!(log,"{}",json!({"k":"ended","turn":prev,"outcome":"failed","reason":"turnTimeout","t":1000})).unwrap();
    }
    writeln!(log,"{}",json!({"k":"begun","turn":id,"key":"group:70001","backend":"dsh","epoch":epoch,"t":1001})).unwrap();
}

#[test]
fn hf136_fix3_sends_without_a_known_start_and_after_console_restart() {
    use sha2::{Digest, Sha256};
    for evidence in ["no-turn", "unknown-start", "stale-start", "expired-budget"] {
        let onebot = FakeOneBot::start(70001, false);
        let root = fixture_root("hf136-fix3-send", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
            "group:70001", &[70001], &[], 100, json!([]));
        hf123_config(&root, |_| {});
        let epoch: String = Sha256::digest(AGENT_A.as_bytes())[..8].iter().map(|b|format!("{b:02x}")).collect();
        if evidence != "no-turn" {
            let mut begun = json!({"k":"begun","turn":"restart-turn","key":"group:70001","backend":"dsh","epoch":epoch});
            if evidence != "unknown-start" { begun["t"] = json!(1001); }
            let mut ledger = fs::OpenOptions::new().create(true).append(true)
                .open(root.join("data/turn-ledger.jsonl")).unwrap();
            writeln!(ledger, "{begun}").unwrap();
            if evidence == "expired-budget" {
                writeln!(ledger, "{}", json!({"k":"budget","turn":"restart-turn","deadline":2000})).unwrap();
            }
        }
        let ledger_before = fs::read(root.join("data/turn-ledger.jsonl")).ok();
        for phase in ["before-restart", "after-restart"] {
            // Drop reaps the first console before the same durable turn/epoch is reused.
            let console = ConsoleProcess::start(&root);
            let sent = post_send(&console, AGENT_A,
                json!({"key":"group:70001","messages":[phase]}));
            assert_eq!(sent.status, 200, "{evidence}/{phase}: {}", sent.body);
            assert_eq!(sent.body["sent"], 1, "{evidence}/{phase}: {}", sent.body);
            assert_ne!(sent.body["held"], true, "{}", sent.body);
            assert_ne!(sent.body["turnBudgetExhausted"], true, "{}", sent.body);
            drop(console);
        }
        assert_eq!(fs::read(root.join("data/turn-ledger.jsonl")).ok(), ledger_before);
        assert_eq!(onebot.finish().iter().filter(|r|r.path == "/send_group_msg").count(), 2);
        let outbox = fs::read_to_string(root.join("data/send-outbox.jsonl")).unwrap();
        assert!(!outbox.lines().any(|line|serde_json::from_str::<Value>(line).unwrap()["k"] == "superseded"));
    }
}

#[test]
fn hf123_fix1_owner_notice_claim_survives_restart_within_same_ledger_turn() {
    use std::sync::atomic::Ordering;
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root(
        "hf123-fix1-notice-restart",
        CONSOLE_A,
        AGENT_A,
        ONEBOT_A,
        &onebot.url,
        "group:70001",
        &[70001],
        &[],
        100,
        json!([]),
    );
    hf123_config(&root, |_| {});
    hf123_turn(&root, "same-notice-turn", None);
    let turn_ledger = fs::read(root.join("data/turn-ledger.jsonl")).unwrap();
    let claims = || -> Vec<Value> {
        fs::read_to_string(root.join("data/send-outbox.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["k"] == "send_guard_notice")
            .collect()
    };
    let console = ConsoleProcess::start(&root);
    for expected in [200, 200, 429] {
        let response = post_send(
            &console,
            AGENT_A,
            json!({"key":"group:70001","messages":["notice-restart-fixture"]}),
        );
        assert_eq!(response.status, expected, "{}", response.body);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while onebot.private_request_count.load(Ordering::SeqCst) < 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        onebot.private_request_count.load(Ordering::SeqCst),
        1,
        "observe the owner notice before restarting"
    );
    let before_restart = claims();
    assert_eq!(before_restart.len(), 1);
    assert!(before_restart[0]["turnScope"]
        .as_str()
        .unwrap()
        .ends_with(":same-notice-turn"));
    drop(console);

    let console = ConsoleProcess::start(&root);
    let refused = post_send(
        &console,
        AGENT_A,
        json!({"key":"group:70001","messages":["notice-restart-fixture"]}),
    );
    assert_eq!(
        refused.status, 429,
        "the same ledger turn must still refuse repeats: {}",
        refused.body
    );
    assert_eq!(
        fs::read(root.join("data/turn-ledger.jsonl")).unwrap(),
        turn_ledger,
        "restart must not mint another ledger turn in this fixture"
    );
    // Give an incorrectly queued duplicate notice time to reach the fake, not just its durable claim.
    let deadline = Instant::now() + Duration::from_secs(3);
    while onebot.private_request_count.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let requests = onebot.finish();
    let notices: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/send_private_msg")
        .collect();
    let after_restart = claims();
    assert_eq!(
        (notices.len(), after_restart.len()),
        (1, 1),
        "the whole ledger turn gets exactly one owner notice and one durable claim"
    );
    assert_eq!(notices[0].body["user_id"], 9001);
    assert_eq!(
        after_restart, before_restart,
        "the original durable claim survives unchanged"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == "/send_group_msg")
            .count(),
        2
    );
}

#[test]
fn hf123_outbox_carries_repeats_across_timeout_and_console_restart() {
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf123-cross", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    hf123_config(&root, |_| {});
    hf123_turn(&root, "old-turn", None);
    let console = ConsoleProcess::start(&root);
    for _ in 0..2 {
        assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情28]"]})).status, 200);
    }
    hf123_turn(&root, "new-turn", Some("old-turn"));
    drop(console);
    let console = ConsoleProcess::start(&root);
    let refused = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情028][表情:忽略]"]}));
    assert_eq!(refused.status, 429, "a new turn cannot restart the same normalized loop: {}", refused.body);
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["a fresh reply"]})).status, 200);
    hf123_config(&root, |cfg| cfg["socialV2"]["send"]["repeatWindowSends"] = json!(1));
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情28]"]})).status, 200, "last-N window is hot-read");
    let requests = onebot.finish();
    assert_eq!(requests.iter().filter(|r|r.path == "/send_group_msg").count(), 4);
}

#[test]
fn hf123_resumed_old_batch_spends_the_current_turn_cap() {
    use sha2::{Digest, Sha256};
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf123-resume-scope", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    // Pins the cap the eight resumed bubbles exhaust; the default is covered by the engagement case.
    hf123_config(&root, |cfg| cfg["socialV2"]["send"]["maxPerTurn"] = json!(8));
    hf123_turn(&root, "old-turn", None);
    let epoch: String = Sha256::digest(AGENT_A.as_bytes())[..8].iter().map(|b|format!("{b:02x}")).collect();
    let messages: Vec<_> = (0..8).map(|i|format!("resumed-{i}")).collect();
    fs::write(root.join("data/send-outbox.jsonl"), format!("{}\n", json!({
        "k":"intent","batch":"old-pending","key":"group:70001","epoch":epoch,
        "turnScope":format!("{epoch}:old-turn"),"messages":messages,"delays":[0,0,0,0,0,0,0],"t":1001
    }))).unwrap();
    hf123_turn(&root, "new-turn", Some("old-turn"));
    let console = ConsoleProcess::start(&root);
    let resumed = post_send(&console, AGENT_A, json!({"key":"group:70001","resumeBatchId":"old-pending"}));
    assert_eq!(resumed.status, 200, "{}", resumed.body);
    assert_eq!(resumed.body["sent"], 8);
    let capped = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["ninth-current-turn"]}));
    assert_eq!(capped.status, 429, "resumed sends belong to the submitting turn: {}", capped.body);
    assert_eq!(onebot.finish().iter().filter(|r|r.path == "/send_group_msg").count(), 8);
}

#[test]
fn hf123_last_n_window_uses_submission_order_after_an_old_batch_resume() {
    use sha2::{Digest, Sha256};
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf123-window-order", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    hf123_config(&root, |_| {});
    hf123_turn(&root, "old-turn", None);
    let epoch: String = Sha256::digest(AGENT_A.as_bytes())[..8].iter().map(|b|format!("{b:02x}")).collect();
    fs::write(root.join("data/send-outbox.jsonl"), format!("{}\n", json!({
        "k":"intent","batch":"old-face","key":"group:70001","epoch":epoch,
        "turnScope":format!("{epoch}:old-turn"),"messages":["[表情28]"],"delays":[],"t":1001
    }))).unwrap();
    hf123_turn(&root, "second-turn", Some("old-turn"));
    let console = ConsoleProcess::start(&root);
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情14]"]})).status, 200);
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","resumeBatchId":"old-face"})).status, 200);
    hf123_turn(&root, "third-turn", Some("second-turn"));
    hf123_config(&root, |cfg| {
        cfg["socialV2"]["send"]["repeatWindowSends"] = json!(1);
        cfg["socialV2"]["send"]["repeatLimit"] = json!(1);
    });
    let refused = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情28]"]}));
    assert_eq!(refused.status, 429, "the last actual send was the resumed face, not the newer intent: {}", refused.body);
    assert_eq!(onebot.finish().iter().filter(|r|r.path == "/send_group_msg").count(), 2);
}

#[test]
fn hf123_last_n_window_includes_typed_bot_sends() {
    use sha2::{Digest, Sha256};
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf123-window-typed", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    hf123_config(&root, |_| {});
    hf123_turn(&root, "old-turn", None);
    let console = ConsoleProcess::start(&root);
    assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情28]"]})).status, 200);
    // A synthetic confirmed sticker is the last bot send; its typed liability is kept intact.
    let epoch: String = Sha256::digest(AGENT_A.as_bytes())[..8].iter().map(|b|format!("{b:02x}")).collect();
    let mut outbox = fs::OpenOptions::new().append(true).open(root.join("data/send-outbox.jsonl")).unwrap();
    for row in [
        json!({"k":"intent","batch":"typed-last","kind":"sticker","key":"group:70001","epoch":epoch,"turnScope":format!("{epoch}:old-turn"),"messages":["[表情包:fixture]"],"t":1002}),
        json!({"k":"attempt","batch":"typed-last","idx":0,"t":1003}),
        json!({"k":"confirmed","batch":"typed-last","idx":0,"messageId":99123,"t":1004}),
    ] { writeln!(outbox,"{row}").unwrap(); }
    drop(outbox);
    hf123_turn(&root, "new-turn", Some("old-turn"));
    hf123_config(&root, |cfg| {
        cfg["socialV2"]["send"]["repeatWindowSends"] = json!(1);
        cfg["socialV2"]["send"]["repeatLimit"] = json!(1);
    });
    let sent = post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["[表情28]"]}));
    assert_eq!(sent.status, 200, "last-N counts all bot sends, including typed ones: {}", sent.body);
    assert_eq!(onebot.finish().iter().filter(|r|r.path == "/send_group_msg").count(), 2);
}

#[test]
fn hf123_turn_epoch_does_not_reset_limits_or_owner_notice() {
    use sha2::{Digest, Sha256};
    let onebot = FakeOneBot::start(70001, false);
    let root = fixture_root("hf123-turn-epoch", CONSOLE_A, AGENT_A, ONEBOT_A, &onebot.url,
        "group:70001", &[70001], &[], 100, json!([]));
    hf123_config(&root, |_| {});
    hf123_turn(&root, "one-turn", None);
    let console = ConsoleProcess::start(&root);
    for status in [200,200,429] {
        assert_eq!(post_send(&console, AGENT_A, json!({"key":"group:70001","messages":["same-turn"]})).status, status);
    }
    let path = root.join("data/social-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["conversations"]["group:70001"]["agentToken"] = json!(AGENT_B);
    fs::write(path, state.to_string()).unwrap();
    let epoch: String = Sha256::digest(AGENT_B.as_bytes())[..8].iter().map(|b|format!("{b:02x}")).collect();
    let mut ledger = fs::OpenOptions::new().append(true).open(root.join("data/turn-ledger.jsonl")).unwrap();
    writeln!(ledger,"{}",json!({"k":"epoch","turn":"one-turn","epoch":epoch,"t":1002})).unwrap();
    drop(ledger);
    let refused = post_send(&console, AGENT_B, json!({"key":"group:70001","messages":["same-turn"]}));
    assert_eq!(refused.status, 429, "a backend epoch change is not a fresh turn: {}", refused.body);
    let deadline = Instant::now()+Duration::from_secs(3);
    while onebot.request_count.load(std::sync::atomic::Ordering::Relaxed) < 3 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let requests = onebot.finish();
    assert_eq!(requests.iter().filter(|r|r.path == "/send_group_msg").count(), 2);
    assert_eq!(requests.iter().filter(|r|r.path == "/send_private_msg").count(), 1);
}
