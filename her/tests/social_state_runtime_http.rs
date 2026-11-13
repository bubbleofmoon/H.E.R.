//! Public HTTP acceptance for selected-root social-state projections.
//!
//! Frozen JavaScript route contracts live in qq-bridge/src/bridge.js:
//! activity (4691), state (4755), unread/recent (4887/4909), states (5115),
//! my-recent (6517), active-members (6696), and pending (4144). Frozen source
//! SHA-256: 52f4c20766ecc0827ee38ed8ab8f8629acb7bd8539cb74bca9a780ea0da7ec80.
//!
//! This test drives two hidden isolated console processes over loopback with
//! synthetic config/state roots. It expects public behavior, not the current
//! 503 route guard. This test checks an empty pending queue; the closed-agent
//! end-to-end tests exercise real question and approval entries.

#[path = "../src/install_root.rs"]
mod install_root;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const KEY: &str = "group:7002";
const DENIED_KEY: &str = "group:599";
const KEY_QUERY: &str = "group%3A7002";
const DENIED_KEY_QUERY: &str = "group%3A599";

struct SyntheticWorkspace(PathBuf);

impl SyntheticWorkspace {
    fn new() -> Self {
        let fixture_base = fixture_base();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let root = fixture_base.join(format!(
            "qqbot-social-state-http-{}-{stamp}",
            std::process::id()
        ));
        let root = crate::install_root::development_root(root);
        fs::create_dir(&root).expect("create unique synthetic workspace");
        let canonical_root = root
            .canonicalize()
            .expect("canonicalize synthetic workspace");
        assert_eq!(
            canonical_root.parent(),
            Some(fixture_base.as_path()),
            "synthetic root must be a direct child of the approved fixture directory"
        );
        assert!(canonical_root.starts_with(&fixture_base));
        Self(canonical_root)
    }

    fn root(&self, name: &str) -> PathBuf {
        crate::install_root::development_root(self.0.join(name))
    }
}

impl Drop for SyntheticWorkspace {
    fn drop(&mut self) {
        if !self.0.exists() {
            return;
        }
        let fixture_base = fixture_base();
        let canonical_root = self
            .0
            .canonicalize()
            .expect("canonicalize synthetic workspace before cleanup");
        let name = canonical_root
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        assert_eq!(
            canonical_root.parent(),
            Some(fixture_base.as_path()),
            "refusing to remove a synthetic root outside its approved fixture directory"
        );
        assert!(
            name.starts_with("qqbot-social-state-http-"),
            "refusing to remove an unrecognized fixture directory"
        );
        fs::remove_dir_all(&canonical_root).expect("remove unique synthetic workspace");
    }
}

fn fixture_base() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("her manifest must be under the repository root")
        .canonicalize()
        .expect("canonicalize repository root");
    let fixture_base = workspace_root
        .join(".scratch")
        .join("js-rust-migration")
        .join("social-state-http-fixtures");
    let fixture_base = crate::install_root::development_root(fixture_base);
    fs::create_dir_all(&fixture_base).expect("create approved synthetic fixture directory");
    let canonical_base = fixture_base
        .canonicalize()
        .expect("canonicalize approved fixture directory");
    assert!(
        canonical_base.starts_with(&workspace_root),
        "approved fixture directory must remain inside the repository worktree"
    );
    canonical_base
}

fn initial_config(marker: &str, console_token: &str) -> Value {
    json!({
        "consoleToken": console_token,
        "ownerQQ": 7001,
        "configMarker": format!("{marker}-config"),
        "allow": {
            "groups": [7002],
            "private": [7001]
        },
        "deny": {
            "groups": [],
            "private": []
        },
        "socialV2": {
            "enabled": true,
            "tools": {}
        }
    })
}

fn recent_messages(marker: &str, base_seq: i64) -> Vec<Value> {
    vec![
        json!({
            "seq": base_seq + 1,
            "sender": format!("{marker}-Alice"),
            "userId": "101",
            "text": format!("{marker}-recent-alice-one"),
            "time": 1000,
            "isSelf": false,
            "isOwner": false
        }),
        json!({
            "seq": base_seq + 2,
            "sender": format!("{marker}-Bob"),
            "userId": "202",
            "text": format!("{marker}-recent-bob"),
            "time": 2000,
            "isSelf": false,
            "isOwner": false
        }),
        json!({
            "seq": base_seq + 3,
            "sender": format!("{marker}-Alice"),
            "userId": "101",
            "text": format!("{marker}-recent-alice-two"),
            "time": 3000,
            "isSelf": false,
            "isOwner": true
        }),
        json!({
            "seq": base_seq + 4,
            "sender": format!("{marker}-Bot"),
            "userId": "7001",
            "text": format!("{marker}-recent-self"),
            "time": 4000,
            "isSelf": true,
            "isOwner": false
        }),
    ]
}

fn conversation(marker: &str, agent_token: &str, unread_base: i64, recent_base: i64) -> Value {
    let unread = (1..=4)
        .map(|offset| {
            json!({
                "seq": unread_base + offset,
                "text": format!("{marker}-unread-{offset}")
            })
        })
        .collect::<Vec<_>>();
    json!({
        "agentToken": agent_token,
        "wakeConfig": {
            "mode": "active",
            "infinite": true,
            "noActionCount": 7,
            "projectionMarker": format!("{marker}-wake"),
            "triggers": { "anyMessage": true }
        },
        "unread": unread,
        "modelSeenSeqs": [999],
        "lastUnreadSeq": unread_base + 4,
        "lastReadThroughSeq": 0,
        "recentMessages": recent_messages(marker, recent_base),
        "lastWakeReason": format!("{marker}-wake-reason"),
        "lastAiReplyAt": unread_base + 500
    })
}

fn prepare_root(
    root: &Path,
    marker: &str,
    console_token: &str,
    agent_token: &str,
    paused: bool,
) -> PathBuf {
    let checked = crate::install_root::development_root(root);
    let root = checked.as_path();
    for relative in ["config", "data", "resources", "user/dsh"] {
        fs::create_dir_all(root.join(relative)).expect("create selected runtime directory");
    }
    write_json_file(
        &root.join("config/config.json"),
        &initial_config(marker, console_token),
    );
    write_json_file(
        &root.join("dependencies.json"),
        &json!({
            "dsh": "http://127.0.0.1:1",
            "model": "http://127.0.0.1:1",
            "onebot": "http://127.0.0.1:1"
        }),
    );
    write_json_file(
        &root.join("data/mode.json"),
        &json!({ "mode": "reserved2" }),
    );
    let mut conversations = serde_json::Map::new();
    let unread_base = if marker == "B" { 100 } else { 0 };
    conversations.insert(
        KEY.to_string(),
        conversation(marker, agent_token, unread_base, unread_base),
    );
    if marker == "A" {
        conversations.insert(
            DENIED_KEY.to_string(),
            conversation("A-disallowed", "agent-A-disallowed-session", 20, 200),
        );
    }
    write_json_file(
        &root.join("data/social-v2.json"),
        &json!({ "paused": paused, "conversations": conversations }),
    );
    let dsh_home = root.join("synthetic-ambient-dsh-home");
    fs::create_dir_all(dsh_home.join("synthetic-user-profile"))
        .expect("create synthetic ambient DSH home");
    fs::write(
        dsh_home.join(".credentials.yaml"),
        "secret: synthetic-not-a-real-credential\n",
    )
    .expect("write synthetic decoy credential");
    dsh_home
}

fn write_json_file(path: &Path, value: &Value) {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).expect("serialize synthetic JSON"),
    )
    .unwrap_or_else(|error| panic!("write synthetic file {}: {error}", path.display()));
}

fn read_json_file(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn reserve_ports() -> (u16, u16) {
    let first = TcpListener::bind(("127.0.0.1", 0)).expect("reserve console A port");
    let second = TcpListener::bind(("127.0.0.1", 0)).expect("reserve console B port");
    let first_port = first.local_addr().expect("read console A port").port();
    let second_port = second.local_addr().expect("read console B port").port();
    assert_ne!(first_port, second_port);
    drop((first, second));
    (first_port, second_port)
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
struct ConsoleProcess {
    child: Child,
    port: u16,
}

impl ConsoleProcess {
    fn start(root: &Path, port: u16, dsh_home: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .arg("--console-serve")
            .arg("--console-port")
            .arg(port.to_string())
            .arg("--runtime-root")
            .arg(root)
            .env_remove("HER_RUNTIME_ROOT")
            .env_remove("APPDATA")
            .env_remove("LOCALAPPDATA")
            .env_remove("HOME")
            .env_remove("HER_FAIL_FIRST")
            .env("DSH_HOME", dsh_home)
            .env("USERPROFILE", dsh_home.join("synthetic-user-profile"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_process_window(&mut command);
        let child = command.spawn().expect("start isolated console");
        let mut process = Self { child, port };
        let health = wait_for_health(&mut process)
            .unwrap_or_else(|error| panic!("isolated console did not start: {error}"));
        assert_eq!(health["ok"], true);
        assert_eq!(health["port"], port);
        assert_eq!(health["runtime"]["mode"], "isolated");
        assert_eq!(health["runtime"]["configLoaded"], true);
        assert_eq!(health["runtime"]["dataConfigured"], true);
        assert_eq!(health["runtime"]["resourcesConfigured"], true);
        process
    }

    fn stop(&mut self) -> Result<(), String> {
        if self
            .child
            .try_wait()
            .map_err(|e| format!("poll console: {e}"))?
            .is_some()
        {
            return Ok(());
        }
        self.child
            .kill()
            .map_err(|e| format!("kill console: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self
                .child
                .try_wait()
                .map_err(|e| format!("wait console: {e}"))?
                .is_some()
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Err("isolated console did not exit within five seconds after kill".to_string())
    }
}

impl Drop for ConsoleProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct HttpResponse {
    status: u16,
    body: String,
}

fn request(
    port: u16,
    path: &str,
    console_token: Option<&str>,
    agent_token: Option<&str>,
) -> Result<HttpResponse, String> {
    let console_header = console_token
        .map(|token| format!("X-Console-Token: {token}\r\n"))
        .unwrap_or_default();
    let agent_header = agent_token
        .map(|token| format!("X-Agent-Token: {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{console_header}{agent_header}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|error| format!("connect isolated console: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("set console read timeout: {error}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write console request: {error}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| format!("read console response: {error}"))?;
    let response =
        String::from_utf8(raw).map_err(|_| "console response is not UTF-8".to_string())?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "console response has no header boundary".to_string())?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "console response status is invalid".to_string())?;
    Ok(HttpResponse {
        status,
        body: body.to_string(),
    })
}

fn wait_for_health(process: &mut ConsoleProcess) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if process
            .child
            .try_wait()
            .map_err(|error| format!("check console process: {error}"))?
            .is_some()
        {
            return Err("isolated console exited before health was ready".to_string());
        }
        if let Ok(response) = request(process.port, "/api/health", None, None) {
            if response.status == 200 {
                return serde_json::from_str(&response.body)
                    .map_err(|error| format!("health response is invalid JSON: {error}"));
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("health endpoint did not become ready within eight seconds".to_string())
}

fn response_json(response: HttpResponse, expected_status: u16, context: &str) -> Value {
    assert_eq!(
        response.status, expected_status,
        "{context}: {}",
        response.body
    );
    serde_json::from_str(&response.body)
        .unwrap_or_else(|error| panic!("{context} response is not JSON: {error}"))
}

fn get_json(
    port: u16,
    path: &str,
    console_token: Option<&str>,
    agent_token: Option<&str>,
    expected_status: u16,
) -> Value {
    let response = request(port, path, console_token, agent_token)
        .unwrap_or_else(|error| panic!("GET {path} failed: {error}"));
    response_json(response, expected_status, &format!("GET {path}"))
}

fn assert_error(response: HttpResponse, status: u16, message: &str, context: &str) {
    assert_eq!(response.status, status, "{context}: {}", response.body);
    let body: Value = serde_json::from_str(&response.body)
        .unwrap_or_else(|error| panic!("{context} response is not JSON: {error}"));
    assert_eq!(
        body,
        json!({ "ok": false, "error": message }),
        "{context} error contract"
    );
}

fn assert_unauthorized(response: HttpResponse, context: &str) {
    assert_error(response, 401, "未授权：请提供控制台访问令牌", context);
}

fn projection<'a>(response: &'a Value, key: &str) -> &'a Value {
    response["conversations"]
        .as_array()
        .unwrap_or_else(|| panic!("{} conversations is not an array", response))
        .iter()
        .find(|item| item["key"].as_str() == Some(key))
        .unwrap_or_else(|| panic!("conversation {key} is missing from {response}"))
}

fn assert_state_summary(value: &Value, marker: &str) {
    assert_eq!(value["ok"], true);
    assert_eq!(value["key"], KEY);
    assert_eq!(
        value["wakeConfig"]["projectionMarker"],
        format!("{marker}-wake")
    );
    assert_eq!(value["unreadCount"], 4);
    assert_eq!(value["recentCount"], 4);
    assert_eq!(value["lastWakeReason"], format!("{marker}-wake-reason"));
    assert_eq!(value["wakeSafety"].is_object(), true);
    assert!(value.get("noActionCount").is_none());
    assert!(value.get("recentMessages").is_none());
    assert!(value.get("unread").is_none());
}
#[test]
fn social_state_routes_use_each_selected_runtime_root() {
    let workspace = SyntheticWorkspace::new();
    let root_a = workspace.root("runtime-a");
    let root_b = workspace.root("runtime-b");
    fs::create_dir_all(&root_a).expect("create runtime A");
    fs::create_dir_all(&root_b).expect("create runtime B");
    let token_a = "social-state-console-token-alpha-6f8a6c";
    let token_b = "social-state-console-token-beta-93b5e1";
    let agent_a = "social-state-agent-token-alpha-a7d390";
    let agent_b = "social-state-agent-token-beta-841f2c";
    let legacy_a = prepare_root(&root_a, "A", token_a, agent_a, false);
    let legacy_b = prepare_root(&root_b, "B", token_b, agent_b, true);
    let (port_a, port_b) = reserve_ports();
    let mut console_a = ConsoleProcess::start(&root_a, port_a, &legacy_a);
    let mut console_b = ConsoleProcess::start(&root_b, port_b, &legacy_b);
    let state_path_a = root_a.join("data/social-v2.json");
    let state_path_b = root_b.join("data/social-v2.json");
    let config_path_a = root_a.join("config/config.json");
    let config_path_b = root_b.join("config/config.json");
    let mode_path_b = root_b.join("data/mode.json");
    let initial_state_a = bytes(&state_path_a);
    let initial_state_b = bytes(&state_path_b);
    let initial_config_a = bytes(&config_path_a);
    let initial_config_b = bytes(&config_path_b);

    let state_a = get_json(
        port_a,
        &format!("/api/socialV2/state?key={KEY_QUERY}"),
        Some(token_a),
        None,
        200,
    );
    let state_b = get_json(
        port_b,
        &format!("/api/socialV2/state?key={KEY_QUERY}"),
        Some(token_b),
        None,
        200,
    );
    assert_state_summary(&state_a, "A");
    assert_state_summary(&state_b, "B");
    assert!(!state_a.to_string().contains("B-wake"));
    assert!(!state_b.to_string().contains("A-wake"));

    let agent_state_a = get_json(
        port_a,
        &format!("/api/socialV2/state?key={KEY_QUERY}"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_state_summary(&agent_state_a, "A");
    assert_eq!(bytes(&state_path_a), initial_state_a);
    assert_eq!(bytes(&state_path_b), initial_state_b);

    let activity_b = get_json(port_b, "/api/socialV2/activity", Some(token_b), None, 200);
    let activity_a = get_json(port_a, "/api/socialV2/activity", Some(token_a), None, 200);
    assert_eq!(activity_a, json!({ "ok": true, "paused": false }));
    assert_eq!(activity_b, json!({ "ok": true, "paused": true }));

    let states_b = get_json(port_b, "/api/socialV2/states", Some(token_b), None, 200);
    let states_a = get_json(port_a, "/api/socialV2/states", Some(token_a), None, 200);
    assert_eq!(states_a["ok"], true);
    assert_eq!(states_b["ok"], true);
    assert_eq!(states_a["conversations"].as_array().unwrap().len(), 2);
    assert_eq!(states_b["conversations"].as_array().unwrap().len(), 1);
    let summary_a = projection(&states_a, KEY);
    let summary_b = projection(&states_b, KEY);
    assert_eq!(summary_a["wakeConfig"]["projectionMarker"], "A-wake");
    assert_eq!(summary_b["wakeConfig"]["projectionMarker"], "B-wake");
    assert_eq!(summary_a["unreadCount"], 4);
    assert_eq!(summary_b["unreadCount"], 4);
    assert_eq!(summary_a["recentCount"], 4);
    assert_eq!(summary_b["recentCount"], 4);
    assert_eq!(summary_a["lastWakeReason"], "A-wake-reason");
    assert_eq!(summary_b["lastWakeReason"], "B-wake-reason");
    assert_eq!(summary_a["noActionCount"], 7);
    assert_eq!(summary_b["noActionCount"], 7);
    assert!(summary_a.get("wakeSafety").is_none());
    assert!(summary_b.get("wakeSafety").is_none());

    let recent_a = get_json(
        port_a,
        &format!("/api/socialV2/recent?key={KEY_QUERY}&limit=2&offset=1"),
        Some(token_a),
        None,
        200,
    );
    let recent_b = get_json(
        port_b,
        &format!("/api/socialV2/recent?key={KEY_QUERY}&limit=2&offset=1"),
        Some(token_b),
        None,
        200,
    );
    assert_eq!(recent_a["key"], KEY);
    assert_eq!(recent_b["key"], KEY);
    assert_eq!(recent_a["messages"][0]["text"], "A-recent-bob");
    assert_eq!(recent_a["messages"][1]["text"], "A-recent-alice-two");
    assert_eq!(recent_b["messages"][0]["text"], "B-recent-bob");
    assert_eq!(recent_b["messages"][1]["text"], "B-recent-alice-two");

    let my_recent_a = get_json(
        port_a,
        &format!("/api/socialV2/my-recent?key={KEY_QUERY}&limit=1"),
        Some(token_a),
        None,
        200,
    );
    let my_recent_b = get_json(
        port_b,
        &format!("/api/socialV2/my-recent?key={KEY_QUERY}&limit=1"),
        Some(token_b),
        None,
        200,
    );
    assert_eq!(my_recent_a["messages"].as_array().unwrap().len(), 1);
    assert_eq!(my_recent_b["messages"].as_array().unwrap().len(), 1);
    assert_eq!(my_recent_a["messages"][0]["text"], "A-recent-self");
    assert_eq!(my_recent_b["messages"][0]["text"], "B-recent-self");

    let active_a = get_json(
        port_a,
        &format!("/api/socialV2/active-members?key={KEY_QUERY}&limit=2"),
        Some(token_a),
        None,
        200,
    );
    let active_b = get_json(
        port_b,
        &format!("/api/socialV2/active-members?key={KEY_QUERY}&limit=2"),
        Some(token_b),
        None,
        200,
    );
    for (active, marker) in [(&active_a, "A"), (&active_b, "B")] {
        assert_eq!(active["ok"], true);
        assert_eq!(active["key"], KEY);
        assert_eq!(active["members"].as_array().unwrap().len(), 2);
        assert_eq!(active["members"][0]["sender"], format!("{marker}-Alice"));
        assert_eq!(active["members"][0]["userId"], "101");
        assert_eq!(active["members"][0]["count"], 2);
        assert_eq!(active["members"][0]["lastTime"], 3000);
        assert_eq!(active["members"][0]["isOwner"], true);
        assert_eq!(active["members"][1]["sender"], format!("{marker}-Bob"));
        assert_eq!(active["members"][1]["userId"], "202");
        assert_eq!(active["members"][1]["count"], 1);
    }

    // No live bridge has created a request in either selected runtime.
    for (port, token) in [(port_a, token_a), (port_b, token_b)] {
        let pending = get_json(port, "/api/pending", Some(token), None, 200);
        assert_eq!(pending, json!({"pending":[]}));
    }

    let console_unread_a = get_json(
        port_a,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&limit=2"),
        Some(token_a),
        None,
        200,
    );
    let console_unread_b = get_json(
        port_b,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&limit=1"),
        Some(token_b),
        None,
        200,
    );
    assert_eq!(console_unread_a["unreadCount"], 4);
    assert_eq!(console_unread_a["messages"][0]["seq"], 3);
    assert_eq!(console_unread_a["messages"][1]["seq"], 4);
    assert_eq!(console_unread_b["unreadCount"], 4);
    assert_eq!(console_unread_b["messages"][0]["seq"], 104);
    assert_eq!(bytes(&state_path_a), initial_state_a);
    assert_eq!(bytes(&state_path_b), initial_state_b);
    assert_eq!(bytes(&config_path_a), initial_config_a);
    assert_eq!(bytes(&config_path_b), initial_config_b);

    let missing_console = request(
        port_a,
        &format!("/api/socialV2/state?key={KEY_QUERY}"),
        None,
        Some(agent_a),
    )
    .expect("missing-console request");
    assert_unauthorized(missing_console, "missing console credential");
    let foreign_console = request(
        port_a,
        &format!("/api/socialV2/state?key={KEY_QUERY}"),
        Some(token_b),
        None,
    )
    .expect("foreign-console request");
    assert_unauthorized(foreign_console, "B's console token must not authorize A");
    let wrong_console = request(port_b, "/api/socialV2/activity", Some(token_a), None)
        .expect("wrong-console request");
    assert_unauthorized(wrong_console, "A's console token must not authorize B");
    assert_eq!(
        bytes(&state_path_a),
        initial_state_a,
        "bad console credentials must not write A"
    );
    assert_eq!(
        bytes(&state_path_b),
        initial_state_b,
        "bad console credentials must not write B"
    );
    assert_eq!(
        bytes(&config_path_a),
        initial_config_a,
        "bad console credentials must not write A config"
    );
    assert_eq!(
        bytes(&config_path_b),
        initial_config_b,
        "bad console credentials must not write B config"
    );

    let wrong_console_disallowed_key = request(
        port_a,
        &format!("/api/socialV2/unread?key={DENIED_KEY_QUERY}&limit=1"),
        Some(token_b),
        Some("agent-A-disallowed-session"),
    )
    .expect("wrong-console request with agent token and disallowed key");
    assert_unauthorized(
        wrong_console_disallowed_key,
        "console authentication must precede agent/session authorization",
    );
    assert_eq!(bytes(&state_path_a), initial_state_a);
    assert_eq!(bytes(&state_path_b), initial_state_b);
    assert_eq!(bytes(&config_path_a), initial_config_a);
    assert_eq!(bytes(&config_path_b), initial_config_b);

    let agent_paths = [
        format!("/api/socialV2/state?key={KEY_QUERY}"),
        format!("/api/socialV2/unread?key={KEY_QUERY}&limit=2"),
        format!("/api/socialV2/recent?key={KEY_QUERY}&limit=2"),
        format!("/api/socialV2/my-recent?key={KEY_QUERY}&limit=1"),
        format!("/api/socialV2/active-members?key={KEY_QUERY}&limit=2"),
    ];
    for path in &agent_paths {
        let before_a = bytes(&state_path_a);
        let before_b = bytes(&state_path_b);
        let foreign_agent = request(port_a, path, Some(token_a), Some(agent_b))
            .unwrap_or_else(|error| panic!("foreign-agent request for {path}: {error}"));
        assert_eq!(
            bytes(&state_path_a),
            before_a,
            "foreign agent must not mutate A for {path}"
        );
        assert_eq!(
            bytes(&state_path_b),
            before_b,
            "foreign agent request to A must not mutate B for {path}"
        );
        assert_error(foreign_agent, 403, "agent token 无效", path);

        let empty_agent = request(port_a, path, Some(token_a), Some(""))
            .unwrap_or_else(|error| panic!("empty-agent request for {path}: {error}"));
        assert_eq!(
            bytes(&state_path_a),
            before_a,
            "empty agent must not mutate A"
        );
        assert_eq!(
            bytes(&state_path_b),
            before_b,
            "empty agent must not mutate B"
        );
        assert_error(empty_agent, 403, "agent token 不能为空", path);
    }

    for cursor in ["invalid", "-1", "9007199254740992"] {
        let before_a = bytes(&state_path_a);
        let before_b = bytes(&state_path_b);
        let response = request(
            port_a,
            &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq={cursor}&limit=2"),
            Some(token_a),
            None,
        )
        .unwrap_or_else(|error| panic!("invalid unread cursor {cursor}: {error}"));
        assert_eq!(
            bytes(&state_path_a),
            before_a,
            "bad cursor must not write A"
        );
        assert_eq!(
            bytes(&state_path_b),
            before_b,
            "bad cursor must not write B"
        );
        assert_error(
            response,
            400,
            "afterSeq 必须是非负安全整数；从最早未读开始请传 0",
            &format!("invalid cursor {cursor}"),
        );
    }

    let recent_agent_a = get_json(
        port_a,
        &format!("/api/socialV2/recent?key={KEY_QUERY}&limit=2&offset=0"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(recent_agent_a["key"], KEY);
    assert_eq!(recent_agent_a["messages"].as_array().unwrap().len(), 2);
    assert_eq!(recent_agent_a["messages"][0]["seq"], 3);
    assert_eq!(recent_agent_a["messages"][1]["seq"], 4);
    assert_eq!(recent_agent_a["messages"][0]["text"], "A-recent-alice-two");
    assert_eq!(recent_agent_a["messages"][1]["text"], "A-recent-self");
    assert_eq!(recent_agent_a["readThroughSeq"], 0);
    let state_after_recent_agent = bytes(&state_path_a);
    assert_eq!(
        state_after_recent_agent, initial_state_a,
        "agent reads must not persist process-local receipts"
    );
    assert_eq!(bytes(&state_path_b), initial_state_b);
    assert_eq!(bytes(&config_path_a), initial_config_a);
    assert_eq!(bytes(&config_path_b), initial_config_b);

    let my_recent_agent_a = get_json(
        port_a,
        &format!("/api/socialV2/my-recent?key={KEY_QUERY}&limit=1"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(my_recent_agent_a["key"], KEY);
    assert_eq!(my_recent_agent_a["messages"].as_array().unwrap().len(), 1);
    assert_eq!(my_recent_agent_a["messages"][0]["seq"], 4);
    assert_eq!(my_recent_agent_a["messages"][0]["text"], "A-recent-self");
    assert_eq!(my_recent_agent_a["readThroughSeq"], 0);

    let active_agent_a = get_json(
        port_a,
        &format!("/api/socialV2/active-members?key={KEY_QUERY}&limit=2"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(active_agent_a["ok"], true);
    assert_eq!(active_agent_a["key"], KEY);
    assert_eq!(active_agent_a["members"], active_a["members"]);
    assert_eq!(bytes(&state_path_a), state_after_recent_agent);
    assert_eq!(bytes(&state_path_b), initial_state_b);
    assert_eq!(bytes(&config_path_a), initial_config_a);
    assert_eq!(bytes(&config_path_b), initial_config_b);
    let first_after_recent = get_json(
        port_a,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=2"),
        Some(token_a), Some(agent_a), 200,
    );
    assert_eq!(first_after_recent["messages"][0]["seq"], 1);
    assert_eq!(first_after_recent["messages"][1]["seq"], 2);
    assert_eq!(first_after_recent["readThroughSeq"], 4,
        "the earlier recent read must have receipted overlapping unread 3 and 4");
    assert_eq!(bytes(&state_path_a), initial_state_a);
    console_a.stop().expect("stop A to discard process-local receipts");
    console_a = ConsoleProcess::start(&root_a, port_a, &legacy_a);
    let after_restart = get_json(
        port_a,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=4&limit=1"),
        Some(token_a), Some(agent_a), 200,
    );
    assert_eq!(after_restart["messages"], json!([]));
    assert_eq!(after_restart["readThroughSeq"], 0,
        "the new console must ignore both old process receipts and legacy disk receipts");

    let page_one = get_json(
        port_a,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=2"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(page_one["messages"].as_array().unwrap().len(), 2);
    assert_eq!(page_one["messages"][0]["seq"], 1);
    assert_eq!(page_one["messages"][1]["seq"], 2);
    assert_eq!(page_one["readThroughSeq"], 2);
    let page_two = get_json(
        port_a,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=2&limit=2"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(page_two["messages"].as_array().unwrap().len(), 2);
    assert_eq!(page_two["messages"][0]["seq"], 3);
    assert_eq!(page_two["messages"][1]["seq"], 4);
    assert_eq!(page_two["readThroughSeq"], 4);
    assert_eq!(
        bytes(&state_path_a), initial_state_a,
        "unread paging must not persist the model's receipts"
    );
    assert_eq!(bytes(&state_path_b), initial_state_b);

    let before_disallowed_a = bytes(&state_path_a);
    let before_disallowed_b = bytes(&state_path_b);
    let disallowed_agent = request(
        port_a,
        &format!("/api/socialV2/unread?key={DENIED_KEY_QUERY}&afterSeq=0&limit=1"),
        Some(token_a),
        Some("agent-A-disallowed-session"),
    )
    .expect("disallowed-session request");
    assert_eq!(bytes(&state_path_a), before_disallowed_a);
    assert_eq!(bytes(&state_path_b), before_disallowed_b);
    assert_error(
        disallowed_agent,
        403,
        "目标不在当前模式允许范围内",
        "agent session outside selected allow list",
    );

    for path in [
        "/api/socialV2/states",
        "/api/socialV2/activity",
    ] {
        let before_a = bytes(&state_path_a);
        let before_b = bytes(&state_path_b);
        let agent_response = request(port_a, path, Some(token_a), Some(agent_a))
            .unwrap_or_else(|error| panic!("agent management request {path}: {error}"));
        assert_eq!(
            bytes(&state_path_a),
            before_a,
            "console-only {path} must not write A"
        );
        assert_eq!(
            bytes(&state_path_b),
            before_b,
            "console-only {path} must not write B"
        );
        assert_error(agent_response, 403, "该接口仅控制台可用", path);
    }

    let before_paused_a = bytes(&state_path_a);
    let before_paused_b = bytes(&state_path_b);
    let paused_agent = request(
        port_b,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=1"),
        Some(token_b),
        Some(agent_b),
    )
    .expect("paused selected agent request");
    assert_eq!(bytes(&state_path_a), before_paused_a);
    assert_eq!(bytes(&state_path_b), before_paused_b);
    assert_error(
        paused_agent,
        403,
        "AI 已暂停，当前不允许执行 v2 工具",
        "agent request in paused B",
    );

    console_b
        .stop()
        .expect("stop B before disabled-config case");
    let config_b_path = root_b.join("config/config.json");
    let mut disabled_config_b = read_json_file(&config_b_path);
    disabled_config_b["socialV2"]["enabled"] = json!(false);
    write_json_file(&config_b_path, &disabled_config_b);
    let mut state_b_value = read_json_file(&state_path_b);
    state_b_value["paused"] = json!(false);
    write_json_file(&state_path_b, &state_b_value);
    let legacy_b = root_b.join("synthetic-ambient-dsh-home");
    console_b = ConsoleProcess::start(&root_b, port_b, &legacy_b);
    let disabled_b_state = bytes(&state_path_b);
    let disabled_b_config = bytes(&config_b_path);
    let disabled_agent = request(
        port_b,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=1"),
        Some(token_b),
        Some(agent_b),
    )
    .expect("disabled selected agent request");
    assert_eq!(bytes(&state_path_a), before_disallowed_a);
    assert_eq!(bytes(&state_path_b), disabled_b_state);
    assert_eq!(bytes(&config_b_path), disabled_b_config);
    assert_error(
        disabled_agent,
        403,
        "二代模式已关闭，当前不允许执行 v2 工具",
        "agent request when selected socialV2 is disabled",
    );

    console_b.stop().expect("stop B before mode case");
    let mut enabled_config_b = read_json_file(&config_b_path);
    enabled_config_b["socialV2"]["enabled"] = json!(true);
    enabled_config_b["socialV2"]["tools"] = json!({
        "socialState": false,
        "getUnread": false,
        "getRecent": false,
        "getMyRecent": false,
        "getActiveMembers": false
    });
    write_json_file(&config_b_path, &enabled_config_b);
    console_b = ConsoleProcess::start(&root_b, port_b, &legacy_b);
    let tool_disabled_state_a = bytes(&state_path_a);
    let tool_disabled_state_b = bytes(&state_path_b);
    let tool_disabled_config_a = bytes(&config_path_a);
    let tool_disabled_config_b = bytes(&config_b_path);
    for (path, expected_error) in [
        (
            format!("/api/socialV2/state?key={KEY_QUERY}"),
            "工具未启用：qq_social_state",
        ),
        (
            format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=1"),
            "工具未启用：qq_get_unread_messages",
        ),
        (
            format!("/api/socialV2/recent?key={KEY_QUERY}&limit=1&offset=0"),
            "工具未启用：qq_get_recent_messages",
        ),
        (
            format!("/api/socialV2/my-recent?key={KEY_QUERY}&limit=1"),
            "工具未启用：qq_get_my_recent_messages",
        ),
        (
            format!("/api/socialV2/active-members?key={KEY_QUERY}&limit=1"),
            "工具未启用：qq_get_active_members",
        ),
    ] {
        let disabled_tool = request(port_b, &path, Some(token_b), Some(agent_b))
            .unwrap_or_else(|error| panic!("disabled tool request {path}: {error}"));
        assert_eq!(bytes(&state_path_a), tool_disabled_state_a);
        assert_eq!(bytes(&state_path_b), tool_disabled_state_b);
        assert_eq!(bytes(&config_path_a), tool_disabled_config_a);
        assert_eq!(bytes(&config_path_b), tool_disabled_config_b);
        assert_error(disabled_tool, 403, expected_error, &path);
    }

    console_b.stop().expect("stop B before reserved2 mode case");
    let mut enabled_config_b = read_json_file(&config_b_path);
    enabled_config_b["socialV2"]["enabled"] = json!(true);
    enabled_config_b["socialV2"]["tools"] = json!({});
    write_json_file(&config_b_path, &enabled_config_b);
    let mode_b_before = bytes(&mode_path_b);
    write_json_file(&mode_path_b, &json!({ "mode": "chat" }));
    console_b = ConsoleProcess::start(&root_b, port_b, &legacy_b);
    let mode_b_state = bytes(&state_path_b);
    let mode_agent = request(
        port_b,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=1"),
        Some(token_b),
        Some(agent_b),
    )
    .expect("selected chat-mode agent request");
    assert_eq!(bytes(&state_path_a), before_disallowed_a);
    assert_eq!(bytes(&state_path_b), mode_b_state);
    assert_error(
        mode_agent,
        403,
        "该接口仅 reserved2 模式可用",
        "agent request outside reserved2 mode",
    );
    fs::write(&mode_path_b, mode_b_before).expect("restore B selected mode fixture");

    let good_state_a = bytes(&state_path_a);
    for (bad_bytes, expected_error) in [
        (
            b"{malformed selected state".as_slice(),
            "social-state/read: social-state/parse kind=InvalidData line=1 column=2",
        ),
        (b"null".as_slice(), "runtime data is invalid"),
    ] {
        fs::write(&state_path_a, bad_bytes).expect("write invalid selected A state");
        let bad_snapshot = bytes(&state_path_a);
        let state_error = request(
            port_a,
            &format!("/api/socialV2/state?key={KEY_QUERY}"),
            Some(token_a),
            None,
        )
        .expect("read invalid selected social state");
        assert_eq!(
            bytes(&state_path_a),
            bad_snapshot,
            "invalid state read must not rewrite A"
        );
        assert_eq!(
            bytes(&state_path_b),
            mode_b_state,
            "invalid A must not touch B"
        );
        assert_error(
            state_error,
            500,
            expected_error,
            "invalid selected social state projection",
        );

        let unread_error = request(
            port_a,
            &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=1"),
            Some(token_a),
            Some(agent_a),
        )
        .expect("read unread from invalid selected state");
        assert_eq!(
            bytes(&state_path_a),
            bad_snapshot,
            "invalid unread read must not rewrite A"
        );
        assert_eq!(
            bytes(&state_path_b),
            mode_b_state,
            "invalid unread A must not touch B"
        );
        assert_error(
            unread_error,
            500,
            expected_error,
            "invalid selected unread projection",
        );
    }
    fs::write(&state_path_a, &good_state_a).expect("restore good selected A state");
    fs::remove_file(&state_path_a).expect("remove selected A state");
    let missing_state = request(
        port_a,
        &format!("/api/socialV2/state?key={KEY_QUERY}"),
        Some(token_a),
        None,
    )
    .expect("read missing selected state");
    assert!(
        !state_path_a.exists(),
        "missing selected state must stay missing"
    );
    assert_eq!(bytes(&state_path_b), mode_b_state);
    assert_error(
        missing_state,
        503,
        "runtime path is unavailable",
        "missing selected social state preflight",
    );

    console_a.stop().expect("stop isolated console A");
    console_b.stop().expect("stop isolated console B");
}

#[test]
fn social_state_key_and_integral_float_boundaries_match_frozen_contract() {
    const LEADING_ZERO_KEY: &str = "group:007002";
    const LEADING_ZERO_QUERY: &str = "group%3A007002";
    const UNSAFE_KEY: &str = "group:9007199254740992";
    const UNSAFE_KEY_QUERY: &str = "group%3A9007199254740992";

    let workspace = SyntheticWorkspace::new();
    let root_a = workspace.root("runtime-a");
    let root_b = workspace.root("runtime-b");
    fs::create_dir_all(&root_a).expect("create runtime A");
    fs::create_dir_all(&root_b).expect("create runtime B");
    let token_a = "social-boundary-console-token-alpha-88dba4";
    let token_b = "social-boundary-console-token-beta-582f16";
    let agent_a = "social-boundary-agent-token-alpha-1d867c";
    let agent_b = "social-boundary-agent-token-beta-4a90f5";
    let legacy_a = prepare_root(&root_a, "A", token_a, agent_a, false);
    let legacy_b = prepare_root(&root_b, "B", token_b, agent_b, false);

    // An empty allow-list with allowAllWhenEmpty is deliberate here: frozen JS
    // accepts this session policy, then rejects an unsafe numeric key in
    // getSocialV2State. Seed its exact raw key/token so the test proves the key
    // validator cannot be bypassed by either policy or token lookup.
    let config_path_a = root_a.join("config/config.json");
    let mut config_a = read_json_file(&config_path_a);
    config_a["allow"]["groups"] = json!([]);
    config_a["allow"]["private"] = json!([]);
    config_a["allowAllWhenEmpty"] = json!(true);
    write_json_file(&config_path_a, &config_a);
    let state_path_a = root_a.join("data/social-v2.json");
    let mut state_a = read_json_file(&state_path_a);
    state_a["conversations"][UNSAFE_KEY] = conversation("A-unsafe", agent_a, 40, 400);
    write_json_file(&state_path_a, &state_a);

    let state_path_b = root_b.join("data/social-v2.json");
    let config_path_b = root_b.join("config/config.json");
    let (port_a, port_b) = reserve_ports();
    let mut console_a = ConsoleProcess::start(&root_a, port_a, &legacy_a);
    let mut console_b = ConsoleProcess::start(&root_b, port_b, &legacy_b);
    let initial_state_a = bytes(&state_path_a);
    let initial_state_b = bytes(&state_path_b);
    let initial_config_a = bytes(&config_path_a);
    let initial_config_b = bytes(&config_path_b);

    let leading_zero = get_json(
        port_a,
        &format!("/api/socialV2/state?key={LEADING_ZERO_QUERY}"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(
        leading_zero["key"], LEADING_ZERO_KEY,
        "JS echoes the raw query key"
    );
    assert_eq!(
        leading_zero["wakeConfig"]["projectionMarker"], "A-wake",
        "leading zeroes must resolve to the canonical selected conversation"
    );
    assert_eq!(leading_zero["unreadCount"], 4);
    assert_eq!(
        bytes(&state_path_a),
        initial_state_a,
        "read must not create an alias"
    );
    assert_eq!(
        bytes(&state_path_b),
        initial_state_b,
        "A request must not touch B"
    );
    assert_eq!(bytes(&config_path_a), initial_config_a);
    assert_eq!(bytes(&config_path_b), initial_config_b);
    let state_after_alias = read_json_file(&state_path_a);
    assert!(state_after_alias["conversations"].get(KEY).is_some());
    assert!(state_after_alias["conversations"]
        .get(LEADING_ZERO_KEY)
        .is_none());

    let unsafe_key = request(
        port_a,
        &format!("/api/socialV2/state?key={UNSAFE_KEY_QUERY}"),
        Some(token_a),
        Some(agent_a),
    )
    .expect("seeded unsafe-key request");
    assert_eq!(bytes(&state_path_a), initial_state_a);
    assert_eq!(bytes(&state_path_b), initial_state_b);
    assert_eq!(bytes(&config_path_a), initial_config_a);
    assert_eq!(bytes(&config_path_b), initial_config_b);
    assert_error(
        unsafe_key,
        400,
        &format!("无效的会话 key：{UNSAFE_KEY}"),
        "allowAllWhenEmpty and an exact seeded token do not validate an unsafe key",
    );

    // Stop before replacing the selected state so this public request starts
    // from the exact lexical JSON bytes below, even if a future runner caches
    // any read state within the process.
    console_a.stop().expect("stop A before raw-float fixture");
    let mut float_conversations = serde_json::Map::new();
    float_conversations.insert(
        KEY.to_string(),
        json!({
            "agentToken": agent_a,
            "wakeConfig": { "mode": "active", "infinite": true, "triggers": { "anyMessage": true } },
            "unread": [{ "seq": 1, "text": "A-float-unread" }],
            "modelSeenSeqs": [],
            "lastUnreadSeq": 1,
            "lastReadThroughSeq": 0,
            "recentMessages": [{
                "seq": 1,
                "sender": "A-float-sender",
                "userId": "101",
                "text": "A-float-recent",
                "time": 1234,
                "isSelf": false,
                "isOwner": false
            }]
        }),
    );
    let float_state = json!({
        "paused": false,
        "conversations": Value::Object(float_conversations)
    });
    let serialized = serde_json::to_string_pretty(&float_state).expect("serialize float fixture");
    let raw_float_state = serialized.replace("\"seq\": 1,", "\"seq\": 1.0,");
    assert_eq!(
        raw_float_state.matches("\"seq\": 1.0,").count(),
        2,
        "unread and recent seq values must be raw JSON floats"
    );
    fs::write(&state_path_a, raw_float_state.as_bytes()).expect("write raw float state");

    console_a = ConsoleProcess::start(&root_a, port_a, &legacy_a);
    assert_eq!(bytes(&state_path_a), raw_float_state.as_bytes().to_vec());
    let before_float_a = bytes(&state_path_a);
    let before_float_b = bytes(&state_path_b);
    let before_float_config_a = bytes(&config_path_a);
    let before_float_config_b = bytes(&config_path_b);

    let unread = get_json(
        port_a,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=0&limit=1"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(unread["key"], KEY);
    assert_eq!(unread["messages"].as_array().unwrap().len(), 1);
    assert_eq!(unread["messages"][0]["seq"].as_f64(), Some(1.0));
    assert_eq!(unread["messages"][0]["text"], "A-float-unread");
    assert_eq!(unread["readThroughSeq"], 1);
    assert_eq!(
        read_json_file(&state_path_a)["conversations"][KEY]["unread"][0]["seq"].as_f64(),
        Some(1.0)
    );
    assert_eq!(bytes(&state_path_b), before_float_b);
    assert_eq!(bytes(&config_path_a), before_float_config_a);
    assert_eq!(bytes(&config_path_b), before_float_config_b);
    assert_eq!(
        bytes(&state_path_a),
        before_float_a,
        "agent unread must keep its receipt in process memory"
    );

    // Reset to the same raw float bytes so recent is tested independently of
    // the receipt held by the previous console process.
    console_a
        .stop()
        .expect("stop A before independent recent case");
    fs::write(&state_path_a, raw_float_state.as_bytes()).expect("restore raw float state");
    console_a = ConsoleProcess::start(&root_a, port_a, &legacy_a);
    assert_eq!(bytes(&state_path_a), raw_float_state.as_bytes().to_vec());
    let float_after_restart = get_json(
        port_a,
        &format!("/api/socialV2/unread?key={KEY_QUERY}&afterSeq=1&limit=1"),
        Some(token_a), Some(agent_a), 200,
    );
    assert_eq!(float_after_restart["messages"], json!([]));
    assert_eq!(float_after_restart["readThroughSeq"], 0,
        "the float receipt must not survive the console restart");
    let recent = get_json(
        port_a,
        &format!("/api/socialV2/recent?key={KEY_QUERY}&limit=1&offset=0"),
        Some(token_a),
        Some(agent_a),
        200,
    );
    assert_eq!(recent["key"], KEY);
    assert_eq!(recent["messages"].as_array().unwrap().len(), 1);
    assert_eq!(recent["messages"][0]["seq"].as_f64(), Some(1.0));
    assert_eq!(recent["messages"][0]["text"], "A-float-recent");
    assert_eq!(recent["readThroughSeq"], 1);
    assert_eq!(
        read_json_file(&state_path_a)["conversations"][KEY]["unread"][0]["seq"].as_f64(),
        Some(1.0)
    );
    assert_eq!(bytes(&state_path_a), raw_float_state.as_bytes());
    assert_eq!(bytes(&state_path_b), before_float_b);
    assert_eq!(bytes(&config_path_a), before_float_config_a);
    assert_eq!(bytes(&config_path_b), before_float_config_b);

    console_a.stop().expect("stop isolated console A");
    console_b.stop().expect("stop isolated console B");
}
