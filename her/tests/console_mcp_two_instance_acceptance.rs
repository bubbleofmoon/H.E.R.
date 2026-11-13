//! Ticket 03 combined acceptance: two isolated runtime roots, each served by its own console
//! process and MCP stdio process on distinct ports, driven through interleaved public HTTP and
//! MCP calls. Every dependency is a synthetic loopback fake; the ambient DSH home, user profile
//! and memory override point at a decoy directory whose bytes must stay unchanged.

#[path = "../src/install_root.rs"]
mod install_root;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SESSION_KEY: &str = "private:7001";
const SESSION_QUERY: &str = "private%3A7001";
const OWNER_ID: i64 = 7001;

const MEMORY_SCHEMA: &str = "CREATE TABLE memory (
    id INTEGER PRIMARY KEY AUTOINCREMENT, owner_type TEXT NOT NULL, owner_id TEXT NOT NULL,
    scope TEXT NOT NULL, kind TEXT NOT NULL, subject TEXT, content TEXT NOT NULL,
    tags TEXT, source TEXT NOT NULL, importance REAL NOT NULL DEFAULT 0.5,
    hits INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
    last_used_at INTEGER, expires_at INTEGER
 );
 CREATE VIRTUAL TABLE memory_fts USING fts5(seg, tokenize='unicode61');
 CREATE TABLE memory_acl (
    id INTEGER PRIMARY KEY AUTOINCREMENT, reader_type TEXT NOT NULL, reader_id TEXT NOT NULL,
    read_type TEXT NOT NULL, read_id TEXT NOT NULL, level TEXT NOT NULL DEFAULT 'read',
    created_at INTEGER NOT NULL
 );
 CREATE TABLE team_member (
    team_id TEXT NOT NULL, member_type TEXT NOT NULL, member_id TEXT NOT NULL,
    PRIMARY KEY (team_id, member_type, member_id)
 );
 CREATE TABLE memory_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
    actor TEXT, action TEXT, memory_id INTEGER, detail TEXT
 );";

struct FixtureWorkspace(PathBuf);

impl FixtureWorkspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "qqbot 票据03 two-instance-{}-{stamp}",
            std::process::id()
        )));
        fs::create_dir_all(&root).expect("create synthetic two-instance workspace");
        Self(root)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for FixtureWorkspace {
    fn drop(&mut self) {
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_millis() as i64
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

fn write_json(path: &Path, value: &Value) {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).expect("serialize fixture JSON"),
    )
    .expect("write fixture JSON");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read fixture JSON")).expect("parse fixture JSON")
}

/// Every regular file below `dir`, keyed by relative path, with its exact bytes.
fn snapshot_tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("read decoy directory") {
            let entry = entry.expect("read decoy entry");
            let path = entry.path();
            let kind = entry.file_type().expect("read decoy entry type");
            if kind.is_dir() {
                walk(base, &path, out);
            } else {
                let relative = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                out.insert(relative, fs::read(&path).expect("read decoy file"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn memory_contents(db: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(db).expect("open fixture memory database");
    let mut statement = connection
        .prepare("SELECT content FROM memory ORDER BY id")
        .expect("prepare memory content query");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query memory content");
    rows.map(|row| row.expect("read memory content")).collect()
}

fn acl_read_ids(db: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(db).expect("open fixture memory database");
    let mut statement = connection
        .prepare("SELECT read_id FROM memory_acl ORDER BY id")
        .expect("prepare ACL query");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query ACL rows");
    rows.map(|row| row.expect("read ACL row")).collect()
}

// ---------------------------------------------------------------------------------------------
// Fake DSH: answers agentPresets/list only for a cookie signed with this root's secret.
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct DshObservation {
    request_line: String,
    cookie_valid: bool,
}

struct FakeDsh {
    base_url: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<Vec<DshObservation>, String>>>,
}

impl FakeDsh {
    fn start(secret: Vec<u8>, preset_id: &str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake DSH loopback");
        listener
            .set_nonblocking(true)
            .expect("set fake DSH listener nonblocking");
        let authority = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let base_url = format!("http://{authority}");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let preset_id = preset_id.to_string();
        let worker = thread::spawn(move || {
            let mut observations = Vec::new();
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => observations.push(serve_fake_dsh_request(
                        stream, &authority, &secret, &preset_id,
                    )?),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(10035) =>
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(format!("accept fake DSH request failed: {error}")),
                }
            }
            Ok(observations)
        });
        Self {
            base_url,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Vec<DshObservation> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker
            .take()
            .expect("fake DSH worker is present")
            .join()
            .expect("fake DSH worker must not panic")
            .expect("fake DSH worker must not fail")
    }
}

impl Drop for FakeDsh {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_fake_dsh_request(
    stream: TcpStream,
    authority: &str,
    secret: &[u8],
    preset_id: &str,
) -> Result<DshObservation, String> {
    stream
        .set_nonblocking(false)
        .map_err(|_| "set fake DSH stream blocking".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| "set fake DSH request timeout".to_string())?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|_| "read fake DSH request line".to_string())?;
    let mut cookie = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|_| "read fake DSH headers".to_string())?
            == 0
        {
            return Err("fake DSH request ended before headers".to_string());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            if name == "content-length" {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name == "cookie" {
                cookie = value.trim().to_string();
            }
        }
    }
    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|_| "read fake DSH request body".to_string())?;
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let cookie_valid = verify_dsh_cookie(authority, &cookie, secret);
    let request_line = request_line.trim().to_string();
    let valid_call = cookie_valid
        && request_line.starts_with("POST /api/agentPresets/list ")
        && request.get("method").and_then(Value::as_str) == Some("agentPresets/list");
    let (status, response_body) = if valid_call {
        (
            "200 OK",
            json!({
                "type": "server-response",
                "rpcId": request.get("rpcId").cloned().unwrap_or(Value::Null),
                "result": { "ok": true, "value": { "presets": [{ "id": preset_id, "trust": "fixture" }] } }
            })
            .to_string(),
        )
    } else {
        ("401 Unauthorized", String::new())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len()
    );
    reader
        .get_mut()
        .write_all(response.as_bytes())
        .map_err(|_| "write fake DSH response".to_string())?;
    Ok(DshObservation {
        request_line,
        cookie_valid,
    })
}

fn verify_dsh_cookie(authority: &str, cookie: &str, secret: &[u8]) -> bool {
    let Some((name, value)) = cookie.split_once('=') else {
        return false;
    };
    let expected_name = format!(
        "dsh-auth-{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(authority.as_bytes()))
    );
    if name != expected_name {
        return false;
    }
    let pieces: Vec<&str> = value.split('.').collect();
    if pieces.len() != 3 || pieces[0] != "v1" {
        return false;
    }
    let Ok(signature) = URL_SAFE_NO_PAD.decode(pieces[2]) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(pieces[1].as_bytes());
    if mac.verify_slice(&signature).is_err() {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(pieces[1])
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|claims| {
            claims
                .get("authority")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some(authority)
}

/// A loopback listener that must never be contacted (ambient/legacy configuration decoys).
struct Sentinel {
    url: String,
    listener: TcpListener,
}

impl Sentinel {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind sentinel loopback");
        listener
            .set_nonblocking(true)
            .expect("set sentinel listener nonblocking");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        Self { url, listener }
    }

    fn assert_untouched(&self) {
        match self.listener.accept() {
            Ok((_, peer)) => panic!("decoy endpoint {} was contacted by {peer}", self.url),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(10035) => {}
            Err(error) => panic!("sentinel listener failed: {error}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Selected runtime roots.
// ---------------------------------------------------------------------------------------------

struct RootFixture {
    root: PathBuf,
    marker: &'static str,
    console_token: String,
    agent_token: String,
    session_id: String,
    port: u16,
}

#[allow(clippy::too_many_arguments)]
fn prepare_root(
    root: &Path,
    marker: &'static str,
    port: u16,
    selected_dsh_url: &str,
    legacy_dsh_url: &str,
    legacy_onebot_url: &str,
    secret: &[u8],
    ledger_tokens: (i64, i64),
    dsh_usage_tokens: i64,
) -> RootFixture {
    let checked = crate::install_root::development_root(root);
    let root = checked.as_path();
    for relative in ["config", "data", "resources/roles", "user/dsh"] {
        fs::create_dir_all(root.join(relative)).expect("create selected runtime directory");
    }
    let console_token = format!("two-instance-console-token-{marker}");
    let agent_token = format!("two-instance-agent-token-{marker}");
    let session_id = format!("{marker}-dsh-session");
    write_json(
        &root.join("config/config.json"),
        &json!({
            "consoleToken": console_token,
            "consolePort": port,
            "ownerQQ": OWNER_ID,
            "allow": { "groups": [], "private": [OWNER_ID.to_string()] },
            "configMarker": format!("{marker}-config-marker"),
            "dsh": {
                "baseUrl": legacy_dsh_url,
                "provider": "fixture",
                "model": format!("{marker}-global-model"),
                "reasoningEffort": "high"
            },
            "snowluma": { "httpUrl": legacy_onebot_url, "accessToken": format!("{marker}-onebot") },
            "socialV2": {
                "enabled": true,
                "tools": { "memory": true },
                "send": { "burstEnabled": false }
            }
        }),
    );
    write_json(
        &root.join("dependencies.json"),
        &json!({
            "dsh": selected_dsh_url,
            "model": "http://127.0.0.1:1",
            "onebot": "http://127.0.0.1:1"
        }),
    );
    let now = now_ms();
    let recent: Vec<Value> = (1..=3)
        .map(|seq| {
            json!({
                "seq": seq,
                "sender": format!("{marker}-sender"),
                "userId": "101",
                "text": format!("{marker}-recent-message-{seq}"),
                "time": 1000 * seq,
                "isSelf": false,
                "isOwner": false
            })
        })
        .collect();
    write_json(
        &root.join("data/social-v2.json"),
        &json!({
            "conversations": {
                SESSION_KEY: {
                    "agentToken": agent_token,
                    "recentMessages": recent,
                    "activeTopics": [{
                        "text": format!("{marker}-seed-topic"),
                        "lastMentionAt": now,
                        "participants": [],
                        "pendingQuestion": ""
                    }]
                }
            }
        }),
    );
    fs::write(root.join("data/mode.json"), br#"{"mode":"reserved2"}"#)
        .expect("write selected mode");
    write_json(
        &root.join("data/sessions.json"),
        &json!({ "sessions": { SESSION_KEY: { "sessionId": session_id } } }),
    );
    let ledger_row = json!({
        "k": "turn", "t": now, "sid": session_id, "turn": 1, "step": 1, "retry": 0, "seq": 1,
        "b": { "cacheMiss": ledger_tokens.0, "output": ledger_tokens.1 },
        "model": format!("{marker}-ledger-model"), "provider": format!("{marker}-provider")
    });
    fs::write(
        root.join("data/token-usage.jsonl"),
        format!("{ledger_row}\n"),
    )
    .expect("write selected ledger");
    write_json(
        &root.join("user/dsh/.dshw-usage.json"),
        &json!({ "events": [{ "ts": now, "tokens": dsh_usage_tokens, "cost": 0.5 }] }),
    );
    fs::write(
        root.join("user/dsh/.credentials.yaml"),
        format!("secret: {}\n", URL_SAFE_NO_PAD.encode(secret)),
    )
    .expect("write selected DSH credential fixture");
    let connection =
        rusqlite::Connection::open(root.join("data/memory.db")).expect("create selected memory DB");
    connection
        .execute_batch(MEMORY_SCHEMA)
        .expect("initialize selected memory schema");
    drop(connection);
    RootFixture {
        root: root.to_path_buf(),
        marker,
        console_token,
        agent_token,
        session_id,
        port,
    }
}

/// Ambient decoys that a process must ignore once a runtime root is selected.
fn prepare_ambient_decoy(dir: &Path, legacy_secret: &[u8]) {
    let checked = crate::install_root::development_root(dir);
    let dir = checked.as_path();
    fs::create_dir_all(dir.join("profile")).expect("create ambient decoy profile");
    fs::write(
        dir.join(".credentials.yaml"),
        format!("secret: {}\n", URL_SAFE_NO_PAD.encode(legacy_secret)),
    )
    .expect("write ambient DSH credential decoy");
    write_json(
        &dir.join(".dshw-usage.json"),
        &json!({ "events": [{ "ts": now_ms(), "tokens": 987_654, "cost": 99.0 }] }),
    );
    let connection =
        rusqlite::Connection::open(dir.join("memory-override.db")).expect("create decoy memory DB");
    connection
        .execute_batch(MEMORY_SCHEMA)
        .expect("initialize decoy memory schema");
}

fn reserve_distinct_ports() -> (u16, u16) {
    let first = TcpListener::bind(("127.0.0.1", 0)).expect("reserve first console port");
    let second = TcpListener::bind(("127.0.0.1", 0)).expect("reserve second console port");
    let ports = (
        first.local_addr().unwrap().port(),
        second.local_addr().unwrap().port(),
    );
    assert_ne!(ports.0, ports.1);
    ports
}

fn isolated_command(root: &Path, port: u16, ambient: &Path, mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg(mode)
        .arg("--console-port")
        .arg(port.to_string())
        .arg("--runtime-root")
        .arg(root)
        .env_remove("HER_RUNTIME_ROOT")
        .env_remove("APPDATA")
        .env_remove("LOCALAPPDATA")
        .env_remove("HOME")
        .env("DSH_HOME", ambient)
        .env("USERPROFILE", ambient.join("profile"))
        .env("HER_MEMORY_DB", ambient.join("memory-override.db"));
    hide_process_window(&mut command);
    command
}

// ---------------------------------------------------------------------------------------------
// Console process + raw HTTP client.
// ---------------------------------------------------------------------------------------------

struct ConsoleProcess {
    child: Child,
    port: u16,
}

impl ConsoleProcess {
    fn start(fixture: &RootFixture, ambient: &Path) -> Self {
        let mut command = isolated_command(&fixture.root, fixture.port, ambient, "--console-serve");
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let child = command.spawn().expect("start isolated console");
        let mut process = Self {
            child,
            port: fixture.port,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                process.child.try_wait().expect("poll console").is_none(),
                "isolated console {} exited before health",
                fixture.marker
            );
            if let Ok(response) = http(process.port, "GET", "/api/health", None, None, None) {
                if response.0 == 200 {
                    let health: Value = serde_json::from_str(&response.1).unwrap();
                    assert_eq!(health["port"], fixture.port);
                    assert_eq!(health["runtime"]["mode"], "isolated");
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "isolated console health timed out"
            );
            thread::sleep(Duration::from_millis(50));
        }
        process
    }
}

impl Drop for ConsoleProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    console_token: Option<&str>,
    agent_token: Option<&str>,
    body: Option<&Value>,
) -> Result<(u16, String), String> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let console_header = console_token
        .map(|token| format!("X-Console-Token: {token}\r\n"))
        .unwrap_or_default();
    let agent_header = agent_token
        .map(|token| format!("X-Agent-Token: {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{console_header}{agent_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect console: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write console request: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read console response: {e}"))?;
    let text = String::from_utf8(raw).map_err(|_| "console response is not UTF-8".to_string())?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "console response has no header boundary".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| "console response status is invalid".to_string())?;
    Ok((status, body.to_string()))
}

fn call(
    fixture: &RootFixture,
    method: &str,
    path: &str,
    agent: bool,
    body: Option<&Value>,
    expected: u16,
) -> Value {
    let agent_token = agent.then_some(fixture.agent_token.as_str());
    let (status, text) = http(
        fixture.port,
        method,
        path,
        Some(&fixture.console_token),
        agent_token,
        body,
    )
    .unwrap_or_else(|error| {
        panic!(
            "{} {method} {path} transport failed: {error}",
            fixture.marker
        )
    });
    assert_eq!(
        status, expected,
        "{} {method} {path}: {text}",
        fixture.marker
    );
    serde_json::from_str(&text).unwrap_or_else(|_| {
        panic!(
            "{} {method} {path} returned non-JSON: {text}",
            fixture.marker
        )
    })
}

// ---------------------------------------------------------------------------------------------
// MCP stdio process.
// ---------------------------------------------------------------------------------------------

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    fn start(fixture: &RootFixture, ambient: &Path) -> Self {
        let mut command = isolated_command(&fixture.root, fixture.port, ambient, "--mcp-serve");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("start isolated MCP process");
        let stdin = child.stdin.take().expect("MCP stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("MCP stdout is piped"));
        let mut process = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let initialized = process.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "ticket-03-two-instance", "version": "1" }
            }),
        );
        assert_eq!(initialized["result"]["serverInfo"]["name"], "snowluma-safe");
        writeln!(
            process.stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .expect("write MCP initialized notification");
        process
            .stdin
            .flush()
            .expect("flush MCP initialized notification");
        process
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        )
        .expect("write MCP request");
        self.stdin.flush().expect("flush MCP request");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read MCP response");
        assert!(
            !line.is_empty(),
            "isolated MCP process exited before replying"
        );
        let response: Value = serde_json::from_str(&line).expect("MCP response is JSON");
        assert_eq!(response["id"], id);
        response
    }

    /// Calls a tool with this root's session credentials merged into `arguments`.
    fn tool(&mut self, fixture: &RootFixture, name: &str, arguments: Value) -> (bool, String) {
        let mut arguments = arguments;
        arguments["key"] = json!(SESSION_KEY);
        if arguments.get("token").is_none() {
            arguments["token"] = json!(fixture.agent_token);
        }
        let response = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let is_error = response["result"]["isError"].as_bool() == Some(true);
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} response has no text: {response}"))
            .to_string();
        (is_error, text)
    }

    fn ok_tool(&mut self, fixture: &RootFixture, name: &str, arguments: Value) -> String {
        let (is_error, text) = self.tool(fixture, name, arguments);
        assert!(!is_error, "{} MCP {name} failed: {text}", fixture.marker);
        text
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------------

#[test]
fn two_isolated_console_and_mcp_instances_interleave_without_crossing_roots() {
    let workspace = FixtureWorkspace::new();
    let ambient = workspace.path("ambient decoy home");
    let legacy_secret = [0x31u8; 32];
    prepare_ambient_decoy(&ambient, &legacy_secret);
    let ambient_before = snapshot_tree(&ambient);

    let secret_a = [0x2au8; 32];
    let secret_b = [0x6bu8; 32];
    let dsh_a = FakeDsh::start(secret_a.to_vec(), "selected-preset-alpha");
    let dsh_b = FakeDsh::start(secret_b.to_vec(), "selected-preset-beta");
    let legacy_dsh = Sentinel::start();
    let legacy_onebot = Sentinel::start();
    let (port_a, port_b) = reserve_distinct_ports();
    let a = prepare_root(
        &workspace.path("运行根 A"),
        "alpha",
        port_a,
        &dsh_a.base_url,
        &legacy_dsh.url,
        &legacy_onebot.url,
        &secret_a,
        (10, 11),
        1_000,
    );
    let b = prepare_root(
        &workspace.path("运行根 B"),
        "beta",
        port_b,
        &dsh_b.base_url,
        &legacy_dsh.url,
        &legacy_onebot.url,
        &secret_b,
        (3, 4),
        200,
    );

    let console_a = ConsoleProcess::start(&a, &ambient);
    let console_b = ConsoleProcess::start(&b, &ambient);
    assert_ne!(console_a.port, console_b.port);
    let mut mcp_a = McpProcess::start(&a, &ambient);
    let mut mcp_b = McpProcess::start(&b, &ambient);
    let tools_a = mcp_a.request("tools/list", json!({}));
    let tools_b = mcp_b.request("tools/list", json!({}));
    assert_eq!(tools_a["result"]["tools"], tools_b["result"]["tools"]);
    // The 38 tools of the frozen JS `mcp-snowluma-safe.js` (no `socialV2.tools` switch set, so all
    // registered) plus the 7 memory/settings tools. Ticket 14 (fecd0812) regenerated the spec from the
    // frozen JS: it added `qq_send_voice`, `qq_list_voices` and `qq_set_sticker_remark` to the old 35.
    assert_eq!(tools_a["result"]["tools"].as_array().unwrap().len(), 38 + 7);

    // 1. Config writes, interleaved with MCP memory writes on the other root.
    let saved_a = call(
        &a,
        "POST",
        "/api/session-models",
        false,
        Some(&json!({ "key": SESSION_KEY, "model": "alpha-session-model" })),
        200,
    );
    assert_eq!(saved_a["ok"], true);
    let wrote_b = mcp_b.ok_tool(&b, "memory_write", json!({ "content": "betaonlyfact" }));
    assert!(wrote_b.contains('#'), "B memory_write: {wrote_b}");
    call(
        &b,
        "POST",
        "/api/session-models",
        false,
        Some(&json!({ "key": SESSION_KEY, "model": "beta-session-model" })),
        200,
    );
    mcp_a.ok_tool(&a, "memory_write", json!({ "content": "alphaonlyfact" }));
    let models_b = call(&b, "GET", "/api/session-models", false, None, 200);
    let models_a = call(&a, "GET", "/api/session-models", false, None, 200);
    assert_eq!(
        models_a["sessions"][SESSION_KEY]["model"],
        "alpha-session-model"
    );
    assert_eq!(
        models_b["sessions"][SESSION_KEY]["model"],
        "beta-session-model"
    );
    assert_eq!(models_a["global"]["model"], "alpha-global-model");
    assert_eq!(models_b["global"]["model"], "beta-global-model");

    // 2. Memory written over MCP is visible to the same root's console HTTP only.
    let search_b = call(
        &b,
        "GET",
        &format!("/api/memory/search?key={SESSION_QUERY}&query=alphaonlyfact"),
        true,
        None,
        200,
    );
    assert!(
        !search_b.to_string().contains("alphaonlyfact"),
        "B saw A memory: {search_b}"
    );
    let search_a = call(
        &a,
        "GET",
        &format!("/api/memory/search?key={SESSION_QUERY}&query=alphaonlyfact"),
        true,
        None,
        200,
    );
    assert_eq!(search_a["count"], 1, "A search: {search_a}");
    assert!(search_a.to_string().contains("alphaonlyfact"));
    assert!(!search_a.to_string().contains("betaonlyfact"));
    let mcp_search_b = mcp_b.ok_tool(&b, "memory_search", json!({ "query": "betaonlyfact" }));
    assert!(mcp_search_b.contains("betaonlyfact") && !mcp_search_b.contains("alphaonlyfact"));

    // 3. Memory ACL granted through console A is only visible to MCP A.
    let granted = call(
        &a,
        "POST",
        "/api/memory/acl",
        false,
        Some(&json!({
            "action": "grant", "readerType": "session", "readerId": "alpha-reader",
            "readType": "session", "readId": "alpha-acl-target", "level": "read"
        })),
        200,
    );
    assert_eq!(granted["ok"], true);
    let acl_b = mcp_b.ok_tool(&b, "memory_acl", json!({ "action": "list" }));
    assert!(
        !acl_b.contains("alpha-acl-target"),
        "B MCP saw A ACL: {acl_b}"
    );
    let acl_a = mcp_a.ok_tool(&a, "memory_acl", json!({ "action": "list" }));
    assert!(
        acl_a.contains("alpha-acl-target"),
        "A MCP ACL list: {acl_a}"
    );
    let acl_list_b = call(&b, "GET", "/api/memory/acl", false, None, 200);
    assert_eq!(acl_list_b["acl"], json!([]));

    // 4. Owner settings written through console A; MCP reads each root's own config.
    let set_a = call(
        &a,
        "POST",
        "/api/memory/settings-set",
        true,
        Some(
            &json!({ "key": SESSION_KEY, "settingKey": "socialV2.send.burstEnabled", "value": true }),
        ),
        200,
    );
    assert_eq!(set_a["value"], true);
    // The JS `settings_get` schema field is `settingKey`; ticket 17 (db0ac01d) moved the Rust schema
    // from `setting` to it, so the old key is stripped and the call would list every setting.
    let setting_b = mcp_b.ok_tool(
        &b,
        "settings_get",
        json!({ "settingKey": "socialV2.send.burstEnabled" }),
    );
    let setting_a = mcp_a.ok_tool(
        &a,
        "settings_get",
        json!({ "settingKey": "socialV2.send.burstEnabled" }),
    );
    assert_eq!(setting_b, "socialV2.send.burstEnabled = false");
    assert_eq!(setting_a, "socialV2.send.burstEnabled = true");
    let read_back_b = call(
        &b,
        "GET",
        &format!(
            "/api/memory/settings-get?key={SESSION_QUERY}&settingKey=socialV2.send.burstEnabled"
        ),
        true,
        None,
        200,
    );
    assert_eq!(read_back_b["value"], false);

    // 5. qq_* MCP tools reach their own console (inline social memory and recent messages).
    let appended = mcp_a.ok_tool(
        &a,
        "qq_memory_append",
        json!({ "category": "activeTopic", "content": "alpha-mcp-topic" }),
    );
    assert!(
        appended.contains("\"ok\":true"),
        "A qq_memory_append: {appended}"
    );
    let memory_b = call(
        &b,
        "GET",
        &format!("/api/socialV2/memory?key={SESSION_QUERY}&category=activeTopic"),
        true,
        None,
        200,
    );
    assert!(memory_b.to_string().contains("beta-seed-topic"));
    assert!(
        !memory_b.to_string().contains("alpha"),
        "B inline memory leaked A: {memory_b}"
    );
    let query_b = mcp_b.ok_tool(&b, "qq_memory_query", json!({ "category": "activeTopic" }));
    assert!(query_b.contains("beta-seed-topic") && !query_b.contains("alpha-mcp-topic"));
    let memory_a = call(
        &a,
        "GET",
        &format!("/api/socialV2/memory?key={SESSION_QUERY}&category=activeTopic"),
        true,
        None,
        200,
    );
    assert!(
        memory_a.to_string().contains("alpha-mcp-topic"),
        "A inline memory: {memory_a}"
    );
    let recent_b = mcp_b.ok_tool(&b, "qq_get_recent_messages", json!({ "limit": 3 }));
    assert!(recent_b.contains("beta-recent-message-3") && !recent_b.contains("alpha"));
    let recent_a = mcp_a.ok_tool(&a, "qq_get_recent_messages", json!({ "limit": 3 }));
    assert!(recent_a.contains("alpha-recent-message-3") && !recent_a.contains("beta"));

    // 6. DSH-backed presets use each root's dependency URL and selected credentials.
    let presets_b = call(&b, "GET", "/api/presets", false, None, 200);
    let presets_a = call(&a, "GET", "/api/presets", false, None, 200);
    assert_eq!(presets_a["presets"][0]["id"], "selected-preset-alpha");
    assert_eq!(presets_b["presets"][0]["id"], "selected-preset-beta");

    // 7. Usage views read each root's ledger, session map and selected DSH usage file.
    let usage_b = call(
        &b,
        "GET",
        "/api/usage/attribution?days=90",
        false,
        None,
        200,
    );
    let usage_a = call(
        &a,
        "GET",
        "/api/usage/attribution?days=90",
        false,
        None,
        200,
    );
    assert_eq!(usage_a["our"]["tokens"], 21.0);
    assert_eq!(usage_b["our"]["tokens"], 7.0);
    assert_eq!(usage_a["dshTotal"]["tokens"], 1_000.0);
    assert_eq!(usage_b["dshTotal"]["tokens"], 200.0);
    let summary_a = call(&a, "GET", "/api/tokens/summary?limit=20", false, None, 200);
    let summary_b = call(&b, "GET", "/api/tokens/summary?limit=20", false, None, 200);
    assert!(!summary_a.to_string().contains(&b.session_id));
    assert!(!summary_b.to_string().contains(&a.session_id));
    for (fixture, provider, foreign) in [
        (&a, "alpha-provider", "beta"),
        (&b, "beta-provider", "alpha"),
    ] {
        let turns = call(
            fixture,
            "GET",
            &format!("/api/tokens/turns?key={SESSION_QUERY}&limit=5"),
            false,
            None,
            200,
        );
        assert_eq!(turns["turns"].as_array().unwrap().len(), 1);
        let by_provider = call(fixture, "GET", "/api/tokens/by-provider", false, None, 200);
        assert!(by_provider.to_string().contains(provider));
        assert!(!by_provider.to_string().contains(foreign));
    }

    // 8. Role resources stay in each root.
    call(
        &a,
        "POST",
        "/api/roles/create",
        false,
        Some(&json!({ "name": "AlphaRole", "content": "alpha role body" })),
        200,
    );
    let roles_b = call(&b, "GET", "/api/roles", false, None, 200);
    assert!(!roles_b.to_string().contains("AlphaRole"));
    let role_a = call(
        &a,
        "GET",
        "/api/roles/content?name=AlphaRole",
        false,
        None,
        200,
    );
    assert!(role_a.to_string().contains("alpha role body"));

    // 9. Credentials never cross roots, and still-guarded operations fail closed.
    let (cross_console, _) = http(
        a.port,
        "GET",
        "/api/tokens/summary",
        Some(&b.console_token),
        None,
        None,
    )
    .unwrap();
    assert_eq!(cross_console, 401);
    let (cross_agent, cross_agent_body) = http(
        a.port,
        "GET",
        &format!("/api/memory/search?key={SESSION_QUERY}&query=alphaonlyfact"),
        Some(&a.console_token),
        Some(&b.agent_token),
        None,
    )
    .unwrap();
    assert_eq!(
        cross_agent, 403,
        "B agent token on A memory: {cross_agent_body}"
    );
    let (foreign_mcp_error, foreign_mcp_text) = mcp_a.tool(
        &a,
        "memory_search",
        json!({ "query": "alphaonlyfact", "token": b.agent_token }),
    );
    // Frozen JS `mcp-memory.js` reports a bridge refusal as plain text without `isError`
    // (`return text(`检索失败：${e.message}`)`); the refusal itself is the bridge's 403 text.
    assert!(
        !foreign_mcp_error,
        "JS reports memory_search failures as plain text: {foreign_mcp_text}"
    );
    assert_eq!(
        foreign_mcp_text, "检索失败：agent token 无效（必须是该会话自己的令牌）",
        "A MCP accepted B token"
    );
    let ledger_a_before = fs::read(a.root.join("data/token-usage.jsonl")).unwrap();
    let (guarded, guarded_body) = http(
        a.port,
        "POST",
        "/api/tokens/reset",
        Some(&a.console_token),
        None,
        Some(&json!({})),
    )
    .unwrap();
    assert_eq!(guarded, 503, "guarded reset: {guarded_body}");
    assert_eq!(
        fs::read(a.root.join("data/token-usage.jsonl")).unwrap(),
        ledger_a_before
    );

    // Final on-disk state: each root carries only its own writes.
    drop((mcp_a, mcp_b));
    drop((console_a, console_b));
    for (fixture, own, foreign, burst) in
        [(&a, "alpha", "beta", true), (&b, "beta", "alpha", false)]
    {
        let config = read_json(&fixture.root.join("config/config.json"));
        assert_eq!(
            config["sessions"][SESSION_KEY]["model"],
            format!("{own}-session-model")
        );
        assert_eq!(config["socialV2"]["send"]["burstEnabled"], burst);
        assert!(!config
            .to_string()
            .contains(&format!("{foreign}-session-model")));
        let state = fs::read_to_string(fixture.root.join("data/social-v2.json")).unwrap();
        assert!(
            !state.contains(&format!("{foreign}-")),
            "{own} state has {foreign} data"
        );
        let memories = memory_contents(&fixture.root.join("data/memory.db"));
        assert_eq!(memories, vec![format!("{own}onlyfact")]);
    }
    let state_a = fs::read_to_string(a.root.join("data/social-v2.json")).unwrap();
    assert!(state_a.contains("alpha-mcp-topic"));
    assert_eq!(
        acl_read_ids(&a.root.join("data/memory.db")),
        vec!["alpha-acl-target"]
    );
    assert!(acl_read_ids(&b.root.join("data/memory.db")).is_empty());
    assert!(a.root.join("resources/roles/AlphaRole.md").is_file());
    assert!(!b.root.join("resources/roles/AlphaRole.md").exists());
    assert_eq!(
        snapshot_tree(&ambient),
        ambient_before,
        "ambient decoy home changed"
    );
    assert!(memory_contents(&ambient.join("memory-override.db")).is_empty());
    legacy_dsh.assert_untouched();
    legacy_onebot.assert_untouched();
    for (dsh, marker) in [(dsh_a, "alpha"), (dsh_b, "beta")] {
        let observations = dsh.finish();
        assert_eq!(
            observations.len(),
            1,
            "{marker} DSH requests: {observations:?}"
        );
        assert!(
            observations[0].cookie_valid,
            "{marker} DSH cookie must use selected secret"
        );
        assert!(observations[0]
            .request_line
            .starts_with("POST /api/agentPresets/list "));
    }
}
