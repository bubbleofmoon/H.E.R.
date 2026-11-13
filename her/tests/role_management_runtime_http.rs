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

const OWNER_ID: i64 = 7001;
const ROLE_POST_MAX_BODY_BYTES: usize = 1_000_000;

struct FixtureWorkspace(PathBuf);

impl FixtureWorkspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let root =
            crate::install_root::development_root(std::env::temp_dir().join(format!("qqbot-role-http-{}-{stamp}", std::process::id())));
        fs::create_dir_all(&root).expect("create synthetic role HTTP workspace");
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

fn prepare_runtime_root(root: &Path, console_token: &str, marker: &str) -> PathBuf {
    let checked = crate::install_root::development_root(root);
    let root = checked.as_path();
    for relative in ["config", "data", "resources/roles", "user/dsh"] {
        fs::create_dir_all(root.join(relative)).expect("create selected runtime directory");
    }

    let config = json!({
        "consoleToken": console_token,
        "ownerQQ": OWNER_ID,
        "allow": { "groups": [], "private": [OWNER_ID.to_string()] },
        "role": { "maxInjectChars": 6000, "profile": format!("profile-{marker}") },
        "configMarker": marker,
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
    fs::write(
        root.join("data/social-v2.json"),
        serde_json::to_vec(&json!({ "conversations": {} }))
            .expect("serialize synthetic session state"),
    )
    .expect("write synthetic session state");

    let dsh_home = root.join("ambient-dsh-home");
    fs::create_dir_all(&dsh_home).expect("create synthetic ambient DSH home");
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

fn captured_output(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("<unable to read {}: {error}>", path.display()),
    }
}

fn startup_failure_diagnostics(
    label: &str,
    root: &Path,
    port: u16,
    last_health_probe: &str,
    detail: &str,
    stdout_path: &Path,
    stderr_path: &Path,
) -> String {
    format!(
        "console startup failed (label={label:?}, root={}, port={port}): {detail}\n\
         last health probe outcome: {last_health_probe}\n\
         stdout ({}):\n{}\n\
         stderr ({}):\n{}",
        root.display(),
        stdout_path.display(),
        captured_output(stdout_path),
        stderr_path.display(),
        captured_output(stderr_path),
    )
}

struct ConsoleProcess {
    child: Child,
    port: u16,
    label: String,
    root: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    last_health_probe: String,
}

impl ConsoleProcess {
    fn start(label: &str, root: &Path, port: u16, dsh_home: &Path) -> Self {
        let stdout_path = root.join("console.stdout.log");
        let stderr_path = root.join("console.stderr.log");
        let stdout_file = fs::File::create(&stdout_path).unwrap_or_else(|error| {
            panic!(
                "{}",
                startup_failure_diagnostics(
                    label,
                    root,
                    port,
                    "not attempted",
                    &format!("could not create stdout capture file: {error}"),
                    &stdout_path,
                    &stderr_path,
                )
            )
        });
        let stderr_file = fs::File::create(&stderr_path).unwrap_or_else(|error| {
            panic!(
                "{}",
                startup_failure_diagnostics(
                    label,
                    root,
                    port,
                    "not attempted",
                    &format!("could not create stderr capture file: {error}"),
                    &stdout_path,
                    &stderr_path,
                )
            )
        });
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
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        hide_process_window(&mut command);
        let child = command.spawn().unwrap_or_else(|error| {
            panic!(
                "{}",
                startup_failure_diagnostics(
                    label,
                    root,
                    port,
                    "not attempted",
                    &format!("could not spawn selected-root console process: {error}"),
                    &stdout_path,
                    &stderr_path,
                )
            )
        });
        let mut process = Self {
            child,
            port,
            label: label.to_string(),
            root: root.to_path_buf(),
            stdout_path,
            stderr_path,
            last_health_probe: "not attempted".to_string(),
        };
        let health = match wait_for_health(&mut process) {
            Ok(health) => health,
            Err(error) => {
                let cleanup = process
                    .stop()
                    .map(|()| "child stopped after failed health check".to_string())
                    .unwrap_or_else(|cleanup_error| format!("cleanup failed: {cleanup_error}"));
                panic!("{}", process.startup_failure_diagnostics(&format!("{error}; {cleanup}")));
            }
        };
        assert_eq!(
            health["ok"],
            true,
            "{}",
            process.startup_failure_diagnostics("health response had `ok` != true")
        );
        assert_eq!(
            health["port"],
            port,
            "{}",
            process.startup_failure_diagnostics("health response reported a different port")
        );
        assert_eq!(
            health["runtime"]["mode"],
            "isolated",
            "{}",
            process.startup_failure_diagnostics("health response reported a different runtime mode")
        );
        assert_eq!(
            health["runtime"]["configLoaded"],
            true,
            "{}",
            process.startup_failure_diagnostics("health response reported configLoaded != true")
        );
        assert_eq!(
            health["runtime"]["resourcesConfigured"],
            true,
            "{}",
            process.startup_failure_diagnostics(
                "health response reported resourcesConfigured != true"
            )
        );
        process
    }

    fn startup_failure_diagnostics(&self, detail: &str) -> String {
        startup_failure_diagnostics(
            &self.label,
            &self.root,
            self.port,
            &self.last_health_probe,
            detail,
            &self.stdout_path,
            &self.stderr_path,
        )
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
    body: Option<&Value>,
) -> Result<HttpResponse, String> {
    let body = body.map(Value::to_string).unwrap_or_default();
    http_request_raw_body(port, method, path, console_token, &body)
}

fn http_request_raw_body(
    port: u16,
    method: &str,
    path: &str,
    console_token: Option<&str>,
    body: &str,
) -> Result<HttpResponse, String> {
    let token_header = console_token
        .map(|token| format!("X-Console-Token: {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{token_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    read_http_response(&mut stream)
}

fn http_request_declared_body_length(
    port: u16,
    method: &str,
    path: &str,
    console_token: &str,
    declared_length: usize,
) -> Result<HttpResponse, String> {
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Console-Token: {console_token}\r\nContent-Type: application/json\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|_| "connect to selected console port for declared-length test".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| "set declared-length response timeout".to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|_| "write declared-length-only HTTP request".to_string())?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|_| "shutdown declared-length request write side".to_string())?;
    read_http_response(&mut stream)
}

fn http_request_chunked_body(
    port: u16,
    method: &str,
    path: &str,
    console_token: &str,
    body: &[u8],
) -> Result<HttpResponse, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|_| "connect to selected console port for chunked request".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| "set selected console chunked read timeout".to_string())?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Console-Token: {console_token}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_| "write selected console chunked headers".to_string())?;
    for chunk in body.chunks(5) {
        write!(stream, "{:x}\r\n", chunk.len())
            .map_err(|_| "write selected console chunk size".to_string())?;
        stream
            .write_all(chunk)
            .map_err(|_| "write selected console chunk body".to_string())?;
        stream
            .write_all(b"\r\n")
            .map_err(|_| "write selected console chunk delimiter".to_string())?;
    }
    stream
        .write_all(b"0\r\n\r\n")
        .map_err(|_| "finish selected console chunked body".to_string())?;
    read_http_response(&mut stream)
}


fn read_http_response(stream: &mut TcpStream) -> Result<HttpResponse, String> {
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
        if let Some(status) = process
            .child
            .try_wait()
            .map_err(|error| format!("check selected console process: {error}"))?
        {
            return Err(format!(
                "selected console exited before health was ready with {status}"
            ));
        }
        match http_request(process.port, "GET", "/api/health", None, None) {
            Ok(response) => {
                process.last_health_probe = format!("HTTP status {}", response.status);
                if response.status == 200 {
                    return serde_json::from_str(&response.body).map_err(|_| {
                        process.last_health_probe = "HTTP 200 with invalid JSON".to_string();
                        "health response was not JSON".to_string()
                    });
                }
            }
            Err(error) => process.last_health_probe = format!("request failed: {error}"),
        }
        thread::sleep(Duration::from_millis(50));
    }

    match process.child.try_wait() {
        Ok(Some(status)) => Err(format!(
            "selected console health endpoint timed out; final child exit check: {status}"
        )),
        Ok(None) => Err(
            "selected console health endpoint timed out; final child exit check: still running"
                .to_string(),
        ),
        Err(error) => Err(format!(
            "selected console health endpoint timed out; final child exit check failed: {error}"
        )),
    }
}

fn expect_json(response: HttpResponse, status: u16, context: &str) -> Value {
    assert_eq!(response.status, status, "{context}: {}", response.body);
    serde_json::from_str(&response.body)
        .unwrap_or_else(|error| panic!("{context} response was not JSON: {error}"))
}

fn post_json(port: u16, token: &str, path: &str, body: &Value, status: u16) -> Value {
    let response = http_request(port, "POST", path, Some(token), Some(body))
        .unwrap_or_else(|error| panic!("POST {path} failed at transport: {error}"));
    expect_json(response, status, &format!("POST {path}"))
}

fn assert_http_error(response: HttpResponse, status: u16, error: &str, context: &str) {
    assert_eq!(response.status, status, "{context}: {}", response.body);
    let body: Value = serde_json::from_str(&response.body)
        .unwrap_or_else(|parse_error| panic!("{context} response was not JSON: {parse_error}"));
    assert_eq!(body, json!({ "ok": false, "error": error }), "{context}");
}

fn post_raw_error(port: u16, token: &str, path: &str, body: &str, status: u16, error: &str) {
    let response = http_request_raw_body(port, "POST", path, Some(token), body).unwrap_or_else(
        |transport_error| panic!("POST {path} failed at transport: {transport_error}"),
    );
    assert_http_error(
        response,
        status,
        error,
        &format!("POST {path} with raw JSON body"),
    );
}

fn get_json(port: u16, token: &str, path: &str, status: u16) -> Value {
    let response = http_request(port, "GET", path, Some(token), None)
        .unwrap_or_else(|error| panic!("GET {path} failed at transport: {error}"));
    expect_json(response, status, &format!("GET {path}"))
}

fn selected_state(root: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(root.join("data/current-role.json")).expect("read selected current-role state"),
    )
    .expect("parse selected current-role state")
}

fn assert_role_names(roles: &Value, expected: &[&str], current: Option<&str>) {
    let names: Vec<&str> = roles["roles"]
        .as_array()
        .expect("role list is an array")
        .iter()
        .map(|name| name.as_str().expect("role name is a string"))
        .collect();
    assert_eq!(names.as_slice(), expected);
    match current {
        Some(role) => assert_eq!(roles["current"], role),
        None => assert!(roles["current"].is_null()),
    }
}

fn create_directory_reparse_point(link: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let output = Command::new("cmd.exe")
            .arg("/d")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .creation_flags(0x0800_0000)
            .output()
            .map_err(|error| format!("launch synthetic junction helper: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        return Err(format!(
            "synthetic junction creation failed with {:?}; stdout={:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .map_err(|error| format!("create synthetic directory symlink: {error}"))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (link, target);
        Err("directory reparse-point fixture is unsupported on this platform".to_string())
    }
}

struct DirectoryReparsePoint(PathBuf);

impl Drop for DirectoryReparsePoint {
    fn drop(&mut self) {
        #[cfg(windows)]
        let _ = fs::remove_dir(&self.0);
        #[cfg(unix)]
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn role_management_lifecycle_uses_each_selected_runtime_root() {
    let workspace = FixtureWorkspace::new();
    let root_a = workspace.root("selected role root A");
    let root_b = workspace.root("selected role root B");
    let token_a = "public-role-console-token-a";
    let token_b = "public-role-console-token-b";
    let dsh_home_a = prepare_runtime_root(&root_a, token_a, "config-a");
    let dsh_home_b = prepare_runtime_root(&root_b, token_b, "config-b");
    let config_a_path = root_a.join("config/config.json");
    let config_b_path = root_b.join("config/config.json");
    let config_a_initial = fs::read(&config_a_path).expect("snapshot A selected config");
    let config_b_initial = fs::read(&config_b_path).expect("snapshot B selected config");

    // This is the path a raw `roles/../../outside.md` could reach if name sanitization or
    // selected-area containment were bypassed. Keep it inside the synthetic runtime root but
    // outside its `resources` area; no production path is involved.
    let outside_sentinel = root_a.join("outside.md");
    fs::write(&outside_sentinel, b"outside resource-area sentinel")
        .expect("write synthetic outside-resource sentinel");
    let outside_sentinel_before = fs::read(&outside_sentinel).expect("snapshot outside sentinel");

    let (port_a, port_b) = reserve_distinct_ports();
    let mut console_a = ConsoleProcess::start(
        "role lifecycle root A",
        &root_a,
        port_a,
        &dsh_home_a,
    );
    let mut console_b = ConsoleProcess::start(
        "role lifecycle root B",
        &root_b,
        port_b,
        &dsh_home_b,
    );
    assert_ne!(console_a.port, console_b.port);

    let initial_a = "A original persona\nselected-root alpha";
    let initial_b = "B original persona\nselected-root beta";
    for (process, token, content, root, foreign_marker) in [
        (
            &console_a,
            token_a,
            initial_a,
            &root_a,
            "selected-root beta",
        ),
        (
            &console_b,
            token_b,
            initial_b,
            &root_b,
            "selected-root alpha",
        ),
    ] {
        let created = post_json(
            process.port,
            token,
            "/api/roles/create",
            &json!({ "name": "Shared", "content": content }),
            200,
        );
        assert_eq!(created["ok"], true);
        assert_eq!(created["role"], "Shared");
        assert!(created["warnings"].is_array());
        let role_file = root.join("resources/roles/Shared.md");
        assert_eq!(
            fs::read_to_string(&role_file).expect("read created selected role"),
            format!("{content}\n")
        );
        assert!(!fs::read_to_string(role_file)
            .unwrap()
            .contains(foreign_marker));
    }

    let a_roles = get_json(port_a, token_a, "/api/roles", 200);
    let b_roles = get_json(port_b, token_b, "/api/roles", 200);
    assert_role_names(&a_roles, &["Shared"], None);
    assert_role_names(&b_roles, &["Shared"], None);
    assert_eq!(a_roles["limits"]["nameMax"], 40);
    assert_eq!(a_roles["limits"]["contentMaxBytes"], 64 * 1024);

    let content_a = get_json(port_a, token_a, "/api/roles/content?name=Shared", 200);
    let content_b = get_json(port_b, token_b, "/api/roles/content?name=Shared", 200);
    assert_eq!(content_a["name"], "Shared");
    assert_eq!(content_a["content"], format!("{initial_a}\n"));
    assert_eq!(content_b["name"], "Shared");
    assert_eq!(content_b["content"], format!("{initial_b}\n"));
    assert_eq!(content_a["bytes"], format!("{initial_a}\n").len());
    assert_eq!(
        content_a["chars"],
        format!("{initial_a}\n").encode_utf16().count()
    );

    let wrong_console_token = get_json(port_a, token_b, "/api/roles/content?name=Shared", 401);
    assert_eq!(wrong_console_token["ok"], false);
    let wrong_create = post_json(
        port_a,
        token_b,
        "/api/roles/create",
        &json!({ "name": "Unauthorized", "content": "must not be written" }),
        401,
    );
    assert_eq!(wrong_create["ok"], false);
    assert!(!root_a.join("resources/roles/Unauthorized.md").exists());

    let empty_name = post_json(
        port_a,
        token_a,
        "/api/roles/create",
        &json!({ "name": "***", "content": "invalid name" }),
        400,
    );
    assert_eq!(
        empty_name["error"],
        "人格名不能为空（仅限中文 / 字母 / 数字 / 横线）"
    );
    let reserved_name = post_json(
        port_a,
        token_a,
        "/api/roles/create",
        &json!({ "name": "README", "content": "reserved name" }),
        400,
    );
    assert_eq!(reserved_name["error"], "「README」是保留名称");
    let device_name = post_json(
        port_a,
        token_a,
        "/api/roles/create",
        &json!({ "name": "CON", "content": "device name" }),
        400,
    );
    assert_eq!(
        device_name["error"],
        "「CON」是系统保留名称（Windows 设备名），请换一个"
    );
    assert!(!root_a.join("resources/roles/README.md").exists());
    assert!(!root_a.join("resources/roles/CON.md").exists());

    let duplicate = post_json(
        port_a,
        token_a,
        "/api/roles/create",
        &json!({ "name": "Shared", "content": "must not replace A" }),
        400,
    );
    assert_eq!(duplicate["error"], "人格「Shared」已存在");
    assert_eq!(
        fs::read_to_string(root_a.join("resources/roles/Shared.md")).unwrap(),
        format!("{initial_a}\n")
    );

    let sanitized_create = post_json(
        port_a,
        token_a,
        "/api/roles/create",
        &json!({ "name": "../../outside", "content": "sanitized safe basename" }),
        200,
    );
    assert_eq!(sanitized_create["role"], "outside");
    assert_eq!(
        fs::read_to_string(root_a.join("resources/roles/outside.md")).unwrap(),
        "sanitized safe basename\n"
    );
    assert_eq!(
        fs::read(&outside_sentinel).expect("outside sentinel survives sanitized create"),
        outside_sentinel_before
    );
    let sanitized_read = get_json(
        port_a,
        token_a,
        "/api/roles/content?name=../../outside",
        200,
    );
    assert_eq!(sanitized_read["name"], "outside");
    assert_eq!(sanitized_read["content"], "sanitized safe basename\n");

    let missing_role = get_json(port_a, token_a, "/api/roles/content?name=Missing", 404);
    assert_eq!(missing_role["error"], "人格「Missing」不存在");
    let sanitized_missing = get_json(port_a, token_a, "/api/roles/content?name=../../absent", 404);
    assert_eq!(sanitized_missing["error"], "人格「absent」不存在");
    let invalid_read_name = get_json(port_a, token_a, "/api/roles/content?name=README", 400);
    assert_eq!(invalid_read_name["error"], "「README」是保留名称");

    let updated_a = "A revised persona\n中文 and 😀";
    let updated_b = "B revised persona\n另一组选定根";
    for (process, token, content) in [
        (&console_a, token_a, updated_a),
        (&console_b, token_b, updated_b),
    ] {
        let updated = post_json(
            process.port,
            token,
            "/api/roles/update",
            &json!({ "name": "Shared", "content": content }),
            200,
        );
        assert_eq!(updated["ok"], true);
        assert_eq!(updated["role"], "Shared");
        assert_eq!(updated["chars"], content.encode_utf16().count());
        assert!(updated["warnings"].is_array());
    }
    assert_eq!(
        fs::read_to_string(root_a.join("resources/roles/Shared.md")).unwrap(),
        format!("{updated_a}\n")
    );
    assert_eq!(
        fs::read_to_string(root_b.join("resources/roles/Shared.md")).unwrap(),
        format!("{updated_b}\n")
    );
    let after_update_a = get_json(port_a, token_a, "/api/roles/content?name=Shared", 200);
    let after_update_b = get_json(port_b, token_b, "/api/roles/content?name=Shared", 200);
    assert_eq!(after_update_a["content"], format!("{updated_a}\n"));
    assert_eq!(after_update_b["content"], format!("{updated_b}\n"));
    assert!(!after_update_a["content"]
        .as_str()
        .unwrap()
        .contains("另一组选定根"));
    assert!(!after_update_b["content"]
        .as_str()
        .unwrap()
        .contains("A revised persona"));

    let wrong_update = post_json(
        port_a,
        token_b,
        "/api/roles/update",
        &json!({ "name": "Shared", "content": "unauthorized overwrite" }),
        401,
    );
    assert_eq!(wrong_update["ok"], false);
    let nonexistent_update = post_json(
        port_a,
        token_a,
        "/api/roles/update",
        &json!({ "name": "Absent", "content": "must not create" }),
        404,
    );
    assert_eq!(nonexistent_update["error"], "人格「Absent」不存在");
    let sanitized_update_missing = post_json(
        port_a,
        token_a,
        "/api/roles/update",
        &json!({ "name": "../../absent", "content": "outside must not change" }),
        404,
    );
    assert_eq!(sanitized_update_missing["error"], "人格「absent」不存在");
    assert_eq!(
        fs::read(&outside_sentinel).expect("outside sentinel survives sanitized update"),
        outside_sentinel_before
    );
    assert_eq!(
        fs::read_to_string(root_a.join("resources/roles/outside.md")).unwrap(),
        "sanitized safe basename\n"
    );

    let invalid_update_content = post_json(
        port_a,
        token_a,
        "/api/roles/update",
        &json!({ "name": "Shared", "content": "   " }),
        400,
    );
    assert_eq!(invalid_update_content["error"], "人格内容不能为空");

    let rename_duplicate = post_json(
        port_a,
        token_a,
        "/api/roles/rename",
        &json!({ "from": "Shared", "to": "outside" }),
        400,
    );
    assert_eq!(rename_duplicate["error"], "人格「outside」已存在");
    let rename_missing = post_json(
        port_b,
        token_b,
        "/api/roles/rename",
        &json!({ "from": "Absent", "to": "RenamedB" }),
        404,
    );
    assert_eq!(rename_missing["error"], "人格「Absent」不存在");
    let rename_invalid = post_json(
        port_a,
        token_a,
        "/api/roles/rename",
        &json!({ "from": "Shared", "to": "***" }),
        400,
    );
    assert_eq!(
        rename_invalid["error"],
        "人格名不能为空（仅限中文 / 字母 / 数字 / 横线）"
    );

    let select_missing = post_json(
        port_a,
        token_a,
        "/api/role",
        &json!({ "role": "Missing" }),
        400,
    );
    assert_eq!(
        select_missing["error"],
        "角色「Missing」不存在（roles/Missing.md）"
    );
    assert!(
        !root_a.join("data/current-role.json").exists(),
        "failed selection must not create or alter role state"
    );

    let mode_a = post_json(
        port_a,
        token_a,
        "/api/role-mode",
        &json!({ "mode": "silent" }),
        200,
    );
    assert_eq!(mode_a, json!({ "ok": true, "roleMode": "silent" }));
    let select_a = post_json(
        port_a,
        token_a,
        "/api/role",
        &json!({ "role": "Shared" }),
        200,
    );
    assert_eq!(select_a["role"], "Shared");
    assert_eq!(
        selected_state(&root_a),
        json!({ "role": "Shared", "mode": "silent" })
    );

    let select_b = post_json(
        port_b,
        token_b,
        "/api/role",
        &json!({ "role": "Shared" }),
        200,
    );
    assert_eq!(select_b["role"], "Shared");
    assert_eq!(
        selected_state(&root_b),
        json!({ "role": "Shared", "mode": "active" })
    );
    let mode_b_silent = post_json(
        port_b,
        token_b,
        "/api/role-mode",
        &json!({ "mode": "silent" }),
        200,
    );
    assert_eq!(mode_b_silent, json!({ "ok": true, "roleMode": "silent" }));
    assert_role_names(
        &get_json(port_b, token_b, "/api/roles", 200),
        &["Shared"],
        Some("Shared"),
    );
    let mode_b_active_response = http_request_raw_body(
        port_b,
        "POST",
        "/api/role-mode",
        Some(token_b),
        "",
    )
    .expect("POST /api/role-mode blank-body request must reach selected console");
    let mode_b_active = expect_json(mode_b_active_response, 200, "blank POST /api/role-mode");
    assert_eq!(mode_b_active, json!({ "ok": true, "roleMode": "active" }));
    assert_role_names(
        &get_json(port_b, token_b, "/api/roles", 200),
        &["Shared"],
        Some("Shared"),
    );
    let mode_b_unsupported_value = post_json(
        port_b,
        token_b,
        "/api/role-mode",
        &json!({ "mode": "unexpected" }),
        200,
    );
    assert_eq!(mode_b_unsupported_value, json!({ "ok": true, "roleMode": "active" }));
    assert_role_names(
        &get_json(port_b, token_b, "/api/roles", 200),
        &["Shared"],
        Some("Shared"),
    );
    assert_eq!(
        selected_state(&root_a),
        json!({ "role": "Shared", "mode": "silent" })
    );
    assert_eq!(
        selected_state(&root_b),
        json!({ "role": "Shared", "mode": "active" })
    );
    assert_eq!(fs::read(&config_a_path).unwrap(), config_a_initial);
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_initial);

    for (process, token, from, to, mode) in [
        (&console_a, token_a, "Shared", "RenamedA", "silent"),
        (&console_b, token_b, "Shared", "RenamedB", "active"),
    ] {
        let renamed = post_json(
            process.port,
            token,
            "/api/roles/rename",
            &json!({ "from": from, "to": to }),
            200,
        );
        assert_eq!(renamed["ok"], true);
        assert_eq!(renamed["role"], to);
        assert_eq!(renamed["renamed"], true);
        assert_eq!(renamed["currentFollowed"], true);
        assert_eq!(
            selected_state(if to == "RenamedA" { &root_a } else { &root_b }),
            json!({ "role": to, "mode": mode })
        );
    }
    assert!(!root_a.join("resources/roles/Shared.md").exists());
    assert!(!root_b.join("resources/roles/Shared.md").exists());
    assert_eq!(
        fs::read_to_string(root_a.join("resources/roles/RenamedA.md")).unwrap(),
        format!("{updated_a}\n")
    );
    assert_eq!(
        fs::read_to_string(root_b.join("resources/roles/RenamedB.md")).unwrap(),
        format!("{updated_b}\n")
    );
    assert_role_names(
        &get_json(port_a, token_a, "/api/roles", 200),
        // Frozen JS `localeCompare('zh-CN')` order: case-insensitive, so "outside" precedes "RenamedA".
        &["outside", "RenamedA"],
        Some("RenamedA"),
    );
    assert_role_names(
        &get_json(port_b, token_b, "/api/roles", 200),
        &["RenamedB"],
        Some("RenamedB"),
    );

    let clear_selection = post_json(port_a, token_a, "/api/role", &json!({}), 200);
    assert_eq!(clear_selection["ok"], true);
    assert!(clear_selection["role"].is_null());
    assert_eq!(
        selected_state(&root_a),
        json!({ "role": null, "mode": "silent" })
    );
    let reselect_a = post_json(
        port_a,
        token_a,
        "/api/role",
        &json!({ "role": "RenamedA" }),
        200,
    );
    assert_eq!(reselect_a["role"], "RenamedA");
    assert_eq!(
        selected_state(&root_a),
        json!({ "role": "RenamedA", "mode": "silent" })
    );

    let limit_a = post_json(
        port_a,
        token_a,
        "/api/roles/limit",
        &json!({ "maxInjectChars": 500 }),
        200,
    );
    assert_eq!(limit_a["ok"], true);
    assert_eq!(limit_a["maxInjectChars"], 1000);
    assert_eq!(limit_a["limits"]["injectMaxChars"], 1000);
    assert_eq!(fs::read(&config_b_path).unwrap(), config_b_initial);
    let config_a_after_limit: Value = serde_json::from_slice(&fs::read(&config_a_path).unwrap())
        .expect("parse A config after role limit update");
    assert_eq!(config_a_after_limit["role"]["maxInjectChars"], 1000);
    assert_eq!(config_a_after_limit["role"]["profile"], "profile-config-a");
    assert_eq!(config_a_after_limit["configMarker"], "config-a");
    assert_eq!(config_a_after_limit["consoleToken"], token_a);

    let limit_a_default = post_json(port_a, token_a, "/api/roles/limit", &json!({}), 200);
    assert_eq!(limit_a_default["maxInjectChars"], 6000);
    assert_eq!(limit_a_default["limits"]["injectMaxChars"], 6000);
    let config_a_after_default_limit: Value =
        serde_json::from_slice(&fs::read(&config_a_path).unwrap())
            .expect("parse A config after default role limit update");
    assert_eq!(config_a_after_default_limit["role"]["maxInjectChars"], 6000);
    assert_eq!(
        config_a_after_default_limit["role"]["profile"],
        "profile-config-a"
    );
    assert_eq!(config_a_after_default_limit["configMarker"], "config-a");
    assert_eq!(config_a_after_default_limit["consoleToken"], token_a);

    let limit_b = post_json(
        port_b,
        token_b,
        "/api/roles/limit",
        &json!({ "maxInjectChars": 25000 }),
        200,
    );
    assert_eq!(limit_b["maxInjectChars"], 20000);
    assert_eq!(limit_b["limits"]["injectMaxChars"], 20000);
    let config_b_after_limit: Value = serde_json::from_slice(&fs::read(&config_b_path).unwrap())
        .expect("parse B config after role limit update");
    assert_eq!(config_b_after_limit["role"]["maxInjectChars"], 20000);
    assert_eq!(config_b_after_limit["role"]["profile"], "profile-config-b");
    assert_eq!(config_b_after_limit["configMarker"], "config-b");
    assert_eq!(config_b_after_limit["consoleToken"], token_b);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&config_a_path).unwrap()).unwrap()["role"]
            ["maxInjectChars"],
        6000
    );

    let delete_missing = post_json(
        port_a,
        token_a,
        "/api/roles/delete",
        &json!({ "name": "Absent" }),
        404,
    );
    assert_eq!(delete_missing["error"], "人格「Absent」不存在");
    let delete_invalid = post_json(
        port_a,
        token_a,
        "/api/roles/delete",
        &json!({ "name": "README" }),
        400,
    );
    assert_eq!(delete_invalid["error"], "「README」是保留名称");

    let deleted_a = post_json(
        port_a,
        token_a,
        "/api/roles/delete",
        &json!({ "name": "RenamedA" }),
        200,
    );
    assert_eq!(deleted_a["ok"], true);
    assert_eq!(deleted_a["deleted"], "RenamedA");
    assert_eq!(deleted_a["clearedCurrent"], true);
    assert!(!root_a.join("resources/roles/RenamedA.md").exists());
    assert_eq!(
        selected_state(&root_a),
        json!({ "role": null, "mode": "silent" })
    );
    assert_eq!(
        selected_state(&root_b),
        json!({ "role": "RenamedB", "mode": "active" })
    );
    assert!(root_b.join("resources/roles/RenamedB.md").exists());

    fs::remove_dir_all(root_a.join("resources/roles"))
        .expect("remove only A selected role-resource directory");
    let missing_resource = get_json(port_a, token_a, "/api/roles", 503);
    assert_eq!(missing_resource["ok"], false);
    assert!(!missing_resource
        .to_string()
        .contains(&root_a.to_string_lossy().to_string()));
    let b_after_a_missing = get_json(port_b, token_b, "/api/roles", 200);
    assert_role_names(&b_after_a_missing, &["RenamedB"], Some("RenamedB"));

    let deleted_b = post_json(
        port_b,
        token_b,
        "/api/roles/delete",
        &json!({ "name": "RenamedB" }),
        200,
    );
    assert_eq!(deleted_b["deleted"], "RenamedB");
    assert_eq!(deleted_b["clearedCurrent"], true);
    assert_eq!(
        selected_state(&root_b),
        json!({ "role": null, "mode": "active" })
    );
    assert_role_names(&get_json(port_b, token_b, "/api/roles", 200), &[], None);
    assert_eq!(
        fs::read(&outside_sentinel).unwrap(),
        outside_sentinel_before
    );

    console_a
        .stop()
        .expect("stop selected A console process within cleanup bound");
    console_b
        .stop()
        .expect("stop selected B console process within cleanup bound");
}

#[test]
fn role_mode_accepts_chunked_body_and_changes_only_selected_mode() {
    let workspace = FixtureWorkspace::new();
    let root_a = workspace.root("chunked role-mode root A");
    let root_b = workspace.root("chunked role-mode root B");
    let token_a = "chunked-role-mode-token-a";
    let token_b = "chunked-role-mode-token-b";
    let dsh_home_a = prepare_runtime_root(&root_a, token_a, "chunked-mode-a");
    let dsh_home_b = prepare_runtime_root(&root_b, token_b, "chunked-mode-b");
    let state_a = root_a.join("data/current-role.json");
    let state_b = root_b.join("data/current-role.json");
    fs::write(&state_a, br#"{"role":"SelectedA","mode":"active"}"#)
        .expect("write selected A role state");
    fs::write(&state_b, br#"{"role":"SelectedB","mode":"silent"}"#)
        .expect("write selected B role state");
    let state_b_before = fs::read(&state_b).expect("snapshot selected B state");
    let (port_a, port_b) = reserve_distinct_ports();
    let mut console_a = ConsoleProcess::start(
        "chunked role-mode root A",
        &root_a,
        port_a,
        &dsh_home_a,
    );
    let mut console_b = ConsoleProcess::start(
        "chunked role-mode root B",
        &root_b,
        port_b,
        &dsh_home_b,
    );

    let response = http_request_chunked_body(
        port_a,
        "POST",
        "/api/role-mode",
        token_a,
        br#"{"mode":"silent"}"#,
    )
    .expect("POST /api/role-mode chunked request reaches selected console");
    let response_json = expect_json(response, 200, "chunked POST /api/role-mode");
    assert_eq!(response_json, json!({ "ok": true, "roleMode": "silent" }));
    let state_a_after = fs::read(&state_a).expect("read selected A role state after chunked post");
    assert_eq!(
        serde_json::from_slice::<Value>(&state_a_after).unwrap(),
        json!({ "role": "SelectedA", "mode": "silent" }),
        "chunked JSON body must reach the role-mode handler and preserve the selected role"
    );
    assert_eq!(
        fs::read(&state_b).expect("read selected B role state after A post"),
        state_b_before,
        "A's chunked request must not change B"
    );
    console_a
        .stop()
        .expect("stop selected A console process within cleanup bound");
    console_b
        .stop()
        .expect("stop selected B console process within cleanup bound");
}

#[test]
fn role_management_rejects_bad_json_shapes_without_selected_root_writes() {
    let workspace = FixtureWorkspace::new();
    let root_a = workspace.root("body-shape root A");
    let root_b = workspace.root("body-shape root B");
    let token_a = "body-shape-console-token-a";
    let token_b = "body-shape-console-token-b";
    let dsh_home_a = prepare_runtime_root(&root_a, token_a, "body-config-a");
    let dsh_home_b = prepare_runtime_root(&root_b, token_b, "body-config-b");
    let config_a = root_a.join("config/config.json");
    let config_b = root_b.join("config/config.json");
    let state_a = root_a.join("data/current-role.json");
    let state_b = root_b.join("data/current-role.json");
    let role_a = root_a.join("resources/roles/BodyA.md");
    let role_b = root_b.join("resources/roles/BodyB.md");
    fs::write(&state_a, br#"{"role":"BodyA","mode":"silent"}"#)
        .expect("write selected A role state fixture");
    fs::write(&state_b, br#"{"role":"BodyB","mode":"active"}"#)
        .expect("write selected B role state fixture");
    fs::write(&role_a, b"A body-shape sentinel\n").expect("write selected A role fixture");
    fs::write(&role_b, b"B body-shape sentinel\n").expect("write selected B role fixture");
    let state_a_before = fs::read(&state_a).expect("snapshot A state");
    let state_b_before = fs::read(&state_b).expect("snapshot B state");
    let config_a_before = fs::read(&config_a).expect("snapshot A config");
    let config_b_before = fs::read(&config_b).expect("snapshot B config");
    let role_a_before = fs::read(&role_a).expect("snapshot A role");
    let role_b_before = fs::read(&role_b).expect("snapshot B role");

    let (port_a, port_b) = reserve_distinct_ports();
    let mut console_a = ConsoleProcess::start("bad JSON root A", &root_a, port_a, &dsh_home_a);
    let mut console_b = ConsoleProcess::start("bad JSON root B", &root_b, port_b, &dsh_home_b);

    let bodies = [
        ("{malformed", "请求体必须是合法 JSON"),
        ("null", "请求体必须是 JSON 对象"),
        ("[]", "请求体必须是 JSON 对象"),
        ("7", "请求体必须是 JSON 对象"),
        ("\"scalar\"", "请求体必须是 JSON 对象"),
        ("true", "请求体必须是 JSON 对象"),
    ];
    let routes = [
        "/api/roles/create",
        "/api/roles/update",
        "/api/roles/rename",
        "/api/roles/delete",
        "/api/role",
        "/api/role-mode",
        "/api/roles/limit",
    ];
    for (route_index, route) in routes.iter().enumerate() {
        for (body_index, (body, expected_error)) in bodies.iter().enumerate() {
            let (port, token) = if (route_index + body_index) % 2 == 0 {
                (port_a, token_a)
            } else {
                (port_b, token_b)
            };
            post_raw_error(port, token, route, body, 400, expected_error);
            assert_eq!(
                fs::read(&state_a).unwrap(),
                state_a_before,
                "A state after {route} body {body}"
            );
            assert_eq!(
                fs::read(&state_b).unwrap(),
                state_b_before,
                "B state after {route} body {body}"
            );
            assert_eq!(
                fs::read(&config_a).unwrap(),
                config_a_before,
                "A config after {route} body {body}"
            );
            assert_eq!(
                fs::read(&config_b).unwrap(),
                config_b_before,
                "B config after {route} body {body}"
            );
            assert_eq!(
                fs::read(&role_a).unwrap(),
                role_a_before,
                "A role after {route} body {body}"
            );
            assert_eq!(
                fs::read(&role_b).unwrap(),
                role_b_before,
                "B role after {route} body {body}"
            );
        }
    }

    let unauthorized_oversized_response = http_request_declared_body_length(
        port_a,
        "POST",
        "/api/role-mode",
        "wrong-console-token",
        ROLE_POST_MAX_BODY_BYTES + 1,
    )
    .expect("unauthorized oversized declaration receives the existing auth response");
    assert_http_error(
        unauthorized_oversized_response,
        401,
        "未授权：请提供控制台访问令牌",
        "POST /api/role-mode oversized Content-Length with invalid console token",
    );

    let oversized_response = http_request_declared_body_length(
        port_a,
        "POST",
        "/api/role-mode",
        token_a,
        ROLE_POST_MAX_BODY_BYTES + 1,
    )
    .expect("oversized declared-length request receives a bounded response without body bytes");
    assert_http_error(
        oversized_response,
        413,
        "请求体过大（超过 1MB）",
        "POST /api/role-mode with oversized declared Content-Length",
    );
    assert_eq!(fs::read(&state_a).unwrap(), state_a_before, "A state after oversized request");
    assert_eq!(fs::read(&state_b).unwrap(), state_b_before, "B state after oversized request");
    assert_eq!(fs::read(&config_a).unwrap(), config_a_before, "A config after oversized request");
    assert_eq!(fs::read(&config_b).unwrap(), config_b_before, "B config after oversized request");
    assert_eq!(fs::read(&role_a).unwrap(), role_a_before, "A role after oversized request");
    assert_eq!(fs::read(&role_b).unwrap(), role_b_before, "B role after oversized request");

    assert_role_names(
        &get_json(port_a, token_a, "/api/roles", 200),
        &["BodyA"],
        Some("BodyA"),
    );
    assert_role_names(
        &get_json(port_b, token_b, "/api/roles", 200),
        &["BodyB"],
        Some("BodyB"),
    );
    console_a
        .stop()
        .expect("stop selected A console process within cleanup bound");
    console_b
        .stop()
        .expect("stop selected B console process within cleanup bound");
}

#[test]
fn role_management_rejects_invalid_selected_state_before_mutations() {
    let workspace = FixtureWorkspace::new();
    let root_a = workspace.root("invalid-state root A");
    let root_b = workspace.root("invalid-state root B");
    let token_a = "invalid-state-console-token-a";
    let token_b = "invalid-state-console-token-b";
    let dsh_home_a = prepare_runtime_root(&root_a, token_a, "invalid-config-a");
    let dsh_home_b = prepare_runtime_root(&root_b, token_b, "invalid-config-b");
    let data_a = root_a.join("data");
    let state_a = data_a.join("current-role.json");
    let state_b = root_b.join("data/current-role.json");
    let role_a = root_a.join("resources/roles/RenamedA.md");
    let role_b = root_b.join("resources/roles/RenamedB.md");
    let valid_state_a = br#"{"role":"RenamedA","mode":"silent"}"#;
    let valid_state_b = br#"{"role":"RenamedB","mode":"active"}"#;
    fs::write(&state_a, valid_state_a).expect("write selected A state fixture");
    fs::write(&state_b, valid_state_b).expect("write selected B state fixture");
    fs::write(&role_a, b"A rename/delete sentinel\n").expect("write selected A role fixture");
    fs::write(&role_b, b"B rename/delete sentinel\n").expect("write selected B role fixture");
    let state_b_before = fs::read(&state_b).expect("snapshot selected B state");
    let role_a_before = fs::read(&role_a).expect("snapshot selected A role");
    let role_b_before = fs::read(&role_b).expect("snapshot selected B role");

    let (port_a, port_b) = reserve_distinct_ports();
    let mut console_a =
        ConsoleProcess::start("invalid selected-state root A", &root_a, port_a, &dsh_home_a);
    let mut console_b =
        ConsoleProcess::start("invalid selected-state root B", &root_b, port_b, &dsh_home_b);

    fs::remove_file(&state_a).expect("remove only A current-role fixture");
    let missing_a = get_json(port_a, token_a, "/api/roles", 200);
    assert_role_names(&missing_a, &["RenamedA"], None);
    assert!(
        !state_a.exists(),
        "a read must retain missing-state default behavior"
    );
    assert_eq!(fs::read(&state_b).unwrap(), state_b_before);
    assert_role_names(
        &get_json(port_b, token_b, "/api/roles", 200),
        &["RenamedB"],
        Some("RenamedB"),
    );
    fs::write(&state_a, valid_state_a).expect("restore selected A state fixture");

    let invalid_states: [(&str, &[u8]); 5] = [
        ("malformed JSON", b"{malformed"),
        ("null document", b"null"),
        ("array document", b"[]"),
        ("number document", b"7"),
        ("string document", b"\"scalar\""),
    ];
    for (label, invalid_bytes) in invalid_states {
        fs::write(&state_a, invalid_bytes).expect("write invalid selected A state");
        let expected_state_bytes = fs::read(&state_a).expect("snapshot invalid A state bytes");

        let list_response = http_request(port_a, "GET", "/api/roles", Some(token_a), None)
            .expect("GET selected A roles with invalid state");
        assert_http_error(
            list_response,
            503,
            "runtime role state is invalid",
            &format!("GET /api/roles with {label}"),
        );
        for (path, body) in [
            ("/api/role", "{}"),
            ("/api/role-mode", r#"{"mode":"active"}"#),
            ("/api/roles/rename", r#"{"from":"RenamedA","to":"MovedA"}"#),
            ("/api/roles/delete", r#"{"name":"RenamedA"}"#),
        ] {
            let response = http_request_raw_body(port_a, "POST", path, Some(token_a), body)
                .unwrap_or_else(|error| panic!("POST {path} with {label} state: {error}"));
            assert_http_error(
                response,
                503,
                "runtime role state is invalid",
                &format!("POST {path} with {label}"),
            );
            assert_eq!(
                fs::read(&state_a).unwrap(),
                expected_state_bytes,
                "A state after {path} with {label}"
            );
            assert_eq!(
                fs::read(&role_a).unwrap(),
                role_a_before,
                "A role after {path} with {label}"
            );
            assert!(!root_a.join("resources/roles/MovedA.md").exists());
            assert_eq!(fs::read(&state_b).unwrap(), state_b_before);
            assert_eq!(fs::read(&role_b).unwrap(), role_b_before);
        }
        assert_role_names(
            &get_json(port_b, token_b, "/api/roles", 200),
            &["RenamedB"],
            Some("RenamedB"),
        );
        fs::write(&state_a, valid_state_a).expect("restore selected A state after invalid fixture");
    }

    let saved_data_a = workspace.root("saved selected data A");
    let outside_data_a = workspace.root("outside selected data A");
    #[cfg(windows)]
    // Retain the live owner's locked handle while allowing this synthetic data
    // directory to move. No second owner starts during this replacement.
    fs::rename(data_a.join("logs/rbridge-console.lock"), root_a.join("parked-console.lock"))
        .expect("park the synthetic owner file before replacing its data directory");
    fs::rename(&data_a, &saved_data_a).expect("move A data aside for a reparse fixture");
    fs::create_dir_all(&outside_data_a).expect("create synthetic outside data directory");
    let outside_state = outside_data_a.join("current-role.json");
    fs::write(&outside_state, b"outside current-role sentinel")
        .expect("write synthetic outside state sentinel");
    let outside_state_before = fs::read(&outside_state).expect("snapshot outside state sentinel");
    create_directory_reparse_point(&data_a, &outside_data_a)
        .unwrap_or_else(|error| panic!("create data-area reparse fixture: {error}"));
    let junction = DirectoryReparsePoint(data_a.clone());

    // A GET first loads the selected config, which since ticket 13 (8b5646a5) also reads
    // `data/mode.json` for the closed-agent owner overlay; a reparse-point data area therefore fails
    // the request closed (503) before the role route runs. POSTs are refused earlier, at the audit
    // target check.
    let list_response = http_request(port_a, "GET", "/api/roles", Some(token_a), None)
        .expect("GET selected A roles through data-area reparse fixture");
    assert_http_error(
        list_response,
        503,
        "runtime config is unavailable",
        "GET /api/roles through selected data reparse point",
    );
    for (path, body) in [
        ("/api/role", "{}"),
        ("/api/role-mode", r#"{"mode":"active"}"#),
        ("/api/roles/rename", r#"{"from":"RenamedA","to":"MovedA"}"#),
        ("/api/roles/delete", r#"{"name":"RenamedA"}"#),
    ] {
        let response = http_request_raw_body(port_a, "POST", path, Some(token_a), body)
            .unwrap_or_else(|error| {
                panic!("POST {path} through data-area reparse fixture: {error}")
            });
        assert_http_error(
            response,
            503,
            "selected runtime audit target is unavailable",
            &format!("POST {path} through selected data reparse point"),
        );
        assert_eq!(
            fs::read(&role_a).unwrap(),
            role_a_before,
            "A role after {path}"
        );
        assert!(!root_a.join("resources/roles/MovedA.md").exists());
        assert_eq!(fs::read(&state_b).unwrap(), state_b_before);
        assert_eq!(fs::read(&role_b).unwrap(), role_b_before);
        assert_eq!(fs::read(&outside_state).unwrap(), outside_state_before);
    }
    assert_role_names(
        &get_json(port_b, token_b, "/api/roles", 200),
        &["RenamedB"],
        Some("RenamedB"),
    );

    drop(junction);
    fs::rename(&saved_data_a, &data_a).expect("restore selected A data directory");
    assert_eq!(fs::read(&state_a).unwrap().as_slice(), valid_state_a);
    console_a
        .stop()
        .expect("stop selected A console process within cleanup bound");
    console_b
        .stop()
        .expect("stop selected B console process within cleanup bound");
}
