#[path = "../src/install_root.rs"]
mod install_root;

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MEMORY_SCHEMA: &str = r#"
CREATE TABLE memory (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_type TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  kind TEXT NOT NULL,
  subject TEXT,
  content TEXT NOT NULL,
  tags TEXT,
  source TEXT NOT NULL,
  importance REAL NOT NULL DEFAULT 0.5,
  hits INTEGER NOT NULL DEFAULT 0,
  pinned INTEGER NOT NULL DEFAULT 0,
  archived INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER,
  expires_at INTEGER
);
CREATE VIRTUAL TABLE memory_fts USING fts5(seg, tokenize='unicode61');
CREATE TABLE memory_acl (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  reader_type TEXT NOT NULL,
  reader_id TEXT NOT NULL,
  read_type TEXT NOT NULL,
  read_id TEXT NOT NULL,
  level TEXT NOT NULL DEFAULT 'read',
  created_at INTEGER NOT NULL
);
CREATE TABLE team_member (
  team_id TEXT NOT NULL,
  member_type TEXT NOT NULL,
  member_id TEXT NOT NULL,
  PRIMARY KEY (team_id, member_type, member_id)
);
CREATE TABLE memory_audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  actor TEXT,
  action TEXT,
  memory_id INTEGER,
  detail TEXT
);
"#;

struct TempRoot(PathBuf);

impl Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_root(label: &str) -> TempRoot {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let path = crate::install_root::development_root(std::env::temp_dir().join(format!(
        "qqbot mcp runtime {label} {} {stamp}",
        std::process::id()
    )));
    fs::create_dir_all(path.join("config")).expect("create config fixture");
    fs::create_dir_all(path.join("data")).expect("create data fixture");
    fs::create_dir_all(path.join("resources")).expect("create resources fixture");
    TempRoot(path)
}

fn seed_memory_db(path: &Path, include_acl_fixture: bool) {
    let conn = Connection::open(path).expect("create ordinary runtime memory database");
    conn.execute_batch(MEMORY_SCHEMA)
        .expect("create memory database schema");
    seed_memory(&conn, "private:9002", "private", "foreignprivatemarker");
    if include_acl_fixture {
        seed_memory(&conn, "group:777", "group", "aclsharemarker");
    }
}

fn seed_memory(conn: &Connection, owner_id: &str, scope: &str, content: &str) {
    conn.execute(
        "INSERT INTO memory(owner_type,owner_id,scope,kind,subject,content,source,importance,created_at) \
         VALUES('session',?1,?2,'fact',NULL,?3,'fixture',0.5,1700000000000)",
        params![owner_id, scope, content],
    )
    .expect("seed isolated memory row");
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO memory_fts(rowid,seg) VALUES(?1,?2)",
        params![id, content],
    )
    .expect("index isolated memory fixture");
}

fn memory_row_count(path: &Path) -> i64 {
    Connection::open(path)
        .expect("open selected DB to verify no mutation")
        .query_row("SELECT COUNT(*) FROM memory", [], |row| row.get(0))
        .expect("count selected memory rows")
}

struct FakeHttpEndpoint {
    address: String,
    requests: Receiver<String>,
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl FakeHttpEndpoint {
    fn start(response_body: &'static str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake loopback endpoint");
        listener
            .set_nonblocking(true)
            .expect("make fake endpoint nonblocking");
        let address = listener.local_addr().expect("read fake endpoint address");
        let (request_tx, requests) = mpsc::channel();
        let (stop, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            while stop_rx.try_recv().is_err() {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_http_request(&mut stream);
                        let _ = request_tx.send(request);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                            response_body.len(),
                            response_body
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address: format!("http://127.0.0.1:{}", address.port()),
            requests,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    fn port(&self) -> u16 {
        self.address
            .rsplit_once(':')
            .expect("fake endpoint URL has port")
            .1
            .parse()
            .expect("fake endpoint port is numeric")
    }

    fn next_request(&self) -> String {
        self.requests
            .recv_timeout(Duration::from_secs(3))
            .expect("fake endpoint should receive the expected request")
    }

    fn assert_no_request(&self) {
        assert!(
            self.requests
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "a denied or unrelated tool call must not reach this endpoint"
        );
    }
}

impl Drop for FakeHttpEndpoint {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set fake endpoint timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let count = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => break,
        };
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn write_fixture(
    root: &Path,
    console_port: u16,
    console_token: &str,
    owner: i64,
    allowed_group: i64,
    access_token: &str,
    onebot_url: &str,
    reader_token: &str,
    group_token: &str,
    db_acl_fixture: bool,
) {
    let config = json!({
        "consoleToken": console_token,
        "consolePort": console_port,
        "ownerQQ": owner,
        "allow": {"groups": [allowed_group], "private": []},
        "sendDelayMs": 10,
        "snowluma": {
            // This deliberately points at a harmless closed port; isolated MCP must use the
            // selected dependencies.json OneBot URL instead.
            "httpUrl": "http://127.0.0.1:1",
            "accessToken": access_token
        }
    });
    fs::write(
        root.join("config/config.json"),
        serde_json::to_vec_pretty(&config).expect("serialize config fixture"),
    )
    .expect("write selected config fixture");

    let state = json!({
        "conversations": {
            "private:9001": {"agentToken": reader_token},
            "private:9002": {"agentToken": "owner-session-token"},
            "group:4242": {"agentToken": group_token}
        }
    });
    fs::write(
        root.join("data/social-v2.json"),
        serde_json::to_vec_pretty(&state).expect("serialize state fixture"),
    )
    .expect("write selected state fixture");

    let dependencies = json!({
        "dsh": "http://127.0.0.1:1",
        "model": "http://127.0.0.1:2",
        "onebot": onebot_url
    });
    fs::write(
        root.join("dependencies.json"),
        serde_json::to_vec_pretty(&dependencies).expect("serialize dependency fixture"),
    )
    .expect("write selected dependency fixture");
    seed_memory_db(&root.join("data/memory.db"), db_acl_fixture);
}

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpProcess {
    fn start(root: &Path, memory_override: &Path, private_home: &Path) -> Self {
        let runtime_config: Value = serde_json::from_slice(
            &fs::read(root.join("config/config.json"))
                .expect("read selected config for CLI fixture"),
        )
        .expect("selected config fixture is JSON");
        let console_port = runtime_config["consolePort"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .expect("selected config fixture has an explicit non-zero console port")
            .to_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .arg("--mcp-serve")
            .arg("--runtime-root")
            .arg(root)
            .arg("--console-port")
            .arg(console_port)
            .env_remove("HER_RUNTIME_ROOT")
            .env("HER_MEMORY_DB", memory_override)
            .env("DSH_HOME", private_home)
            .env("USERPROFILE", private_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        hide_process_window(&mut command);
        let mut child = command.spawn().expect("start isolated MCP process");
        let stdin = child.stdin.take().expect("MCP stdin is piped");
        let stdout = child.stdout.take().expect("MCP stdout is piped");
        let mut process = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let initialized = process.request(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name":"mcp-runtime-test", "version":"1"}
            }
        }));
        assert_eq!(initialized["result"]["serverInfo"]["name"], "snowluma-safe");
        writeln!(
            process.stdin,
            "{}",
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
        )
        .expect("write MCP initialized notification");
        process
            .stdin
            .flush()
            .expect("flush MCP initialized notification");
        process
    }

    fn request(&mut self, request: Value) -> Value {
        writeln!(self.stdin, "{request}").expect("write MCP JSON-RPC request");
        self.stdin.flush().expect("flush MCP JSON-RPC request");
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read MCP JSON-RPC response");
        assert!(
            !line.is_empty(),
            "isolated MCP process exited before replying"
        );
        serde_json::from_str(&line).expect("MCP response is JSON")
    }

    fn tools(&mut self, id: u64) -> Value {
        self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list"
        }))
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
fn hide_process_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_process_window(_command: &mut Command) {}

#[cfg(windows)]
struct TestJunction(PathBuf);

#[cfg(windows)]
impl Drop for TestJunction {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

#[cfg(windows)]
fn create_test_junction(link: &Path, target: &Path) -> Option<TestJunction> {
    let mut command = Command::new("cmd.exe");
    command
        .arg("/d")
        .arg("/c")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target);
    hide_process_window(&mut command);
    let output = command
        .output()
        .expect("run hidden cmd.exe for test junction");
    if !output.status.success() {
        eprintln!("SKIP: Windows did not permit creating the sidecar junction fixture");
        return None;
    }
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(link).expect("inspect sidecar junction fixture");
    assert_ne!(
        metadata.file_attributes() & 0x400,
        0,
        "sidecar fixture must be a reparse point"
    );
    Some(TestJunction(link.to_path_buf()))
}

fn call_args(key: &str, token: &str) -> Value {
    json!({"key": key, "token": token})
}

fn result_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("MCP tool response includes text content")
}

fn is_tool_error(response: &Value) -> bool {
    response["result"]["isError"].as_bool() == Some(true)
        || [
            "写入失败：",
            "检索失败：",
            "删除失败：",
            "权限操作失败：",
            "改配置失败：",
            "读取失败：",
        ]
        .iter()
        .any(|prefix| result_text(response).starts_with(prefix))
}

#[test]
fn isolated_mcp_processes_keep_config_memory_owner_and_acl_state_separate() {
    let root_a = temp_root("alpha");
    let root_b = temp_root("beta");
    let override_root = temp_root("memory override sentinel");
    let private_home = temp_root("personal home sentinel");

    let onebot_a = FakeHttpEndpoint::start(r#"{"source":"onebot-alpha"}"#);
    let onebot_b = FakeHttpEndpoint::start(r#"{"source":"onebot-beta"}"#);
    let console_a = FakeHttpEndpoint::start(r#"{"source":"console-alpha"}"#);
    let console_b = FakeHttpEndpoint::start(r#"{"source":"console-beta"}"#);

    write_fixture(
        &root_a,
        console_a.port(),
        "console-token-alpha",
        9001,
        101,
        "onebot-token-alpha",
        &onebot_a.address,
        "agent-token-alpha",
        "group-token-alpha",
        false,
    );
    write_fixture(
        &root_b,
        console_b.port(),
        "console-token-beta",
        9002,
        202,
        "onebot-token-beta",
        &onebot_b.address,
        "agent-token-beta",
        "group-token-beta",
        true,
    );
    seed_memory_db(&override_root.join("data/memory.db"), false);

    let override_database = override_root.join("data/memory.db");
    let mut a = McpProcess::start(&root_a, &override_database, &private_home);
    let mut b = McpProcess::start(&root_b, &override_database, &private_home);

    for response in [a.tools(1), b.tools(1)] {
        let tools = response["result"]["tools"]
            .as_array()
            .expect("tools/list result");
        assert_eq!(
            tools.len(),
            45,
            "the public MCP tool list remains compatible"
        );
        for expected in [
            "memory_write",
            "memory_acl",
            "settings_set",
            "qq_status",
            "qq_send_group_message",
        ] {
            assert!(
                tools.iter().any(|tool| tool["name"] == expected),
                "the public MCP tool list must retain {expected}"
            );
        }
    }

    let mut args_a = call_args("private:9001", "agent-token-alpha");
    args_a["settingKey"] = json!("sendDelayMs");
    args_a["value"] = json!(111);
    let set_a = a.call(2, "settings_set", args_a);
    assert!(!is_tool_error(&set_a), "A must update its selected config");

    let mut args_b = call_args("private:9002", "owner-session-token");
    args_b["settingKey"] = json!("sendDelayMs");
    args_b["value"] = json!(222);
    let set_b = b.call(2, "settings_set", args_b);
    assert!(!is_tool_error(&set_b), "B must update its selected config");

    let mut get_a = call_args("private:9001", "agent-token-alpha");
    get_a["settingKey"] = json!("sendDelayMs");
    let read_a = result_text(&a.call(3, "settings_get", get_a)).to_string();
    let mut get_b = call_args("private:9002", "owner-session-token");
    get_b["settingKey"] = json!("sendDelayMs");
    let read_b = result_text(&b.call(3, "settings_get", get_b)).to_string();
    assert!(read_a.contains("111") && !read_a.contains("222"));
    assert!(read_b.contains("222") && !read_b.contains("111"));

    let invalid_token = a.call(
        4,
        "settings_get",
        json!({"key":"private:9001", "token":"wrong-token", "settingKey":"sendDelayMs"}),
    );
    assert!(
        is_tool_error(&invalid_token),
        "the selected state's token is required"
    );
    let foreign_root_token = b.call(
        4,
        "settings_get",
        json!({"key":"private:9001", "token":"agent-token-alpha", "settingKey":"sendDelayMs"}),
    );
    assert!(
        is_tool_error(&foreign_root_token),
        "a token from A must not authenticate in B"
    );

    let readonly = a.call(
        14,
        "settings_set",
        json!({
            "key":"private:9001",
            "token":"agent-token-alpha",
            "settingKey":"ownerQQ",
            "value":1234
        }),
    );
    assert!(
        is_tool_error(&readonly),
        "the original readonly settings contract must remain enforced"
    );

    let alpha_write = a.call(
        5,
        "memory_write",
        json!({
            "key":"private:9001",
            "token":"agent-token-alpha",
            "content":"alphaMemoryMarker"
        }),
    );
    assert!(
        !is_tool_error(&alpha_write),
        "A must open and write its ordinary isolated DB"
    );
    let beta_write = b.call(
        5,
        "memory_write",
        json!({
            "key":"private:9001",
            "token":"agent-token-beta",
            "content":"betaMemoryMarker"
        }),
    );
    assert!(
        !is_tool_error(&beta_write),
        "B must open and write its ordinary isolated DB"
    );

    let owner_search = a.call(
        6,
        "memory_search",
        json!({"key":"private:9001", "token":"agent-token-alpha", "query":"foreignprivatemarker"}),
    );
    assert!(result_text(&owner_search).contains("foreignprivatemarker"));
    let non_owner_search = b.call(
        6,
        "memory_search",
        json!({"key":"private:9001", "token":"agent-token-beta", "query":"foreignprivatemarker"}),
    );
    assert!(
        !result_text(&non_owner_search).contains("foreignprivatemarker"),
        "owner status must come from B's selected config"
    );

    let alpha_search = a.call(
        7,
        "memory_search",
        json!({"key":"private:9001", "token":"agent-token-alpha", "query":"alphaMemoryMarker"}),
    );
    assert!(result_text(&alpha_search).contains("alphaMemoryMarker"));
    let alpha_from_b = b.call(
        7,
        "memory_search",
        json!({"key":"private:9001", "token":"agent-token-beta", "query":"alphaMemoryMarker"}),
    );
    assert!(
        !result_text(&alpha_from_b).contains("alphaMemoryMarker"),
        "an isolated process must ignore HER_MEMORY_DB and another root's writes"
    );
    let beta_search = b.call(
        8,
        "memory_search",
        json!({"key":"private:9001", "token":"agent-token-beta", "query":"betaMemoryMarker"}),
    );
    assert!(result_text(&beta_search).contains("betaMemoryMarker"));
    assert_eq!(
        memory_row_count(&override_database),
        1,
        "isolated writes must leave the environment DB untouched"
    );

    let group_key = "group:4242";
    let group_token = "group-token-beta";
    let before_acl = b.call(
        9,
        "memory_search",
        json!({"key":group_key, "token":group_token, "query":"aclsharemarker"}),
    );
    assert!(result_text(&before_acl).contains("没有找到相关记忆"));
    let grant = b.call(
        10,
        "memory_acl",
        json!({
            "key":"private:9002",
            "token":"owner-session-token",
            "readerType":"session",
            "readerId":group_key,
            "action":"grant",
            "readType":"session",
            "readId":"group:777"
        }),
    );
    assert!(
        !is_tool_error(&grant),
        "selected DB must accept the ACL grant"
    );
    let after_grant = b.call(
        11,
        "memory_search",
        json!({"key":group_key, "token":group_token, "query":"aclsharemarker"}),
    );
    assert!(result_text(&after_grant).contains("aclsharemarker"));
    let revoke = b.call(
        12,
        "memory_acl",
        json!({
            "key":"private:9002",
            "token":"owner-session-token",
            "readerType":"session",
            "readerId":group_key,
            "action":"revoke",
            "readType":"session",
            "readId":"group:777"
        }),
    );
    assert!(!is_tool_error(&revoke));
    let after_revoke = b.call(
        13,
        "memory_search",
        json!({"key":group_key, "token":group_token, "query":"aclsharemarker"}),
    );
    assert!(result_text(&after_revoke).contains("没有找到相关记忆"));

    onebot_a.assert_no_request();
    onebot_b.assert_no_request();
    console_a.assert_no_request();
    console_b.assert_no_request();
}

#[test]
fn isolated_mcp_reloads_selected_config_and_fails_closed_when_it_is_missing_or_invalid() {
    let root = temp_root("config reload and failures");
    let override_root = temp_root("config failure override sentinel");
    let private_home = temp_root("config failure personal home");
    let onebot = FakeHttpEndpoint::start(r#"{"source":"onebot"}"#);
    let console = FakeHttpEndpoint::start(r#"{"source":"console"}"#);
    write_fixture(
        &root,
        console.port(),
        "console-token",
        9001,
        101,
        "onebot-token",
        &onebot.address,
        "agent-token",
        "group-token",
        false,
    );
    seed_memory_db(&override_root.join("data/memory.db"), false);

    let selected_config = root.join("config/config.json");
    let selected_db = root.join("data/memory.db");
    let mut mcp = McpProcess::start(&root, &override_root.join("data/memory.db"), &private_home);

    // Each call rereads the chosen file. Replacing it with another valid document is observed.
    let replacement = json!({
        "consoleToken":"console-token",
        "consolePort":console.port(),
        "ownerQQ":9001,
        "allow":{"groups":[101],"private":[]},
        "sendDelayMs":333,
        "snowluma":{"httpUrl":"http://127.0.0.1:1","accessToken":"onebot-token"}
    });
    fs::write(
        &selected_config,
        serde_json::to_vec_pretty(&replacement).expect("serialize replacement config"),
    )
    .expect("replace selected config after MCP startup");
    let mut get = call_args("private:9001", "agent-token");
    get["settingKey"] = json!("sendDelayMs");
    let reloaded = mcp.call(1, "settings_get", get);
    assert!(!is_tool_error(&reloaded));
    assert!(
        result_text(&reloaded).contains("333"),
        "MCP must observe the replaced selected config"
    );

    let initial_rows = memory_row_count(&selected_db);
    fs::remove_file(&selected_config).expect("remove selected config after MCP startup");
    let missing = mcp.call(
        2,
        "memory_write",
        json!({
            "key":"private:9001",
            "token":"agent-token",
            "content":"must-not-write-with-missing-config"
        }),
    );
    assert!(
        is_tool_error(&missing),
        "missing selected config must fail the tool"
    );
    assert_eq!(
        memory_row_count(&selected_db),
        initial_rows,
        "a missing config must not mutate the selected DB"
    );

    fs::write(&selected_config, b"{not valid json").expect("write malformed selected config");
    let invalid = mcp.call(
        3,
        "memory_write",
        json!({
            "key":"private:9001",
            "token":"agent-token",
            "content":"must-not-write-with-invalid-config"
        }),
    );
    assert!(
        is_tool_error(&invalid),
        "invalid selected config must fail the tool"
    );
    assert_eq!(
        memory_row_count(&selected_db),
        initial_rows,
        "an invalid config must not mutate the selected DB"
    );
    assert_eq!(
        memory_row_count(&override_root.join("data/memory.db")),
        1,
        "MCP must not fall back to the environment DB"
    );
    onebot.assert_no_request();
    console.assert_no_request();
}

#[test]
fn isolated_qq_tools_use_selected_fake_endpoints_tokens_and_allow_lists() {
    let root_a = temp_root("qq alpha");
    let root_b = temp_root("qq beta");
    let override_root = temp_root("qq memory override sentinel");
    let private_home = temp_root("qq personal home sentinel");

    let onebot_a =
        FakeHttpEndpoint::start(r#"{"status":"ok","retcode":0,"data":{"source":"onebot-alpha"}}"#);
    let onebot_b =
        FakeHttpEndpoint::start(r#"{"status":"ok","retcode":0,"data":{"source":"onebot-beta"}}"#);
    // Real console send routes explicitly confirm success; source markers prove isolation.
    let console_a = FakeHttpEndpoint::start(r#"{"ok":true,"source":"console-alpha"}"#);
    let console_b = FakeHttpEndpoint::start(r#"{"ok":true,"source":"console-beta"}"#);

    write_fixture(
        &root_a,
        console_a.port(),
        "console-token-alpha",
        9001,
        101,
        "onebot-token-alpha",
        &onebot_a.address,
        "agent-token-alpha",
        "group-token-alpha",
        false,
    );
    write_fixture(
        &root_b,
        console_b.port(),
        "console-token-beta",
        9002,
        202,
        "onebot-token-beta",
        &onebot_b.address,
        "agent-token-beta",
        "group-token-beta",
        false,
    );
    seed_memory_db(&override_root.join("data/memory.db"), false);

    let override_database = override_root.join("data/memory.db");
    let mut a = McpProcess::start(&root_a, &override_database, &private_home);
    let mut b = McpProcess::start(&root_b, &override_database, &private_home);

    for (process, token) in [(&mut a, "agent-token-alpha"), (&mut b, "agent-token-beta")] {
        let status = process.call(1, "qq_status", call_args("private:9001", token));
        assert!(!is_tool_error(&status));
    }
    let onebot_request_a = onebot_a.next_request();
    let onebot_request_b = onebot_b.next_request();
    assert!(onebot_request_a.starts_with("POST /get_login_info HTTP/1.1"));
    assert!(onebot_request_a
        .to_ascii_lowercase()
        .contains("authorization: bearer onebot-token-alpha"));
    assert!(onebot_request_b.starts_with("POST /get_login_info HTTP/1.1"));
    assert!(onebot_request_b
        .to_ascii_lowercase()
        .contains("authorization: bearer onebot-token-beta"));
    // JS `qq_status` also reads `get_status` once `get_login_info` succeeded.
    for (onebot, token) in [
        (&onebot_a, "onebot-token-alpha"),
        (&onebot_b, "onebot-token-beta"),
    ] {
        let status_request = onebot.next_request();
        assert!(status_request.starts_with("POST /get_status HTTP/1.1"));
        assert!(status_request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {token}")));
    }

    let recent_a = a.call(
        2,
        "qq_get_recent_messages",
        json!({"key":"private:9001", "token":"agent-token-alpha", "limit":3}),
    );
    let recent_b = b.call(
        2,
        "qq_get_recent_messages",
        json!({"key":"private:9001", "token":"agent-token-beta", "limit":3}),
    );
    assert!(result_text(&recent_a).contains("console-alpha"));
    assert!(result_text(&recent_b).contains("console-beta"));
    let recent_request_a = console_a.next_request();
    let recent_request_b = console_b.next_request();
    assert!(recent_request_a.starts_with("GET /api/socialV2/recent?"));
    assert!(recent_request_a
        .to_ascii_lowercase()
        .contains("x-agent-token: agent-token-alpha"));
    assert!(recent_request_a
        .to_ascii_lowercase()
        .contains("x-console-token: console-token-alpha"));
    assert!(recent_request_b.starts_with("GET /api/socialV2/recent?"));
    assert!(recent_request_b
        .to_ascii_lowercase()
        .contains("x-agent-token: agent-token-beta"));
    assert!(recent_request_b
        .to_ascii_lowercase()
        .contains("x-console-token: console-token-beta"));

    let send_a = a.call(
        3,
        "qq_send_group_message",
        json!({
            "key":"private:9001",
            "token":"agent-token-alpha",
            "groupId":101,
            "message":"synthetic-alpha"
        }),
    );
    assert!(
        !is_tool_error(&send_a),
        "A's configured group must be allowed"
    );
    let send_b = b.call(
        3,
        "qq_send_group_message",
        json!({
            "key":"private:9001",
            "token":"agent-token-beta",
            "groupId":202,
            "message":"synthetic-beta"
        }),
    );
    assert!(
        !is_tool_error(&send_b),
        "B's configured group must be allowed"
    );
    let request_send_a = console_a.next_request();
    let request_send_b = console_b.next_request();
    assert!(request_send_a.starts_with("POST /api/send/group HTTP/1.1"));
    assert!(request_send_a.contains("synthetic-alpha"));
    assert!(request_send_b.starts_with("POST /api/send/group HTTP/1.1"));
    assert!(request_send_b.contains("synthetic-beta"));

    // Frozen JS `qq_send_group_message` has no allow-list check of its own: it forwards to the
    // bridge's `/api/send/group`, and the console decides (JS `bridge.js` send routes; Rust
    // `qq_tools.rs` `call_tool`, ticket 14 fecd0812 removed the old MCP-side `target_allowed`).
    // The fake console accepts everything, so the isolation property left to check here is that
    // the call reaches A's own console and no other endpoint.
    let forwarded = a.call(
        4,
        "qq_send_group_message",
        json!({
            "key":"private:9001",
            "token":"agent-token-alpha",
            "groupId":202,
            "message":"reaches-only-alpha-console"
        }),
    );
    assert!(
        !is_tool_error(&forwarded),
        "the MCP tool forwards; the console owns the allow list"
    );
    let request_forwarded = console_a.next_request();
    assert!(request_forwarded.starts_with("POST /api/send/group HTTP/1.1"));
    assert!(request_forwarded.contains("reaches-only-alpha-console"));
    assert!(request_forwarded.contains("202"));
    console_a.assert_no_request();
    console_b.assert_no_request();
    onebot_a.assert_no_request();
    onebot_b.assert_no_request();
}

#[test]
fn isolated_qq_send_without_explicit_confirmation_reports_unknown_delivery() {
    let root = temp_root("qq unconfirmed send");
    let override_root = temp_root("qq unconfirmed memory override");
    let private_home = temp_root("qq unconfirmed personal home");
    let onebot = FakeHttpEndpoint::start(r#"{"source":"onebot"}"#);
    // HTTP 200 alone cannot confirm delivery, even with a valid JSON body.
    let console = FakeHttpEndpoint::start(r#"{"source":"unconfirmed-console"}"#);
    write_fixture(
        &root,
        console.port(),
        "console-token",
        9001,
        101,
        "onebot-token",
        &onebot.address,
        "agent-token",
        "group-token",
        false,
    );
    let mut mcp = McpProcess::start(&root, &override_root.join("data/memory.db"), &private_home);
    let response = mcp.call(
        1,
        "qq_send_group_message",
        json!({"key":"private:9001", "token":"agent-token", "groupId":101, "message":"synthetic-unconfirmed"}),
    );
    assert!(is_tool_error(&response));
    assert_eq!(response["result"]["_meta"]["herDelivery"], "unknown");
    assert!(result_text(&response).contains("发送响应无法确认"));
    let request = console.next_request();
    assert!(request.starts_with("POST /api/send/group HTTP/1.1"));
    assert!(request.contains("synthetic-unconfirmed"));
    console.assert_no_request();
    onebot.assert_no_request();
}

#[test]
fn isolated_mcp_rejects_memory_database_sidecar_reparse_points() {
    let root = temp_root("sidecar security");
    let override_root = temp_root("sidecar outside sentinel");
    let private_home = temp_root("sidecar personal home");
    let onebot = FakeHttpEndpoint::start(r#"{"source":"onebot"}"#);
    let console = FakeHttpEndpoint::start(r#"{"source":"console"}"#);
    write_fixture(
        &root,
        console.port(),
        "console-token",
        9001,
        101,
        "onebot-token",
        &onebot.address,
        "agent-token",
        "group-token",
        false,
    );
    seed_memory_db(&override_root.join("data/memory.db"), false);
    let outside_dir = override_root.join("outside-sidecar-directory");
    fs::create_dir(&outside_dir).expect("create outside sidecar directory");
    let outside_sidecar = outside_dir.join("outside-wal-sentinel");
    fs::write(&outside_sidecar, "outside-sidecar-sentinel")
        .expect("write outside sidecar sentinel");

    let mut mcp = McpProcess::start(&root, &override_root.join("data/memory.db"), &private_home);
    let selected_db = root.join("data/memory.db");
    let initial_rows = memory_row_count(&selected_db);
    let mut tested_sidecars = 0;
    for (request_id, suffix) in ["-wal", "-shm", "-journal"].into_iter().enumerate() {
        let link = root.join(format!("data/memory.db{suffix}"));
        #[cfg(windows)]
        let file_link = std::os::windows::fs::symlink_file(&outside_sidecar, &link);
        #[cfg(windows)]
        let junction = if file_link.is_err() {
            create_test_junction(&link, &outside_dir)
        } else {
            None
        };
        #[cfg(windows)]
        let linked = file_link.is_ok() || junction.is_some();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside_sidecar, &link).is_ok();

        if !linked {
            eprintln!("SKIP: platform did not permit creating the {suffix} symlink fixture");
            continue;
        }

        let response = mcp.call(
            request_id as u64 + 1,
            "memory_write",
            json!({
                "key":"private:9001",
                "token":"agent-token",
                "content":format!("must-not-open-sidecar{suffix}")
            }),
        );
        assert!(
            is_tool_error(&response),
            "MCP must fail closed on a linked SQLite {suffix} sidecar"
        );
        // SQLite on Unix refuses to open a database whose sidecar is a symlink, so the checker below
        // must look at the database after the link is gone.
        #[cfg(unix)]
        fs::remove_file(&link).expect("remove sidecar symlink fixture");
        assert_eq!(
            memory_row_count(&selected_db),
            initial_rows,
            "a linked sidecar must not mutate the selected DB"
        );
        assert_eq!(
            fs::read_to_string(&outside_sidecar).expect("read outside sentinel after MCP call"),
            "outside-sidecar-sentinel",
            "a linked sidecar must not mutate the outside sentinel"
        );
        tested_sidecars += 1;

        #[cfg(windows)]
        if file_link.is_ok() {
            fs::remove_file(&link).expect("remove sidecar file symlink fixture");
        } else {
            drop(junction);
        }
    }
    assert_eq!(
        tested_sidecars, 3,
        "all SQLite sidecar reparse negatives must execute"
    );
    onebot.assert_no_request();
    console.assert_no_request();
}
