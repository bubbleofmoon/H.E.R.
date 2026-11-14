//! Health check through seam 1: real CLI, selected roots and loopback fakes.
#[path = "../src/install_root.rs"]
mod install_root;
#[path = "../src/process_identity.rs"]
mod process_identity;

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use process_identity::{KillOutcome, Liveness, ProcessIdentity};

struct Child(Option<std::process::Child>);
impl Drop for Child {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// How long the rig's cleanup gives each lifecycle CLI call and the owners' exit.
const CLEANUP_BOUND: Duration = Duration::from_secs(10);
/// How long a finished child's pipes may stay open: a detached grandchild can inherit them.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// One output pipe read to its end on its own thread; the bytes so far are readable at any time.
struct Drain {
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Drain {
    fn new<R: Read + Send + 'static>(pipe: Option<R>) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = bytes.clone();
        let reader = pipe.and_then(|mut pipe| {
            std::thread::Builder::new()
                .name("fixture-pipe".into())
                .spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match pipe.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => sink
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .extend_from_slice(&buf[..n]),
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                })
                .ok()
        });
        Self { bytes, reader }
    }
    fn closed(&self) -> bool {
        self.reader.as_ref().map_or(true, |reader| reader.is_finished())
    }
    fn bytes(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// Runs `cmd` with both output pipes drained on reader threads, so a child printing more than
/// one pipe buffer (4 KiB on Windows) cannot block. Waits at most `allow` for the exit, then at
/// most `PIPE_GRACE` for the pipes to close (a detached grandchild may hold an inherited pipe).
/// On timeout the child is killed through its own handle and reaped within 5 s; a child that
/// ignores the kill leaks its handle and the error says so. The error carries what it printed
/// so far. Never panics, so `Rig::drop` can use it while unwinding.
fn run_bounded(cmd: &mut Command, allow: Duration) -> Result<std::process::Output, String> {
    let mut process = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let stdout = Drain::new(process.stdout.take());
    let stderr = Drain::new(process.stderr.take());
    let mut child = Child(Some(process));
    let limit = Instant::now() + allow;
    let mut status = loop {
        let Some(process) = child.0.as_mut() else {
            break Err("lost its process handle".to_string());
        };
        match process.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < limit => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                break Err(format!(
                    "exceeded the fixture's {}-second limit",
                    allow.as_secs()
                ))
            }
            Err(e) => break Err(format!("could not be polled: {e}")),
        }
    };
    if let Err(reason) = &mut status {
        // The timeout path kills through the std `Child` handle: the handle names this exact
        // process instance, so no pid lookup can race a recycled pid — the recorded-identity
        // rule that `kill_verified` enforces for handleless processes is satisfied directly.
        // The reap after the kill is bounded: a child that ignores the kill leaks its handle
        // and is reported, never followed by a blocking `wait` that could stall the fixture.
        let mut leaked = false;
        if let Some(process) = child.0.as_mut() {
            let _ = process.kill();
            let reaped = Instant::now() + Duration::from_secs(5);
            loop {
                match process.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < reaped => std::thread::sleep(Duration::from_millis(5)),
                    Ok(None) => {
                        *reason += "; child did not exit after kill";
                        leaked = true;
                        break;
                    }
                    Err(_) => {
                        leaked = true;
                        break;
                    }
                }
            }
        }
        if leaked {
            // Dropping a std `Child` is non-blocking and leaves the process running: that is
            // the leak. Keeping it in the wrapper would call blocking `wait` in `Child::drop`.
            child.0 = None;
        }
    }
    // Exited (or reaped after the kill): the wrapper's Drop only reaps here.
    drop(child);
    let grace = Instant::now() + PIPE_GRACE;
    while !(stdout.closed() && stderr.closed()) && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(5));
    }
    match status {
        Ok(status) => Ok(std::process::Output {
            status,
            stdout: stdout.bytes(),
            stderr: stderr.bytes(),
        }),
        Err(reason) => Err(format!(
            "{reason}; partial stdout={}; partial stderr={}",
            String::from_utf8_lossy(&stdout.bytes()),
            String::from_utf8_lossy(&stderr.bytes())
        )),
    }
}

/// Cleanup diagnostics; `writeln!` to a possibly closed stderr cannot panic inside `Drop`.
fn cleanup_note(args: std::fmt::Arguments) {
    let _ = writeln!(std::io::stderr(), "rig cleanup: {args}");
}

#[derive(Clone)]
enum Reply {
    Json(u16, Value),
    Silent,
    /// One reply per request in order; the last one repeats.
    Script(Vec<(u16, Value)>),
}

struct Server {
    port: u16,
    reply: Arc<Mutex<Reply>>,
    /// Cookie headers of the requests that reached `path`, in order.
    cookies: Arc<Mutex<Vec<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(path: &'static str, reply: Reply) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let reply = Arc::new(Mutex::new(reply));
        let state = reply.clone();
        let cookies = Arc::new(Mutex::new(Vec::new()));
        let seen = cookies.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            while !stopping.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let accepted_at = Instant::now();
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                let read_limit = Instant::now() + Duration::from_secs(5);
                let mut complete = false;
                while !stopping.load(Ordering::Relaxed) && Instant::now() < read_limit {
                    let n = match stream.read(&mut buf) {
                        Ok(n) => n,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue
                        }
                        Err(_) => break,
                    };
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            complete = true;
                            break;
                        }
                    }
                }
                if !complete {
                    eprintln!(
                        "fixture expected={path}: incomplete request after {}ms, bytes={}",
                        accepted_at.elapsed().as_millis(),
                        request.len()
                    );
                    continue;
                }
                let text = String::from_utf8_lossy(&request);
                if text.is_empty() {
                    continue;
                } // supervisor connect-only observation
                let selected = {
                    let mut state = state.lock().unwrap();
                    match &mut *state {
                        Reply::Script(steps) if steps.len() > 1 => {
                            let (status, body) = steps.remove(0);
                            Reply::Json(status, body)
                        }
                        Reply::Script(steps) => Reply::Json(steps[0].0, steps[0].1.clone()),
                        other => other.clone(),
                    }
                };
                if text.lines().next().unwrap_or("").contains(path) {
                    seen.lock().unwrap().push(
                        text.lines()
                            .find(|line| line.starts_with("Cookie: "))
                            .unwrap_or("")
                            .to_string(),
                    );
                }
                let (status, body) = if !text.lines().next().unwrap_or("").contains(path) {
                    (404, json!({}))
                } else if path == "/get_status"
                    && !text.contains("Authorization: Bearer fixture-snow")
                {
                    (401, json!({}))
                } else if path == "/api/settings/describe" && !text.contains("Cookie: dsh-auth-") {
                    (401, json!({}))
                } else {
                    match selected {
                        Reply::Json(status, mut body) => {
                            // Real DSH (frozen JS `dsh-client.js` checks `full.rpcId !== message.rpcId`) answers a
                            // `client-request` with a `server-response` carrying that request's `rpcId`.
                            if path == "/api/settings/describe"
                                && body.get("type") == Some(&json!("server-response"))
                            {
                                let request_body =
                                    text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                                if let Some(id) = serde_json::from_str::<Value>(request_body)
                                    .ok()
                                    .and_then(|envelope| envelope.get("rpcId").cloned())
                                {
                                    body["rpcId"] = id;
                                }
                            }
                            (status, body)
                        }
                        Reply::Script(_) => unreachable!("resolved above"),
                        Reply::Silent => {
                            while !stopping.load(Ordering::Relaxed)
                                && matches!(*state.lock().unwrap(), Reply::Silent)
                            {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            continue;
                        }
                    }
                };
                let mut body = body;
                // DSH echoes the request's rpcId in its server-response (the bounded client checks it).
                if body["type"] == "server-response" && body.get("rpcId").is_none() {
                    let request_body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                    if let Some(id) = serde_json::from_str::<Value>(request_body)
                        .ok()
                        .and_then(|v| v.get("rpcId").cloned())
                    {
                        body["rpcId"] = id;
                    }
                }
                let body = body.to_string();
                let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let sent = stream.write_all(response.as_bytes());
                eprintln!("fixture expected={path}, request={:?}, elapsed={}ms: HTTP {status}, body={body}, write={sent:?}", text.lines().next(), accepted_at.elapsed().as_millis());
            }
        });
        Self {
            port,
            reply,
            cookies,
            stop,
            thread: Some(thread),
        }
    }
    fn set(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn hidden(exe: &str) -> Command {
    let mut cmd = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.env_remove("HER_RUNTIME_ROOT");
    cmd
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = install_root::development_root(std::env::temp_dir().join(format!(
            "her-health-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        for dir in ["data", "config", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("config/config.json"),
            json!({"consoleToken":"fixture-console", "consolePort":1}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(root.join("dependencies.json"), json!({"onebot":"http://127.0.0.1:1", "dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:1"}).to_string()).unwrap();
        Self(root)
    }
    fn output(&self, extra: &[&str]) -> std::process::Output {
        self.output_bounded(extra, Duration::from_secs(12))
    }
    /// The CLI child must exit within `allow`, so a fixture that lets it wait on a product
    /// deadline says so; every bound stays explicit and every path still ends in a failure.
    fn output_bounded(&self, extra: &[&str], allow: Duration) -> std::process::Output {
        let mut cmd = hidden(env!("CARGO_BIN_EXE_her-cli"));
        cmd.args(["lifecycle", "assert-running", "--runtime-root"])
            .arg(&self.0)
            .args(extra);
        run_bounded(&mut cmd, allow).unwrap_or_else(|error| panic!("assert-running {error}"))
    }
    fn check(&self, extra: &[&str]) -> (i32, Value) {
        self.check_bounded(extra, Duration::from_secs(12))
    }
    fn check_bounded(&self, extra: &[&str], allow: Duration) -> (i32, Value) {
        let out = self.output_bounded(extra, allow);
        let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "JSON required: {e}; stdout={}; stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap(), value)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn snapshot(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(
        root: &std::path::Path,
        dir: &std::path::Path,
        entries: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let directory = path.is_dir();
            entries.insert(
                path.strip_prefix(root).unwrap().to_path_buf(),
                if directory {
                    None
                } else {
                    Some(std::fs::read(&path).unwrap())
                },
            );
            if directory {
                visit(root, &path, entries);
            }
        }
    }
    let mut entries = std::collections::BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

#[test]
fn health_never_creates_missing_gate_directories_or_locks() {
    for directories in [
        vec![],
        vec!["lifecycle"],
        vec!["logs"],
        vec!["lifecycle", "logs"],
    ] {
        let fixture = Fixture::new();
        for dir in &directories {
            std::fs::create_dir(fixture.0.join("data").join(dir)).unwrap();
        }
        if directories.contains(&"lifecycle") {
            std::fs::write(fixture.0.join("data/lifecycle/desired-state.json"),
                json!({"v":1,"state":"stopped","by":"fixture","reason":"fixture hold","at":0,"until":null,"revision":1}).to_string()).unwrap();
        }
        // A responding listener forces the console's owner-gate validation/probe path.
        let console = Server::new("/api/health", Reply::Json(200, json!({})));
        console.set(Reply::Json(
            200,
            json!({"ok":true,"pid":std::process::id(),"port":console.port}),
        ));
        std::fs::write(
            fixture.0.join("config/config.json"),
            json!({"consoleToken":"fixture-console","consolePort":console.port}).to_string(),
        )
        .unwrap();
        let before = snapshot(&fixture.0);
        let (code, verdict) = fixture.check(&[]);
        assert_eq!(code, 1, "{verdict}");
        for part in ["supervisor", "console"] {
            assert_eq!(verdict["parts"][part]["verdict"], "fail", "{verdict}");
        }
        assert_eq!(
            snapshot(&fixture.0),
            before,
            "health modified runtime with directories {directories:?}"
        );
    }
}

#[test]
fn unavailable_runtime_contents_report_json_and_wait_until_the_deadline() {
    for missing in ["root", "data"] {
        let parent = Fixture::new();
        let absent = Fixture(parent.0.join("not-created"));
        let fixture = if missing == "root" {
            &absent
        } else {
            std::fs::remove_file(parent.0.join("data/social-v2.json")).unwrap();
            std::fs::remove_dir(parent.0.join("data")).unwrap();
            &parent
        };
        let before = snapshot(&parent.0);
        for args in [vec![], vec!["--wait", "250ms"]] {
            let started = Instant::now();
            let (code, verdict) = fixture.check(&args);
            assert_eq!(code, 1, "missing {missing}: {verdict}");
            assert_eq!(verdict["v"], 1);
            assert_eq!(verdict["ok"], false);
            for part in ["desired", "supervisor", "console", "snowluma", "dsh"] {
                assert_eq!(verdict["parts"][part]["verdict"], "fail", "{verdict}");
                assert!(!verdict["parts"][part]["reason"]
                    .as_str()
                    .unwrap()
                    .is_empty());
            }
            if !args.is_empty() {
                assert!(
                    started.elapsed() >= Duration::from_millis(250),
                    "missing {missing} returned before deadline"
                );
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_eq!(snapshot(&parent.0), before);
        }
        // Contents appearing during the wait must reach a later snapshot.
        let prepared = Fixture::new();
        let (code, verdict) = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                if missing == "root" {
                    std::fs::rename(&prepared.0, &fixture.0).unwrap();
                } else {
                    std::fs::rename(prepared.0.join("data"), fixture.0.join("data")).unwrap();
                }
            });
            fixture.check(&["--wait", "600ms"])
        });
        assert_eq!(code, 1, "{verdict}");
        assert_eq!(
            verdict["parts"]["desired"]["verdict"], "pass",
            "new {missing} was not rechecked: {verdict}"
        );
    }
}

#[test]
fn unavailable_runtime_does_not_hide_malformed_cli_input() {
    let parent = Fixture::new();
    let absent = Fixture(parent.0.join("not-created"));
    for args in [
        vec!["--wait", "later"],
        vec!["--wait", "-1s"],
        vec!["--wait", "18446744073709551615h"],
        vec!["--wait"],
        vec!["--unknown"],
        vec!["--wait", "1s", "--wait", "1s"],
        vec!["extra-positional"],
        vec!["--confirm"],
    ] {
        let out = absent.output(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
    for root in [PathBuf::new(), PathBuf::from("relative-root")] {
        let out = hidden(env!("CARGO_BIN_EXE_her-cli"))
            .args(["lifecycle", "assert-running", "--runtime-root"])
            .arg(root)
            .args(["--wait", "250ms"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
    }
    let out = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .args(["lifecycle", "assert-running", "--wait", "250ms"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

struct Rig {
    /// Every process instance this rig caused, by role (`console`, `live`, `supervisor`):
    /// recorded at spawn and from the owner markers, stopped by `Drop` with `kill_verified`.
    owners: Vec<(&'static str, ProcessIdentity)>,
    console: Option<Child>,
    supervisor: Option<Child>,
    fixture: Fixture,
    snowluma: Server,
    dsh: Server,
    config: Value,
}
impl Rig {
    fn new() -> Self {
        let fixture = Fixture::new();
        let snowluma = Server::new(
            "/get_status",
            Reply::Json(
                200,
                json!({"status":"ok", "retcode":0, "data":{"online":true,"good":true}}),
            ),
        );
        let dsh = Server::new(
            "/api/settings/describe",
            Reply::Json(
                200,
                json!({"type":"server-response", "result":{"ok":true,"value":{}}}),
            ),
        );
        let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserve.local_addr().unwrap().port();
        let mut tasks = json!({});
        for task in [
            "watchdog",
            "console-restart",
            "start-all",
            "memory-digest",
            "memory-digest-check",
            "backup",
            "upstream-check",
        ] {
            tasks[task] = json!({"enabled":false});
        }
        let config = json!({"consoleToken":"fixture-console", "consolePort":port,
            "snowluma":{"accessToken":"fixture-snow"}, "tasks":tasks,
            "backends":{"mcpRustBridge":false}, "rollout":{"rustGroups":[]}});
        std::fs::write(fixture.0.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(
            fixture.0.join("user/dsh/.credentials.yaml"),
            "secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
        )
        .unwrap();
        std::fs::write(
            fixture.0.join("dependencies.json"),
            json!({"onebot":snowluma.url(), "dsh":dsh.url(), "model":dsh.url()}).to_string(),
        )
        .unwrap();
        drop(reserve);
        let spawn = |args: &[&str]| {
            Child(Some(
                hidden(env!("CARGO_BIN_EXE_her"))
                    .args(args)
                    .arg("--runtime-root")
                    .arg(&fixture.0)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            ))
        };
        let console = Some(spawn(&[
            "--console-serve",
            "--console-port",
            &port.to_string(),
        ]));
        let supervisor = Some(spawn(&["supervise"]));
        let mut owners = Vec::new();
        for (role, child) in [("console", &console), ("supervisor", &supervisor)] {
            let pid = child
                .as_ref()
                .and_then(|child| child.0.as_ref())
                .map(std::process::Child::id);
            if let Some(Ok(Some(identity))) = pid.map(process_identity::identity_of) {
                owners.push((role, identity));
            }
        }
        let rig = Self {
            owners,
            console,
            supervisor,
            fixture,
            snowluma,
            dsh,
            config,
        };
        let limit = Instant::now() + Duration::from_secs(10);
        loop {
            let out = hidden(env!("CARGO_BIN_EXE_her-cli"))
                .args(["lifecycle", "status", "--runtime-root"])
                .arg(&rig.fixture.0)
                .output()
                .unwrap();
            let status: Value = serde_json::from_slice(&out.stdout).unwrap();
            if status["owners"]["console"]["state"] == "held"
                && status["owners"]["supervisor"]["state"] == "held"
            {
                // The owner is acquired just before the console binds its listener.
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    break;
                }
            }
            assert!(Instant::now() < limit, "owners not ready: {status}");
            std::thread::sleep(Duration::from_millis(20));
        }
        rig
    }
    fn save_config(&self) {
        std::fs::write(
            self.fixture.0.join("config/config.json"),
            self.config.to_string(),
        )
        .unwrap();
    }
    /// Adds every owner the runtime published in `data/logs/<lock>.owner.json`. Reads only the
    /// marker: a held `.lock` file is byte-range locked on Windows (os error 33).
    fn record_owners(&mut self) {
        for (name, role) in [
            ("rbridge-live.lock", "live"),
            ("rbridge-console.lock", "console"),
            ("rbridge-supervisor.lock", "supervisor"),
        ] {
            let marker = self
                .fixture
                .0
                .join("data/logs")
                .join(format!("{name}.owner.json"));
            let Ok(bytes) = std::fs::read(&marker) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let Ok(identity) = serde_json::from_value::<ProcessIdentity>(value["identity"].clone())
            else {
                continue;
            };
            if !self.owners.iter().any(|(_, known)| *known == identity) {
                self.owners.push((role, identity));
            }
        }
    }
    fn kill_recorded(&self, select: impl Fn(&str) -> bool) {
        for (role, identity) in self.owners.iter().filter(|(role, _)| select(role)) {
            match process_identity::kill_verified(identity) {
                KillOutcome::Gone => {}
                KillOutcome::Killed => cleanup_note(format_args!(
                    "killed {role} pid {} that outlived the lifecycle stop",
                    identity.pid
                )),
                outcome => cleanup_note(format_args!(
                    "LEAK RISK: {role} pid {} was not stopped: {outcome:?}",
                    identity.pid
                )),
            }
        }
    }
    fn running_owners(&self) -> Vec<String> {
        self.owners
            .iter()
            .filter(|(_, identity)| process_identity::check_identity(identity) == Liveness::Matches)
            .map(|(role, identity)| format!("{role} pid {}", identity.pid))
            .collect()
    }
}
impl Drop for Rig {
    /// Stops everything this rig caused on every exit path (pass, failed assert, panic, timeout).
    /// Never panics and never asserts: a panic while unwinding aborts the whole test binary.
    /// Field drops follow: `Child` handles kill and reap, and `Fixture` removes the directory last.
    fn drop(&mut self) {
        self.record_owners();
        // 1. The product's own stop, each call bounded; failures are reported, never fatal.
        let stops: [(&str, &[&str]); 2] = [
            (
                "set stopped",
                &["lifecycle", "set", "stopped", "--confirm", "--runtime-root"],
            ),
            (
                "stop-supervisor",
                &["lifecycle", "stop-supervisor", "--confirm", "--runtime-root"],
            ),
        ];
        for (label, args) in stops {
            let mut cmd = hidden(env!("CARGO_BIN_EXE_her-cli"));
            cmd.args(args).arg(&self.fixture.0);
            match run_bounded(&mut cmd, CLEANUP_BOUND) {
                Ok(out) if out.status.success() => {}
                Ok(out) => cleanup_note(format_args!(
                    "lifecycle {label} exited {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
                Err(error) => cleanup_note(format_args!("lifecycle {label} {error}")),
            }
            self.record_owners();
        }
        // 2. Give the recorded owners time to exit on their own.
        let deadline = Instant::now() + CLEANUP_BOUND;
        while Instant::now() < deadline && !self.running_owners().is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
        // 3. Pinned kills by pid plus start stamp; the supervisor goes last.
        self.kill_recorded(|role| role != "supervisor");
        self.kill_recorded(|role| role == "supervisor");
        // 4. A child the supervisor started just before it died publishes its marker late.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let known = self.owners.len();
            self.record_owners();
            if self.owners.len() > known {
                self.kill_recorded(|_| true);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // 5. TerminateProcess is asynchronous: confirm exits before Fixture removes the directory.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let running = self.running_owners();
            if running.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                cleanup_note(format_args!("still running after cleanup: {running:?}"));
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn an_unhealthy_runtime_names_every_part_and_reason() {
    let fixture = Fixture::new();
    let (code, verdict) = fixture.check(&[]);
    assert_eq!(code, 1);
    assert_eq!(verdict["v"], 1);
    assert_eq!(verdict["ok"], false);
    assert_eq!(verdict["parts"]["desired"]["verdict"], "pass");
    for part in ["supervisor", "console", "snowluma", "dsh"] {
        assert_eq!(verdict["parts"][part]["verdict"], "fail", "{verdict}");
        assert!(!verdict["parts"][part]["reason"]
            .as_str()
            .unwrap()
            .is_empty());
    }
}

#[test]
fn a_running_runtime_passes_through_the_cli_twin() {
    let rig = Rig::new();
    let (code, verdict) = rig.fixture.check(&[]);
    assert_eq!(code, 0, "{verdict}");
    assert_eq!(verdict["ok"], true);
    for part in ["desired", "supervisor", "console", "snowluma", "dsh"] {
        assert_eq!(verdict["parts"][part]["verdict"], "pass", "{verdict}");
        assert!(!verdict["parts"][part]["reason"]
            .as_str()
            .unwrap()
            .is_empty());
    }
}

fn only_fails(fixture: &Fixture, part: &str) {
    eprintln!("checking independent failure: {part}");
    let (code, verdict) = fixture.check(&[]);
    assert_eq!(code, 1, "{verdict}");
    assert_eq!(verdict["ok"], false);
    for name in ["desired", "supervisor", "console", "snowluma", "dsh"] {
        assert_eq!(
            verdict["parts"][name]["verdict"],
            if name == part { "fail" } else { "pass" },
            "{verdict}"
        );
        assert!(!verdict["parts"][name]["reason"]
            .as_str()
            .unwrap()
            .is_empty());
    }
}

#[test]
fn a_console_reporting_failure_is_unhealthy_even_with_a_matching_owner_pid() {
    let mut rig = Rig::new();
    let pid = rig.console.as_ref().unwrap().0.as_ref().unwrap().id();
    let fake = Server::new(
        "/api/health",
        Reply::Json(200, json!({"ok":false,"pid":pid})),
    );
    fake.set(Reply::Json(
        200,
        json!({"ok":false,"pid":pid,"port":fake.port}),
    ));
    rig.config["consolePort"] = fake.port.into();
    rig.save_config();
    only_fails(&rig.fixture, "console");
}

#[test]
fn each_required_part_can_fail_independently_without_changing_desired_state() {
    let mut rig = Rig::new();
    for data in [
        json!({"online":true,"good":false}),
        json!({"online":false,"good":true}),
        json!({}),
    ] {
        rig.snowluma
            .set(Reply::Json(200, json!({"status":"ok","data":data})));
        only_fails(&rig.fixture, "snowluma");
    }
    rig.snowluma.set(Reply::Json(
        200,
        json!({"status":"ok","data":{"online":true,"good":true}}),
    ));
    for (status, body) in [(401, json!({})), (200, json!({"type":"unexpected"}))] {
        rig.dsh.set(Reply::Json(status, body));
        only_fails(&rig.fixture, "dsh");
    }
    rig.dsh.set(Reply::Json(
        200,
        json!({"type":"server-response","result":{"ok":true,"value":{}}}),
    ));
    let desired = rig.fixture.0.join("data/lifecycle/desired-state.json");
    std::fs::create_dir_all(desired.parent().unwrap()).unwrap();
    for state in ["stopped", "maintenance"] {
        let bytes = json!({"v":1,"state":state,"by":"fixture","reason":"fixture hold","at":0,"until":null,"revision":1}).to_string();
        std::fs::write(&desired, &bytes).unwrap();
        only_fails(&rig.fixture, "desired");
        assert_eq!(
            std::fs::read_to_string(&desired).unwrap(),
            bytes,
            "health must not change intent"
        );
    }
    std::fs::write(&desired, "invalid").unwrap();
    only_fails(&rig.fixture, "desired");
    std::fs::remove_file(&desired).unwrap();
    let marker_path = rig
        .fixture
        .0
        .join("data/logs/rbridge-supervisor.lock.owner.json");
    let original = std::fs::read(&marker_path).unwrap();
    let mut marker: Value = serde_json::from_slice(&original).unwrap();
    marker["identity"]["start"] = (marker["identity"]["start"].as_u64().unwrap() + 1).into();
    std::fs::write(&marker_path, marker.to_string()).unwrap();
    only_fails(&rig.fixture, "supervisor");
    std::fs::write(&marker_path, &original).unwrap();
    drop(rig.supervisor.take());
    only_fails(&rig.fixture, "supervisor");
    // Restore the held supervisor for an independent console failure.
    let child = hidden(env!("CARGO_BIN_EXE_her"))
        .args(["supervise", "--runtime-root"])
        .arg(&rig.fixture.0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    rig.supervisor = Some(Child(Some(child)));
    let limit = Instant::now() + Duration::from_secs(10);
    while rig.fixture.check(&[]).0 != 0 {
        assert!(
            Instant::now() < limit,
            "replacement fixture supervisor not ready"
        );
    }
    drop(rig.console.take());
    only_fails(&rig.fixture, "console");
    std::fs::write(rig.fixture.0.join("config/config.json"), "invalid").unwrap();
    let (code, verdict) = rig.fixture.check(&[]);
    assert_eq!(code, 1);
    assert_eq!(verdict["ok"], false);
    for part in ["desired", "supervisor", "console", "snowluma", "dsh"] {
        assert_eq!(verdict["parts"][part]["verdict"], "fail", "{verdict}");
    }
}

#[test]
fn wait_polls_until_the_runtime_becomes_healthy() {
    let rig = Rig::new();
    rig.dsh.set(Reply::Json(401, json!({})));
    let started = Instant::now();
    let (code, verdict) = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(250));
            rig.dsh.set(Reply::Json(
                200,
                json!({"type":"server-response","result":{"ok":true,"value":{}}}),
            ));
        });
        rig.fixture.check(&["--wait", "2s"])
    });
    assert_eq!(code, 0, "{verdict}");
    assert_eq!(verdict["ok"], true);
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn wait_timeout_bounds_silent_listeners_and_names_the_failed_part() {
    for part in ["console", "snowluma", "dsh"] {
        let mut rig = Rig::new();
        let console = Server::new("/api/health", Reply::Silent);
        match part {
            "console" => {
                rig.config["consolePort"] = console.port.into();
                rig.save_config();
            }
            "snowluma" => rig.snowluma.set(Reply::Silent),
            "dsh" => rig.dsh.set(Reply::Silent),
            _ => unreachable!(),
        }
        let started = Instant::now();
        let (code, verdict) = rig.fixture.check(&["--wait", "150ms"]);
        assert_eq!(code, 1, "{part}: {verdict}");
        assert_eq!(verdict["ok"], false);
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{part} exceeded deadline: {:?}",
            started.elapsed()
        );
        for name in ["desired", "supervisor", "console", "snowluma", "dsh"] {
            assert_eq!(
                verdict["parts"][name]["verdict"],
                if name == part { "fail" } else { "pass" },
                "{part}: {verdict}"
            );
            assert!(!verdict["parts"][name]["reason"]
                .as_str()
                .unwrap()
                .is_empty());
        }
    }
}

#[test]
fn wait_rereads_desired_state_and_zero_wait_takes_one_snapshot() {
    let rig = Rig::new();
    let desired = rig.fixture.0.join("data/lifecycle/desired-state.json");
    std::fs::create_dir_all(desired.parent().unwrap()).unwrap();
    std::fs::write(&desired, json!({"v":1,"state":"stopped","by":"fixture","reason":"fixture hold","at":0,"until":null,"revision":1}).to_string()).unwrap();
    let started = Instant::now();
    let (code, verdict) = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(250));
            std::fs::remove_file(&desired).unwrap();
        });
        rig.fixture.check(&["--wait", "2s"])
    });
    assert_eq!(code, 0, "{verdict}");
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(rig.fixture.check(&["--wait", "0ms"]).0, 0);
}

#[test]
fn wait_duration_units_are_accepted_and_invalid_durations_are_usage_errors() {
    let rig = Rig::new();
    for duration in ["1", "1s", "1m", "1h"] {
        assert_eq!(rig.fixture.check(&["--wait", duration]).0, 0, "{duration}");
    }
    for args in [
        vec!["--wait", "-1s"],
        vec!["--wait", "1.5s"],
        vec!["--wait", "later"],
        vec!["--wait", "18446744073709551615h"],
        vec!["--wait"],
        vec!["--wait", "1s", "--wait", "1s"],
    ] {
        let out = rig.fixture.output(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("--wait"));
    }
}

fn login_fixture(dsh: &Server) -> Fixture {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("user/dsh/.credentials.yaml"),
        "secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
    )
    .unwrap();
    std::fs::write(
        fixture.0.join("dependencies.json"),
        json!({"onebot":"http://127.0.0.1:1", "dsh":dsh.url(), "model":"http://127.0.0.1:1"})
            .to_string(),
    )
    .unwrap();
    fixture
}

fn described() -> Value {
    json!({"type":"server-response","result":{"ok":true,"value":{}}})
}

#[test]
fn two_rejected_fresh_login_cookies_fail_the_dsh_login_part_and_name_the_likely_causes() {
    let dsh = Server::new("/api/settings/describe", Reply::Json(401, json!({})));
    let fixture = login_fixture(&dsh);
    let (code, verdict) = fixture.check(&[]);
    assert_eq!(code, 1, "{verdict}");
    assert_eq!(verdict["ok"], false);
    let login = &verdict["parts"]["dshLogin"];
    assert_eq!(login["verdict"], "fail", "{verdict}");
    let reason = login["reason"].as_str().unwrap();
    assert!(reason.contains("clock skew"), "{reason}");
    assert!(reason.contains("DSH version change"), "{reason}");
    assert_eq!(verdict["parts"]["dsh"]["verdict"], "fail", "{verdict}");
    let cookies = dsh.cookies.lock().unwrap().clone();
    assert_eq!(
        cookies.len(),
        2,
        "one fresh cookie, one refreshed retry, nothing more: {cookies:?}"
    );
    assert!(
        cookies.iter().all(|c| c.starts_with("Cookie: dsh-auth-")),
        "{cookies:?}"
    );
}

#[test]
fn one_rejected_login_cookie_is_refreshed_and_the_dsh_login_part_passes() {
    let dsh = Server::new(
        "/api/settings/describe",
        Reply::Script(vec![(401, json!({})), (200, described())]),
    );
    let fixture = login_fixture(&dsh);
    let (_, verdict) = fixture.check(&[]);
    for part in ["dsh", "dshLogin"] {
        assert_eq!(verdict["parts"][part]["verdict"], "pass", "{verdict}");
    }
    assert_eq!(
        dsh.cookies.lock().unwrap().len(),
        2,
        "the rejection was refreshed and repeated once"
    );

    // Unreadable credentials: no login cookie can be made at all.
    std::fs::write(
        fixture.0.join("user/dsh/.credentials.yaml"),
        "secret: broken\n",
    )
    .unwrap();
    let (_, verdict) = fixture.check(&[]);
    assert_eq!(verdict["parts"]["dshLogin"]["verdict"], "fail", "{verdict}");
    assert_eq!(
        dsh.cookies.lock().unwrap().len(),
        2,
        "nothing is sent without credentials"
    );
}

#[test]
fn a_running_runtime_reports_a_working_dsh_login() {
    let rig = Rig::new();
    let (code, verdict) = rig.fixture.check(&[]);
    assert_eq!(code, 0, "{verdict}");
    assert_eq!(verdict["parts"]["dshLogin"]["verdict"], "pass", "{verdict}");
    assert!(!verdict["parts"]["dshLogin"]["reason"]
        .as_str()
        .unwrap()
        .is_empty());
}
#[test]
fn confirmed_restore_restarts_native_bridge_owners_and_passes_shared_health_with_fake_peers() {
    restore_with_fake_peers(false);
}

#[test]
fn configured_live_never_ready_rolls_back_and_alerts() {
    restore_with_fake_peers(true);
}

fn restore_with_fake_peers(never_ready: bool) {
    fn console_restore(port: u16, path: &str, body: Value) -> (u16, Value) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let body = body.to_string();
        write!(stream, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nX-Console-Token: fixture-console\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").expect("console response");
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }
    struct SocketPeer {
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        url: String,
    }
    impl SocketPeer {
        fn new() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopping = stop.clone();
            let thread = std::thread::spawn(move || {
                while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                    let Ok((stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    let Ok(mut socket) = tokio_tungstenite::tungstenite::accept(stream) else {
                        continue;
                    };
                    while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                        match socket.read() {
                            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                                if let Ok(request) = serde_json::from_str::<Value>(&text) {
                                    let data = if request["action"] == "get_login_info" {
                                        json!({"user_id":10001,"nickname":"restore fixture"})
                                    } else {
                                        json!({})
                                    };
                                    let reply = json!({"status":"ok","retcode":0,"echo":request["echo"],"data":data});
                                    if socket
                                        .send(tokio_tungstenite::tungstenite::Message::Text(
                                            reply.to_string(),
                                        ))
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(tokio_tungstenite::tungstenite::Error::Io(e))
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                                ) => {}
                            Err(_) => break,
                        }
                    }
                }
            });
            Self {
                stop,
                thread: Some(thread),
                url,
            }
        }
    }
    impl Drop for SocketPeer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    for &offline in if never_ready {
        &[true][..]
    } else {
        &[false, true][..]
    } {
        let mut rig = Rig::new();
        let socket = SocketPeer::new();
        let deps = rig.fixture.0.join("dependencies.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&deps).unwrap()).unwrap();
        config["onebot_ws"] = if never_ready {
            // A refused WebSocket connection prevents live startup; HTTP peers remain healthy.
            json!("ws://127.0.0.1:1")
        } else {
            json!(socket.url)
        };
        std::fs::write(deps, config.to_string()).unwrap();
        rig.config["backends"]["mcpRustBridge"] = json!(true);
        rig.config["rollout"]["rustGroups"] = json!(["group:123456"]);
        rig.config["tasks"]["watchdog"]["enabled"] = json!(true);
        rig.save_config();
        let initialized = Instant::now() + Duration::from_secs(10);
        while !rig.fixture.0.join("data/backup-notices.json").exists() {
            assert!(
                Instant::now() < initialized,
                "supervisor initialization did not finish"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        if offline {
            let stopped = hidden(env!("CARGO_BIN_EXE_her-cli"))
                .args(["lifecycle", "set", "stopped", "--confirm", "--runtime-root"])
                .arg(&rig.fixture.0)
                .output()
                .unwrap();
            assert!(stopped.status.success());
            drop(rig.supervisor.take());
        }
        let root = &rig.fixture.0;
        for dir in ["resources/roles", "resources/docs", "resources/patches"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let db = rusqlite::Connection::open(root.join("data/memory.db")).unwrap();
        db.execute_batch("CREATE TABLE restore_probe(value TEXT); INSERT INTO restore_probe VALUES('archived memory');").unwrap();
        drop(db);
        std::fs::write(root.join("resources/roles/restore.md"), "archived role").unwrap();
        let snapshot = root.join("archive-memory.db");
        let source = rusqlite::Connection::open(root.join("data/memory.db")).unwrap();
        let mut copy = rusqlite::Connection::open(&snapshot).unwrap();
        rusqlite::backup::Backup::new(&source, &mut copy)
            .unwrap()
            .run_to_completion(32, Duration::from_millis(1), None)
            .unwrap();
        drop(copy);
        drop(source);
        std::fs::create_dir_all(root.join("backups")).unwrap();
        let archive = root.join("backups/qqbot-2001-01-01T00-00-00.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let mut files = serde_json::Map::new();
        for (name, source) in [
            ("memory.db", "archive-memory.db"),
            ("config.json", "config/config.json"),
            ("roles/restore.md", "resources/roles/restore.md"),
            ("state/social-v2.json", "data/social-v2.json"),
        ] {
            use sha2::{Digest, Sha256};
            let bytes = std::fs::read(root.join(source)).unwrap();
            files.insert(
                name.into(),
                json!({"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}),
            );
            zip.start_file(name, zip::write::FileOptions::default())
                .unwrap();
            zip.write_all(&bytes).unwrap();
        }
        zip.start_file("manifest.json", zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(json!({"version":1,"files":files}).to_string().as_bytes())
            .unwrap();
        zip.finish().unwrap();
        rusqlite::Connection::open(root.join("data/memory.db"))
            .unwrap()
            .execute("UPDATE restore_probe SET value='current memory'", [])
            .unwrap();
        std::fs::write(root.join("resources/roles/restore.md"), "current role").unwrap();
        let command = || {
            let mut cmd = hidden(env!("CARGO_BIN_EXE_her-cli"));
            cmd.args(["backup", "restore"])
                .arg(&archive)
                .arg("--runtime-root")
                .arg(root);
            cmd
        };
        if offline {
            let preview = command().arg("--preview").output().unwrap();
            assert!(
                preview.status.success(),
                "{}",
                String::from_utf8_lossy(&preview.stderr)
            );
            let text = String::from_utf8_lossy(&preview.stdout);
            let p: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
            let mut restore = command();
            restore
                .arg("--confirm")
                .arg(p["preview"]["confirmation"].as_str().unwrap());
            let output = run_bounded(&mut restore, Duration::from_secs(150)).unwrap_or_else(
                |error| panic!("restore exceeded health/rollback budget: {error}"),
            );
            if never_ready {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    !output.status.success(),
                    "live never became ready: stdout={stdout} stderr={stderr}"
                );
                assert!(
                    stderr.contains("rolled back") && stderr.contains("live"),
                    "{stderr}"
                );
                assert!(
                    stdout.contains("[notify]") && stdout.contains("Restore rolled back"),
                    "failure alert: {stdout}"
                );
                let value: String = rusqlite::Connection::open(root.join("data/memory.db"))
                    .unwrap()
                    .query_row("SELECT value FROM restore_probe", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(
                    value, "current memory",
                    "rollback restores the pre-restore database"
                );
                assert_eq!(
                    std::fs::read_to_string(root.join("resources/roles/restore.md")).unwrap(),
                    "current role"
                );
                let audit = std::fs::read_to_string(root.join("data/audit/audit.jsonl")).unwrap();
                assert!(audit
                    .lines()
                    .any(
                        |line| serde_json::from_str::<Value>(line).unwrap()["result"] == "failed"
                    ));
                continue;
            }
            assert!(
                output.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        } else {
            let port = rig.config["consolePort"].as_u64().unwrap() as u16;
            let name = archive.file_name().unwrap().to_str().unwrap();
            let (status, plan) = console_restore(
                port,
                "/api/backups/restore/preview",
                json!({"archive":name,"memoryOnly":false}),
            );
            assert_eq!(status, 200, "{plan}");
            let (status, accepted) = console_restore(
                port,
                "/api/backups/restore",
                json!({"archive":name,"memoryOnly":false,"confirmation":plan["preview"]["confirmation"]}),
            );
            assert_eq!(status, 202, "{accepted}");
            let deadline = Instant::now() + Duration::from_secs(150);
            loop {
                let audit = std::fs::read_to_string(root.join("data/audit/audit.jsonl"))
                    .unwrap_or_default();
                let result = audit
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .find(|event| {
                        event["action"] == "backup.restore"
                            && matches!(event["result"].as_str(), Some("succeeded" | "failed"))
                    });
                if let Some(result) = result {
                    assert_eq!(result["result"], "succeeded", "{result}");
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "detached console restore did not finish"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let status = hidden(env!("CARGO_BIN_EXE_her-cli"))
            .args(["lifecycle", "status", "--runtime-root"])
            .arg(root)
            .output()
            .unwrap();
        let owners: Value = serde_json::from_slice(&status.stdout).unwrap();
        let heartbeat: Value = serde_json::from_slice(
            &std::fs::read(root.join("data/rust-heartbeat-live.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(owners["owners"]["live"]["state"], "held", "{owners}");
        assert_eq!(
            heartbeat["pid"], owners["owners"]["live"]["owner"]["identity"]["pid"],
            "{heartbeat} {owners}"
        );
        assert_eq!(heartbeat["live"], true, "{heartbeat}");
        assert_eq!(heartbeat["groups"], json!(["group:123456"]));
        assert_eq!(
            std::fs::read_to_string(root.join("resources/roles/restore.md")).unwrap(),
            "archived role"
        );
        let value: String = rusqlite::Connection::open(root.join("data/memory.db"))
            .unwrap()
            .query_row("SELECT value FROM restore_probe", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "archived memory");
        if !offline {
            assert!(
                rig
                    .supervisor
                    .as_mut()
                    .unwrap()
                    .0
                    .as_mut()
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_none(),
                "supervisor stays running"
            );
        }
        let (health_code, health_report) = rig.fixture.check(&[]);
        eprintln!("post-restore health (offline={offline}, code={health_code}): {health_report}");
        if health_code != 0 {
            let status = hidden(env!("CARGO_BIN_EXE_her-cli"))
                .args(["lifecycle", "status", "--runtime-root"])
                .arg(root)
                .output()
                .unwrap();
            eprintln!(
                "post-restore owner/desired snapshot: {}",
                String::from_utf8_lossy(&status.stdout)
            );
        }
        assert_eq!(
            health_code, 0,
            "unchanged fake peers still answer shared health (offline={offline}): {health_report}"
        );
    }
}

#[test]
fn a_fresh_running_revision_reconciles_a_missing_console_without_a_state_change() {
    let mut rig = Rig::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rig.fixture.0.join("data/backup-notices.json").exists() {
        assert!(
            Instant::now() < deadline,
            "supervisor did not complete its first poll"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    rig.config["backends"]["mcpRustBridge"] = json!(true);
    rig.config["rollout"]["rustGroups"] = json!(["group:123456"]);
    rig.save_config();
    drop(rig.console.take());
    let running = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .args(["lifecycle", "set", "running", "--runtime-root"])
        .arg(&rig.fixture.0)
        .output()
        .unwrap();
    assert!(running.status.success());
    // The supervisor notices the new revision on its one-second scheduler pass and only starts the
    // console owner after that: measured at ~5 s of supervisor delay and ~7 s to a healthy
    // console on the Windows host, so this check outlives the shared 12-second CLI bound.
    let (code, verdict) = rig.fixture.check_bounded(&["--wait", "30s"], Duration::from_secs(40));
    assert_eq!(code, 0, "a new running revision must reconcile: {verdict}");
}

/// Makes the rig's supervisor start a console the fixture holds no handle for; returns its identity.
fn supervisor_started_console(rig: &mut Rig) -> ProcessIdentity {
    let notices = rig.fixture.0.join("data/backup-notices.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !notices.exists() {
        assert!(
            Instant::now() < deadline,
            "supervisor did not complete its first poll"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let original = rig
        .console
        .as_ref()
        .and_then(|console| console.0.as_ref())
        .map(std::process::Child::id);
    rig.config["backends"]["mcpRustBridge"] = json!(true);
    rig.config["rollout"]["rustGroups"] = json!(["group:123456"]);
    rig.save_config();
    drop(rig.console.take());
    let mut set = hidden(env!("CARGO_BIN_EXE_her-cli"));
    set.args(["lifecycle", "set", "running", "--runtime-root"])
        .arg(&rig.fixture.0);
    let running = run_bounded(&mut set, Duration::from_secs(12))
        .unwrap_or_else(|error| panic!("lifecycle set running {error}"));
    assert!(
        running.status.success(),
        "{}",
        String::from_utf8_lossy(&running.stderr)
    );
    let marker = rig
        .fixture
        .0
        .join("data/logs/rbridge-console.lock.owner.json");
    // Same budget as the reconcile test: ~7 s to a healthy console measured on the Windows host.
    let deadline = Instant::now() + Duration::from_secs(40);
    let identity = loop {
        let identity = std::fs::read(&marker)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| serde_json::from_value::<ProcessIdentity>(value["identity"].clone()).ok());
        if let Some(identity) = identity {
            if Some(identity.pid) != original
                && process_identity::check_identity(&identity) == Liveness::Matches
            {
                break identity;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the supervisor did not start a console within 40 s; marker={}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    // The owner marker is published before the console loads its config (entry.rs acquires the
    // gate first), so a live marker pid does not yet prove the console runs: a console that
    // exits once the fixture is deleted during that window makes both tests pass falsely.
    // Require shared health for this runtime — the console part passes only when /api/health
    // answers with this console's pid — and the same marker identity, before any cleanup.
    // `--wait` re-snapshots until every part passes, like the reconcile test.
    let (code, verdict) = rig
        .fixture
        .check_bounded(&["--wait", "30s"], Duration::from_secs(40));
    assert_eq!(code, 0, "the supervisor-started console never answered health: {verdict}");
    let reread = std::fs::read(&marker)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| serde_json::from_value::<ProcessIdentity>(value["identity"].clone()).ok());
    assert_eq!(
        reread.as_ref(),
        Some(&identity),
        "the console marker no longer names the console that answered health"
    );
    identity
}

fn assert_stopped_after_drop(console: &ProcessIdentity) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match process_identity::check_identity(console) {
            Liveness::Gone | Liveness::Mismatch(_) => return,
            state => assert!(
                Instant::now() < deadline,
                "the supervisor-started console {} outlived the rig: {state:?}",
                console.pid
            ),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn rig_drop_stops_a_console_the_supervisor_started() {
    let mut rig = Rig::new();
    let console = supervisor_started_console(&mut rig);
    drop(rig);
    assert_stopped_after_drop(&console);
}

#[test]
fn rig_drop_stops_a_console_the_supervisor_started_when_the_test_panics() {
    let mut console = None;
    let unwound: std::thread::Result<()> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut rig = Rig::new();
            console = Some(supervisor_started_console(&mut rig));
            panic!("fixture panic after the supervisor started a console");
        }));
    assert!(unwound.is_err(), "the closure must panic");
    let console = console.expect("the console was started before the panic");
    assert_stopped_after_drop(&console);
}
