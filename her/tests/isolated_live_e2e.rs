#![cfg_attr(not(windows), allow(dead_code))] // helpers of the Windows-only CLI test
#[path = "../src/install_root.rs"]
mod install_root;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        Message,
    },
};

const GROUP_ID: i64 = 420042;
const SELF_ID: i64 = 9001;
const SELECTED_TOKEN: &str = "ticket04-selected-onebot-token";
const SESSION_TOKEN: &str = "ticket04-selected-agent-token";
const MODEL_KEY: &str = "ticket04-selected-model-key";
const MODEL_REPLY: &str = "ticket04 synthetic model reply";
const INBOUND_TEXT: &str = "选定根里的群消息";
// Keep fakes alive across the complete 30 s CLI budget (including a cold
// PowerShell start). Closing them at 9 s manufactures a dependency outage.
const FAKE_TURN_TIMEOUT: Duration = Duration::from_secs(45);
const CONSOLE_START_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her ticket04 live e2e 中文 root {} {stamp}",
            std::process::id()
        )));
        fs::create_dir_all(root.join("config")).expect("create selected config");
        fs::create_dir_all(root.join("data")).expect("create selected data");
        fs::create_dir_all(root.join("resources")).expect("create selected resources");
        Self(root)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct HttpObservation {
    request_line: String,
    headers: String,
    body: Value,
}

fn bind_http() -> (TcpListener, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fake HTTP");
    let address = listener.local_addr().expect("read fake HTTP address");
    (listener, format!("http://127.0.0.1:{}", address.port()))
}

fn bind_ws() -> (TcpListener, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fake WS");
    let address = listener.local_addr().expect("read fake WS address");
    (listener, format!("ws://127.0.0.1:{}", address.port()))
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve explicit console port");
    listener
        .local_addr()
        .expect("read explicit console port")
        .port()
}

fn read_http_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .map_err(|error| format!("set fake HTTP read timeout: {error}"))?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end;
    let content_length;
    loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("read fake HTTP request: {error}"))?;
        if count == 0 {
            return Err("fake HTTP client closed before headers".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        let split = bytes.windows(4).position(|window| window == b"\r\n\r\n");
        if let Some(split) = split {
            let headers = String::from_utf8_lossy(&bytes[..split]).to_string();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= split + 4 + length {
                header_end = split;
                content_length = length;
                break;
            }
        }
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let request_line = headers.lines().next().unwrap_or("").to_string();
    let body_start = header_end + 4;
    Ok((
        request_line,
        headers,
        bytes[body_start..body_start + content_length].to_vec(),
    ))
}

fn send_http_json(stream: &mut TcpStream, status: &str, value: &Value) -> Result<(), String> {
    let body = value.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| format!("write fake HTTP response: {error}"))
}

fn response_text(value: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(value)
        .map_err(|error| format!("fake HTTP body is invalid JSON: {error}"))
}

fn accept_with_deadline(listener: &TcpListener, deadline: Instant) -> Result<TcpStream, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set fake listener nonblocking: {error}"))?;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|error| format!("set fake HTTP client blocking: {error}"))?;
                return Ok(stream);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(10035) =>
            {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for fake HTTP request".into());
                }
                thread::sleep(Duration::from_millis(15));
            }
            Err(error) => return Err(format!("accept fake HTTP request: {error}")),
        }
    }
}

fn serve_model(listener: TcpListener, stop: Arc<AtomicBool>) -> Result<HttpObservation, String> {
    let deadline = Instant::now() + FAKE_TURN_TIMEOUT;
    let mut first = None;
    // Keep serving until candidate shutdown, including an already queued wake reminder.
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        let mut stream = match accept_with_deadline(&listener, Instant::now() + Duration::from_millis(100)) {
            Ok(stream) => stream,
            Err(error) if error.contains("timed out") => continue,
            Err(error) => return Err(error),
        };
        let (request_line, headers, raw_body) = read_http_request(&mut stream)?;
        let body = response_text(&raw_body)?;
        send_http_json(&mut stream, "200 OK", &json!({"text": MODEL_REPLY}))?;
        if first.is_none() { first = Some(HttpObservation { request_line, headers, body }); }
    }
    first.ok_or_else(|| "timed out waiting for fake model request".into())
}

fn serve_onebot(
    listener: TcpListener,
    sent: Arc<AtomicBool>,
    observations: Arc<Mutex<Vec<HttpObservation>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    // The console refreshes its catalog during startup; serve it before the live/model fakes
    // begin their own deadlines, then remain available through the complete CLI turn.
    let deadline = Instant::now() + CONSOLE_START_TIMEOUT + FAKE_TURN_TIMEOUT;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline
    {
        let mut stream =
            match accept_with_deadline(&listener, Instant::now() + Duration::from_millis(250)) {
                Ok(stream) => stream,
                Err(error) if error.contains("timed out") => continue,
                Err(error) => return Err(error),
            };
        let (request_line, headers, raw_body) = read_http_request(&mut stream)?;
        let body = response_text(&raw_body)?;
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let observation = HttpObservation {
            request_line,
            headers,
            body,
        };
        observations.lock().unwrap().push(observation);
        match path.as_str() {
            "/get_group_member_info" => {
                send_http_json(
                    &mut stream,
                    "200 OK",
                    &json!({"status":"ok","retcode":0,"data":{"card":"selected-fixture-card"}}),
                )?;
            }
            "/send_group_msg" => {
                send_http_json(
                    &mut stream,
                    "200 OK",
                    &json!({"status":"ok","retcode":0,"data":{"message_id":770042}}),
                )?;
                sent.store(true, Ordering::SeqCst);
            }
            _ => send_http_json(
                &mut stream,
                "404 Not Found",
                &json!({"status":"failed","retcode":404,"message":"unexpected fixture endpoint"}),
            )?,
        }
    }
    if sent.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err("timed out waiting for OneBot outbound message".into())
    }
}

fn serve_dsh_connection(
    mut stream: TcpStream,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    deadline: Instant,
) -> Result<(), String> {
    // Independent sockets: an idle connection or a monitor waiting to send its open
    // frame must not block HTTP readiness requests on another connection.
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
    let mut first = [0u8; 1];
    loop {
        if stop.load(Ordering::SeqCst) { return Ok(()); }
        if Instant::now() >= deadline { return Err("fake DSH initial byte deadline".into()); }
        match stream.peek(&mut first) {
            Ok(0) => return Ok(()),
            Ok(_) => break,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {},
            Err(e) => return Err(format!("fake DSH initial peek: {e}")),
        }
    }
    stream.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    if first[0] == b'G' {
        let mut path = String::new();
        let mut socket = tokio_tungstenite::tungstenite::accept_hdr(
            stream,
            |request: &Request, response: Response| {
                path = request.uri().path().to_string();
                Ok(response)
            },
        ).map_err(|e| format!("fake DSH websocket handshake: {e}"))?;
        socket.get_mut().set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
        let frame = loop {
            if stop.load(Ordering::SeqCst) { return Ok(()); }
            if Instant::now() >= deadline { return Err("fake DSH open frame deadline".into()); }
            match socket.read() {
                Ok(frame) => break frame,
                Err(tokio_tungstenite::tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {},
                Err(e) => return Err(format!("fake DSH websocket open frame: {e}")),
            }
        };
        let open: Value = serde_json::from_str(frame.to_text().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        requests.lock().unwrap().push(format!("GET {path} stream:{}", open["endpoint"].as_str().unwrap_or("<missing>")));
        let _ = socket.close(None);
    } else {
        let (line, _, _) = read_http_request(&mut stream)?;
        requests.lock().unwrap().push(line);
        let _ = send_http_json(&mut stream, "503 Service Unavailable", &json!({}));
    }
    Ok(())
}

fn serve_dsh_probe(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set fake DSH listener nonblocking: {error}"))?;
    let deadline = Instant::now() + FAKE_TURN_TIMEOUT;
    let mut workers = Vec::new();
    let mut result = Ok(());
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                let (stop, requests) = (stop.clone(), requests.clone());
                workers.push(thread::spawn(move || serve_dsh_connection(stream, stop, requests, deadline)));
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(10035) =>
            {
                thread::sleep(Duration::from_millis(15));
            }
            Err(error) => {
                result = Err(format!("accept fake DSH request: {error}"));
                break;
            }
        }
    }
    for worker in workers {
        let finished = worker.join().map_err(|_| "fake DSH connection worker panicked".to_string()).and_then(|r| r);
        if result.is_ok() { result = finished; }
    }
    result
}

fn is_background_dsh_request(request: &str) -> bool {
    matches!(
        request,
        "POST /api/settings/describe HTTP/1.1" | "GET /api/remote.mux stream:$events"
    )
}

#[test]
fn dsh_fixture_distinguishes_monitor_traffic_from_conversation_work() {
    let (listener, url) = bind_http();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (stopped, observed) = (stop.clone(), requests.clone());
    let worker = thread::spawn(move || serve_dsh_probe(listener, stopped, observed));
    // An established background connection may not have sent its first byte yet.
    // It must not prevent the sentinel from observing work on another connection.
    let _idle = TcpStream::connect(address).unwrap();
    let clients = std::panic::catch_unwind(|| {
        for path in ["settings/describe", "session/prompt"] {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            write!(stream, "POST /api/{path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            assert!(response.starts_with("HTTP/1.1 503"));
        }
        for endpoint in ["$events", "session/follow"] {
            let stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (mut socket, _) = tokio_tungstenite::tungstenite::client(
                format!("{}/api/remote.mux", url.replacen("http://", "ws://", 1)),
                stream,
            )
            .unwrap();
            socket.send(Message::Text(json!({"type":"open","streamId":"fixture","endpoint":endpoint,"payload":{"args":{}}}).to_string())).unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        }
    });
    stop.store(true, Ordering::SeqCst);
    let result = worker.join().expect("join DSH fixture");
    if let Err(panic) = clients {
        std::panic::resume_unwind(panic);
    }
    result.expect("serve DSH fixture requests");
    let observed = requests.lock().unwrap();
    assert_eq!(
        *observed,
        [
            "POST /api/settings/describe HTTP/1.1",
            "POST /api/session/prompt HTTP/1.1",
            "GET /api/remote.mux stream:$events",
            "GET /api/remote.mux stream:session/follow",
        ]
    );
    assert_eq!(
        observed
            .iter()
            .filter(|request| is_background_dsh_request(request))
            .count(),
        2
    );
    assert!(
        !is_background_dsh_request(&observed[1]),
        "a prompt is conversation work"
    );
    assert!(
        !is_background_dsh_request(&observed[3]),
        "a follow stream is conversation work"
    );
}

#[derive(Debug)]
struct WsObservation {
    authorization: Option<String>,
    login_action: Option<String>,
}

fn serve_ws(
    listener: TcpListener,
    sent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<WsObservation, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set fake WS listener nonblocking: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create fake WS runtime: {error}"))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|error| format!("adopt fake WS listener: {error}"))?;
        let (socket, _) = tokio::time::timeout(Duration::from_secs(8), listener.accept())
            .await
            .map_err(|_| "timed out waiting for live candidate WS connection".to_string())?
            .map_err(|error| format!("accept live candidate WS connection: {error}"))?;
        let auth_slot = Arc::new(Mutex::new(None::<String>));
        let callback_auth = auth_slot.clone();
        let mut socket = tokio::time::timeout(
            Duration::from_secs(5),
            accept_hdr_async(socket, move |request: &Request, response: Response| {
                *callback_auth.lock().unwrap() = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                Ok(response)
            }),
        )
        .await
        .map_err(|_| "timed out completing fake WS handshake".to_string())?
        .map_err(|error| format!("complete fake WS handshake: {error}"))?;

        let login_message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .map_err(|_| "timed out waiting for get_login_info".to_string())?
            .ok_or_else(|| "candidate closed before get_login_info".to_string())?
            .map_err(|error| format!("read get_login_info: {error}"))?;
        let login_text = match login_message {
            Message::Text(text) => text.to_string(),
            other => return Err(format!("expected login text message, received {other:?}")),
        };
        let login: Value = serde_json::from_str(&login_text)
            .map_err(|error| format!("get_login_info was not JSON: {error}"))?;
        let login_action = login
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_string);
        if login_action.as_deref() != Some("get_login_info") {
            return Err(format!(
                "unexpected initial OneBot WS action: {login_action:?}"
            ));
        }
        socket
            .send(Message::Text(
                json!({
                    "echo":"login",
                    "data":{"user_id":SELF_ID,"nickname":"ticket04-fixture-bot"}
                })
                .to_string(),
            ))
            .await
            .map_err(|error| format!("send fake login response: {error}"))?;
        socket
            .send(Message::Text(
                json!({
                    "post_type":"message",
                    "message_type":"group",
                    "group_id":GROUP_ID,
                    "user_id":8808,
                    "self_id":SELF_ID,
                    "raw_message":INBOUND_TEXT,
                    "message":[
                        {"type":"at","data":{"qq":SELF_ID.to_string()}},
                        {"type":"text","data":{"text":format!(" {INBOUND_TEXT}")}}
                    ],
                    "sender":{"nickname":"ticket04-fixture-user"}
                })
                .to_string(),
            ))
            .await
            .map_err(|error| format!("send fake inbound group message: {error}"))?;

        let deadline = Instant::now() + FAKE_TURN_TIMEOUT;
        while !sent.load(Ordering::SeqCst)
            && !stop.load(Ordering::SeqCst)
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !sent.load(Ordering::SeqCst) {
            return Err("OneBot did not observe the selected live reply before deadline".into());
        }
        let _ = socket.send(Message::Close(None)).await;
        let authorization = auth_slot.lock().unwrap().clone();
        Ok(WsObservation {
            authorization,
            login_action,
        })
    })
}

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

fn selected_runtime_fixture(root: &Path, dependencies: &Value) {
    fs::create_dir_all(root.join("data/logs")).expect("create selected logs");
    fs::create_dir_all(root.join("resources")).expect("create selected resources");
    fs::create_dir_all(root.join("user/dsh")).expect("create selected DSH home");
    fs::write(
        root.join("config/config.json"),
        json!({
            "consoleToken":"ticket04-console-token",
            "ownerQQ":8808,
            "allow":{"groups":[GROUP_ID],"private":[]},
            "snowluma":{
                "accessToken":SELECTED_TOKEN,
                "httpUrl":"http://127.0.0.1:9",
                "wsUrl":"ws://127.0.0.1:9"
            },
            "socialV2":{
                "wake":{"batchWindowMs":1000},
                "send":{"maxMessageChars":800,"gapBaseMs":0,"gapPerCharMs":0,"maxGapMs":0}
            },
            "slang":{"enabled":true,"extractMinMessages":1000},
            "backends":{
                "sessions":{"group:420042":"ticket04-live-cli"},
                "list":[{
                    "id":"ticket04-live-cli",
                    "kind":"cli",
                    "cmd":"powershell.exe",
                    "args":["-NoLogo","-NoProfile","-NonInteractive","-ExecutionPolicy","Bypass","-File","model-fixture.ps1"]
                }]
            }
        })
        .to_string(),
    )
    .expect("write selected config");
    fs::write(
        root.join("data/social-v2.json"),
        json!({
            "conversations":{
                "group:420042":{
                    "agentToken":SESSION_TOKEN,
                    "unread":[],
                    "recentMessages":[],
                    "wakeConfig":{"triggers":{"atMention":true}}
                }
            }
        })
        .to_string(),
    )
    .expect("write selected social state");
    fs::write(
        root.join("data/personas.json"),
        r#"{"format":1,"nextId":2,"default":"p1","overrides":{},"importedRolesAt":null,"personas":{"p1":{"id":"p1","activeVersion":1,"versions":[{"version":1,"approved":true,"name":"fixture-persona-from-selected-root","core":"selected-root persona core"}]}}}"#,
    )
    .expect("write selected persona library");
    fs::write(
        root.join("user/dsh/.credentials.yaml"),
        format!("secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\nOPENCODE_GO_API_KEY: \"{MODEL_KEY}\"\n"),
    )
    .expect("write synthetic selected DSH credential fixture");
    fs::write(root.join("dependencies.json"), dependencies.to_string())
        .expect("write selected loopback dependencies");

    let model_script = format!(
        r#"
$ErrorActionPreference = 'Stop'
# .NET Framework file APIs need the ordinary form of the canonical Rust path.
$fixtureRoot = $env:HER_RUNTIME_ROOT
if ($fixtureRoot.StartsWith('\\?\UNC\')) {{ $fixtureRoot = '\\' + $fixtureRoot.Substring(8) }}
elseif ($fixtureRoot.StartsWith('\\?\')) {{ $fixtureRoot = $fixtureRoot.Substring(4) }}
$fixtureLog = [IO.Path]::Combine($fixtureRoot, 'data', 'logs', 'model-fixture.log')
[IO.File]::WriteAllText($fixtureLog, "started`n")
$utf8 = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8
$stdin = [IO.StreamReader]::new([Console]::OpenStandardInput(), $utf8)
$prompt = $stdin.ReadToEnd()
$stdin.Dispose()
[IO.File]::AppendAllText($fixtureLog, "stdin-read`n")
# A fresh selected home has no module-analysis cache. Do not discover the CI
# host's installed modules just to find the two JSON cmdlets this fake needs.
$PSModuleAutoLoadingPreference = 'None'
Import-Module ([IO.Path]::Combine($PSHOME, 'Modules', 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Utility.psd1'))
[IO.File]::AppendAllText($fixtureLog, "utility-imported`n")
$probe = @{{
  prompt = $prompt
  modelBase = $env:ANTHROPIC_BASE_URL
  apiKey = $env:OPENCODE_GO_API_KEY
  sessionKey = $env:QQ_SESSION_KEY
  sessionToken = $env:QQ_SESSION_TOKEN
  home = $env:HOME
  userProfile = $env:USERPROFILE
  dshHome = $env:DSH_HOME
  dshCredentials = $env:DSH_CREDENTIALS
  appData = $env:APPDATA
  localAppData = $env:LOCALAPPDATA
  xdgConfig = $env:XDG_CONFIG_HOME
  xdgCache = $env:XDG_CACHE_HOME
  xdgData = $env:XDG_DATA_HOME
  temp = $env:TEMP
  tmp = $env:TMP
  runtimeRoot = $env:HER_RUNTIME_ROOT
  cwd = $ExecutionContext.SessionState.Path.CurrentFileSystemLocation.Path
  ambientSentinel = [Environment]::GetEnvironmentVariable('TICKET04_AMBIENT_SENTINEL')
}}
$endpoint = ($env:ANTHROPIC_BASE_URL).TrimEnd('/') + '/ticket04/generate'
$request = [Net.HttpWebRequest]::Create($endpoint)
$request.Proxy = $null
$request.KeepAlive = $false
$request.Timeout = 5000
$request.ReadWriteTimeout = 5000
$request.Method = 'POST'
$request.ContentType = 'application/json'
$request.Headers.Add('X-API-Key', $env:OPENCODE_GO_API_KEY)
$body = [Text.Encoding]::UTF8.GetBytes(($probe | ConvertTo-Json -Compress -Depth 5))
[IO.File]::AppendAllText($fixtureLog, "request-ready`n")
$request.ContentLength = $body.Length
$stream = $request.GetRequestStream()
$stream.Write($body, 0, $body.Length)
$stream.Close()
$response = $request.GetResponse()
$reader = [IO.StreamReader]::new($response.GetResponseStream())
$result = $reader.ReadToEnd() | ConvertFrom-Json
$reader.Close()
$response.Close()
[Console]::Out.Write([string]$result.text)
[IO.File]::AppendAllText($fixtureLog, "completed`n")
"#
    );
    fs::write(root.join("resources/model-fixture.ps1"), model_script)
        .expect("write selected model CLI fixture");
}

fn spawn_candidate(root: &Path, console_port: u16) -> Child {
    let ambient = crate::install_root::development_root(std::env::temp_dir().join(format!(
        "ticket04 ambient profile sentinel {}",
        std::process::id()
    )));
    spawn_candidate_with_ambient(root, console_port, &ambient)
}

fn spawn_candidate_with_ambient(root: &Path, console_port: u16, ambient: &Path) -> Child {
    candidate_command(root, console_port, ambient)
        .spawn().expect("spawn selected-root live candidate")
}

fn candidate_command(root: &Path, console_port: u16, ambient: &Path) -> Command {
    let mut command = runtime_command(root, ambient);
    command
        .arg("--live")
        .arg("--live-groups")
        .arg("group:420042")
        .arg("--no-reconnect")
        .arg("--console-port")
        .arg(console_port.to_string());
    command
}

fn runtime_command(root: &Path, ambient: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--runtime-root")
        .arg(root)
        .env_remove("HER_RUNTIME_ROOT")
        .env(
            "TICKET04_AMBIENT_SENTINEL",
            "ambient-credential-profile-must-not-survive",
        )
        .env("HOME", ambient)
        .env("USERPROFILE", ambient)
        .env("DSH_HOME", ambient)
        .env("APPDATA", ambient.join("Roaming"))
        .env("LOCALAPPDATA", ambient.join("Local"))
        .env("XDG_CONFIG_HOME", ambient.join(".config"))
        .env("XDG_CACHE_HOME", ambient.join(".cache"))
        .env("XDG_DATA_HOME", ambient.join(".local/share"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_process_window(&mut command);
    command
}

// Keep the console alive until the complete fake-service scenario ends, including assertions.
struct CompanionConsole(Child);

impl CompanionConsole {
    fn start(root: &Path, console_port: u16, ambient: &Path, epoch: i64) -> Self {
        let mut command = runtime_command(root, ambient);
        command
            .args([
                "--console-serve",
                "--console-port",
                &console_port.to_string(),
            ])
            .env("HER_PARITY_EPOCH_MS", epoch.to_string())
            .stdout(fs::File::create(root.join("data/logs/console-out.log")).unwrap())
            .stderr(fs::File::create(root.join("data/logs/console-err.log")).unwrap());
        let console = Self(
            command
                .spawn()
                .expect("spawn selected-root companion console"),
        );
        let deadline = Instant::now() + CONSOLE_START_TIMEOUT;
        loop {
            if let Ok(mut stream) = TcpStream::connect_timeout(
                &([127, 0, 0, 1], console_port).into(),
                Duration::from_millis(200),
            ) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let request = format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{console_port}\r\nConnection: close\r\n\r\n");
                let mut response = String::new();
                if stream.write_all(request.as_bytes()).is_ok()
                    && stream.read_to_string(&mut response).is_ok()
                    && response.starts_with("HTTP/1.1 200 ")
                {
                    let health: Value =
                        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
                    assert_eq!(health["runtime"]["mode"], "isolated");
                    assert_eq!(health["port"], console_port);
                    return console;
                }
            }
            assert!(
                Instant::now() < deadline,
                "selected-root companion console did not become ready"
            );
            thread::sleep(Duration::from_millis(40));
        }
    }
}

impl Drop for CompanionConsole {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// Exercise companion-console setup on every platform; the complete CLI child/environment case
// below still requires Windows process-tree containment and keeps all its original assertions.
#[test]
fn isolated_live_companion_console_routes_selected_reply() {
    let root = TempRoot::new();
    let ambient = TempRoot::new();
    let (listener, onebot_url) = bind_http();
    let dependencies = json!({"dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:2",
        "onebot":onebot_url, "onebot_ws":"ws://127.0.0.1:3"});
    selected_runtime_fixture(root.path(), &dependencies);
    let port = free_port();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let sent = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (sending, stopping, records) = (sent.clone(), stop.clone(), seen.clone());
    let gateway = thread::spawn(move || serve_onebot(listener, sending, records, stopping));
    let _console = CompanionConsole::start(root.path(), port, ambient.path(), epoch);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body = json!({"key":"group:420042", "messages":[MODEL_REPLY]}).to_string();
    write!(stream, "POST /api/socialV2/send-message HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nx-console-token: ticket04-console-token\r\nx-agent-token: {SESSION_TOKEN}\r\nx-agent-call: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    let received = stream.read_to_string(&mut response);
    stop.store(true, Ordering::SeqCst);
    let result = gateway.join().unwrap();
    assert!(received.is_ok(), "{received:?}");
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    let reply: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(reply["sent"], 1, "{reply}");
    assert!(result.is_ok(), "{result:?}");
    let requests = seen.lock().unwrap();
    // The console also refreshes its sticker catalog at startup. Count QQ sends separately
    // while still refusing any unrelated gateway action in this synthetic scenario.
    assert!(
        requests.iter().all(|request| matches!(
            request.request_line.as_str(),
            "POST /fetch_custom_face_detail HTTP/1.1" | "POST /send_group_msg HTTP/1.1"
        )),
        "{requests:?}"
    );
    let sends: Vec<_> = requests
        .iter()
        .filter(|request| request.request_line == "POST /send_group_msg HTTP/1.1")
        .collect();
    assert_eq!(sends.len(), 1, "{requests:?}");
    assert!(sends[0]
        .headers
        .to_ascii_lowercase()
        .contains(&format!("authorization: bearer {SELECTED_TOKEN}")));
    assert_eq!(sends[0].body["group_id"], GROUP_ID);
    assert_eq!(sends[0].body["message"][0]["data"]["text"], MODEL_REPLY);
}

fn wait_candidate(mut child: Child, timeout: Duration) -> (std::process::Output, Option<String>) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return (
                    child
                        .wait_with_output()
                        .expect("collect live candidate output"),
                    None,
                );
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .expect("collect failed candidate output");
                return (output, Some(format!("poll live candidate failed: {error}")));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed-out candidate output");
            return (output, Some(format!("live candidate exceeded {timeout:?}")));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_candidate_with_ws_probe(
    mut child: Child,
    listener: &TcpListener,
    timeout: Duration,
) -> (std::process::Output, Option<String>, bool) {
    listener
        .set_nonblocking(true)
        .expect("set fake WS probe nonblocking");
    let stdout = child.stdout.take().expect("capture candidate stdout");
    let stderr = child.stderr.take().expect("capture candidate stderr");
    let stdout_reader = thread::spawn(move || {
        let mut stdout = stdout;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let deadline = Instant::now() + timeout;
    let mut ws_contacted = false;
    let mut candidate_error = None;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                ws_contacted = true;
                drop(stream);
                let _ = child.kill();
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(10035) => {}
            Err(error) => {
                candidate_error = Some(format!("accept fake WS probe: {error}"));
                let _ = child.kill();
            }
        }

        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                candidate_error = Some(format!("poll live candidate failed: {error}"));
                let _ = child.kill();
            }
        }
        if Instant::now() >= deadline {
            candidate_error = Some(format!("live candidate exceeded {timeout:?}"));
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let status = child.wait().expect("wait for live candidate exit");
    let stdout = stdout_reader
        .join()
        .expect("join candidate stdout reader")
        .expect("read candidate stdout");
    let stderr = stderr_reader
        .join()
        .expect("join candidate stderr reader")
        .expect("read candidate stderr");
    (
        std::process::Output {
            status,
            stdout,
            stderr,
        },
        candidate_error,
        ws_contacted,
    )
}

fn assert_selected_gate_released(root: &Path) {
    let gate_path = root.join("data/logs/rbridge-live.lock");
    assert!(gate_path.is_file(), "selected-root gate file should persist");
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate_path)
        .expect("reopen persistent selected-root gate");
    gate.try_lock()
        .expect("candidate exit must release the selected-root gate");
}

fn run_isolated_preflight_failure(
    expected_diagnostics: &[&str],
    expected_exit_code: Option<i32>,
    sessions: &[u8],
    rollout_groups: Option<&[&str]>,
    selected_credentials: Option<&str>,
) {
    let root = TempRoot::new();
    let ambient = TempRoot::new();
    fs::write(
        ambient.path().join(".credentials.yaml"),
        "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n",
    )
    .expect("write synthetic ambient credential sentinel");
    let (ws_listener, ws_url) = bind_ws();
    let console_port = free_port();
    let dependencies = json!({
        "dsh":"http://127.0.0.1:9",
        "model":"http://127.0.0.1:9",
        "onebot":"http://127.0.0.1:9",
        "onebot_ws":ws_url
    });
    selected_runtime_fixture(root.path(), &dependencies);
    if let Some(credentials) = selected_credentials {
        fs::write(root.path().join("user/dsh/.credentials.yaml"), credentials)
            .expect("write selected credential failure fixture");
    }
    let session_path = root.path().join("data/sessions.json");
    fs::write(&session_path, sessions).expect("write selected session sentinel");
    if let Some(groups) = rollout_groups {
        let config_path = root.path().join("config/config.json");
        let mut config: Value = serde_json::from_slice(
            &fs::read(&config_path).expect("read selected synthetic config"),
        )
        .expect("parse selected synthetic config");
        config["rollout"] = json!({"enabled":true,"rustGroups":groups});
        fs::write(&config_path, config.to_string()).expect("write rollout mismatch fixture");
    }
    let sessions_before = fs::read(&session_path).expect("read selected session sentinel");

    let child = spawn_candidate_with_ambient(root.path(), console_port, ambient.path());
    let (output, candidate_error, ws_contacted) = wait_candidate_with_ws_probe(
        child,
        &ws_listener,
        Duration::from_secs(10),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(candidate_error.is_none(), "{candidate_error:?}; stdout={stdout}; stderr={stderr}");
    assert!(!ws_contacted, "preflight failure contacted selected OneBot WS; stdout={stdout}; stderr={stderr}");
    assert!(!output.status.success(), "invalid selected-root preflight unexpectedly succeeded");
    assert!(
        output.status.code().is_some_and(|code| code != 0),
        "failed candidate must have a non-zero process exit"
    );
    if let Some(expected) = expected_exit_code {
        assert_eq!(output.status.code(), Some(expected), "candidate exited through the wrong failure path");
    }
    let all_output = format!("{stdout}\n{stderr}");
    for expected in expected_diagnostics {
        assert!(
            all_output.contains(expected),
            "candidate did not reach the expected local failure branch {expected:?}; stdout={stdout}; stderr={stderr}"
        );
    }
    assert!(
        !root.path().join("data/rust-heartbeat-live.json").exists(),
        "failed candidate must not publish a live heartbeat"
    );
    assert_eq!(
        fs::read(&session_path).expect("read selected sessions after failure"),
        sessions_before,
        "failed candidate must not overwrite selected session state"
    );
    assert!(
        !root.path().join("data/agents").exists(),
        "failed preflight must not create a selected workspace"
    );
    assert_selected_gate_released(root.path());
}

const VALID_SESSION_STATE: &[u8] = br#"{"workspace_id":"candidate-workspace-sentinel","sessions":{"group:420042":{"sessionId":"candidate-session-sentinel","workspaceId":"candidate-workspace-sentinel"}}}"#;

#[test]
fn isolated_live_rollout_mismatch_exits_before_ws_and_heartbeat() {
    run_isolated_preflight_failure(
        &["灰度配置不一致，拒绝启动", "group:420099"],
        Some(2),
        VALID_SESSION_STATE,
        Some(&["group:420099"]),
        None,
    );
}

#[test]
fn isolated_live_corrupt_sessions_exit_before_ws_without_rewriting_state() {
    run_isolated_preflight_failure(
        &[
            "isolated live pipeline initialization failed; refusing to publish a live heartbeat",
            "候选会话映射 JSON 损坏",
        ],
        None,
        b"{broken selected session sentinel",
        None,
        None,
    );
}

#[test]
fn isolated_live_invalid_selected_credentials_do_not_fall_back_to_ambient_home() {
    run_isolated_preflight_failure(
        &[
            "isolated live pipeline initialization failed; refusing to publish a live heartbeat",
            "selected runtime DSH credentials are unavailable or invalid",
        ],
        None,
        VALID_SESSION_STATE,
        None,
        Some("secret: \"invalid-selected-credential\"\n"),
    );
}

fn normalize_path(value: &str) -> String {
    let canonical = fs::canonicalize(value).expect("canonicalize an existing fixture path");
    let value = canonical.to_string_lossy();
    let mut normalized = value.replace('/', "\\");
    if let Some(unc) = normalized.strip_prefix("\\\\?\\UNC\\") {
        normalized = format!("\\\\{unc}");
    } else if let Some(dos) = normalized.strip_prefix("\\\\?\\") {
        normalized = dos.to_string();
    }
    normalized
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase()
}

fn assert_runtime_path(value: &Value, field: &str, expected: &Path) {
    let actual = value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("model child did not report {field}: {value}"));
    assert_eq!(
        normalize_path(actual),
        normalize_path(&expected.to_string_lossy()),
        "model child {field} was not from the selected runtime"
    );
}

// Windows only: the subject is the isolated CLI backend, whose process-tree containment exists only
// as the Windows job-object path (`backend.rs` `run_isolated_cli`: `cfg(windows)`; other platforms
// answer "isolated CLI process-tree containment is unsupported on this platform"), and the model
// fixture is a PowerShell script run through `powershell.exe`.
#[cfg(windows)]
#[test]
fn isolated_live_cli_routes_selected_ws_prompt_model_and_onebot_through_fake_endpoints() {
    let root = TempRoot::new();
    let ambient_root = TempRoot::new();
    let (dsh_listener, dsh_url) = bind_http();
    let (model_listener, model_url) = bind_http();
    let (onebot_listener, onebot_url) = bind_http();
    let (ws_listener, ws_url) = bind_ws();
    let console_port = free_port();
    let dependencies = json!({
        "dsh":dsh_url,
        "model":model_url,
        "onebot":onebot_url,
        "onebot_ws":ws_url
    });
    selected_runtime_fixture(root.path(), &dependencies);
    selected_runtime_fixture(ambient_root.path(), &dependencies);
    let config_path = root.path().join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    cfg["socialV2"]["context"] = json!({"stripFromModelText":["奸"]});
    fs::write(config_path, cfg.to_string()).unwrap();
    let state_path = root.path().join("data/social-v2.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let strip_fixture = json!({"messageId":"strip82","sender":"奸昵称",
        "text":"奸消息 [引用 奸人：奸原话] [图片] [CQ:at,qq=123]"});
    state["conversations"]["group:420042"]["recentMessages"] = json!([strip_fixture]);
    fs::write(&state_path, state.to_string()).unwrap();
    fs::write(
        ambient_root.path().join("data/personas.json"),
        r#"{"format":1,"nextId":2,"default":"p1","overrides":{},"importedRolesAt":null,"personas":{"p1":{"id":"p1","activeVersion":1,"versions":[{"version":1,"approved":true,"name":"ticket04-decoy-persona","core":"decoy persona core"}]}}}"#,
    )
    .expect("write distinct decoy runtime persona");
    let decoy_window = br#"{"windows":{"group:420042":[{"sender":"decoy","text":"preserve decoy window","time":1}]},"consumedAt":{}}"#;
    fs::write(
        ambient_root.path().join("data/slang-windows.json"),
        decoy_window,
    )
    .expect("write second-runtime learning sentinel");
    let decoy_artifacts = [
        (
            "data/slang-windows.json",
            fs::read(ambient_root.path().join("data/slang-windows.json")).unwrap(),
        ),
        (
            "data/rust-heartbeat-live.json",
            b"{\"owner\":\"decoy-heartbeat\"}".to_vec(),
        ),
        (
            "data/token-usage-rust.jsonl",
            b"decoy-ledger-sentinel\n".to_vec(),
        ),
        (
            "data/logs/rbridge-live.jsonl",
            b"decoy-live-log-sentinel\n".to_vec(),
        ),
        (
            "data/logs/rbridge-live-wakes.jsonl",
            b"decoy-wake-log-sentinel\n".to_vec(),
        ),
        (
            "data/logs/turn-timing.jsonl",
            b"decoy-timing-log-sentinel\n".to_vec(),
        ),
        (
            "data/logs/rbridge-live.lock",
            b"999999\ndecoy-lock-marker\n".to_vec(),
        ),
    ];
    for (relative, contents) in &decoy_artifacts[1..] {
        fs::write(ambient_root.path().join(relative), contents)
            .unwrap_or_else(|error| panic!("write decoy artifact {relative}: {error}"));
    }
    let decoy_credentials =
        fs::read(ambient_root.path().join("user/dsh/.credentials.yaml"))
            .expect("read second-runtime credential sentinel");

    let sent = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let dsh_requests = Arc::new(Mutex::new(Vec::new()));
    let onebot_observations = Arc::new(Mutex::new(Vec::new()));

    let onebot_stop = stop.clone();
    let onebot_sent = sent.clone();
    let onebot_records = onebot_observations.clone();
    let onebot_thread = thread::spawn(move || {
        serve_onebot(onebot_listener, onebot_sent, onebot_records, onebot_stop)
    });
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let _console = CompanionConsole::start(root.path(), console_port, ambient_root.path(), epoch);
    let model_stop = stop.clone();
    let model_thread = thread::spawn(move || serve_model(model_listener, model_stop));
    let dsh_stop = stop.clone();
    let dsh_seen = dsh_requests.clone();
    let dsh_thread = thread::spawn(move || serve_dsh_probe(dsh_listener, dsh_stop, dsh_seen));
    let ws_sent = sent.clone();
    let ws_stop = stop.clone();
    let ws_thread = thread::spawn(move || serve_ws(ws_listener, ws_sent, ws_stop));

    let child = candidate_command(root.path(), console_port, ambient_root.path())
        .env("HER_PARITY_EPOCH_MS", epoch.to_string())
        .spawn().expect("spawn selected-root live candidate");
    let candidate_pid = child.id();
    // Observe the backend's own bounded result before the outer watchdog fires.
    let (output, candidate_error) = wait_candidate(child, Duration::from_secs(60));
    stop.store(true, Ordering::SeqCst);

    let ws_result = ws_thread.join().expect("join fake WS server");
    let model_result = model_thread.join().expect("join fake model server");
    let onebot_result = onebot_thread.join().expect("join fake OneBot server");
    let dsh_result = dsh_thread.join().expect("join fake DSH sentinel");

    let mut selected_log_files = [
        "data/logs/rbridge-live.jsonl",
        "data/logs/rbridge-live-wakes.jsonl",
        "data/logs/opencode-trial.jsonl",
        "data/logs/model-fixture.log",
        "data/rust-heartbeat-live.json",
        "data/slang-windows.json",
        "data/sessions.json",
        "data/turn-ledger.jsonl",
        "data/logs/rbridge-live.lock",
    ]
    .into_iter()
    .map(|relative| {
        let path = root.path().join(relative);
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|error| format!("<unavailable: {error}; path={path:?}>"));
        (relative.to_string(), contents)
    })
    .collect::<Vec<_>>();
    // A timeout before the backend reports its result used to discard the only
    // evidence of a fake CLI startup/transport error.
    if let Ok(files) = fs::read_dir(root.path().join("data/logs")) {
        for entry in files.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("cli-") && (name.ends_with(".out") || name.ends_with(".err")) {
                let bytes = fs::read(entry.path()).unwrap_or_default();
                selected_log_files.push((name, String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]).into_owned()));
            }
        }
    }
    let onebot_records = format!("{:?}", *onebot_observations.lock().unwrap());
    eprintln!(
        "ticket04 CLI diagnostic: pid={candidate_pid} child_error={candidate_error:?} exit={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    eprintln!(
        "ticket04 fake-server diagnostic: ws={ws_result:?}\nmodel={model_result:?}\nonebot={onebot_result:?}\ndsh={dsh_result:?}\ndsh_requests={:?}\nonebot_records={onebot_records}",
        *dsh_requests.lock().unwrap(),
    );
    eprintln!("ticket04 selected-root decision/timing/log files: {selected_log_files:#?}");

    assert!(
        candidate_error.is_none(),
        "{candidate_error:?}; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "isolated live CLI failed with {:?}; stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let ws = ws_result.expect("the public CLI must complete the fake WebSocket exchange");
    assert_eq!(
        ws.authorization.as_deref(),
        Some("Bearer ticket04-selected-onebot-token")
    );
    assert_eq!(ws.login_action.as_deref(), Some("get_login_info"));

    let model = model_result.expect("the selected CLI backend must call the selected fake model");
    assert!(model.request_line.starts_with("POST /ticket04/generate "));
    assert!(model
        .headers
        .to_ascii_lowercase()
        .contains(&format!("x-api-key: {MODEL_KEY}")));
    assert_eq!(model.body["modelBase"], model_url);
    assert_eq!(model.body["apiKey"], MODEL_KEY);
    assert_eq!(model.body["sessionKey"], "group:420042");
    assert_eq!(model.body["sessionToken"], SESSION_TOKEN);
    let received_prompt = model.body["prompt"].as_str()
        .unwrap_or_else(|| panic!("fake model received no text prompt; request body={}", model.body));
    assert!(!received_prompt.contains('奸'), "{received_prompt}");
    assert!(received_prompt.contains("昵称"), "{received_prompt}");
    assert!(received_prompt.contains("[引用 人：原话] [图片] [CQ:at,qq=123]"), "{received_prompt}");
    let stored: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert!(stored["conversations"]["group:420042"]["recentMessages"].as_array().unwrap().contains(&strip_fixture));
    assert!(
        received_prompt.contains("fixture-persona-from-selected-root"),
        "fake model received no selected-root persona; received prompt={received_prompt:?}"
    );
    assert!(
        !received_prompt.contains("ticket04-decoy-persona"),
        "fake model received the decoy persona; received prompt={received_prompt:?}"
    );
    let snapshot: Value = serde_json::from_str(received_prompt.split("【本次消息快照】\n")
        .nth(1).expect("CLI model must receive the selected wake snapshot").lines().next().unwrap())
        .expect("CLI wake snapshot JSON");
    assert_eq!(snapshot["messages"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["messages"][0]["userId"], "8808");
    assert!(snapshot["messages"][0]["text"].as_str().unwrap().contains(INBOUND_TEXT));
    assert_eq!(snapshot["includedUnreadCount"], 1);
    assert_eq!(snapshot["readThroughSeq"], snapshot["messages"][0]["seq"]);
    assert_eq!(snapshot["partial"], false);
    assert_eq!(model.body["ambientSentinel"], Value::Null);

    let dsh_home = root.path().join("user/dsh");
    assert_runtime_path(&model.body, "home", &dsh_home);
    assert_runtime_path(&model.body, "userProfile", &dsh_home);
    assert_runtime_path(&model.body, "dshHome", &dsh_home);
    assert_runtime_path(
        &model.body,
        "dshCredentials",
        &dsh_home.join(".credentials.yaml"),
    );
    assert_runtime_path(&model.body, "appData", &dsh_home.join("AppData/Roaming"));
    assert_runtime_path(&model.body, "localAppData", &dsh_home.join("AppData/Local"));
    assert_runtime_path(&model.body, "xdgConfig", &dsh_home.join(".config"));
    assert_runtime_path(&model.body, "xdgCache", &dsh_home.join(".cache"));
    assert_runtime_path(&model.body, "xdgData", &dsh_home.join(".local/share"));
    assert_runtime_path(&model.body, "temp", &root.path().join("data/tmp"));
    assert_runtime_path(&model.body, "tmp", &root.path().join("data/tmp"));
    assert_runtime_path(&model.body, "cwd", &root.path().join("resources"));
    assert_eq!(
        normalize_path(model.body["runtimeRoot"].as_str().unwrap()),
        normalize_path(&root.path().to_string_lossy())
    );
    assert!(model.body["cwd"].as_str().unwrap().contains("中文 root"));

    let sent_requests = onebot_observations.lock().unwrap();
    assert!(
        onebot_result.is_ok(),
        "fake OneBot server: {onebot_result:?}"
    );
    assert!(
        sent_requests.iter().any(|request| {
            request
                .request_line
                .starts_with("POST /get_group_member_info ")
                && request
                    .headers
                    .to_ascii_lowercase()
                    .contains(&format!("authorization: bearer {SELECTED_TOKEN}"))
                && request.body["group_id"] == GROUP_ID
        }),
        "selected OneBot did not serve login identity/card lookup: {sent_requests:?}"
    );
    let send = sent_requests
        .iter()
        .find(|request| request.request_line.starts_with("POST /send_group_msg "))
        .expect("the public live path must send the generated model reply");
    assert!(send
        .headers
        .to_ascii_lowercase()
        .contains(&format!("authorization: bearer {SELECTED_TOKEN}")));
    assert_eq!(send.body["group_id"], GROUP_ID);
    assert_eq!(
        send.body["message"][0]["data"]["text"], MODEL_REPLY,
        "the selected model response must reach the selected OneBot endpoint"
    );
    let observed_dsh = dsh_requests.lock().unwrap().clone();
    assert!(
        observed_dsh.iter().all(|request| is_background_dsh_request(request)),
        "the direct CLI session path must issue no DSH work; only the independent readiness/event monitor may contact it: {:?}",
        observed_dsh
    );
    assert!(dsh_result.is_ok(), "fake DSH listener: {dsh_result:?}");

    let heartbeat: Value = serde_json::from_slice(
        &fs::read(root.path().join("data/rust-heartbeat-live.json"))
            .expect("selected live heartbeat"),
    )
    .expect("heartbeat JSON");
    assert_eq!(heartbeat["live"], true);
    assert_eq!(heartbeat["groups"], json!(["group:420042"]));
    assert_eq!(heartbeat["pid"], candidate_pid);
    for relative in [
        "data/logs/rbridge-live.jsonl",
        "data/logs/rbridge-live-wakes.jsonl",
        "data/logs/opencode-trial.jsonl",
    ] {
        let path = root.path().join(relative);
        assert!(
            path.is_file(),
            "expected selected-root live artifact {relative}"
        );
    }
    let decision_log = fs::read_to_string(root.path().join("data/logs/rbridge-live.jsonl"))
        .expect("selected live decision log");
    assert!(decision_log.lines().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|row| row["type"] == "start" && row["shadow"] == false)
    }));
    let wake_log = fs::read_to_string(root.path().join("data/logs/rbridge-live-wakes.jsonl"))
        .expect("selected live wake log");
    assert!(wake_log.lines().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|row| row["key"] == "group:420042")
    }));
    let trial: Value = serde_json::from_str(
        fs::read_to_string(root.path().join("data/logs/opencode-trial.jsonl"))
            .expect("selected backend trial log")
            .lines()
            .last()
            .expect("one CLI backend result"),
    )
    .expect("backend trial JSON");
    assert_eq!(trial["backend"], "ticket04-live-cli");
    assert_eq!(trial["ok"], true);
    assert!(root.path().join("data/agents").is_dir());
    let learning: Value = serde_json::from_slice(
        &fs::read(root.path().join("data/slang-windows.json"))
            .expect("selected-root live learning window"),
    )
    .expect("selected learning window JSON");
    let learned_messages = learning["windows"]["group:420042"]
        .as_array()
        .expect("selected group learning window");
    assert_eq!(learned_messages.len(), 1);
    // JS plainContent excludes quoted originals, but retains resolved @ mentions
    // (bridge.js segmentsToText -> feedSlangWindow).
    assert_eq!(learned_messages[0]["sender"], "ticket04-fixture-user");
    assert_eq!(learned_messages[0]["text"], format!("@selected-fixture-card {INBOUND_TEXT}"));
    assert!(
        !learned_messages
            .iter()
            .any(|message| message["text"] == "preserve decoy window"),
        "selected learning window imported a message from the second runtime"
    );
    for (relative, expected) in &decoy_artifacts {
        let observed = fs::read(ambient_root.path().join(relative))
            .unwrap_or_else(|error| panic!("read decoy artifact {relative}: {error}"));
        assert_eq!(
            observed.as_slice(),
            expected.as_slice(),
            "selected live work modified second-runtime artifact {relative}"
        );
    }
    assert_eq!(
        fs::read(ambient_root.path().join("user/dsh/.credentials.yaml")).unwrap(),
        decoy_credentials,
        "selected live work modified second-runtime credentials"
    );
    assert!(
        !ambient_root.path().join("data/slang-windows.lock").exists(),
        "selected learning path created its lock in the second runtime"
    );
    // OwnerGate keeps the OS lock file persistent and publishes identity in its sidecar.
    let owner: Value = serde_json::from_slice(
        &fs::read(root.path().join("data/logs/rbridge-live.lock.owner.json"))
            .expect("persistent selected-root live owner marker"),
    )
    .expect("selected live owner marker JSON");
    assert_eq!(owner["identity"]["pid"], candidate_pid);
    assert!(owner["identity"]["start"].as_u64().unwrap() > 0);
    assert_eq!(
        normalize_path(owner["identity"]["exe"].as_str().unwrap()),
        normalize_path(env!("CARGO_BIN_EXE_her")),
    );
    assert_eq!(owner["role"], "live");
    assert_eq!(owner["groups"], json!(["group:420042"]));
    let live_gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("data/logs/rbridge-live.lock"))
        .expect("reopen selected live gate after candidate exit");
    live_gate
        .try_lock()
        .expect("successful live candidate exit must release the selected-root owner lock");
    let learning_gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("data/slang-windows.lock"))
        .expect("open selected learning-window gate");
    learning_gate
        .try_lock()
        .expect("selected learning window feed must release its persistent gate");
    assert!(String::from_utf8_lossy(&output.stdout).contains("[her] connected"));
}
