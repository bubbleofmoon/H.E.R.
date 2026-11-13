//! Runtime configuration behavior through an isolated console process.
#[path = "../src/install_root.rs"]
mod install_root;
mod support;
#[path = "support/selftest.rs"]
mod support_selftest;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
const CONSOLE_TOKEN: &str = "synthetic-console-token";
const OWNER: &str = "501";
const GROUP: &str = "502";
fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "config-import-{label}-{}-{nanos}",
        std::process::id()
    ));
    let dir = crate::install_root::development_root(dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes a synthetic legacy deployment (JS layout: config.json + state/ + public/ + roles/).
/// It uses the legacy spellings the JS bridge accepts: string ownerQQ, `privates`/`group`
/// aliases, a `simulation` mode and a fallback list with one invalid entry.
fn write_runtime(root: &Path) {
    let checked = crate::install_root::development_root(root);
    let root = checked.as_path();
    fs::create_dir_all(root.join("state")).unwrap();
    fs::create_dir_all(root.join("public")).unwrap();
    fs::create_dir_all(root.join("roles")).unwrap();
    let config = json!({
        "consoleToken": CONSOLE_TOKEN,
        "ownerQQ": OWNER,
        "allow": { "privates": [OWNER], "group": [GROUP] },
        "deny": { "private": [], "groups": [] },
        "dsh": {
            "baseUrl": "http://127.0.0.1:1",
            "provider": "fixture-provider",
            "model": "fixture-model",
            "fallbacks": [
                { "provider": "fixture-provider", "model": "fixture-model" },
                { "provider": "fixture-backup", "model": "fixture-backup-model" },
                { "provider": "", "model": "ignored" }
            ]
        },
        "snowluma": { "httpUrl": "http://127.0.0.1:1", "wsUrl": "ws://127.0.0.1:1" }
    });
    for dir in ["config", "data", "resources", "user/dsh"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(root.join("config/config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
    fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
    fs::write(root.join("data/mode.json"), r#"{"mode":"simulation","closedAgentPreset":""}"#).unwrap();
    fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
}
fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn isolate_env(command: &mut Command, home: &Path) {
    command
        .env_remove("HER_RUNTIME_ROOT")
        .env_remove("APPDATA")
        .env_remove("LOCALAPPDATA")
        .env_remove("HOME")
        .env("DSH_HOME", home)
        .env("USERPROFILE", home.join("synthetic-user-profile"));
    hide_window(command);
}

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("utf-8 fixture path")
}

struct Console {
    child: Child,
    port: u16,
}

impl Console {
    fn start(root: &Path) -> Self {
        let (child, port) = crate::support::ports::start_console(Duration::from_secs(10), |port| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
            command
                .arg("--console-serve")
                .arg("--console-port")
                .arg(port.to_string())
                .arg("--runtime-root")
                .arg(root)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            isolate_env(&mut command, &unique_dir("console-home"));
            command.spawn().expect("start isolated console candidate")
        });
        let mut console = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok((200, _)) = http(port, "GET", "/api/health", None, None) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "isolated console did not become healthy"
            );
            assert!(
                console.child.try_wait().unwrap().is_none(),
                "console exited during startup"
            );
            thread::sleep(Duration::from_millis(50));
        }
        console
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Result<(u16, String), String> {
    http_with_agent(port, method, path, token, None, body)
}

fn http_with_agent(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    agent: Option<&str>,
    body: Option<&Value>,
) -> Result<(u16, String), String> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let token_header = token
        .map(|t| format!("X-Console-Token: {t}\r\n"))
        .unwrap_or_default();
    let agent_header = agent.map(|t| format!("X-Agent-Token: {t}\r\n")).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{token_header}{agent_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8(raw).map_err(|e| e.to_string())?;
    let (head, body) = text.split_once("\r\n\r\n").ok_or("no header boundary")?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or("bad status line")?;
    Ok((status, body.to_string()))
}

fn get_json(console: &Console, path: &str) -> Value {
    let (status, body) = http(console.port, "GET", path, Some(CONSOLE_TOKEN), None).unwrap();
    assert_eq!(status, 200, "GET {path}: {body}");
    serde_json::from_str(&body).unwrap()
}

#[test]
fn isolated_console_preserves_config_aliases_and_model_chain_defaults() {
    let target = unique_dir("runtime-config");
    write_runtime(&target);

    let console = Console::start(&target);

    let whitelist = get_json(&console, "/api/whitelist");
    assert_eq!(
        whitelist["ownerQQ"],
        json!(501),
        "string ownerQQ is normalized at runtime"
    );
    assert_eq!(
        whitelist["allow"]["private"],
        json!([OWNER]),
        "allow.privates alias is honored"
    );
    assert_eq!(
        whitelist["allow"]["groups"],
        json!([GROUP]),
        "allow.group alias is honored"
    );

    let chain = get_json(&console, "/api/model-chain");
    let entries = chain["chain"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "invalid fallback is skipped and duplicates are removed: {chain}"
    );
    assert_eq!(entries[0]["provider"], "fixture-provider");
    assert_eq!(entries[1]["provider"], "fixture-backup");
    assert_eq!(
        entries[0]["effort"], "max",
        "missing reasoningEffort defaults to max"
    );
    assert_eq!(entries[1]["effort"], "max");
}

#[test]
fn simulation_alias_is_the_reserved2_gate() {
    let target = unique_dir("runtime-config");
    write_runtime(&target);
    let body = json!({ "key": format!("group:{GROUP}"), "messages": ["synthetic"] });

    // data/mode.json says "simulation": the reserved2 gate passes, so the next check is the agent token.
    {
        let console = Console::start(&target);
        let (status, text) = http(
            console.port,
            "POST",
            "/api/socialV2/send-message",
            Some(CONSOLE_TOKEN),
            Some(&body),
        )
        .unwrap();
        assert_eq!(status, 403);
        assert!(
            text.contains("必须携带 agent token"),
            "simulation must pass the mode gate: {text}"
        );
    }

    // A chat mode is still rejected by the reserved2 gate.
    fs::write(target.join("data/mode.json"), r#"{"mode":"chat"}"#).unwrap();
    let console = Console::start(&target);
    let (status, text) = http(
        console.port,
        "POST",
        "/api/socialV2/send-message",
        Some(CONSOLE_TOKEN),
        Some(&body),
    )
    .unwrap();
    assert_eq!(status, 403);
    assert!(
        text.contains("该接口仅 reserved2 模式可用"),
        "chat must fail the gate: {text}"
    );
}

