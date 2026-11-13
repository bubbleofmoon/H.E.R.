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

const SESSION_KEY: &str = "private:7001";
const OWNER_ID: i64 = 7001;
const ACTIVE_MEMORY_PATH: &str =
    "/api/socialV2/memory?key=private%3A7001&category=activeTopic";
const FULL_MEMORY_PATH: &str = "/api/socialV2/memory?key=private%3A7001";

struct FixtureWorkspace(PathBuf);

impl FixtureWorkspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let root =
            crate::install_root::development_root(std::env::temp_dir().join(format!("qqbot-social-http-{}-{stamp}", std::process::id())));
        fs::create_dir_all(&root).expect("create synthetic social HTTP workspace");
        Self(root)
    }

    fn root(&self, name: &str) -> PathBuf {
        crate::install_root::development_root(self.0.join(name))
    }
}

impl Drop for FixtureWorkspace {
    fn drop(&mut self) {
        let temp = std::env::temp_dir();
        if self.0.starts_with(&temp) {
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

fn prepare_runtime_root(
    root: &Path,
    console_token: &str,
    agent_token: &str,
    marker: &str,
    initial_sleep_min_ms: i64,
) -> PathBuf {
    let checked = crate::install_root::development_root(root);
    let root = checked.as_path();
    for relative in ["config", "data", "resources/roles", "user/dsh"] {
        fs::create_dir_all(root.join(relative)).expect("create selected runtime directory");
    }

    let social_config = json!({
        "enabled": true,
        "customRootMarker": format!("{marker}-config"),
        "tools": {
            "memory": true,
            "sendMessage": false
        },
        "wake": {
            "sleepMinMs": initial_sleep_min_ms,
            "recommendedProbability": 0.25,
            "recommendedKeywords": [format!("{marker}-seed-keyword")]
        },
        "send": { "maxMessageChars": 800 }
    });
    let config = json!({
        "consoleToken": console_token,
        "ownerQQ": OWNER_ID,
        "allow": { "groups": [], "private": [OWNER_ID.to_string()] },
        "configMarker": format!("{marker}-top-level-config"),
        "socialV2": social_config,
        "dsh": {
            "baseUrl": "http://127.0.0.1:1",
            "provider": "fixture",
            "model": "fixture-model",
            "reasoningEffort": "high"
        }
    });
    let dependencies = json!({
        "dsh": "http://127.0.0.1:1",
        "model": "http://127.0.0.1:1",
        "onebot": "http://127.0.0.1:1"
    });
    fs::write(
        root.join("config/config.json"),
        serde_json::to_vec_pretty(&config).expect("serialize selected config fixture"),
    )
    .expect("write selected config fixture");
    fs::write(
        root.join("dependencies.json"),
        serde_json::to_vec(&dependencies).expect("serialize synthetic dependency map"),
    )
    .expect("write synthetic dependency map");

    let now = now_ms();
    let session_state = json!({
        "conversations": {
            "private:7001": {
                "agentToken": agent_token,
                "activeTopics": [{
                    "text": format!("{marker}-seed-topic"),
                    "lastMentionAt": now,
                    "participants": [],
                    "pendingQuestion": ""
                }],
                "pendingThoughts": [{
                    "text": format!("{marker}-pending-thought"),
                    "createdAt": now,
                    "expiresAt": now + 3_600_000,
                    "motivation": "synthetic fixture"
                }],
                "memberImpressions": {
                    (format!("{marker}-friend")): {
                        "traits": [format!("{marker}-trait")],
                        "interactionCount": 1,
                        "lastSeenAt": now
                    }
                }
            }
        }
    });
    fs::write(
        root.join("data/social-v2.json"),
        serde_json::to_vec(&session_state).expect("serialize selected social state"),
    )
    .expect("write selected social state");
    fs::write(root.join("data/mode.json"), br#"{"mode":"reserved2"}"#)
        .expect("write selected reserved2 mode");

    let dsh_home = root.join("synthetic-ambient-dsh-home");
    fs::create_dir_all(&dsh_home).expect("create synthetic ambient DSH home");
    fs::create_dir_all(dsh_home.join("synthetic-user-profile"))
        .expect("create synthetic user profile");
    dsh_home
}

fn reserve_distinct_ports() -> (u16, u16) {
    let first = TcpListener::bind(("127.0.0.1", 0)).expect("reserve first console port");
    let second = TcpListener::bind(("127.0.0.1", 0)).expect("reserve second console port");
    let first_port = first.local_addr().expect("read first console port").port();
    let second_port = second
        .local_addr()
        .expect("read second console port")
        .port();
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
            .env("DSH_HOME", dsh_home)
            .env("USERPROFILE", dsh_home.join("synthetic-user-profile"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_process_window(&mut command);
        let child = command
            .spawn()
            .expect("start selected-root console process");
        let mut process = Self { child, port };
        let health = wait_for_health(&mut process)
            .unwrap_or_else(|error| panic!("selected-root console did not start: {error}"));
        assert_eq!(health["ok"], true);
        assert_eq!(health["port"], port);
        assert_eq!(health["runtime"]["mode"], "isolated");
        assert_eq!(health["runtime"]["configLoaded"], true);
        assert_eq!(health["runtime"]["resourcesConfigured"], true);
        process
    }

    fn stop(&mut self) -> Result<(), String> {
        if self
            .child
            .try_wait()
            .map_err(|error| format!("poll selected console process: {error}"))?
            .is_some()
        {
            return Ok(());
        }
        self.child
            .kill()
            .map_err(|error| format!("terminate selected console process: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self
                .child
                .try_wait()
                .map_err(|error| format!("wait for selected console termination: {error}"))?
                .is_some()
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Err("selected console process did not exit within five seconds after kill".to_string())
    }
}

impl Drop for ConsoleProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct HttpResponse {
    status: u16,
    body: String,
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    console_token: Option<&str>,
    agent_token: Option<&str>,
    body: Option<&str>,
) -> Result<HttpResponse, String> {
    let body = body.unwrap_or("");
    let console_header = console_token
        .map(|token| format!("X-Console-Token: {token}\r\n"))
        .unwrap_or_default();
    let agent_header = agent_token
        .map(|token| format!("X-Agent-Token: {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{console_header}{agent_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|_| "connect to selected console port".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| "set selected console read timeout".to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|_| "write selected console HTTP request".to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|_| "read selected console HTTP response".to_string())?;
    let response =
        String::from_utf8(raw).map_err(|_| "console response was not UTF-8".to_string())?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "console response had no header boundary".to_string())?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "console response status was invalid".to_string())?;
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
            .map_err(|_| "check selected console process".to_string())?
            .is_some()
        {
            return Err("selected console exited before health was ready".to_string());
        }
        if let Ok(response) = http_request(process.port, "GET", "/api/health", None, None, None) {
            if response.status == 200 {
                return serde_json::from_str(&response.body)
                    .map_err(|_| "health response was not JSON".to_string());
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("selected console health endpoint timed out".to_string())
}

fn expect_json(response: HttpResponse, status: u16, context: &str) -> Value {
    assert_eq!(response.status, status, "{context}: {}", response.body);
    serde_json::from_str(&response.body)
        .unwrap_or_else(|error| panic!("{context} response was not JSON: {error}"))
}

fn get_json(
    port: u16,
    console_token: &str,
    agent_token: Option<&str>,
    path: &str,
    status: u16,
) -> Value {
    let response = http_request(port, "GET", path, Some(console_token), agent_token, None)
        .unwrap_or_else(|error| panic!("GET {path} failed at transport: {error}"));
    expect_json(response, status, &format!("GET {path}"))
}

fn post_json(
    port: u16,
    console_token: &str,
    agent_token: Option<&str>,
    path: &str,
    body: &Value,
    status: u16,
) -> Value {
    let body = body.to_string();
    let response = http_request(
        port,
        "POST",
        path,
        Some(console_token),
        agent_token,
        Some(&body),
    )
    .unwrap_or_else(|error| panic!("POST {path} failed at transport: {error}"));
    expect_json(response, status, &format!("POST {path}"))
}

fn assert_http_error(response: HttpResponse, status: u16, error: &str, context: &str) {
    assert_eq!(response.status, status, "{context}: {}", response.body);
    let body: Value = serde_json::from_str(&response.body)
        .unwrap_or_else(|parse_error| panic!("{context} response was not JSON: {parse_error}"));
    assert_eq!(body, json!({ "ok": false, "error": error }), "{context}");
}

fn topic_texts(body: &Value) -> Vec<String> {
    body["raw"]["activeTopics"]
        .as_array()
        .expect("inline activeTopics is an array")
        .iter()
        .filter_map(|topic| topic["text"].as_str().map(str::to_owned))
        .collect()
}

fn assert_selected_memory(
    body: &Value,
    marker: &str,
    foreign_marker: &str,
    expected_topics: &[String],
) {
    assert_eq!(body["ok"], true);
    assert_eq!(body["key"], SESSION_KEY);
    assert_eq!(body["category"], "activeTopic");
    assert_eq!(topic_texts(body), expected_topics);
    assert!(body["formatted"].as_str().is_some());
    assert!(!body.to_string().contains(foreign_marker));
    assert!(body.to_string().contains(marker));
}

fn read_json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read selected fixture JSON"))
        .expect("parse selected fixture JSON")
}

#[test]
fn social_memory_and_config_use_each_selected_runtime_root() {
    let workspace = FixtureWorkspace::new();
    let root_a = workspace.root("selected social root A");
    let root_b = workspace.root("selected social root B");
    let console_token_a = "synthetic-social-console-token-alpha-0001";
    let console_token_b = "synthetic-social-console-token-beta-0002";
    let agent_token_a = "synthetic-social-agent-token-alpha-0001";
    let agent_token_b = "synthetic-social-agent-token-beta-0002";
    let dsh_home_a =
        prepare_runtime_root(&root_a, console_token_a, agent_token_a, "alpha", 111_000);
    let dsh_home_b =
        prepare_runtime_root(&root_b, console_token_b, agent_token_b, "beta", 222_000);

    let state_a_path = root_a.join("data/social-v2.json");
    let state_b_path = root_b.join("data/social-v2.json");
    let config_a_path = root_a.join("config/config.json");
    let config_b_path = root_b.join("config/config.json");
    let mode_a_path = root_a.join("data/mode.json");
    let mode_a_initial = fs::read(&mode_a_path).expect("snapshot selected A mode");
    let state_a_initial = fs::read(&state_a_path).expect("snapshot selected A social state");
    let state_b_initial = fs::read(&state_b_path).expect("snapshot selected B social state");
    let config_a_initial = fs::read(&config_a_path).expect("snapshot selected A config");
    let config_b_initial = fs::read(&config_b_path).expect("snapshot selected B config");

    let (port_a, port_b) = reserve_distinct_ports();
    let mut console_a = ConsoleProcess::start(&root_a, port_a, &dsh_home_a);
    let mut console_b = ConsoleProcess::start(&root_b, port_b, &dsh_home_b);
    assert_ne!(console_a.port, console_b.port);

    // A console token from another root must not read or mutate this root.
    let wrong_console_read = http_request(
        port_a,
        "GET",
        ACTIVE_MEMORY_PATH,
        Some(console_token_b),
        None,
        None,
    )
    .unwrap_or_else(|error| panic!("cross-root console read failed at transport: {error}"));
    assert_http_error(
        wrong_console_read,
        401,
        "未授权：请提供控制台访问令牌",
        "B console token on A",
    );
    let wrong_console_write = http_request(
        port_a,
        "POST",
        "/api/socialV2/memory-append",
        Some(console_token_b),
        None,
        Some(
            &json!({
                "key": SESSION_KEY,
                "category": "activeTopic",
                "content": "must-not-write-through-foreign-console-token"
            })
            .to_string(),
        ),
    )
    .unwrap_or_else(|error| panic!("cross-root console write failed at transport: {error}"));
    assert_http_error(
        wrong_console_write,
        401,
        "未授权：请提供控制台访问令牌",
        "cross-root console token write",
    );
    assert_eq!(fs::read(&state_a_path).unwrap(), state_a_initial);
    assert_eq!(fs::read(&state_b_path).unwrap(), state_b_initial);
    assert_eq!(fs::read(&config_a_path).unwrap(), config_a_initial);
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_initial);

    // Legacy JS declares socialV2/config admin-only when any agent token is present.
    let agent_on_config = http_request(
        port_a,
        "GET",
        "/api/socialV2/config",
        Some(console_token_a),
        Some(agent_token_a),
        None,
    )
    .unwrap_or_else(|error| panic!("agent-token config request failed at transport: {error}"));
    assert_http_error(
        agent_on_config,
        403,
        "该接口仅控制台可用",
        "agent token on admin-only social config",
    );
    assert_eq!(fs::read(&config_a_path).unwrap(), config_a_initial);
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_initial);

    let memory_a = get_json(
        port_a,
        console_token_a,
        Some(agent_token_a),
        ACTIVE_MEMORY_PATH,
        200,
    );
    let memory_b = get_json(
        port_b,
        console_token_b,
        Some(agent_token_b),
        ACTIVE_MEMORY_PATH,
        200,
    );
    assert_selected_memory(
        &memory_a,
        "alpha",
        "beta-",
        &["alpha-seed-topic".to_string()],
    );
    assert_selected_memory(
        &memory_b,
        "beta",
        "alpha-",
        &["beta-seed-topic".to_string()],
    );

    // A valid console token must not make a foreign session agent token valid.
    let foreign_agent_read = http_request(
        port_a,
        "GET",
        ACTIVE_MEMORY_PATH,
        Some(console_token_a),
        Some(agent_token_b),
        None,
    )
    .unwrap_or_else(|error| panic!("foreign agent-token read failed at transport: {error}"));
    assert_http_error(
        foreign_agent_read,
        403,
        "agent token 无效",
        "B agent token on A conversation",
    );
    let foreign_agent_write = http_request(
        port_a,
        "POST",
        "/api/socialV2/memory-append",
        Some(console_token_a),
        Some(agent_token_b),
        Some(
            &json!({
                "key": SESSION_KEY,
                "category": "activeTopic",
                "content": "must-not-write-through-foreign-agent-token"
            })
            .to_string(),
        ),
    )
    .unwrap_or_else(|error| panic!("foreign agent-token write failed at transport: {error}"));
    assert_http_error(
        foreign_agent_write,
        403,
        "agent token 无效",
        "B agent token write on A conversation",
    );
    assert_eq!(fs::read(&state_a_path).unwrap(), state_a_initial);
    assert_eq!(fs::read(&state_b_path).unwrap(), state_b_initial);

    // A valid agent token is still bound to the selected config allow-list.
    let denied_allow_config: Value = serde_json::from_slice(&config_b_initial)
        .expect("parse selected B config for allow-list rejection");
    let mut denied_allow_config = denied_allow_config;
    denied_allow_config["allow"]["private"] = json!([]);
    let denied_allow_config_bytes = serde_json::to_vec_pretty(&denied_allow_config)
        .expect("serialize selected B deny-list fixture");
    fs::write(&config_b_path, &denied_allow_config_bytes)
        .expect("write selected B deny-list fixture");
    let disallowed_agent_read = http_request(
        port_b,
        "GET",
        ACTIVE_MEMORY_PATH,
        Some(console_token_b),
        Some(agent_token_b),
        None,
    )
    .unwrap_or_else(|error| panic!("disallowed agent read failed at transport: {error}"));
    assert_http_error(
        disallowed_agent_read,
        403,
        "目标不在当前模式允许范围内",
        "valid B agent token outside selected allow-list",
    );
    assert_eq!(fs::read(&state_a_path).unwrap(), state_a_initial);
    assert_eq!(fs::read(&state_b_path).unwrap(), state_b_initial);
    assert_eq!(fs::read(&config_b_path).unwrap(), denied_allow_config_bytes);
    fs::write(&config_b_path, &config_b_initial)
        .expect("restore selected B config after allow-list fixture");

    // A valid, allowed A agent token cannot append when the selected tool flag is false.
    let mut disabled_memory_config: Value = serde_json::from_slice(&config_a_initial)
        .expect("parse selected A config for tool-disabled fixture");
    disabled_memory_config["socialV2"]["tools"]["memory"] = json!(false);
    let disabled_memory_config_bytes = serde_json::to_vec_pretty(&disabled_memory_config)
        .expect("serialize selected A tool-disabled config");
    fs::write(&config_a_path, &disabled_memory_config_bytes)
        .expect("write selected A tool-disabled fixture");
    let disabled_tool_append = http_request(
        port_a,
        "POST",
        "/api/socialV2/memory-append",
        Some(console_token_a),
        Some(agent_token_a),
        Some(
            &json!({
                "key": SESSION_KEY,
                "category": "activeTopic",
                "content": "must-not-write-when-selected-memory-tool-disabled"
            })
            .to_string(),
        ),
    )
    .unwrap_or_else(|error| panic!("tool-disabled append failed at transport: {error}"));
    assert_http_error(
        disabled_tool_append,
        403,
        "工具未启用：qq_memory_append",
        "selected memory tool disabled",
    );
    assert_eq!(fs::read(&state_a_path).unwrap(), state_a_initial);
    assert_eq!(fs::read(&state_b_path).unwrap(), state_b_initial);
    assert_eq!(
        fs::read(&config_a_path).unwrap(),
        disabled_memory_config_bytes
    );
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_initial);
    fs::write(&config_a_path, &config_a_initial)
        .expect("restore selected A config after tool-disabled fixture");

    // Invalid category is a real handler error and must not persist partial state.
    let invalid_append = post_json(
        port_a,
        console_token_a,
        None,
        "/api/socialV2/memory-append",
        &json!({
            "key": SESSION_KEY,
            "category": "not-a-memory-category",
            "content": "invalid-category-must-not-persist"
        }),
        400,
    );
    assert_eq!(
        invalid_append["error"],
        "category 必须是 activeTopic / pendingThought / memberImpression"
    );
    assert_eq!(fs::read(&state_a_path).unwrap(), state_a_initial);
    assert_eq!(fs::read(&state_b_path).unwrap(), state_b_initial);

    for (port, console_token, agent_token, marker, foreign) in [
        (port_a, console_token_a, agent_token_a, "alpha", "beta-"),
        (port_b, console_token_b, agent_token_b, "beta", "alpha-"),
    ] {
        let appended = post_json(
            port,
            console_token,
            Some(agent_token),
            "/api/socialV2/memory-append",
            &json!({
                "key": SESSION_KEY,
                "category": "activeTopic",
                "content": format!("{marker}-appended-topic")
            }),
            200,
        );
        assert_eq!(appended["ok"], true);
        assert_eq!(appended["key"], SESSION_KEY);
        assert_eq!(appended["category"], "activeTopic");
        assert_eq!(appended["content"], format!("{marker}-appended-topic"));
        assert!(appended["memory"]
            .as_str()
            .unwrap()
            .contains(&format!("{marker}-appended-topic")));
        assert!(!appended.to_string().contains(foreign));

        let updated = post_json(
            port,
            console_token,
            Some(agent_token),
            "/api/socialV2/memory-update",
            &json!({
                "key": SESSION_KEY,
                "category": "activeTopic",
                "oldContent": format!("{marker}-appended-topic"),
                "newContent": format!("{marker}-updated-topic")
            }),
            200,
        );
        assert_eq!(updated["ok"], true);
        assert_eq!(updated["key"], SESSION_KEY);
        assert_eq!(updated["category"], "activeTopic");
        assert!(updated["memory"]
            .as_str()
            .unwrap()
            .contains(&format!("{marker}-updated-topic")));
        assert!(!updated.to_string().contains(foreign));

        let removed = post_json(
            port,
            console_token,
            Some(agent_token),
            "/api/socialV2/memory-remove",
            &json!({
                "key": SESSION_KEY,
                "category": "activeTopic",
                "content": format!("{marker}-updated-topic")
            }),
            200,
        );
        assert_eq!(removed["ok"], true);
        assert_eq!(removed["key"], SESSION_KEY);
        assert_eq!(removed["category"], "activeTopic");
        assert!(!removed["memory"]
            .as_str()
            .unwrap()
            .contains(&format!("{marker}-updated-topic")));
        assert!(!removed.to_string().contains(foreign));

        let clear_fixture = post_json(
            port,
            console_token,
            Some(agent_token),
            "/api/socialV2/memory-append",
            &json!({
                "key": SESSION_KEY,
                "category": "activeTopic",
                "content": format!("{marker}-clear-fixture")
            }),
            200,
        );
        assert_eq!(clear_fixture["ok"], true);
        let cleared = post_json(
            port,
            console_token,
            Some(agent_token),
            "/api/socialV2/memory-clear",
            &json!({ "key": SESSION_KEY, "category": "activeTopic" }),
            200,
        );
        assert_eq!(cleared["ok"], true);
        assert_eq!(cleared["key"], SESSION_KEY);
        assert_eq!(cleared["category"], "activeTopic");
        assert!(!cleared.to_string().contains(foreign));
    }

    for (port, console_token, marker, foreign) in [
        (port_a, console_token_a, "alpha", "beta-"),
        (port_b, console_token_b, "beta", "alpha-"),
    ] {
        let full = get_json(port, console_token, None, FULL_MEMORY_PATH, 200);
        assert_eq!(full["ok"], true);
        assert_eq!(full["key"], SESSION_KEY);
        assert_eq!(full["category"], "");
        assert_eq!(full["raw"]["activeTopics"], json!([]));
        assert!(full["raw"]["pendingThoughts"]
            .to_string()
            .contains(&format!("{marker}-pending-thought")));
        assert!(full["raw"]["memberImpressions"]
            .to_string()
            .contains(&format!("{marker}-trait")));
        assert!(!full.to_string().contains(foreign));
        assert!(full["formatted"].as_str().is_some());
    }

    let initial_config_a = get_json(port_a, console_token_a, None, "/api/socialV2/config", 200);
    let initial_config_b = get_json(port_b, console_token_b, None, "/api/socialV2/config", 200);
    assert_eq!(initial_config_a["ok"], true);
    assert_eq!(initial_config_b["ok"], true);
    assert_eq!(
        initial_config_a["config"]["customRootMarker"],
        "alpha-config"
    );
    assert_eq!(
        initial_config_b["config"]["customRootMarker"],
        "beta-config"
    );
    assert_eq!(initial_config_a["config"]["wake"]["sleepMinMs"], 111_000);
    assert_eq!(initial_config_b["config"]["wake"]["sleepMinMs"], 222_000);
    assert!(!initial_config_a.to_string().contains("beta-"));
    assert!(!initial_config_b.to_string().contains("alpha-"));

    let wrong_console_config_write = http_request(
        port_a,
        "POST",
        "/api/socialV2/config",
        Some(console_token_b),
        None,
        Some(
            &json!({
                "customRootMarker": "must-not-write-config-with-foreign-token"
            })
            .to_string(),
        ),
    )
    .unwrap_or_else(|error| panic!("cross-root config write failed at transport: {error}"));
    assert_http_error(
        wrong_console_config_write,
        401,
        "未授权：请提供控制台访问令牌",
        "cross-root config write",
    );
    assert_eq!(fs::read(&config_a_path).unwrap(), config_a_initial);
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_initial);

    let config_update_a = json!({
        "enabled": false,
        "customRootMarker": "alpha-updated-config",
        "tools": { "sendMessage": false },
        "wake": {
            "sleepMinMs": -50,
            "recommendedProbability": 1.7,
            "recommendedKeywords": "alpha, first"
        },
        "send": { "maxMessageChars": "1234" }
    });
    let update_response_a = post_json(
        port_a,
        console_token_a,
        None,
        "/api/socialV2/config",
        &config_update_a,
        200,
    );
    assert_eq!(update_response_a["ok"], true);
    assert_eq!(
        update_response_a["config"]["customRootMarker"],
        "alpha-updated-config"
    );
    assert_eq!(update_response_a["config"]["wake"]["sleepMinMs"], 0);
    assert_eq!(
        update_response_a["config"]["wake"]["recommendedProbability"],
        1.0
    );
    assert_eq!(
        update_response_a["config"]["wake"]["recommendedKeywords"],
        json!(["alpha", "first"])
    );
    assert_eq!(update_response_a["config"]["send"]["maxMessageChars"], 1234);
    let config_a_after_update = fs::read(&config_a_path).expect("read updated selected A config");
    assert_ne!(config_a_after_update, config_a_initial);
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_initial);
    let config_b_still_initial =
        get_json(port_b, console_token_b, None, "/api/socialV2/config", 200);
    assert_eq!(
        config_b_still_initial["config"]["customRootMarker"],
        "beta-config"
    );
    assert_eq!(
        config_b_still_initial["config"]["wake"]["sleepMinMs"],
        222_000
    );
    assert!(!config_b_still_initial.to_string().contains("alpha-"));

    let config_update_b = json!({
        "enabled": true,
        "customRootMarker": "beta-updated-config",
        "tools": { "sendMessage": true },
        "wake": {
            "sleepMinMs": 999_999,
            "recommendedProbability": -0.5,
            "recommendedKeywords": "beta, second"
        },
        "send": { "maxMessageChars": "2345" }
    });
    let update_response_b = post_json(
        port_b,
        console_token_b,
        None,
        "/api/socialV2/config",
        &config_update_b,
        200,
    );
    assert_eq!(update_response_b["ok"], true);
    assert_eq!(
        update_response_b["config"]["customRootMarker"],
        "beta-updated-config"
    );
    assert_eq!(update_response_b["config"]["wake"]["sleepMinMs"], 999_999);
    assert_eq!(
        update_response_b["config"]["wake"]["recommendedProbability"],
        0.0
    );
    assert_eq!(
        update_response_b["config"]["wake"]["recommendedKeywords"],
        json!(["beta", "second"])
    );
    assert_eq!(update_response_b["config"]["send"]["maxMessageChars"], 2345);
    let config_b_after_update = fs::read(&config_b_path).expect("read updated selected B config");
    assert_ne!(config_b_after_update, config_b_initial);
    assert_eq!(fs::read(&config_a_path).unwrap(), config_a_after_update);

    let final_config_a = get_json(port_a, console_token_a, None, "/api/socialV2/config", 200);
    let final_config_b = get_json(port_b, console_token_b, None, "/api/socialV2/config", 200);
    assert_eq!(
        final_config_a["config"]["customRootMarker"],
        "alpha-updated-config"
    );
    assert_eq!(final_config_a["config"]["enabled"], false);
    assert_eq!(final_config_a["config"]["tools"]["sendMessage"], false);
    assert_eq!(final_config_a["config"]["wake"]["sleepMinMs"], 0);
    assert_eq!(
        final_config_a["config"]["wake"]["recommendedProbability"],
        1.0
    );
    assert_eq!(
        final_config_a["config"]["wake"]["recommendedKeywords"],
        json!(["alpha", "first"])
    );
    assert_eq!(final_config_a["config"]["send"]["maxMessageChars"], 1234);
    assert_eq!(
        final_config_b["config"]["customRootMarker"],
        "beta-updated-config"
    );
    assert_eq!(final_config_b["config"]["enabled"], true);
    assert_eq!(final_config_b["config"]["tools"]["sendMessage"], true);
    assert_eq!(final_config_b["config"]["wake"]["sleepMinMs"], 999_999);
    assert_eq!(
        final_config_b["config"]["wake"]["recommendedProbability"],
        0.0
    );
    assert_eq!(
        final_config_b["config"]["wake"]["recommendedKeywords"],
        json!(["beta", "second"])
    );
    assert_eq!(final_config_b["config"]["send"]["maxMessageChars"], 2345);
    assert!(!final_config_a.to_string().contains("beta-"));
    assert!(!final_config_b.to_string().contains("alpha-"));

    let final_file_a = read_json_file(&config_a_path);
    let final_file_b = read_json_file(&config_b_path);
    assert_eq!(final_file_a["consoleToken"], console_token_a);
    assert_eq!(final_file_b["consoleToken"], console_token_b);
    assert_eq!(final_file_a["configMarker"], "alpha-top-level-config");
    assert_eq!(final_file_b["configMarker"], "beta-top-level-config");
    assert_eq!(
        final_file_a["socialV2"]["customRootMarker"],
        "alpha-updated-config"
    );
    assert_eq!(
        final_file_b["socialV2"]["customRootMarker"],
        "beta-updated-config"
    );
    assert!(!final_file_a.to_string().contains("beta-"));
    assert!(!final_file_b.to_string().contains("alpha-"));

    for (root, agent_token, marker, foreign) in [
        (&root_a, agent_token_a, "alpha", "beta-"),
        (&root_b, agent_token_b, "beta", "alpha-"),
    ] {
        let state = read_json_file(&root.join("data/social-v2.json"));
        assert_eq!(
            state["conversations"][SESSION_KEY]["agentToken"],
            agent_token
        );
        assert_eq!(
            state["conversations"][SESSION_KEY]["activeTopics"],
            json!([])
        );
        assert_eq!(
            state["conversations"][SESSION_KEY]["pendingThoughts"][0]["text"],
            format!("{marker}-pending-thought")
        );
        assert!(state["conversations"][SESSION_KEY]["memberImpressions"]
            .to_string()
            .contains(&format!("{marker}-trait")));
        assert!(!state.to_string().contains(foreign));
    }
    assert_ne!(
        fs::read(&state_a_path).unwrap(),
        state_a_initial,
        "A social state must be persisted inside A's selected root"
    );
    assert_ne!(
        fs::read(&state_b_path).unwrap(),
        state_b_initial,
        "B social state must be persisted inside B's selected root"
    );

    console_a
        .stop()
        .expect("stop selected A console process before the mode probe");
    // The config update above disabled A's social engine, and the frozen gate order reports
    // that before the mode. Re-enable it so this probe isolates the selected mode gate.
    let mut enabled_config_a = read_json_file(&config_a_path);
    enabled_config_a["socialV2"]["enabled"] = json!(true);
    fs::write(
        &config_a_path,
        serde_json::to_vec_pretty(&enabled_config_a).expect("serialize re-enabled A config"),
    )
    .expect("re-enable selected A social engine before the mode probe");
    fs::write(&mode_a_path, br#"{"mode":"chat"}"#).expect("write selected A non-reserved2 mode");
    let mut mode_console_a = ConsoleProcess::start(&root_a, port_a, &dsh_home_a);
    let state_a_before_mode_probe = fs::read(&state_a_path).expect("snapshot A before mode probe");
    let config_a_before_mode_probe =
        fs::read(&config_a_path).expect("snapshot A config before mode probe");
    // B was legitimately mutated earlier in this test; the probe must leave its current bytes.
    let state_b_before_mode_probe = fs::read(&state_b_path).expect("snapshot B before mode probe");
    let config_b_before_mode_probe =
        fs::read(&config_b_path).expect("snapshot B config before mode probe");
    let mode_rejected_agent_read = http_request(
        port_a,
        "GET",
        ACTIVE_MEMORY_PATH,
        Some(console_token_a),
        Some(agent_token_a),
        None,
    )
    .unwrap_or_else(|error| panic!("non-reserved2 agent read failed at transport: {error}"));
    assert_http_error(
        mode_rejected_agent_read,
        403,
        "该接口仅 reserved2 模式可用",
        "agent memory request while selected mode is chat",
    );
    assert_eq!(fs::read(&state_a_path).unwrap(), state_a_before_mode_probe);
    assert_eq!(
        fs::read(&config_a_path).unwrap(),
        config_a_before_mode_probe
    );
    assert_eq!(fs::read(&state_b_path).unwrap(), state_b_before_mode_probe);
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_before_mode_probe);
    mode_console_a
        .stop()
        .expect("stop mode-probe console process within cleanup bound");
    fs::write(&mode_a_path, &mode_a_initial).expect("restore selected A mode fixture");

    console_b
        .stop()
        .expect("stop selected B console process within cleanup bound");
}
