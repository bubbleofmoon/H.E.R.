//! Ticket 26: the auxiliary MCP servers (`--mcp-web-search-serve`, `--mcp-host-serve`) through the real
//! binary over stdio. Every external input is a started loopback fake or a literal that is rejected before
//! any connection; nothing here contacts the internet, DSH, QQ or a model.
#[path = "../src/install_root.rs"]
mod install_root;

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_root(label: &str) -> TempRoot {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = crate::install_root::development_root(std::env::temp_dir().join(format!("qqbot aux {label} {} {stamp}", std::process::id())));
    for dir in ["config", "data", "resources"] {
        fs::create_dir_all(path.join(dir)).unwrap();
    }
    TempRoot(path)
}

/// A selected runtime root: `config` is merged over the mandatory fields.
fn write_root(root: &Path, onebot_url: &str, config: Value) {
    let checked = crate::install_root::development_root(root);
    let root = checked.as_path();
    let mut cfg = json!({"consoleToken": "aux-test-console-token"});
    for (k, v) in config.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    fs::write(root.join("config/config.json"), serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
    fs::write(root.join("data/social-v2.json"), b"{}").unwrap();
    let deps = json!({"dsh": "http://127.0.0.1:1", "model": "http://127.0.0.1:2", "onebot": onebot_url});
    fs::write(root.join("dependencies.json"), serde_json::to_vec_pretty(&deps).unwrap()).unwrap();
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Mcp {
    fn start(mode: &str, root: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .arg(mode)
            .arg("--runtime-root")
            .arg(root)
            .arg("--console-port")
            .arg("31999")
            .env_remove("HER_RUNTIME_ROOT")
            .env("DSH_HOME", root.join("user/dsh"))
            .env("USERPROFILE", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("start auxiliary MCP process");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    fn send(&mut self, request: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "auxiliary MCP process exited before replying");
        serde_json::from_str(&line).expect("MCP response is JSON")
    }

    fn request(&mut self, request: Value) -> Value {
        self.send(&request);
        self.read()
    }

    fn initialize(&mut self) -> Value {
        self.request(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "ticket-26", "version": "1"}}}))
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}}))
    }

    /// Close stdin and return the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        self.child.wait().unwrap().code()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Recorded independent expected auxiliary tool answers.
fn baseline() -> Value {
    serde_json::from_slice(&fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/spec/aux-baseline.json")).unwrap()).unwrap()
}

fn default_root(label: &str) -> TempRoot {
    let root = temp_root(label);
    write_root(&root.0, "http://127.0.0.1:1", json!({}));
    root
}

#[test]
fn web_search_server_enumerates_the_frozen_tools_and_rejects_empty_and_loopback_inputs() {
    let root = default_root("ws-enum");
    let expected = baseline();
    let mut mcp = Mcp::start("--mcp-web-search-serve", &root.0);
    assert_eq!(mcp.initialize(), expected["search"][0]);
    assert_eq!(mcp.request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})), expected["search"][1]);
    assert_eq!(mcp.call(3, "web_search", json!({"query": "[CQ:at,qq=123]  "})), expected["search"][2]);
    assert_eq!(mcp.call(4, "web_fetch", json!({"url": "http://127.0.0.1/"})), expected["search"][3]);
    assert_eq!(mcp.finish(), Some(0), "stdin EOF must end the server with exit code 0");
}

#[test]
fn host_server_enumerates_the_frozen_status_tool() {
    let root = default_root("host-enum");
    let expected = baseline();
    let mut mcp = Mcp::start("--mcp-host-serve", &root.0);
    assert_eq!(mcp.initialize(), expected["host"][0]);
    assert_eq!(mcp.request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})), expected["host"][1]);
    assert_eq!(mcp.finish(), Some(0));
}

// ----- DSH profile entries (`her dsh-mcp-config`) -------------------------------------------------------

fn dsh_config(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_her")).arg("dsh-mcp-config").args(args).output().expect("run dsh-mcp-config");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).trim().to_string(), String::from_utf8_lossy(&out.stderr).trim().to_string())
}

const EXE: &str = "/opt/qq bot/it's her";

fn expected_block() -> String {
    let command = "'/opt/qq bot/it''s her'";
    let entry = |id: &str, server: &str, arg: &str, extra: &str| format!(
        "- insert:\n    - id: {id}\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: {server}\n        transport: stdio\n        command: {command}\n        args:\n          - '{arg}'\n{extra}"
    );
    format!(
        "# === qq-bridge MCP BEGIN ===\n# 由 her dsh-mcp-config 维护；这段区块会被整体替换，请勿手工编辑内部条目。\n{}{}{}# === qq-bridge MCP END ===\n",
        entry("mcp-snowluma", "snowluma", "--mcp-serve", "        toolCallTimeoutMs: 725000\n"),
        entry("mcp-snowluma-host", "snowluma-host", "--mcp-host-serve", ""),
        entry("mcp-web-search-safe", "web-search-safe", "--mcp-web-search-serve", ""),
    )
}

#[test]
fn generated_runtime_entries_start_all_three_selected_servers() {
    let root = temp_root("generated runtime it's selected");
    let (gateway, _) = fake_gateway(vec![Reply::Body(
        200,
        r#"{"status":"ok","retcode":0,"data":{"user_id":10001,"nickname":"synthetic"}}"#,
    )]);
    write_root(
        &root.0,
        &gateway,
        json!({"socialV2":{"tools":{"getPrompt":false}}}),
    );
    let file = root.0.join("cordis.patch.yml");
    let console = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = console.local_addr().unwrap().port().to_string();
    let (code, _, error) = dsh_config(&[
        "--patch-file",
        file.to_str().unwrap(),
        "--bridge-exe",
        env!("CARGO_BIN_EXE_her"),
        "--runtime-root",
        root.0.to_str().unwrap(),
        "--console-port",
        &port,
    ]);
    assert_eq!(code, 0, "{error}");
    let text = fs::read_to_string(&file).unwrap();
    let scalar = |s: &str| {
        s.trim()
            .strip_prefix('\'')
            .unwrap()
            .strip_suffix('\'')
            .unwrap()
            .replace("''", "'")
    };
    let mut names = Vec::new();
    for entry in text.split("- insert:\n").skip(1) {
        let server = entry
            .lines()
            .find_map(|l| l.trim().strip_prefix("serverName: "))
            .unwrap();
        names.push(server.to_string());
        let exe = scalar(
            entry
                .lines()
                .find_map(|l| l.trim().strip_prefix("command: "))
                .unwrap(),
        );
        let args: Vec<String> = entry
            .lines()
            .filter_map(|l| l.strip_prefix("          - "))
            .map(scalar)
            .collect();
        assert_eq!(
            &args[1..],
            &[
                "--runtime-root",
                root.0.to_str().unwrap(),
                "--console-port",
                &port
            ]
        );
        let mut command = Command::new(exe);
        command
            .args(&args)
            .env_remove("HER_RUNTIME_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Mcp {
            child,
            stdin,
            stdout,
        };
        let init = mcp.initialize();
        let expected_name = if server == "snowluma" {
            "snowluma-safe"
        } else {
            server
        };
        assert_eq!(
            init["result"]["serverInfo"]["name"], expected_name,
            "{init}"
        );
        let tools = mcp.request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
        let tools = tools["result"]["tools"].as_array().unwrap();
        let answer = match server {
            "snowluma" => {
                assert!(tools.iter().any(|t| t["name"] == "qq_social_state"));
                assert!(
                    !tools.iter().any(|t| t["name"] == "qq_get_prompt"),
                    "selected config gates tools"
                );
                let listener = console.try_clone().unwrap();
                listener.set_nonblocking(true).unwrap();
                let responder = std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error)
                                if error.kind() == std::io::ErrorKind::WouldBlock
                                    && std::time::Instant::now() < deadline =>
                            {
                                std::thread::sleep(std::time::Duration::from_millis(10))
                            }
                            Err(error) => panic!("selected console was not called: {error}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut request = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        assert_ne!(
                            reader.read_line(&mut line).unwrap(),
                            0,
                            "console request EOF"
                        );
                        if line == "\r\n" {
                            break;
                        }
                        request.push_str(&line);
                    }
                    assert!(
                        request.starts_with("GET /api/socialV2/state?key=group%3A10001 "),
                        "{request}"
                    );
                    let body = r#"{"selectedRuntime":"synthetic-console"}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                });
                let answer = mcp.call(
                    3,
                    "qq_social_state",
                    json!({"key":"group:10001","token":"fixture-agent"}),
                );
                responder.join().unwrap();
                assert!(answer.to_string().contains("synthetic-console"), "{answer}");
                answer
            }
            "snowluma-host" => {
                assert_eq!(tools.len(), 1);
                let answer = mcp.call(3, "snowluma_status", json!({}));
                assert!(answer.to_string().contains("synthetic"), "{answer}");
                answer
            }
            "web-search-safe" => {
                assert!(tools.iter().any(|t| t["name"] == "web_fetch"));
                let answer = mcp.call(3, "web_search", json!({"query":"[CQ:at,qq=123]  "}));
                assert_eq!(answer, baseline()["search"][2]);
                answer
            }
            other => panic!("unexpected server {other}"),
        };
        assert!(answer["result"]["content"].is_array(), "{answer}");
        assert_eq!(mcp.finish(), Some(0));
    }
    assert_eq!(names, ["snowluma", "snowluma-host", "web-search-safe"]);
}

#[test]
fn dsh_config_refuses_incomplete_or_invalid_runtime_selection_before_writing() {
    let root = temp_root("dsh-runtime-options");
    let file = root.0.join("cordis.patch.yml");
    for options in [
        vec!["--runtime-root", root.0.to_str().unwrap()],
        vec!["--console-port", "3100"],
        vec!["--runtime-root", "relative", "--console-port", "3100"],
        vec![
            "--runtime-root",
            root.0.to_str().unwrap(),
            "--console-port",
            "0",
        ],
        vec![
            "--runtime-root",
            root.0.to_str().unwrap(),
            "--console-port",
            "65536",
        ],
    ] {
        let mut args = vec!["--patch-file", file.to_str().unwrap()];
        args.extend(options);
        assert_eq!(dsh_config(&args).0, 2, "{args:?}");
        assert!(!file.exists(), "invalid selection wrote a profile");
    }
}
#[test]
fn dsh_config_installs_all_three_servers_pointing_at_the_bridge_and_is_idempotent() {
    let root = temp_root("dsh-config");
    let file = root.0.join("profiles/web/cordis.patch.yml");
    let file = file.to_str().unwrap();
    assert_eq!(dsh_config(&["--patch-file", file, "--bridge-exe", EXE]).1, "written");
    assert_eq!(fs::read_to_string(file).unwrap(), expected_block());
    assert_eq!(dsh_config(&["--patch-file", file, "--bridge-exe", EXE]).1, "unchanged");
    assert_eq!(dsh_config(&["--patch-file", file, "--bridge-exe", EXE, "--check"]), (0, "unchanged".into(), String::new()));
    let backups = fs::read_dir(root.0.join("profiles/web")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".bak-")).count();
    assert_eq!(backups, 0, "an unchanged file must not be rewritten or backed up");
    // another bridge path: --check reports the difference without touching the file
    assert_eq!(dsh_config(&["--patch-file", file, "--bridge-exe", "/elsewhere/her", "--check"]).0, 1);
    assert_eq!(fs::read_to_string(file).unwrap(), expected_block());
}

#[test]
fn dsh_config_replaces_only_its_block_keeps_the_rest_and_backs_up() {
    let root = temp_root("dsh-config-keep");
    let file = root.0.join("cordis.patch.yml");
    let before = "\u{feff}# user comment\r\n- insert:\r\n    - id: other-plugin\r\n      name: x\r\n# === qq-bridge MCP BEGIN ===\n- insert:\n    - id: mcp-snowluma-host\n      config:\n        command: 'node'\n# === qq-bridge MCP END ===\n# tail\r\n";
    fs::write(&file, before).unwrap();
    assert_eq!(dsh_config(&["--patch-file", file.to_str().unwrap(), "--bridge-exe", EXE]).1, "written");
    let after = fs::read_to_string(&file).unwrap();
    assert_eq!(after, format!("# user comment\r\n- insert:\r\n    - id: other-plugin\r\n      name: x\r\n{}# tail\r\n", expected_block()));
    let backup = fs::read_dir(&root.0).unwrap().map(|e| e.unwrap().path()).find(|p| p.to_string_lossy().contains(".bak-")).expect("a backup of the original");
    assert_eq!(fs::read_to_string(backup).unwrap(), before);
}

#[test]
fn dsh_config_refuses_unowned_managed_ids_and_broken_markers_without_writing() {
    let root = temp_root("dsh-config-refuse");
    let file = root.0.join("cordis.patch.yml");
    for original in [
        "- insert:\n    - id: mcp-web-search-safe\n      config:\n        command: node\n",
        "# === qq-bridge MCP BEGIN ===\n",
        "# === qq-bridge MCP END ===\n# === qq-bridge MCP BEGIN ===\n",
        "# === qq-bridge MCP BEGIN ===\n# === qq-bridge MCP END ===\n- insert:\n    - id: 'mcp-snowluma'\n",
    ] {
        fs::write(&file, original).unwrap();
        let (code, _, stderr) = dsh_config(&["--patch-file", file.to_str().unwrap(), "--bridge-exe", EXE]);
        assert_eq!(code, 2, "{original:?}: {stderr}");
        assert!(stderr.contains("nothing was changed"), "{stderr}");
        assert_eq!(fs::read_to_string(&file).unwrap(), original);
    }
    assert_eq!(dsh_config(&["--bridge-exe", EXE]).0, 2);
    assert_eq!(dsh_config(&["--patch-file", file.to_str().unwrap(), "--bogus"]).0, 2);
}

// ----- snowluma_status against a started loopback gateway ---------------------------------------------------

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

enum Reply {
    Body(u16, &'static str),
    Stall,
}

/// A one-connection-per-request gateway fake: answers the scripted replies in order and records raw requests.
fn fake_gateway(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&log);
    std::thread::spawn(move || {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            seen.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_string());
            match reply {
                Reply::Body(status, body) => {
                    let _ = write!(stream, "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                }
                Reply::Stall => std::thread::sleep(std::time::Duration::from_secs(8)),
            }
        }
    });
    (url, log)
}

fn status_text(mcp: &mut Mcp, id: u64) -> String {
    let answer = mcp.call(id, "snowluma_status", json!({}));
    assert!(answer["result"]["isError"].is_null(), "snowluma_status never reports isError: {answer}");
    answer["result"]["content"][0]["text"].as_str().unwrap().to_string()
}

const UNREACHABLE_HINT: &str = "SnowLuma 网关不可达。请自行启动 SnowLuma（本程序不代装、不代启，原因见项目文档的合规说明 L6）。 若 config.json 的 snowluma.httpUrl 与实际端口不一致，也请一并检查。";

#[test]
fn status_reports_the_online_account_and_authenticates_with_the_configured_token() {
    let (url, log) = fake_gateway(vec![
        Reply::Body(200, r#"{"status":"ok","retcode":0,"data":{"user_id":10001,"nickname":"机器人"}}"#),
        Reply::Body(200, r#"{"status":"failed","retcode":1200,"wording":"not logged in"}"#),
    ]);
    let root = temp_root("status-online");
    write_root(&root.0, &url, json!({"snowluma": {"accessToken": "gw-token", "httpUrl": "http://127.0.0.1:1"}}));
    let mut mcp = Mcp::start("--mcp-host-serve", &root.0);
    mcp.initialize();
    assert_eq!(status_text(&mut mcp, 2), "{\n  \"reachable\": true,\n  \"online\": true,\n  \"user_id\": 10001,\n  \"nickname\": \"机器人\"\n}");
    assert_eq!(status_text(&mut mcp, 3), "{\n  \"reachable\": true,\n  \"online\": false,\n  \"retcode\": 1200,\n  \"wording\": \"not logged in\"\n}");
    let requests = log.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "the selected dependency is used, not snowluma.httpUrl");
    assert!(requests[0].starts_with("GET /get_login_info HTTP/1.1"), "{}", requests[0]);
    assert!(requests[0].to_ascii_lowercase().contains("authorization: bearer gw-token"), "{}", requests[0]);
    assert_eq!(mcp.finish(), Some(0));
}

/// Gap batch small-parity item 5 (accepted deviation 18): a lone surrogate escape in the OneBot answer does
/// not make the gateway look unreachable; it is replaced with U+FFFD.
#[test]
fn status_survives_a_lone_surrogate_in_the_login_info() {
    let (url, _) = fake_gateway(vec![Reply::Body(
        200,
        r#"{"status":"ok","retcode":0,"data":{"user_id":10001,"nickname":"a\ud83db"}}"#,
    )]);
    let root = temp_root("status-surrogate");
    write_root(&root.0, &url, json!({"snowluma": {"httpUrl": "http://127.0.0.1:1"}}));
    let mut mcp = Mcp::start("--mcp-host-serve", &root.0);
    mcp.initialize();
    assert_eq!(
        status_text(&mut mcp, 2),
        "{\n  \"reachable\": true,\n  \"online\": true,\n  \"user_id\": 10001,\n  \"nickname\": \"a\u{FFFD}b\"\n}"
    );
    assert_eq!(mcp.finish(), Some(0));
}

#[test]
fn status_reports_gateway_failures_with_the_operator_hint() {
    let (url, _) = fake_gateway(vec![Reply::Body(426, "upgrade"), Reply::Body(200, "<html>")]);
    let root = temp_root("status-down");
    write_root(&root.0, &url, json!({}));
    let mut mcp = Mcp::start("--mcp-host-serve", &root.0);
    mcp.initialize();
    assert_eq!(status_text(&mut mcp, 2), format!("{{\n  \"reachable\": false,\n  \"httpStatus\": 426,\n  \"hint\": {}\n}}", serde_json::to_string(UNREACHABLE_HINT).unwrap()));
    let bad_json: Value = serde_json::from_str(&status_text(&mut mcp, 3)).unwrap();
    assert_eq!(bad_json["reachable"], false);
    assert!(bad_json["error"].as_str().unwrap().starts_with("Unexpected token"), "{bad_json}");
    assert_eq!(bad_json["hint"], UNREACHABLE_HINT);
    drop(mcp);

    // nothing listens: the gateway is unreachable ("fetch failed"); a configured homeDir drops the port hint
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let root = temp_root("status-closed");
    write_root(&root.0, &dead, json!({"snowluma": {"homeDir": "C:/snowluma"}}));
    let mut mcp = Mcp::start("--mcp-host-serve", &root.0);
    mcp.initialize();
    let text = status_text(&mut mcp, 2);
    assert_eq!(
        text,
        "{\n  \"reachable\": false,\n  \"error\": \"fetch failed\",\n  \"hint\": \"SnowLuma 网关不可达。请自行启动 SnowLuma（本程序不代装、不代启，原因见项目文档的合规说明 L6）。\"\n}"
    );
    assert_eq!(mcp.finish(), Some(0));
}

#[test]
fn a_stalled_gateway_is_abandoned_after_five_seconds() {
    let (url, _) = fake_gateway(vec![Reply::Stall]);
    let root = temp_root("status-stall");
    write_root(&root.0, &url, json!({}));
    let mut mcp = Mcp::start("--mcp-host-serve", &root.0);
    mcp.initialize();
    let started = std::time::Instant::now();
    let parsed: Value = serde_json::from_str(&status_text(&mut mcp, 2)).unwrap();
    let waited = started.elapsed();
    assert_eq!(parsed["error"], "The operation was aborted due to timeout");
    assert!(waited >= std::time::Duration::from_millis(4900) && waited < std::time::Duration::from_secs(7), "{waited:?}");
    assert_eq!(mcp.finish(), Some(0));
}

#[test]
fn startup_errors_exit_with_code_two_and_write_nothing_to_the_protocol_stream() {
    let root = temp_root("startup");
    for (args, why) in [
        (vec!["--mcp-host-serve", "--runtime-root", "/nonexistent-aux-root", "--console-port", "31999"], "runtime root is unavailable"),
        (vec!["--mcp-web-search-serve", "--mcp-serve", "--runtime-root", root.0.to_str().unwrap(), "--console-port", "31999"], "isolated startup mode was selected more than once"),
        (vec!["--mcp-web-search-serve", "--runtime-root", root.0.to_str().unwrap()], "isolated runtime requires an explicit non-zero console port"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_her")).args(&args).env_remove("HER_RUNTIME_ROOT").output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(why), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}
