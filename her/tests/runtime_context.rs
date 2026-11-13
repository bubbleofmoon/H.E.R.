#[path = "../src/install_root.rs"]
mod install_root;

use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSOLE_TOKEN: &str = "isolated-console-token-02";
const OUTSIDE_SENTINEL: &str = "outside-runtime-fixture-sentinel-02";

// A failed assertion must not leave her.exe running and locked for the next build.
struct ChildGuard(Option<Child>);
impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.0.as_ref().unwrap()
    }
}
impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }
}
impl ChildGuard {
    fn wait_with_output(mut self) -> std::io::Result<Output> {
        self.0.take().unwrap().wait_with_output()
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            stop(child);
        }
    }
}

struct OneBotServer {
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl OneBotServer {
    fn start(listener: TcpListener) -> Self {
        use std::io::BufRead;
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (stopped, seen) = (stop.clone(), requests.clone());
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !stopped.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Winsock inherits the listener's nonblocking mode.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut reader = std::io::BufReader::new(&stream);
                        let mut line = String::new();
                        reader
                            .read_line(&mut line)
                            .expect("read live OneBot request");
                        let request_line = line.trim_end().to_string();
                        let mut length = 0;
                        loop {
                            line.clear();
                            reader.read_line(&mut line).unwrap();
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                            if let Some((key, value)) = line.split_once(':') {
                                if key.eq_ignore_ascii_case("content-length") {
                                    length = value.trim().parse::<usize>().unwrap();
                                }
                            }
                        }
                        reader.read_exact(&mut vec![0; length]).unwrap();
                        let body = r#"{"status":"ok","retcode":0,"data":[]}"#;
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                        seen.lock().unwrap().push(request_line);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("accept fake OneBot request: {error}"),
                }
            }
        });
        Self {
            stop,
            requests,
            worker: Some(worker),
        }
    }

    fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.requests.lock().unwrap().len() < count {
            assert!(
                Instant::now() < deadline,
                "startup warm-up request did not complete"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for OneBotServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.expect("join fake OneBot");
        }
    }
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

fn fixture_root() -> TempRoot {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
        "qqbot-runtime-context-{}-{stamp}",
        std::process::id()
    )));
    fs::create_dir_all(root.join("config")).expect("create isolated config directory");
    fs::create_dir_all(root.join("data")).expect("create isolated data directory");
    fs::create_dir_all(root.join("resources")).expect("create isolated resource directory");
    fs::write(
        root.join("config/config.json"),
        format!(
            r#"{{"consoleToken":"{CONSOLE_TOKEN}","ownerQQ":9001,"allow":{{"groups":[7123],"private":[]}}}}"#
        ),
    )
    .expect("write isolated config fixture");
    fs::write(
        root.join("data/social-v2.json"),
        r#"{"paused":true,"conversations":{}}"#,
    )
    .expect("write isolated state fixture");
    // A persisted mode must remain readable after Windows startup ACL hardening,
    // just like the other imported runtime state.
    fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    fs::write(
        root.join("resources/fixture.txt"),
        "isolated-resource-marker",
    )
    .expect("write isolated resource fixture");
    TempRoot(root)
}

fn fake_dependency() -> (TcpListener, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake dependency endpoint");
    let address = listener.local_addr().expect("read fake endpoint address");
    (listener, format!("http://127.0.0.1:{}", address.port()))
}

fn write_dependencies(root: &Path, onebot: &str, dsh: &str, model: &str) {
    let value = serde_json::json!({
        "onebot": onebot,
        "dsh": dsh,
        "model": model,
    });
    fs::write(root.join("dependencies.json"), value.to_string())
        .expect("write isolated dependency fixture");
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve loopback port");
    listener.local_addr().expect("read candidate port").port()
}

fn start_console(root: &Path, port: u16, via_legacy_args: bool) -> ChildGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--console-serve")
        .arg("--console-port")
        .arg(port.to_string())
        .env_remove("HER_RUNTIME_ROOT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if via_legacy_args {
        command.env("HER_RUNTIME_ROOT", root);
    } else {
        command.arg("--runtime-root").arg(root);
    }
    hide_process_window(&mut command);
    ChildGuard(Some(command.spawn().expect("start isolated console candidate")))
}

fn start_console_without_port(root: &Path) -> ChildGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--console-serve")
        .arg("--runtime-root")
        .arg(root)
        .env_remove("HER_RUNTIME_ROOT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_process_window(&mut command);
    ChildGuard(Some(command
        .spawn()
        .expect("start isolated console without a port")))
}
fn wait_for_health(child: &mut Child, port: u16) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if child
            .try_wait()
            .map_err(|_| "check candidate process".to_string())?
            .is_some()
        {
            return Err(
                "isolated console exited before the health endpoint became ready".to_string(),
            );
        }
        if let Ok((status, body)) = request(port, "GET", "/api/health", None) {
            if status == 200 {
                return serde_json::from_str(&body)
                    .map_err(|_| "health response was not JSON".to_string());
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("isolated console health endpoint did not become ready".to_string())
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|_| "connect to candidate HTTP port".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "set candidate HTTP read timeout".to_string())?;
    let auth = token
        .map(|value| format!("X-Console-Token: {value}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Connection: close\r\n\r\n"
    )
    .map_err(|_| "write candidate HTTP request".to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|_| "read candidate HTTP response".to_string())?;
    let response = String::from_utf8(response)
        .map_err(|_| "candidate HTTP response was not UTF-8".to_string())?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "candidate HTTP response had no header boundary".to_string())?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "candidate HTTP status was invalid".to_string())?;
    Ok((status, body.to_string()))
}

fn stop(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn require_exit_within(child: &mut Child, context: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                stop(child);
                panic!("{context} candidate did not exit within five seconds");
            }
            Err(_) => {
                stop(child);
                panic!("could not check whether the {context} candidate exited");
            }
        }
    }
}

#[cfg(windows)]
struct TestJunction(PathBuf);

#[cfg(windows)]
impl Drop for TestJunction {
    fn drop(&mut self) {
        // remove_dir removes the junction itself; it must never traverse its target.
        let _ = fs::remove_dir(&self.0);
    }
}

#[cfg(windows)]
fn create_test_junction(link: &Path, target: &Path) -> TestJunction {
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
        .expect("run hidden cmd.exe to create a synthetic test junction");
    if !output.status.success() {
        let _ = fs::remove_dir(link);
        panic!("could not create the test junction");
    }
    let junction = TestJunction(link.to_path_buf());
    assert!(
        fs::metadata(link)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false),
        "test junction target must be a directory"
    );
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(link).expect("inspect test junction metadata");
    assert_ne!(
        metadata.file_attributes() & 0x400,
        0,
        "test path must be a Windows reparse point"
    );
    junction
}

fn assert_status_reads_fixture(port: u16) {
    let (status, body) = request(port, "GET", "/api/status", Some(CONSOLE_TOKEN))
        .expect("request isolated status endpoint");
    assert_eq!(status, 200, "status endpoint must be available: {body}");
    let body: Value = serde_json::from_str(&body).expect("status body is JSON");
    assert_eq!(
        body["ownerQQ"], 9001,
        "status must load the selected config file"
    );
    assert_eq!(body["allowGroups"], serde_json::json!(["7123"]));
    assert_eq!(
        body["socialV2Paused"], true,
        "status must load state from the selected data directory"
    );
}

#[test]
fn isolated_runtime_starts_health_and_status_without_escaping_to_legacy_paths() {
    let root = fixture_root();
    let (onebot_listener, onebot_url) = fake_dependency();
    let (dsh_listener, dsh_url) = fake_dependency();
    let (model_listener, model_url) = fake_dependency();
    write_dependencies(&root, &onebot_url, &dsh_url, &model_url);
    // Drain requests while the client is alive: killing it before accept/read can
    // reset queued sockets on Windows and discard the request bytes.
    let onebot = OneBotServer::start(onebot_listener);

    let port = free_port();
    let mut child = start_console(&root, port, false);
    let health = wait_for_health(&mut child, port).unwrap_or_else(|error| {
        stop(&mut child);
        panic!("{error}");
    });
    assert_eq!(health["ok"], true);
    assert_eq!(health["port"], port);
    assert_eq!(health["runtime"]["mode"], "isolated");
    assert_eq!(
        health["runtime"]["dependencyNames"],
        serde_json::json!(["dsh", "model", "onebot"]),
        "health must expose safe dependency identities, never endpoint URLs"
    );
    assert_eq!(health["runtime"]["configLoaded"], true);
    assert_eq!(health["runtime"]["dataConfigured"], true);
    assert_eq!(health["runtime"]["resourcesConfigured"], true);
    assert_status_reads_fixture(port);

    // `GET /api/whitelist` serves the selected config like frozen JS (`{allow, deny, ownerQQ}`, ids
    // normalized to strings by `normalizeIdList`). It was an unmigrated, fail-closed route when this
    // test was written; `config_runtime` exercises runtime configuration over HTTP.
    let (status, body) = request(port, "GET", "/api/whitelist", Some(CONSOLE_TOKEN))
        .expect("request the selected whitelist");
    assert_eq!(status, 200, "selected whitelist must be served: {body}");
    let served: Value = serde_json::from_str(&body).expect("whitelist body is JSON");
    assert_eq!(
        served,
        serde_json::json!({
            "allow": { "private": [], "groups": ["7123"] },
            "deny": { "private": [], "groups": [] },
            "ownerQQ": 9001
        }),
        "whitelist must come from the selected config file"
    );
    assert!(
        !body.contains("C:/qqbot"),
        "isolation error must not reveal production paths"
    );
    onebot.wait_for_requests(1);
    stop(&mut child);

    // The old public CLI shape remains usable when an operator selects a runtime root in the environment.
    let legacy_port = free_port();
    let mut child = start_console(&root, legacy_port, true);
    let health = wait_for_health(&mut child, legacy_port).unwrap_or_else(|error| {
        stop(&mut child);
        panic!("{error}");
    });
    assert_eq!(health["runtime"]["mode"], "isolated");
    assert_status_reads_fixture(legacy_port);
    onebot.wait_for_requests(2);
    stop(&mut child);

    // An isolated root must not inherit the production console default port.
    let mut missing_port = start_console_without_port(&root);
    require_exit_within(&mut missing_port, "missing-port");
    let output = missing_port
        .wait_with_output()
        .expect("collect missing-port startup error");
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("explicit non-zero console port"));
    assert!(!diagnostic.contains(CONSOLE_TOKEN));
    assert!(!diagnostic.contains(&root.to_string_lossy().to_string()));
    for listener in [&dsh_listener, &model_listener] {
        listener
            .set_nonblocking(true)
            .expect("inspect fake dependency activity");
        assert!(
            listener.accept().is_err(),
            "health/status must not call the DSH or model dependency"
        );
    }
    // The only OneBot traffic is the startup sticker-library warm-up that the frozen JS bridge also
    // does (`bridge.js` "预热表情库"; Rust `console.rs` `serve_with_context`, ticket 20 ad600e35):
    // one `fetch_custom_face_detail` per console start, nothing else.
    let onebot_requests = onebot.requests.lock().unwrap().clone();
    assert!(
        !onebot_requests.is_empty(),
        "the startup sticker warm-up must have reached the selected OneBot dependency"
    );
    assert!(
        onebot_requests
            .iter()
            .all(|line| line == "POST /fetch_custom_face_detail HTTP/1.1"),
        "health/status must not call OneBot beyond the sticker warm-up: {onebot_requests:?}"
    );

    // Reject public dependency URLs before startup; an explicit isolated root must never fall back.
    write_dependencies(
        &root,
        &onebot_url,
        "https://production.example.invalid",
        &model_url,
    );
    let bad_port = free_port();
    let mut child = start_console(&root, bad_port, false);
    require_exit_within(&mut child, "invalid dependency");
    let output: Output = child
        .wait_with_output()
        .expect("collect sanitized startup error");
    assert!(
        !output.status.success(),
        "invalid dependency context must be rejected"
    );
    let diagnostic = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!diagnostic.contains("production.example.invalid"));
    assert!(!diagnostic.contains(CONSOLE_TOKEN));
    assert!(!diagnostic.contains(&root.to_string_lossy().to_string()));
    let blank_port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .arg("--console-serve")
        .arg("--console-port")
        .arg(blank_port.to_string())
        .env("HER_RUNTIME_ROOT", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_process_window(&mut command);
    let mut blank_root = ChildGuard(Some(command.spawn().expect("start with an empty runtime root")));
    require_exit_within(&mut blank_root, "empty-root");
    let output = blank_root
        .wait_with_output()
        .expect("collect empty-root startup error");
    assert!(
        !output.status.success(),
        "empty runtime root must be rejected"
    );
    let diagnostic = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!diagnostic.contains("C:/qqbot"));
    assert!(!diagnostic.contains(CONSOLE_TOKEN));
}

// This case specifically exercises Windows junction metadata and replacement.
#[cfg(windows)]
#[test]
fn isolated_status_rejects_data_directory_reparse_replacement() {
    let root = fixture_root();
    let outside_root = fixture_root();
    let outside_state = outside_root.join("data/social-v2.json");
    let outside_contents = serde_json::json!({
        "paused": false,
        "sentinel": OUTSIDE_SENTINEL,
    })
    .to_string();
    fs::write(&outside_state, &outside_contents).expect("write outside synthetic sentinel");

    let (onebot_listener, onebot_url) = fake_dependency();
    let (dsh_listener, dsh_url) = fake_dependency();
    let (model_listener, model_url) = fake_dependency();
    write_dependencies(&root, &onebot_url, &dsh_url, &model_url);

    let port = free_port();
    let mut child = start_console(&root, port, false);
    let health = wait_for_health(&mut child, port).unwrap_or_else(|error| {
        stop(&mut child);
        panic!("{error}");
    });
    assert_eq!(health["runtime"]["mode"], "isolated");
    assert_status_reads_fixture(port);

    let selected_data = root.join("data");
    let parked_data = root.join("data-before-reparse-test");
    // Windows refuses a directory rename with an open descendant. Park only the
    // synthetic owner's file first; its handle and byte-range lock stay held.
    // No process is started against this root during the replacement.
    fs::rename(selected_data.join("logs/rbridge-console.lock"), root.join("parked-console.lock")).unwrap();
    fs::rename(&selected_data, &parked_data).expect("move selected fixture data aside");
    let junction = create_test_junction(&selected_data, &outside_root.join("data"));

    let response = request(port, "GET", "/api/status", Some(CONSOLE_TOKEN));
    stop(&mut child);
    let (status, body) = response.expect("request status after runtime data replacement");
    assert_eq!(
        status, 503,
        "status must fail closed when runtime data resolves outside its root"
    );
    assert!(!body.contains(OUTSIDE_SENTINEL));
    assert!(!body.contains(&outside_root.to_string_lossy().to_string()));
    drop(junction);

    assert!(
        !selected_data.exists(),
        "cleanup must remove only the junction and must not traverse it"
    );
    assert_eq!(
        fs::read_to_string(&outside_state).expect("outside sentinel remains available"),
        outside_contents,
        "cleanup must leave the synthetic junction target untouched"
    );
    assert!(parked_data.join("social-v2.json").is_file());
}
