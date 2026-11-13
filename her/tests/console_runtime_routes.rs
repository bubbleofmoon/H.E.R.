#[path = "../src/install_root.rs"]
mod install_root;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SESSION_KEY: &str = "private:7001";
const OWNER_ID: i64 = 7001;

// SnowLuma 1.14.22: GET /api/processes and OneBot POST /get_status.
struct FakeQqLogin {
    port: u16,
    state: Arc<std::sync::Mutex<(u16, Value, Value)>>,
    alerts: Arc<std::sync::Mutex<Vec<Value>>>,
    attempts: Arc<std::sync::Mutex<Vec<(i64, Value)>>>,
    clock: Arc<std::sync::Mutex<Option<(i64, Instant)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl FakeQqLogin {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(std::sync::Mutex::new((200, json!({"list":[{
            "pid":123,"name":"QQ.exe","path":"C:/synthetic/QQ.exe", "injected":true,
            "connected":true,"loggedIn":true,"uin":"sensitive-uin","status":"online",
            "error":"sensitive-error","method":"hook"
        }]}), json!({"status":"ok","retcode":0,"data":{"online":true,"good":true,"time":1}}))));
        let alerts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let clock = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, a, done) = (state.clone(), alerts.clone(), stop.clone());
        let sent = attempts.clone();
        let delivery_clock = clock.clone();
        let worker = thread::spawn(move || while !done.load(Ordering::SeqCst) {
            let Ok((mut stream, _)) = listener.accept() else { thread::sleep(Duration::from_millis(5)); continue; };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new(); reader.read_line(&mut line).unwrap();
            let method = line.split_whitespace().next().unwrap().to_string();
            let path = line.split_whitespace().nth(1).unwrap().to_string();
            let mut size = 0;
            let mut auth = String::new();
            loop {
                line.clear(); reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() { break; }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") { size = value.trim().parse().unwrap(); }
                    if key.eq_ignore_ascii_case("authorization") { auth = value.trim().into(); }
                }
            }
            let mut bytes = vec![0; size]; reader.read_exact(&mut bytes).unwrap();
            let (code, body) = if path == "/api/processes" {
                assert_eq!(method, "GET");
                assert_eq!(auth, "Bearer fixture-webui-session");
                let state = s.lock().unwrap(); (state.0, state.1.clone())
            } else {
                assert_eq!(method, "POST");
                assert!(matches!(path.as_str(), "/get_status" | "/send_private_msg" | "/fetch_custom_face_detail"), "unexpected fake OneBot request: {method} {path}");
                assert_eq!(auth, "Bearer fixture-onebot-token");
                let state = s.lock().unwrap();
                let reply = if path == "/get_status" { state.2.clone() }
                else if path == "/fetch_custom_face_detail" { json!({"status":"ok","retcode":0,"data":[]}) }
                else {
                    let message: Value = serde_json::from_slice(&bytes).unwrap();
                    let now = Self::clock_ms(&delivery_clock.lock().unwrap());
                    sent.lock().unwrap().push((now, message.clone()));
                    if state.2["data"]["online"] == true {
                        a.lock().unwrap().push(message);
                        json!({"status":"ok","retcode":0,"data":{"message_id":1}})
                    } else {
                        json!({"status":"failed","retcode":100,"wording":"QQ is offline"})
                    }
                };
                (200, reply)
            };
            let body = body.to_string();
            let _ = write!(stream, "HTTP/1.1 {code} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        });
        Self { port, state, alerts, attempts, clock, stop, worker: Some(worker) }
    }
    fn clock_ms(clock: &Option<(i64, Instant)>) -> i64 {
        match clock {
            Some((epoch, started)) => epoch + started.elapsed().as_millis() as i64,
            None => SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
        }
    }
    fn configure(&self, root: &Path) {
        let path = root.join("config/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["snowluma"] = json!({"webuiUrl":format!("http://127.0.0.1:{}",self.port),"accessToken":"fixture-onebot-token"});
        fs::write(path, config.to_string()).unwrap();
        fs::write(root.join("data/snowluma-webui-token"), "fixture-webui-session").unwrap();
        fs::write(root.join("dependencies.json"), json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":format!("http://127.0.0.1:{}",self.port)}).to_string()).unwrap();
    }
}
impl Drop for FakeQqLogin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() { assert!(result.is_ok(), "fake SnowLuma worker failed"); }
    }
}

#[test]
fn qq_login_route_reports_safe_state_and_requires_administrator() {
    let workspace = FixtureWorkspace::new("qq-login");
    let root = workspace.root("runtime");
    let home = prepare_runtime_root(&root,"qq-console","qq-agent","http://127.0.0.1:1","http://127.0.0.1:1",None,None);
    let fake = FakeQqLogin::new(); fake.configure(&root);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root,port,&home);
    let response = expect_json(http_request(port,"GET","/api/qq/login",Some("qq-console"),None,None).unwrap(),200,"QQ login state");
    assert_eq!(response, json!({"ok":true,"state":"logged-in","processes":[]}));
    #[cfg(windows)]
    {
        private_token_file(&root,"qq-console");
        let signed = expect_json(http_request(port,"POST","/api/desktop/sign-in",Some("qq-console"),None,Some(&json!({}))).unwrap(),200,"desktop sign-in");
        let session = signed["session"].as_str().unwrap();
        let desktop = expect_json(http_request(port,"GET","/api/qq/login",Some(session),None,None).unwrap(),200,"desktop QQ login state");
        assert_eq!(desktop,response);
        expect_json(http_request(port,"GET","/api/qq/login",Some(session),Some("qq-agent"),None).unwrap(),403,"desktop session cannot confer agent access");
    }
    expect_json(http_request(port,"GET","/api/qq/login",None,None,None).unwrap(),401,"anonymous refused");
    expect_json(http_request(port,"GET","/api/qq/login",None,Some("qq-agent"),None).unwrap(),401,"agent token alone refused");
    expect_json(http_request(port,"GET","/api/qq/login",Some("qq-console"),Some("qq-agent"),None).unwrap(),403,"agent refused");
    {
        let mut state = fake.state.lock().unwrap();
        state.1["list"][0]["loggedIn"] = json!(false);
        state.1["list"][0]["status"] = json!("loaded");
        state.2["data"]["online"] = json!(false);
    }
    let response = expect_json(http_request(port,"GET","/api/qq/login",Some("qq-console"),None,None).unwrap(),200,"QQ needs login");
    assert_eq!(response["state"],"needs-login");
}

#[test]
fn qq_login_webui_transitions_and_unknown_states() {
    let workspace = FixtureWorkspace::new("qq-login-transitions");
    let root = workspace.root("runtime");
    let home = prepare_runtime_root(&root,"qq-console","qq-agent","http://127.0.0.1:1","http://127.0.0.1:1",None,None);
    let fake = FakeQqLogin::new(); fake.configure(&root);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root,port,&home);
    let check = |state: &str| {
        let response = expect_json(http_request(port,"GET","/api/qq/login",Some("qq-console"),None,None).unwrap(),200,"QQ login state");
        assert_eq!(response,json!({"ok":true,"state":state,"processes":[]}));
    };
    check("logged-in");
    let loaded = {
        let mut state = fake.state.lock().unwrap();
        state.1["list"][0]["loggedIn"] = json!(false);
        state.1["list"][0]["status"] = json!("loaded");
        state.2["data"]["online"] = json!(false);
        state.1.clone()
    };
    check("needs-login");
    // This fixture reports no live matching QQ process, so the route must not
    // trust its PID or leak its path, UIN, error or credentials as an action target.
    for (code, webui, onebot) in [
        (200, json!({"list":[]}), json!({"status":"ok","retcode":0,"data":{"online":false,"good":true}})),
        (200, json!({"list":[{"connected":false,"injected":true,"loggedIn":false,"status":"loaded"}]}), json!({"status":"ok","retcode":0,"data":{"online":false}})),
        (200, json!({"list":[{"connected":true,"injected":true,"loggedIn":null,"status":"loaded"}]}), json!({"status":"ok","retcode":0,"data":{}})),
        (200, json!({"list":[{"connected":true,"injected":true,"loggedIn":false,"status":"error"}]}), json!({"status":"failed","retcode":1,"data":{"online":false}})),
        (401, json!({"message":"sensitive-auth-error"}), json!({"status":"ok","retcode":0,"data":{"online":false}})),
        (500, json!({"list":[],"message":"sensitive-error"}), json!({"status":"ok","retcode":0,"data":{"online":null}})),
        (200, loaded.clone(), json!({"status":"ok","retcode":0,"data":{"online":true}})),
        (200, json!({"list":[loaded["list"][0].clone(),{"connected":true,"injected":true,"loggedIn":true,"status":"online"}]}), json!({"status":"ok","retcode":0,"data":{"online":true}})),
    ] {
        *fake.state.lock().unwrap() = (code,webui,onebot);
        check("unknown");
    }
    // A successful OneBot online verdict works without WebUI credentials;
    // receive-chain good:false does not mean that QQ needs login.
    *fake.state.lock().unwrap() = (401,json!({}),json!({"status":"ok","retcode":0,"data":{"online":true,"good":false}}));
    check("logged-in");
    fs::remove_file(root.join("data/snowluma-webui-token")).unwrap();
    fake.state.lock().unwrap().2["data"]["online"] = json!(false);
    check("unknown");
    // Restore the credentials and prove the complete recovery transition.
    fs::write(root.join("data/snowluma-webui-token"),"fixture-webui-session").unwrap();
    *fake.state.lock().unwrap() = (200,loaded,json!({"status":"ok","retcode":0,"data":{"online":false}}));
    check("needs-login");
    {
        let mut state = fake.state.lock().unwrap();
        state.1["list"][0]["loggedIn"] = json!(true);
        state.1["list"][0]["status"] = json!("online");
        state.2["data"]["online"] = json!(true);
    }
    check("logged-in");
    // No configured WebUI means no guessed default endpoint.
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config["snowluma"].as_object_mut().unwrap().remove("webuiUrl");
    fs::write(config_file,config.to_string()).unwrap();
    fake.state.lock().unwrap().2["data"]["online"] = json!(false);
    check("unknown");
    config["snowluma"]["webuiUrl"] = json!(format!("http://127.0.0.1:{}",fake.port));
    fs::write(root.join("config/config.json"),config.to_string()).unwrap();
    drop(fake);
    check("unknown"); // Both configured services are now unreachable.
}

#[cfg(windows)]
#[test]
fn qq_login_verifies_reported_native_process() {
    let workspace = FixtureWorkspace::new("qq-login-native-identity");
    let root = workspace.root("runtime");
    let home = prepare_runtime_root(&root,"qq-console","qq-agent","http://127.0.0.1:1","http://127.0.0.1:1",None,None);
    let fake = FakeQqLogin::new(); fake.configure(&root);
    let (port, native_port) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root,port,&home);
    let native_root = workspace.root("fake-qq-runtime");
    let native_home = prepare_runtime_root(&native_root,"native-fixture","native-agent","http://127.0.0.1:1","http://127.0.0.1:1",None,None);
    let image = native_root.join("resources/QQ.exe");
    fs::copy(env!("CARGO_BIN_EXE_her-cli"), &image).unwrap();
    let mut command = Command::new(&image);
    command.args(["--console-serve","--runtime-root"]).arg(&native_root)
        .args(["--console-port", &native_port.to_string()]).env_remove("HER_RUNTIME_ROOT")
        .env_remove("HOME").env_remove("APPDATA").env_remove("LOCALAPPDATA")
        .env("USERPROFILE",native_root.join("synthetic-profile"))
        .env("DSH_HOME",native_home).stdout(Stdio::null()).stderr(Stdio::null());
    hide_process_window(&mut command);
    let mut native = ConsoleProcess { child: command.spawn().unwrap(), port: native_port };
    wait_for_health(&mut native).unwrap();
    let mut identify = Command::new(env!("CARGO_BIN_EXE_her-cli"));
    identify.args(["process","identify","--runtime-root"]).arg(&native_root)
        .args(["--pid", &native.child.id().to_string()]).env_remove("HER_RUNTIME_ROOT");
    hide_process_window(&mut identify);
    let identified = identify.output().unwrap();
    assert!(identified.status.success());
    let identity: Value = serde_json::from_slice(&identified.stdout).unwrap();
    let identity = &identity["process"];
    println!("owned native fixture identity={identity}");
    assert_eq!(identity["pid"], native.child.id());
    assert!(identity["start"].as_u64().unwrap() > 0);
    {
        let mut state = fake.state.lock().unwrap();
        state.1["list"][0]["pid"] = json!(native.child.id());
        state.1["list"][0]["path"] = json!(image);
        state.1["list"][0]["loggedIn"] = json!(false);
        state.1["list"][0]["status"] = json!("loaded");
        state.2["data"]["online"] = json!(false);
    }
    let read = || expect_json(http_request(port,"GET","/api/qq/login",Some("qq-console"),None,None).unwrap(),200,"native QQ target");
    assert_eq!(read(),json!({"ok":true,"state":"needs-login","processes":[{"pid":identity["pid"],"start":identity["start"]}]}));
    fake.state.lock().unwrap().1["list"][0]["path"] = json!(root.join("resources/QQ.exe"));
    assert_eq!(read()["processes"],json!([]),"a live PID with a mismatched reported image must not be actionable");
    fake.state.lock().unwrap().1["list"][0]["path"] = json!(image);
    drop(native);
    assert_eq!(read()["processes"],json!([]),"an exited instance must not be actionable");
}

#[test]
fn qq_login_alert_episode_survives_supervisor_restarts_and_rearms_after_login() {
    let workspace = FixtureWorkspace::new("qq-login-episodes");
    let root = workspace.root("runtime");
    let home = prepare_runtime_root(&root,"qq-console","qq-agent","http://127.0.0.1:1","http://127.0.0.1:1",None,None);
    let fake = FakeQqLogin::new(); fake.configure(&root);
    let install = workspace.root("protected-install");
    let scheduler = workspace.root("scheduler.xml");
    let mut install_task = Command::new(env!("CARGO_BIN_EXE_her-cli"));
    install_task.arg("install")
        .env("HER_INSTALL_ROOT", &install).env("HER_TASK_SCHEDULER_FILE", &scheduler)
        .env_remove("HER_RUNTIME_ROOT");
    hide_process_window(&mut install_task);
    let installed = install_task.output().unwrap();
    assert!(installed.status.success(), "install matching fixture task: {installed:?}");
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    for task in ["watchdog","memory-digest","memory-digest-check","upstream-check","console-restart","backup"] {
        config["tasks"][task] = json!({"enabled":false});
    }
    fs::write(config_file,config.to_string()).unwrap();
    let online = fake.state.lock().unwrap().clone();
    let mut needed = online.clone();
    needed.1["list"][0]["loggedIn"] = json!(false);
    needed.1["list"][0]["status"] = json!("loaded");
    needed.2["data"]["online"] = json!(false);
    let unknown = (401,json!({"message":"expired"}),json!({"status":"ok","retcode":0,"data":{"online":false}}));
    let episode_file = root.join("data/qq-login-episode.json");
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let assert_spacing = || {
        let attempts = fake.attempts.lock().unwrap();
        for pair in attempts.windows(2) {
            let spacing = pair[1].0 - pair[0].0;
            assert!(spacing >= 60_000, "QQ login delivery attempts must be at least 60 s apart: {spacing} ms");
        }
    };
    for (state, expected, attempts, advance) in [
        (online.clone(),0,0,0),(needed.clone(),0,1,120),(needed.clone(),0,1,120),
        (needed.clone(),0,1,165),(unknown,0,1,165),(needed.clone(),0,1,165),(online.clone(),1,2,240),
        (online.clone(),1,2,240),(needed.clone(),1,3,360),(online.clone(),2,4,480),
        (online,2,4,480),(needed,2,5,600),
    ] {
        let notified = expected > 0 && state.2["data"]["online"] == true;
        *fake.state.lock().unwrap() = state;
        let previous = fs::read(&episode_file).ok();
        // Restart 45 s into the first pending retry, independently of the
        // product deadline. This consumes part of the existing default wait.
        if !cfg!(feature = "parity-clock") && advance == 165 {
            let attempted_at = fake.attempts.lock().unwrap()[0].0;
            let wait_ms = attempted_at + 45_000 - FakeQqLogin::clock_ms(&None);
            if wait_ms > 0 {
                thread::sleep(Duration::from_millis(wait_ms as u64));
            }
        }
        // Default binaries use the host clock: synthetic epoch advances have no
        // effect. Wait for the real deadline only when this step needs a new
        // attempt; rapid restarts at the same epoch must still stay quiet.
        if !cfg!(feature = "parity-clock") && attempts > fake.attempts.lock().unwrap().len() {
            if let Some(bytes) = &previous {
                let pending: Value = serde_json::from_slice(bytes).unwrap();
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
                let wait_ms = pending["retry_after_ms"].as_i64().unwrap().saturating_sub(now);
                assert!(wait_ms <= 60_000, "QQ login retry exceeded the one-minute schedule");
                if wait_ms > 0 {
                    thread::sleep(Duration::from_millis(wait_ms as u64));
                }
            }
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command.args(["supervise","--runtime-root"]).arg(&root)
            .env("HER_INSTALL_ROOT", &install).env("HER_TASK_SCHEDULER_FILE", &scheduler)
            .env_remove("HER_RUNTIME_ROOT").env_remove("APPDATA").env_remove("LOCALAPPDATA").env_remove("HOME")
            .env("DSH_HOME",&home).env("USERPROFILE",home.join("synthetic-profile"))
            .stdout(Stdio::null()).stderr(Stdio::null());
        if cfg!(feature = "parity-clock") {
            let step_epoch = epoch + advance * 1000;
            command.env("HER_PARITY_EPOCH_MS", step_epoch.to_string());
            // Mirror the child clock's epoch + monotonic elapsed time at the
            // fake delivery boundary; never derive attempt times from its state.
            *fake.clock.lock().unwrap() = Some((step_epoch, Instant::now()));
        }
        if advance == 165 {
            let elapsed = FakeQqLogin::clock_ms(&fake.clock.lock().unwrap()) - fake.attempts.lock().unwrap()[0].0;
            assert!((30_000..60_000).contains(&elapsed), "restart must probe a shortened retry before one minute: {elapsed} ms");
            println!("early supervisor restart: {elapsed} ms after first attempt; must stay at one attempt");
        }
        hide_process_window(&mut command);
        let mut supervisor = ConsoleProcess { child: command.spawn().unwrap(), port: 0 };
        let mut identify = Command::new(env!("CARGO_BIN_EXE_her-cli"));
        identify.args(["process","identify","--runtime-root"]).arg(&root)
            .args(["--pid",&supervisor.child.id().to_string()]).env_remove("HER_RUNTIME_ROOT");
        hide_process_window(&mut identify);
        let identity = identify.output().unwrap();
        assert!(identity.status.success());
        let identity: Value = serde_json::from_slice(&identity.stdout).unwrap();
        assert_eq!(identity["process"]["pid"],supervisor.child.id());
        assert!(identity["process"]["start"].as_u64().unwrap() > 0);
        println!("owned supervisor identity={}",identity["process"]);
        let deadline = Instant::now()+Duration::from_secs(10);
        while fs::read(&episode_file).ok().is_none_or(|bytes| Some(bytes) == previous) {
            assert!(supervisor.child.try_wait().unwrap().is_none(),"supervisor exited");
            assert!(Instant::now()<deadline,"supervisor did not publish a QQ login observation");
            thread::sleep(Duration::from_millis(20));
        }
        let read = || serde_json::from_slice::<Value>(&fs::read(&episode_file).unwrap()).unwrap();
        let deadline = Instant::now()+Duration::from_secs(10);
        while fake.alerts.lock().unwrap().len() < expected
            || fake.attempts.lock().unwrap().len() < attempts
            || read()["notified"] != notified
        {
            assert!(supervisor.child.try_wait().unwrap().is_none(),"supervisor exited before delivery completed");
            assert!(Instant::now()<deadline,"QQ login delivery did not complete: {}",read());
            thread::sleep(Duration::from_millis(20));
        }
        // Keep a quiet window after completion to catch duplicate/rapid sends.
        thread::sleep(Duration::from_millis(500));
        assert_spacing();
        assert_eq!(fake.alerts.lock().unwrap().len(),expected,"one alert per confirmed re-login episode");
        assert_eq!(fake.attempts.lock().unwrap().len(),attempts,"delivery attempts at epoch +{advance}s");
        println!("QQ login step: parity-clock={}, advance={advance}s, delivered={expected}, attempts={attempts}",cfg!(feature = "parity-clock"));
        if advance == 120 {
            assert_eq!(fake.attempts.lock().unwrap().len(),1,"failed delivery stays pending without rapid retries across restarts");
        }
        if advance == 600 {
            let pending: Value = serde_json::from_slice(&fs::read(&episode_file).unwrap()).unwrap();
            assert_eq!(pending["notified"],false,"an offline refusal must not acknowledge the episode");
            assert_eq!(fake.attempts.lock().unwrap().len(),5);
            {
                let mut state = fake.state.lock().unwrap();
                state.1["list"][0]["loggedIn"] = json!(true);
                state.1["list"][0]["status"] = json!("online");
                state.2["data"]["online"] = json!(true);
            }
            // Keep this supervisor alive: recovery must deliver on later 30 s
            // polls, not depend on restarting the observer. No test-only poll knob.
            let deadline = Instant::now()+Duration::from_secs(100);
            while fake.alerts.lock().unwrap().len() < 3 {
                assert!(supervisor.child.try_wait().unwrap().is_none(),"supervisor exited before retry");
                assert!(Instant::now()<deadline,"pending QQ notice was lost after recovery");
                thread::sleep(Duration::from_millis(20));
            }
            let checks = read()["checks"].as_u64().unwrap();
            while read()["checks"].as_u64().unwrap() <= checks {
                assert!(Instant::now()<deadline,"supervisor did not poll after successful delivery");
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(read()["notified"],true);
            assert_eq!(fake.alerts.lock().unwrap().len(),3,"successful delivery suppresses later polls");
            assert_eq!(fake.attempts.lock().unwrap().len(),6,"three offline refusals and three successful notices");
            assert_spacing();
            println!("same-supervisor recovery: 3 delivered, 6 attempts; next poll stayed quiet");
        }
        drop(supervisor);
    }
    println!("QQ login attempt times (ms): {:?}", fake.attempts.lock().unwrap().iter().map(|(at, _)| at).collect::<Vec<_>>());
    for notice in fake.alerts.lock().unwrap().iter() {
        assert_eq!(notice["user_id"],OWNER_ID);
        assert!(notice.to_string().contains("QQ needs login"));
        for secret in ["sensitive-uin","fixture-webui-session","fixture-onebot-token","sensitive-error"] {
            assert!(!notice.to_string().contains(secret));
        }
    }
}

struct FixtureWorkspace(PathBuf);

impl FixtureWorkspace {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "qqbot-console-public-{label}-{}-{stamp}",
            std::process::id()
        )));
        fs::create_dir_all(&root).expect("create synthetic HTTP fixture workspace");
        Self(root)
    }

    fn root(&self, name: &str) -> PathBuf {
        self.0.join(name)
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

fn encoded_secret(secret: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(secret)
}

fn prepare_runtime_root(
    root: &Path,
    console_token: &str,
    agent_token: &str,
    selected_dsh_url: &str,
    legacy_dsh_url: &str,
    selected_secret: Option<&[u8]>,
    legacy_secret: Option<&[u8]>,
) -> PathBuf {
    let checked = crate::install_root::development_root(root);
    let root = checked.as_path();
    for relative in ["config", "data", "resources/roles", "user/dsh"] {
        fs::create_dir_all(root.join(relative)).expect("create selected runtime directory");
    }

    let mut conversations = serde_json::Map::new();
    conversations.insert(
        SESSION_KEY.to_string(),
        json!({ "agentToken": agent_token }),
    );
    let config = json!({
        "consoleToken": console_token,
        "ownerQQ": OWNER_ID,
        "allow": { "groups": [], "private": [OWNER_ID.to_string()] },
        "dsh": {
            "baseUrl": legacy_dsh_url,
            "provider": "fixture",
            "model": "fixture-model",
            "reasoningEffort": "high"
        }
    });
    let dependencies = json!({
        "dsh": selected_dsh_url,
        "model": "http://127.0.0.1:1",
        "onebot": "http://127.0.0.1:1"
    });
    fs::write(
        root.join("config/config.json"),
        serde_json::to_vec_pretty(&config).expect("serialize synthetic config"),
    )
    .expect("write selected config fixture");
    fs::write(
        root.join("data/social-v2.json"),
        serde_json::to_vec(&json!({ "conversations": conversations }))
            .expect("serialize synthetic session state"),
    )
    .expect("write selected state fixture");
    fs::write(
        root.join("dependencies.json"),
        serde_json::to_vec(&dependencies).expect("serialize fake dependency map"),
    )
    .expect("write selected dependency fixture");

    if let Some(secret) = selected_secret {
        fs::write(
            root.join("user/dsh/.credentials.yaml"),
            format!("secret: {}\n", encoded_secret(secret)),
        )
        .expect("write selected DSH credential fixture");
    }

    let dsh_home = root.join("ambient-dsh-home");
    fs::create_dir_all(&dsh_home).expect("create synthetic ambient DSH home");
    if let Some(secret) = legacy_secret {
        fs::write(
            dsh_home.join(".credentials.yaml"),
            format!("secret: {}\n", encoded_secret(secret)),
        )
        .expect("write synthetic DSH credential decoy");
    }
    dsh_home
}

fn initialize_memory_fixture(root: &Path) {
    let connection =
        rusqlite::Connection::open(root.join("data/memory.db")).expect("create fixture database");
    connection
        .execute_batch(
            "CREATE TABLE memory (
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
             CREATE TABLE team_member (team_id TEXT NOT NULL, member_type TEXT NOT NULL, member_id TEXT NOT NULL);
             CREATE TABLE memory_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
                actor TEXT, action TEXT, memory_id INTEGER, detail TEXT
             );",
        )
        .expect("initialize synthetic memory schema");
}

fn reserve_distinct_ports() -> (u16, u16) {
    let first = TcpListener::bind(("127.0.0.1", 0)).expect("reserve first console port");
    let second = TcpListener::bind(("127.0.0.1", 0)).expect("reserve second console port");
    let first_port = first.local_addr().expect("read first port").port();
    let second_port = second.local_addr().expect("read second port").port();
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
        Self::start_with(root, port, dsh_home, None, None)
    }

    fn start_with_peer_sid(root: &Path, port: u16, dsh_home: &Path, peer_sid: Option<&str>) -> Self {
        Self::start_with(root, port, dsh_home, peer_sid, None)
    }

    fn start_with_install_root(root: &Path, port: u16, dsh_home: &Path, install: Option<&Path>) -> Self {
        Self::start_with(root, port, dsh_home, None, install)
    }

    fn start_with(root: &Path, port: u16, dsh_home: &Path, peer_sid: Option<&str>, install: Option<&Path>) -> Self {
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
        if let Some(sid) = peer_sid { command.env("HER_TEST_DESKTOP_PEER_SID_HEX", sid); }
        if let Some(install) = install { command.env("HER_INSTALL_ROOT", install); }
        hide_process_window(&mut command);
        let child = command.spawn().expect("start isolated console candidate");
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

#[cfg(windows)]
#[test]
fn desktop_sign_in_refuses_configured_token_without_a_token_file() {
    let workspace = FixtureWorkspace::new("desktop-missing-token-file");
    let root = workspace.root("runtime");
    let token = "fixture-desktop-console-token";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    assert!(!root.join("data/console-token").exists());
    expect_json(http_request(port, "GET", "/api/status", Some(token), None, None).unwrap(), 200, "configured console token still works");
    let denied = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(token), None, Some(&json!({}))).unwrap(), 403, "desktop requires the protected file");
    assert!(denied.get("session").is_none());
}

#[cfg(windows)]
fn token_file_acl(path: &Path, args: &[&str]) {
    let mut command = Command::new("icacls.exe");
    command.arg(path).args(args);
    hide_process_window(&mut command);
    let output = command.output().expect("configure synthetic token file ACL");
    assert!(output.status.success(), "fixture ACL: {output:?}");
}

#[cfg(windows)]
fn private_token_file(root: &Path, token: &str) {
    let path = root.join("data/console-token");
    fs::write(&path, token).unwrap();
    let user = format!("{}:(F)", std::env::var("USERNAME").unwrap());
    token_file_acl(&path, &["/inheritance:r", "/grant:r", &user, "*S-1-5-18:(F)", "*S-1-5-32-544:(F)"]);
}

#[cfg(windows)]
#[test]
fn desktop_sign_in_refuses_broad_token_file_acl() {
    let workspace = FixtureWorkspace::new("desktop-broad-token-file");
    let root = workspace.root("runtime");
    let token = "fixture-desktop-console-token";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    private_token_file(&root, token);
    token_file_acl(&root.join("data/console-token"), &["/grant", "*S-1-1-0:(R)"]);
    let denied = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(token), None, Some(&json!({}))).unwrap(), 403, "broad token file ACL refuses sign-in");
    assert!(denied.get("session").is_none());
}

#[cfg(windows)]
#[test]
fn desktop_sign_in_refuses_unreadable_token_file() {
    use std::os::windows::fs::OpenOptionsExt;
    let workspace = FixtureWorkspace::new("desktop-unreadable-token-file");
    let root = workspace.root("runtime");
    let token = "fixture-desktop-console-token";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    private_token_file(&root, token);
    let _exclusive = fs::OpenOptions::new().read(true).share_mode(0).open(root.join("data/console-token")).unwrap();
    let denied = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(token), None, Some(&json!({}))).unwrap(), 403, "unreadable token file refuses sign-in");
    assert!(denied.get("session").is_none());
}

#[cfg(windows)]
#[test]
fn desktop_session_never_revives_after_token_rotation_a_b_a() {
    let workspace = FixtureWorkspace::new("desktop-token-rotation");
    let root = workspace.root("runtime");
    let token_a = "fixture-desktop-console-token-a";
    let token_b = "fixture-desktop-console-token-b";
    let home = prepare_runtime_root(&root, token_a, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    private_token_file(&root, token_a);
    let signed_in = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(token_a), None, Some(&json!({}))).unwrap(), 200, "initial desktop sign-in");
    let session = signed_in["session"].as_str().unwrap();
    expect_json(http_request(port, "GET", "/api/status", Some(session), None, None).unwrap(), 200, "initial session works");
    for (old, new) in [(token_a, token_b), (token_b, token_a)] {
        expect_json(http_request(port, "POST", "/api/console/token", Some(old), None, Some(&json!({"token":new}))).unwrap(), 200, "successful rotation");
        expect_json(http_request(port, "GET", "/api/status", Some(session), None, None).unwrap(), 401, "rotation permanently revokes the old session");
    }
    let fresh = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(token_a), None, Some(&json!({}))).unwrap(), 200, "new desktop sign-in works after rotation");
    expect_json(http_request(port, "GET", "/api/status", fresh["session"].as_str(), None, None).unwrap(), 200, "new session works");
}

#[cfg(windows)]
#[test]
fn desktop_sign_in_issues_a_console_session_to_the_installing_user() {
    let workspace = FixtureWorkspace::new("desktop-sign-in");
    let root = workspace.root("runtime");
    let token = "fixture-desktop-console-token";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    private_token_file(&root, token);
    expect_json(http_request(port, "GET", "/api/status", None, None, None).unwrap(), 401, "console requires authentication");
    let signed_in = expect_json(
        http_request(port, "POST", "/api/desktop/sign-in", Some(token), None, Some(&json!({}))).unwrap(),
        200,
        "same-user desktop sign-in",
    );
    let session = signed_in["session"].as_str().expect("desktop session credential");
    assert_ne!(session, token, "sign-in issues a session rather than exposing the stored credential");
    expect_json(http_request(port, "GET", "/api/status", Some(session), None, None).unwrap(), 200, "desktop session authenticates console");
}

#[cfg(windows)]
#[test]
fn desktop_sign_in_refuses_a_different_caller_identity_even_with_the_console_credential() {
    let workspace = FixtureWorkspace::new("desktop-foreign-user");
    let root = workspace.root("runtime");
    let token = "fixture-desktop-console-token";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    // Null SID S-1-0-0 simulates a caller identity outside the installing user.
    // This rejection fixture cannot grant authority to a real foreign peer.
    let _console = ConsoleProcess::start_with_peer_sid(&root, port, &home, Some("010100000000000000000000"));
    private_token_file(&root, token);
    expect_json(http_request(port, "GET", "/api/status", Some(token), None, None).unwrap(), 200, "console credential is valid");
    let denied = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(token), None, Some(&json!({}))).unwrap(), 403, "different-user desktop sign-in");
    assert!(denied.get("session").is_none(), "a refused identity must receive no session");
}

#[cfg(windows)]
#[test]
fn desktop_session_reaches_console_memory_administration() {
    let workspace = FixtureWorkspace::new("desktop-memory-administration");
    let root = workspace.root("runtime");
    let token = "fixture-desktop-console-token";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    private_token_file(&root, token);
    expect_json(http_request(port, "GET", "/api/memory/acl", Some(token), None, None).unwrap(), 200, "console memory administration is available");
    let signed_in = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(token), None, Some(&json!({}))).unwrap(), 200, "desktop sign-in");
    let session = signed_in["session"].as_str().unwrap();
    expect_json(http_request(port, "GET", "/api/memory/acl", Some(session), None, None).unwrap(), 200, "desktop session has the console's memory-administration authority");
}

#[test]
fn memory_administration_refuses_the_raw_generated_file_token() {
    let workspace = FixtureWorkspace::new("memory-generated-token");
    let root = workspace.root("runtime");
    let configured = "fixture-memory-configured-token";
    let home = prepare_runtime_root(&root, configured, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    let rotated = expect_json(
        http_request(port, "POST", "/api/console/token", Some(configured), None, Some(&json!({}))).unwrap(),
        200,
        "generate a file-backed console token",
    );
    let generated = rotated["token"].as_str().unwrap();
    expect_json(http_request(port, "GET", "/api/status", Some(generated), None, None).unwrap(), 200, "generated token still authenticates ordinary console routes");
    expect_json(http_request(port, "GET", "/api/memory/acl", Some(generated), None, None).unwrap(), 403, "raw generated token has no memory-administration authority");
}

#[cfg(windows)]
#[test]
fn desktop_session_with_a_generated_token_reaches_memory_administration() {
    let workspace = FixtureWorkspace::new("desktop-generated-memory-administration");
    let root = workspace.root("runtime");
    let configured = "fixture-desktop-configured-token";
    let home = prepare_runtime_root(&root, configured, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let (port, _) = reserve_distinct_ports();
    let _console = ConsoleProcess::start(&root, port, &home);
    let rotated = expect_json(http_request(port, "POST", "/api/console/token", Some(configured), None, Some(&json!({}))).unwrap(), 200, "generate the protected-file credential");
    let generated = rotated["token"].as_str().unwrap();
    let signed_in = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(generated), None, Some(&json!({}))).unwrap(), 200, "sign in through the protected file with an empty configured token");
    let session = signed_in["session"].as_str().unwrap();
    expect_json(http_request(port, "GET", "/api/memory/acl", Some(session), None, None).unwrap(), 200, "generated-token desktop session has memory authority");

    expect_json(http_request(port, "POST", "/api/console/token", Some(generated), None, Some(&json!({"token":generated}))).unwrap(), 200, "rotate to the same credential value");
    // A nonempty invalid agent header reaches the memory handler even after session revocation.
    expect_json(http_request(port, "GET", "/api/memory/acl", Some(session), Some("invalid-memory-agent"), None).unwrap(), 403, "the memory handler rejects the session from the previous generation");
    let fresh = expect_json(http_request(port, "POST", "/api/desktop/sign-in", Some(generated), None, Some(&json!({}))).unwrap(), 200, "sign in to the current generation");
    expect_json(http_request(port, "GET", "/api/memory/acl", fresh["session"].as_str(), None, None).unwrap(), 200, "a current-generation desktop session regains memory authority");
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    console_token: Option<&str>,
    agent_token: Option<&str>,
    body: Option<&Value>,
) -> Result<HttpResponse, String> {
    let body = body.map(Value::to_string).unwrap_or_default();
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

fn wait_for_sticker_warmup(port: u16, token: &str, store: &Path, seeded: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = "startup has not persisted the seeded library".to_string();
    while Instant::now() < deadline {
        // Health only witnesses the listener. These fixtures seed an unnormalized
        // store, so its first replacement witnesses the actual startup worker.
        // Calling the list route before that could itself refresh the cache and
        // leave the forced startup refresh queued behind it.
        if fs::read(store).is_ok_and(|bytes| bytes != seeded) {
            match http_request(port, "GET", "/api/stickers", Some(token), None, None) {
                Ok(response) => {
                    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
                    if response.status == 200 && body["fromCache"] == true
                        && body["syncedAt"].as_i64().is_some_and(|at| at > 0) {
                        return;
                    }
                    last = format!("status {}: {}", response.status, response.body);
                }
                Err(error) => last = error,
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("sticker startup did not publish its completed cache: {last}");
}

#[test]
fn backup_install_root_destination_defaults_and_relative_paths_stay_outside_runtime() {
    let workspace = FixtureWorkspace::new("backup-install-root");
    let install = workspace.root("install");
    let root = install.join("runtime");
    let token = "fixture-installed-backup-console";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let release = install.join("releases/fixture");
    fs::create_dir_all(&release).unwrap();
    let image = release.join(if cfg!(windows) { "her.exe" } else { "her" });
    fs::copy(env!("CARGO_BIN_EXE_her"), &image).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_her-cli"), release.join(if cfg!(windows) { "her-cli.exe" } else { "her-cli" })).unwrap();
    let (port, _) = reserve_distinct_ports();
    let mut command = Command::new(&image);
    command.args(["--console-serve", "--console-port", &port.to_string(), "--runtime-root"])
        .arg(&root).env("HER_INSTALL_ROOT", &install).env_remove("HER_RUNTIME_ROOT")
        .env_remove("HOME").env_remove("APPDATA").env_remove("LOCALAPPDATA")
        .env("DSH_HOME", &home).env("USERPROFILE", home.join("synthetic-profile"))
        .stdout(Stdio::null()).stderr(Stdio::null());
    hide_process_window(&mut command);
    let mut console = ConsoleProcess { child: command.spawn().unwrap(), port };
    wait_for_health(&mut console).expect("throwaway installed console must start");
    let run = |request: &str| {
        let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&json!({"requestId":request}))).unwrap(), 202, "installed backup trigger");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let result = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{}", started["runId"].as_str().unwrap()), Some(token), None, None).unwrap(), 200, "installed backup result");
            if !matches!(result["run"]["state"].as_str(), Some("queued" | "running")) { break result["run"].clone(); }
            assert!(Instant::now() < deadline, "installed backup did not finish: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    };
    assert_eq!(run("backup-installed-default-01")["state"], "succeeded");
    assert!(install.join("backups").is_dir(), "backup must default to the install root's backups directory");
    assert!(!root.join("backups").exists(), "installed backups must be outside runtime");
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config["backup"] = json!({"dir":"custom backups"});
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(run("backup-installed-relative-02")["state"], "succeeded");
    assert!(install.join("custom backups").is_dir());
    assert!(!root.join("custom backups").exists());
    for area in ["releases", "deps", "runtime"] {
        config["backup"] = json!({"dir":install.join(area).join("forbidden backups")});
        fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut check = Command::new(release.join(if cfg!(windows) { "her-cli.exe" } else { "her-cli" }));
        let (probe_port, _) = reserve_distinct_ports();
        check.args(["--console-serve", "--console-port", &probe_port.to_string(), "--runtime-root"]).arg(&root)
            .env("HER_INSTALL_ROOT", &install).env_remove("HER_RUNTIME_ROOT")
            .env_remove("HOME").env_remove("APPDATA").env_remove("LOCALAPPDATA")
            .env("DSH_HOME", &home).env("USERPROFILE", home.join("synthetic-profile"))
            .stdout(Stdio::null()).stderr(Stdio::piped());
        hide_process_window(&mut check);
        let mut probe = ConsoleProcess { child: check.spawn().expect("load probe must start"), port: probe_port };
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = probe.child.try_wait().unwrap() { break status; }
            assert!(Instant::now() < deadline, "invalid backup destination must refuse startup");
            thread::sleep(Duration::from_millis(20));
        };
        let mut reason = String::new();
        probe.child.stderr.take().unwrap().read_to_string(&mut reason).unwrap();
        assert!(!status.success(), "backup destination inside {area} must be refused at load");
        assert!(reason.contains("backup destination"), "load refusal must explain the destination: {reason}");
        drop(probe);
        assert!(!install.join(area).join("forbidden backups").exists());
    }
    drop(console);
}

#[test]
fn backup_console_settings_preserve_raw_config_and_apply_to_the_next_run() {
    let workspace = FixtureWorkspace::new("backup-settings-api");
    let root = workspace.root("runtime");
    let install = workspace.root("protected-install");
    let token = "fixture-backup-settings-console";
    let agent = "fixture-backup-settings-agent";
    let home = prepare_runtime_root(&root, token, agent, "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("ownerQQ");
    config["backup"] = json!({"keep":42});
    config["ticket23Sentinel"] = json!({"preserve":"raw spelling"});
    let original_dsh = config["dsh"].clone();
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start_with_install_root(&root, port, &home, Some(&install));
    assert_eq!(http_request(port, "GET", "/api/backups/settings", None, None, None).unwrap().status, 401);
    assert_eq!(http_request(port, "GET", "/api/backups/settings", Some(token), Some(agent), None).unwrap().status, 403);
    let settings = expect_json(http_request(port, "GET", "/api/backups/settings", Some(token), None, None).unwrap(), 200, "backup settings")["settings"].clone();
    assert_eq!(settings["keepDays"], 7);
    assert_eq!(settings["time"], "04:00");
    assert_eq!(settings["includeState"], true);
    assert!(settings["dir"].is_null());
    let page = http_request(port, "GET", "/tasks", Some(token), None, None).unwrap();
    assert_eq!(page.status, 200);
    assert!(page.body.contains("id=\"backup-settings\""), "task console must expose backup settings");
    let script = page.body.split_once("<script>").unwrap().1.split_once("</script>").unwrap().0;
    let script_file = workspace.root("backup-page.js");
    fs::write(&script_file, script).unwrap();
    let mut syntax = Command::new("node");
    syntax.arg("--check").arg(&script_file).env_remove("NODE_OPTIONS").env_remove("NODE_PATH");
    hide_process_window(&mut syntax);
    let syntax = syntax.output().expect("page syntax checker must start and be reaped");
    assert!(syntax.status.success(), "served page JavaScript must parse: {}", String::from_utf8_lossy(&syntax.stderr));
    let posted = json!({"dir":"next backup's output","keepDays":2,"time":"06:30","includeState":false});
    let saved = expect_json(http_request(port, "POST", "/api/backups/settings", Some(token), None, Some(&posted)).unwrap(), 200, "save backup settings");
    assert_eq!(saved["settings"]["keepDays"], 2);
    assert_eq!(saved["settings"]["time"], "06:30");
    assert_eq!(saved["settings"]["includeState"], false);
    let raw: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    assert!(raw["backup"].get("keep").is_none(), "saving retires the old count key");
    assert_eq!(raw["backup"]["keepDays"], 2);
    assert_eq!(raw["ticket23Sentinel"], json!({"preserve":"raw spelling"}));
    assert_eq!(raw["dsh"], original_dsh, "saving backup settings must preserve unrelated raw config");
    for invalid in [
        json!({"keepDays":0}), json!({"keepDays":1.5}), json!({"time":"24:00"}),
        json!({"includeState":"false"}), json!({"dir":123}), json!({"unknown":true}),
    ] {
        assert_eq!(http_request(port, "POST", "/api/backups/settings", Some(token), None, Some(&invalid)).unwrap().status, 400, "{invalid}");
        assert_eq!(serde_json::from_slice::<Value>(&fs::read(&config_file).unwrap()).unwrap(), raw);
    }
    for area in ["releases", "deps", "runtime"] {
        let forbidden = install.join(area).join("forbidden backups");
        let refused = expect_json(http_request(port, "POST", "/api/backups/settings", Some(token), None, Some(&json!({"dir":forbidden}))).unwrap(), 400, "forbidden backup destination");
        assert!(refused["error"].as_str().unwrap().contains("backup destination"));
        assert!(!forbidden.exists());
        assert_eq!(serde_json::from_slice::<Value>(&fs::read(&config_file).unwrap()).unwrap(), raw);
    }
    assert_eq!(http_request(port, "POST", "/api/backups/settings", Some(token), Some(agent), Some(&posted)).unwrap().status, 403);
    let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&json!({"requestId":"backup-settings-api-01"}))).unwrap(), 202, "backup now");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{}", started["runId"].as_str().unwrap()), Some(token), None, None).unwrap(), 200, "configured backup result");
        if !matches!(result["run"]["state"].as_str(), Some("queued" | "running")) {
            assert_eq!(result["run"]["state"], "succeeded", "{result}");
            break;
        }
        assert!(Instant::now() < deadline, "configured backup did not finish: {result}");
        thread::sleep(Duration::from_millis(20));
    }
    let destination = root.join("next backup's output");
    let archive = fs::read_dir(&destination).unwrap().map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "zip")).unwrap();
    let mut zip = zip::ZipArchive::new(fs::File::open(archive).unwrap()).unwrap();
    assert!(!zip.file_names().any(|name| name.starts_with("state/")));
    let archived: Value = serde_json::from_reader(zip.by_name("config.json").unwrap()).unwrap();
    assert_eq!(archived["backup"]["time"], "06:30");
    assert_eq!(archived["backup"]["keepDays"], 2);
    assert!(archived["backup"].get("keep").is_none());
    assert!(!root.join("backups").exists(), "next run must use the saved destination without restart");
    drop(zip);
    drop(console);
}

#[test]
fn backup_destination_aliases_are_resolved_at_save_and_run_but_load_stays_lexical() {
    let workspace = FixtureWorkspace::new("backup-alias");
    let root = workspace.root("runtime");
    let install = workspace.root("protected-install");
    let token = "fixture-backup-alias-console";
    let home = prepare_runtime_root(&root, token, "fixture-backup-alias-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let protected = install.join("runtime/aliased target");
    fs::create_dir_all(&protected).unwrap();
    let alias = workspace.root("backup alias");
    create_directory_reparse_point(&alias, &protected).expect("alias fixture");
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("ownerQQ");
    config["backup"] = json!({"dir":alias});
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    // Loading the configuration only checks the destination by path; it never resolves the alias.
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start_with_install_root(&root, port, &home, Some(&install));
    let settings = expect_json(http_request(port, "GET", "/api/backups/settings", Some(token), None, None).unwrap(), 200, "lexically valid settings")["settings"].clone();
    assert_eq!(settings["dir"], json!(alias));
    let raw = fs::read(&config_file).unwrap();
    let refused = expect_json(http_request(port, "POST", "/api/backups/settings", Some(token), None, Some(&json!({"dir":alias}))).unwrap(), 400, "aliased destination save");
    assert!(refused["error"].as_str().unwrap().contains("backup destination"), "{refused}");
    assert_eq!(fs::read(&config_file).unwrap(), raw, "a refused save leaves config unchanged");
    let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&json!({"requestId":"backup-alias-run-01"}))).unwrap(), 202, "aliased backup trigger");
    let deadline = Instant::now() + Duration::from_secs(20);
    let run = loop {
        let result = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{}", started["runId"].as_str().unwrap()), Some(token), None, None).unwrap(), 200, "aliased backup result");
        if !matches!(result["run"]["state"].as_str(), Some("queued" | "running")) { break result["run"].clone(); }
        assert!(Instant::now() < deadline, "aliased backup did not finish: {result}");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(run["state"], "failed", "{run}");
    assert!(run["reason"].as_str().unwrap().contains("backup destination"), "{run}");
    assert_eq!(fs::read_dir(&protected).unwrap().count(), 0, "nothing may be written inside install-root runtime");
    drop(console);
    fs::remove_file(&alias).or_else(|_| fs::remove_dir(&alias)).unwrap();
}

#[test]
fn backup_console_list_reports_archive_time_size_verification_and_failed_attempts() {
    let workspace = FixtureWorkspace::new("backup-list-api");
    let root = workspace.root("runtime");
    let install = workspace.root("protected-install");
    let token = "fixture-backup-list-console";
    let agent = "fixture-backup-list-agent";
    let home = prepare_runtime_root(&root, token, agent, "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("ownerQQ");
    fs::write(config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start_with_install_root(&root, port, &home, Some(&install));
    assert_eq!(http_request(port, "GET", "/api/backups", None, None, None).unwrap().status, 401);
    assert_eq!(http_request(port, "GET", "/api/backups", Some(token), Some(agent), None).unwrap().status, 403);
    let empty = expect_json(http_request(port, "GET", "/api/backups", Some(token), None, None).unwrap(), 200, "initial backup list");
    assert!(empty["backups"].as_array().unwrap().is_empty());
    assert!(!root.join("backups").exists(), "listing an unused destination must not create it");
    let run = |request: &str| {
        let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&json!({"requestId":request}))).unwrap(), 202, "list fixture backup");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let result = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{}", started["runId"].as_str().unwrap()), Some(token), None, None).unwrap(), 200, "list fixture result");
            if !matches!(result["run"]["state"].as_str(), Some("queued" | "running")) { break result["run"].clone(); }
            assert!(Instant::now() < deadline, "backup did not finish: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    };
    assert_eq!(run("backup-list-success-01")["state"], "succeeded");
    let destination = root.join("backups");
    let archive = fs::read_dir(&destination).unwrap().map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "zip")).unwrap();
    let name = archive.file_name().unwrap().to_str().unwrap().to_owned();
    let size = fs::metadata(&archive).unwrap().len();
    fs::write(destination.join("personal.zip"), b"unrelated").unwrap();
    fs::create_dir(destination.join("qqbot-1999-01-01T00-00-00.zip")).unwrap();
    fs::write(destination.join("qqbot-2000-01-01T00-00-00.zip"), b"not a zip").unwrap();
    fs::write(root.join("data/memory.db"), b"corrupt database").unwrap();
    let failed = run("backup-list-failure-02");
    assert_eq!(failed["state"], "failed");
    let listed = expect_json(http_request(port, "GET", "/api/backups", Some(token), None, None).unwrap(), 200, "backup list");
    let archives = listed["backups"].as_array().unwrap();
    assert_eq!(archives.len(), 2, "only HER-named regular archives belong in the list");
    let verified = archives.iter().find(|entry| entry["name"] == name).unwrap();
    assert_eq!(verified["size"], size);
    assert!(verified["time"].as_i64().unwrap() > 0);
    assert_eq!(verified["result"], "verified");
    assert_eq!(archives.iter().find(|entry| entry["name"] == "qqbot-2000-01-01T00-00-00.zip").unwrap()["result"], "failed");
    let attempts = listed["failedAttempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["runId"], failed["runId"]);
    assert_eq!(attempts[0]["state"], "failed");
    assert_eq!(fs::read(destination.join("personal.zip")).unwrap(), b"unrelated");
    let page = http_request(port, "GET", "/tasks", Some(token), None, None).unwrap();
    assert_eq!(page.status, 200);
    assert!(page.body.contains("id=\"backup-list\""), "task console must expose the backup list");
    drop(console);
}

#[test]
fn backup_failures_and_missed_daily_slot_alert_once_across_supervisor_restarts() {
    struct Alerts {
        url: String,
        messages: Arc<std::sync::Mutex<Vec<Value>>>,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }
    impl Drop for Alerts {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() { let _ = worker.join(); }
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut alerts = Alerts {
        url: format!("http://{}", listener.local_addr().unwrap()),
        messages: Arc::new(std::sync::Mutex::new(Vec::new())),
        stop: Arc::new(AtomicBool::new(false)), worker: None,
    };
    let (messages, stop) = (alerts.messages.clone(), alerts.stop.clone());
    alerts.worker = Some(thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(300);
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            let (stream, _) = match listener.accept() {
                Ok(socket) => socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(10)); continue; }
                Err(error) => panic!("fake alert accept: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream);
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap() == 0 {
                continue;
            }
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" { break; }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") { length = value.trim().parse::<usize>().unwrap(); }
                }
            }
            assert!(length < 64 * 1024);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            if first.split_whitespace().nth(1) == Some("/send_private_msg") {
                messages.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
            }
            let body = if first.starts_with("POST /get_status ") {
                r#"{"status":"ok","retcode":0,"data":{"online":true,"good":true}}"#
            } else {
                r#"{"status":"ok","retcode":0,"data":{"message_id":1,"messages":[]}}"#
            };
            write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    }));
    let workspace = FixtureWorkspace::new("backup-occurrence-alerts");
    let root = workspace.root("runtime");
    let install = workspace.root("protected-install");
    let token = "fixture-backup-alert-console";
    let home = prepare_runtime_root(&root, token, "fixture-backup-alert-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let dependencies = json!({"onebot":alerts.url,"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1"});
    fs::write(root.join("dependencies.json"), dependencies.to_string()).unwrap();
    // A missing HER task legitimately alerts at startup. Keep this backup-only fixture
    // healthy at the Scheduler boundary without filtering any administrator notices.
    let scheduler = workspace.root("scheduler.xml");
    let mut install_task = Command::new(env!("CARGO_BIN_EXE_her-cli"));
    install_task.arg("install")
        .env("HER_INSTALL_ROOT", &install).env("HER_TASK_SCHEDULER_FILE", &scheduler)
        .env_remove("HER_RUNTIME_ROOT");
    hide_process_window(&mut install_task);
    let installed = install_task.output().unwrap();
    assert!(installed.status.success(), "install matching fixture task: {installed:?}");
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    let now_ms = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    // The next daily slot is 20-80 s away, so the first supervisor can observe and stop before it.
    // Yesterday's slot lies after the day-old config mtime: the first observation must not alert it.
    let slot = (now_ms() + 20_000).div_euclid(60_000) * 60_000 + 60_000;
    let minute = slot.div_euclid(60_000).rem_euclid(1440);
    config["backup"] = json!({"time":format!("{:02}:{:02}", minute / 60, minute % 60)});
    for task in ["watchdog", "memory-digest", "memory-digest-check", "upstream-check", "console-restart"] {
        config["tasks"][task] = json!({"enabled":false});
    }
    config["tasks"]["backup"] = json!({"enabled":true});
    fs::write(&config_file, config.to_string()).unwrap();
    fs::File::options().write(true).open(&config_file).unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(86_400 + 120)).unwrap();
    fs::write(root.join("data/memory.db"), b"corrupt database").unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start_with_install_root(&root, port, &home, Some(&install));
    let wait = |expected: usize| {
        let deadline = Instant::now() + Duration::from_secs(8);
        while alerts.messages.lock().unwrap().len() < expected && Instant::now() < deadline { thread::sleep(Duration::from_millis(20)); }
        assert_eq!(alerts.messages.lock().unwrap().len(), expected, "each failed/missed occurrence must send one private alert");
    };
    for request in ["backup-alert-failure-01", "backup-alert-failure-02"] {
        let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&json!({"requestId":request}))).unwrap(), 202, "failed backup trigger");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let run = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{}", started["runId"].as_str().unwrap()), Some(token), None, None).unwrap(), 200, "failed backup result");
            if run["run"]["state"] == "failed" { break; }
            assert!(Instant::now() < deadline, "failed backup did not finish: {run}");
            thread::sleep(Duration::from_millis(20));
        }
    }
    wait(2);
    let notice_state = root.join("data/backup-notices.json");
    let start_supervisor = || {
        // Capture before spawning: a fast first notice check must not be missed.
        let previous_check = fs::metadata(&notice_state).unwrap().modified().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command.args(["supervise", "--runtime-root"]).arg(&root)
            .env("HER_INSTALL_ROOT", &install).env("TZ", "UTC").env_remove("HER_PARITY_EPOCH_MS")
            .env("HER_TASK_SCHEDULER_FILE", &scheduler)
            .env_remove("HER_RUNTIME_ROOT").env_remove("APPDATA").env_remove("LOCALAPPDATA").env_remove("HOME")
            .env("DSH_HOME", &home).env("USERPROFILE", home.join("synthetic-user-profile"))
            .stdout(Stdio::null()).stderr(Stdio::null());
        hide_process_window(&mut command);
        (ConsoleProcess { child: command.spawn().unwrap(), port: 0 }, previous_check)
    };
    let poll = |supervisor: &mut ConsoleProcess, previous_check: SystemTime| {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut observed_check = None;
        loop {
            assert!(Instant::now() < deadline, "supervisor did not finish a backup notice check");
            assert!(supervisor.child.try_wait().unwrap().is_none());
            // Every notice check atomically rewrites this file, even with no new claims.
            // Allow two more loop periods for sends after the durable claims are saved.
            if fs::metadata(&notice_state).unwrap().modified().unwrap() != previous_check {
                let observed = observed_check.get_or_insert_with(Instant::now);
                if observed.elapsed() >= Duration::from_secs(2) { break; }
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    let (mut supervisor, previous_check) = start_supervisor();
    poll(&mut supervisor, previous_check);
    {
        let messages = alerts.messages.lock().unwrap();
        assert_eq!(messages.len(), 2, "the first observation must not alert old history or slots since the config mtime: {messages:?}");
    }
    drop(supervisor);
    assert!(now_ms() < slot - 1_000, "the first supervisor must stop before the next slot");
    // Stay down across the slot and its minute of grace: the next supervisor finds it missed.
    while now_ms() < slot + 62_000 { thread::sleep(Duration::from_millis(200)); }
    let (mut supervisor, _) = start_supervisor();
    wait(3);
    assert!(supervisor.child.try_wait().unwrap().is_none());
    assert!(alerts.messages.lock().unwrap()[2].to_string().contains("Backup missed"), "{:?}", alerts.messages.lock().unwrap()[2]);
    drop(supervisor);
    let (mut supervisor, previous_check) = start_supervisor();
    poll(&mut supervisor, previous_check);
    assert_eq!(alerts.messages.lock().unwrap().len(), 3, "restart must not repeat failed or missed alerts");
    for message in alerts.messages.lock().unwrap().iter() {
        assert_eq!(message["user_id"], OWNER_ID);
    }
    drop(supervisor);
    drop(console);
}

#[test]
fn backup_job_through_real_console_preserves_selected_wal_contents_settings_and_old_archives() {
    let workspace = FixtureWorkspace::new("backup's-process-seam");
    let root = workspace.root("runtime");
    let token = "fixture-backup-console";
    let home = prepare_runtime_root(&root, token, "fixture-agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let db = rusqlite::Connection::open(root.join("data/memory.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO memory(owner_type,owner_id,scope,kind,content,source,created_at) VALUES('session','private:7001','private','fact','committed WAL snapshot','manual',1);").unwrap();
    for directory in ["resources/patches/nested", "resources/docs"] { fs::create_dir_all(root.join(directory)).unwrap(); }
    fs::write(root.join("resources/roles/main.md"), "selected role").unwrap();
    fs::write(root.join("resources/patches/nested/fix.txt"), "patch").unwrap();
    fs::write(root.join("resources/docs/guide.md"), "guide").unwrap();
    let readme = [0xff, 0xfe, b'R', 0, b'\r', 0, b'\n', 0];
    fs::write(root.join("resources/说明.md"), readme).unwrap();
    fs::write(root.join("data/ignored.jsonl"), "not state").unwrap();
    let destination = root.join("backups");
    fs::create_dir(&destination).unwrap();
    let legacy = ["qqbot-2026-01-01T04-00-00.zip", "qqbot-2026-01-02T04-00-00.zip"];
    for (i, name) in legacy.iter().enumerate() {
        let file = fs::File::create(destination.join(name)).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(100 + i as u64)).unwrap();
    }
    fs::write(destination.join("qqbot-personal.zip"), "unrelated").unwrap();
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config["backup"] = json!({"keep":2});
    config.as_object_mut().unwrap().remove("ownerQQ");
    config["tasks"] = json!({"nodePath":root.join("missing-node"),"installRoot":root});
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &home);
    let selected_now = expect_json(http_request(port, "GET", "/api/tasks", Some(token), None, None).unwrap(), 200, "selected backup clock")["now"].as_i64().unwrap();
    for (index, age) in [8, 1].into_iter().enumerate() {
        fs::OpenOptions::new().write(true).open(destination.join(legacy[index])).unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(selected_now.max(0) as u64) - Duration::from_secs(age * 86_400)).unwrap();
    }
    let run = |request: &str| {
        let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&json!({"requestId":request}))).unwrap(), 202, "backup trigger");
        let id = started["runId"].as_str().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let result = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{id}"), Some(token), None, None).unwrap(), 200, "backup result");
            if !matches!(result["run"]["state"].as_str(), Some("queued" | "running")) { break result["run"].clone(); }
            assert!(Instant::now() < deadline, "backup did not finish: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    };
    let done = run("backup-process-default-01");
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(done["exitCode"], 0);
    assert!(done["pid"].is_null(), "the backup must not start an external job");
    assert!(!destination.join(legacy[0]).exists());
    assert!(destination.join(legacy[1]).exists());
    assert_eq!(fs::read_to_string(destination.join("qqbot-personal.zip")).unwrap(), "unrelated");
    let path = fs::read_dir(&destination).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap() != legacy[1] && p.file_name().unwrap() != "qqbot-personal.zip" && p.file_name().unwrap() != ".her-backup.lock").unwrap();
    let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    let manifest: Value = serde_json::from_reader(archive.by_name("manifest.json").unwrap()).unwrap();
    assert_eq!(manifest["version"], 1);
    for name in ["memory.db", "config.json", "state/social-v2.json", "roles/main.md", "patches/nested/fix.txt", "docs/guide.md", "说明.md"] { assert!(manifest["files"].get(name).is_some(), "missing {name}: {manifest}"); }
    assert!(archive.by_name("state/ignored.jsonl").is_err());
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_string();
        if name == "manifest.json" { continue; }
        let mut bytes = Vec::new(); file.read_to_end(&mut bytes).unwrap();
        assert_eq!(manifest["files"][&name]["size"], bytes.len());
        assert_eq!(manifest["files"][&name]["sha256"], format!("{:x}", Sha256::digest(&bytes)), "{name}");
        match name.as_str() {
            "说明.md" => assert_eq!(bytes, readme),
            "roles/main.md" => assert_eq!(bytes, b"selected role"),
            "memory.db" => {
                let copy_path = workspace.root("copied-memory.db");
                fs::write(&copy_path, bytes).unwrap();
                let copy = rusqlite::Connection::open(copy_path).unwrap();
                assert_eq!(copy.query_row("SELECT content FROM memory", [], |row| row.get::<_, String>(0)).unwrap(), "committed WAL snapshot");
                assert_eq!(copy.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)).unwrap(), "ok");
            }
            _ => {}
        }
    }
    drop(archive);
    let relative = root.join("backup's relative destination");
    config["backup"] = json!({"dir":"backup's relative destination", "keepDays":1, "includeState":false});
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(run("backup-process-relative-02")["state"], "succeeded");
    let relative_zip = fs::read_dir(&relative).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap() != ".her-backup.lock").unwrap();
    let archive = zip::ZipArchive::new(fs::File::open(&relative_zip).unwrap()).unwrap();
    assert!(!archive.file_names().any(|name| name.starts_with("state/")));
    drop(archive);
    let absolute = workspace.root("backup's absolute destination");
    config["backup"] = json!({"dir":absolute, "keepDays":1});
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(run("backup-process-absolute-03")["state"], "succeeded");
    let previous = fs::read_dir(&absolute).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap() != ".her-backup.lock").unwrap();
    let previous_bytes = fs::read(&previous).unwrap();
    drop(db);
    fs::write(root.join("data/memory.db"), "corrupt database").unwrap();
    let failed = run("backup-process-failure-04");
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["exitCode"], 1);
    assert_eq!(fs::read(previous).unwrap(), previous_bytes);
    assert_eq!(fs::read_dir(&absolute).unwrap().filter(|e| e.as_ref().unwrap().file_name() != ".her-backup.lock").count(), 1, "failed backup left an archive or staging directory");
    drop(console); // Kill and reap the only process this fixture starts, including on assertion unwinding.
}

#[test]
fn task_page_and_history_are_available_only_to_the_current_administrator() {
    let workspace = FixtureWorkspace::new("tasks-front");
    let root = workspace.root("runtime");
    let token = "fixture-task-console-token-abcdefghij";
    let agent = "fixture-task-agent-token-abcdefghij";
    let legacy = prepare_runtime_root(&root, token, agent, "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &legacy);

    let no_token = http_request(port, "GET", "/api/tasks", None, None, None).unwrap();
    assert_eq!(no_token.status, 401);
    let tasks = expect_json(http_request(port, "GET", "/api/tasks", Some(token), None, None).unwrap(), 200, "task list");
    assert!(tasks["tasks"].as_array().unwrap().iter().any(|task| task["id"] == "watchdog"));
    assert_eq!(tasks["tasks"].as_array().unwrap().iter().find(|task| task["id"] == "upstream-check").unwrap()["enabled"], false);
    assert!(!tasks["tasks"].as_array().unwrap().iter().any(|task| task["id"] == "ab-report"));
    assert_eq!(tasks["legacy"].as_array().unwrap().iter().find(|task| task["name"] == "QQBot-ABReport").unwrap()["disposition"], "retired");
    assert_eq!(http_request(port, "GET", "/tasks", Some(token), None, None).unwrap().status, 200);
    assert_eq!(http_request(port, "GET", "/api/tasks", Some(token), Some(agent), None).unwrap().status, 403);
    let run = json!({"requestId":"fixture-request-01"});
    assert_eq!(http_request(port, "POST", "/api/tasks/not-a-task/run", Some(token), None, Some(&run)).unwrap().status, 404);
    assert_eq!(http_request(port, "POST", "/api/tasks/ab-report/run", Some(token), None, Some(&run)).unwrap().status, 404);
    let audit = expect_json(http_request(port, "GET", "/api/audit/recent", Some(token), None, None).unwrap(), 200, "audit");
    assert!(audit["entries"].as_array().unwrap().iter().any(|entry| entry.to_string().contains("/api/tasks/not-a-task/run")));

    let config_path = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["consoleToken"] = json!("replacement-console-token-abcdefghij");
    config["tasks"] = json!({"upstream-check":{"enabled":true}});
    fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&run)).unwrap().status, 401);
    let tasks = expect_json(http_request(port, "GET", "/api/tasks", Some("replacement-console-token-abcdefghij"), None, None).unwrap(), 200, "reconfigured tasks");
    assert_eq!(tasks["tasks"].as_array().unwrap().iter().find(|task| task["id"] == "upstream-check").unwrap()["enabled"], true);
    drop(console);
}

#[test]
fn one_runtime_root_cannot_have_two_console_processes_even_on_different_ports() {
    let workspace = FixtureWorkspace::new("console-owner");
    let root = workspace.root("runtime");
    let token = "fixture-console-owner-token-abcdefghij";
    let legacy = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (first_port, second_port) = reserve_distinct_ports();
    let first = ConsoleProcess::start(&root, first_port, &legacy);
    let child = Command::new(env!("CARGO_BIN_EXE_her"))
        .args(["--console-serve", "--console-port", &second_port.to_string(), "--runtime-root"])
        .arg(&root).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let mut second = ConsoleProcess { child, port: second_port };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = second.child.try_wait().unwrap() {
            assert!(!status.success(), "second console must fail to acquire ownership");
            break;
        }
        assert!(Instant::now() < deadline, "second console remained alive on another port");
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(http_request(first_port, "GET", "/api/health", None, None, None).unwrap().status, 200);
    drop((first, second));
}

#[test]
fn task_lifecycle_actions_require_confirmation_and_are_audited() {
    let workspace = FixtureWorkspace::new("task-lifecycle");
    let root = workspace.root("runtime");
    let token = "fixture-task-lifecycle-token-abcdefghij";
    let legacy = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &legacy);
    let action = |body: Value| http_request(port, "POST", "/api/tasks/lifecycle", Some(token), None, Some(&body)).unwrap();
    assert_eq!(action(json!({"state":"maintenance"})).status, 400);
    let refused = expect_json(action(json!({"state":"maintenance", "confirm":true, "reason":"fixture"})), 503, "maintenance without supervisor");
    assert_eq!(refused["applied"], false);
    let tasks = expect_json(http_request(port, "GET", "/api/tasks", Some(token), None, None).unwrap(), 200, "unapplied task state");
    assert_eq!(tasks["desired"]["state"], "running");
    assert!(!root.join("data/lifecycle/desired-state.json").exists());
    // The confirmed local CLI remains available without a supervisor. Keep the
    // maintenance admission check as well as the HTTP refusal and audit checks.
    let maintenance = Command::new(env!("CARGO_BIN_EXE_her"))
        .args(["lifecycle", "set", "maintenance", "--confirm", "--runtime-root"]).arg(&root).output().unwrap();
    assert!(maintenance.status.success(), "{}", String::from_utf8_lossy(&maintenance.stderr));
    let tasks = expect_json(http_request(port, "GET", "/api/tasks", Some(token), None, None).unwrap(), 200, "task state");
    assert_eq!(tasks["desired"]["state"], "maintenance");
    let run = json!({"requestId":"fixture-request-02"});
    assert_eq!(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&run)).unwrap().status, 409);
    assert_eq!(action(json!({"state":"running", "confirm":true})).status, 200);
    let audit = expect_json(http_request(port, "GET", "/api/audit/recent", Some(token), None, None).unwrap(), 200, "lifecycle audit");
    assert!(audit["entries"].as_array().unwrap().iter().filter(|entry| entry["path"] == "/api/tasks/lifecycle").count() >= 3);
    assert!(audit["entries"].as_array().unwrap().iter().any(|entry| entry["path"] == "/api/tasks/lifecycle" && entry["status"] == 503));
    drop(console);
}

#[cfg(unix)]
#[test]
fn task_front_guards_and_real_child_logs_protect_credentials() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = FixtureWorkspace::new("task-guards");
    let root = workspace.root("runtime");
    let token = "fixture-console-secret-sentinel";
    let agent = "fixture-agent-secret-sentinel";
    let legacy = prepare_runtime_root(&root, token, agent, "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let fake_node = root.join("fake-node");
    fs::write(&fake_node, format!("#!/bin/sh\nprintf '%s\\n' '{token} {agent}' >&2\nexit 7\n")).unwrap();
    fs::set_permissions(&fake_node, fs::Permissions::from_mode(0o755)).unwrap();
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config["tasks"] = json!({"nodePath":fake_node, "installRoot":root});
    fs::write(config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &legacy);
    let status = |raw: String| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        text.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse::<u16>().unwrap()
    };
    assert_eq!(status(format!("GET /api/tasks HTTP/1.1\r\nHost: evil.invalid:{port}\r\nConnection: close\r\n\r\n")), 403);
    assert_eq!(status(format!("POST /api/tasks/upstream-check/run HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Console-Token: {token}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")), 415);
    assert_eq!(status(format!("POST /api/tasks/upstream-check/run HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Console-Token: {token}\r\nContent-Type: application/json\r\nOrigin: https://evil.invalid\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")), 403);
    let body = json!({"requestId":"fixture-guard-run-01"});
    let started = expect_json(http_request(port, "POST", "/api/tasks/upstream-check/run", Some(token), None, Some(&body)).unwrap(), 202, "task trigger");
    let id = started["runId"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let run = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{id}"), Some(token), None, None).unwrap(), 200, "task result");
        if run["run"]["state"] == "failed" { assert_eq!(run["run"]["exitCode"], 7); break; }
        assert!(Instant::now() < deadline, "child did not finish: {run}");
        thread::sleep(Duration::from_millis(30));
    }
    let persisted = fs::read_to_string(root.join("data/tasks/logs").join(format!("{id}.jsonl"))).unwrap();
    assert!(!persisted.contains(token) && !persisted.contains(agent), "credentials leaked into persisted task log: {persisted}");
    let recovery = expect_json(http_request(port, "GET", "/api/recovery-record?component=process", Some(token), None, None).unwrap(), 200, "task recovery record");
    assert!(recovery["records"].as_array().unwrap().iter().any(|r| r["runId"] == id && r["outcome"] == "failed"), "missing task failure record: {recovery}");
    let duplicate = expect_json(http_request(port, "POST", "/api/tasks/upstream-check/run", Some(token), None, Some(&body)).unwrap(), 200, "task deduplication");
    assert_eq!(duplicate["runId"], id);
    drop(console);
}

#[cfg(unix)]
#[test]
fn revoked_console_credential_cancels_a_queued_task_before_it_starts() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = FixtureWorkspace::new("task-revocation");
    let root = workspace.root("runtime");
    let token = "fixture-revoked-console-token";
    let legacy = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let marker = root.join("started");
    let fake_node = root.join("fake-node");
    fs::write(&fake_node, format!("#!/bin/sh\nprintf started > '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&fake_node, fs::Permissions::from_mode(0o755)).unwrap();
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config["tasks"] = json!({"nodePath":fake_node, "installRoot":root});
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &legacy);
    fs::create_dir_all(root.join("data/locks")).unwrap();
    let gate = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join("data/locks/social-v2.lock")).unwrap();
    gate.try_lock().unwrap();
    let body = json!({"requestId":"fixture-revoked-request"});
    let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&body)).unwrap(), 202, "queued task");
    let id = started["runId"].as_str().unwrap();
    config["consoleToken"] = json!("fixture-replacement-console-token");
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    drop(gate);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = http_request(port, "GET", &format!("/api/tasks/runs/{id}"), Some("fixture-replacement-console-token"), None, None).unwrap();
        let run = expect_json(response, 200, "revoked task result");
        if run["run"]["state"] == "cancelled" { break; }
        assert!(Instant::now() < deadline, "revoked task did not cancel: {run}");
        thread::sleep(Duration::from_millis(30));
    }
    assert!(!marker.exists(), "revoked queued task started its child");
    drop(console);
}

fn install_console_resource_fixture(root: &Path, marker: &str, agent_token: &str) {
    let public_dir = root.join("resources/public");
    let voice_dir = root.join("resources/audio/nested");
    fs::create_dir_all(&public_dir).expect("create selected public-resource directory");
    fs::create_dir_all(&voice_dir).expect("create selected voice-resource directory");
    fs::write(
        public_dir.join("console.html"),
        format!("<!doctype html><title>{marker}-page</title><main>{marker}</main>"),
    )
    .expect("write selected console page fixture");
    fs::write(
        root.join(format!("resources/audio/{marker}.MP3")),
        b"selected mp3 fixture",
    )
    .expect("write selected top-level voice fixture");
    fs::write(
        voice_dir.join(format!("{marker}.wav")),
        b"selected nested voice fixture",
    )
    .expect("write selected nested voice fixture");
    fs::write(
        root.join(format!("resources/audio/{marker}.txt")),
        b"not an audio file",
    )
    .expect("write non-audio decoy");

    let config_path = root.join("config/config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&config_path).expect("read selected config fixture"))
            .expect("parse selected config fixture");
    config["socialV2"] = json!({
        "voice": { "enabled": true, "dir": "audio" },
        "tools": { "sendVoice": true }
    });
    fs::write(
        config_path,
        serde_json::to_vec_pretty(&config).expect("serialize selected config fixture"),
    )
    .expect("write selected voice config fixture");

    fs::write(
        root.join("data/social-v2.json"),
        serde_json::to_vec(&json!({
            "conversations": {
                "private:7001": { "agentToken": agent_token }
            }
        }))
        .expect("serialize selected session-token fixture"),
    )
    .expect("write selected session-token fixture");

    let first = json!({ "name": format!("{marker}-first") }).to_string();
    let last = json!({ "name": format!("{marker}-last") }).to_string();
    fs::write(
        root.join("data/tool-calls.jsonl"),
        format!("{first}\nnot-json\n{last}\n"),
    )
    .expect("write selected tool-log fixture");
}

fn set_selected_voice_directory(root: &Path, relative: &str) {
    let config_path = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(
        &fs::read(&config_path).expect("read selected config before voice-path change"),
    )
    .expect("parse selected config before voice-path change");
    config["socialV2"]["voice"]["dir"] = json!(relative);
    fs::write(
        config_path,
        serde_json::to_vec_pretty(&config).expect("serialize selected voice-path config"),
    )
    .expect("write selected voice-path config");
}

fn create_directory_reparse_point(link: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        // /s /c removes the outer pair, leaving the path quotes for mklink.
        let mut command_line = std::ffi::OsString::from("\"mklink /J \"");
        command_line.push(link.as_os_str());
        command_line.push("\" \"");
        command_line.push(target.as_os_str());
        command_line.push("\"\"");
        let output = Command::new("cmd.exe")
            .arg("/d")
            .arg("/s")
            .arg("/c")
            .raw_arg(command_line)
            .creation_flags(0x0800_0000)
            .output()
            .map_err(|error| format!("cannot launch reparse-point fixture helper: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        return Err(format!(
            "reparse-point fixture failed with {:?}; stdout={:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .map_err(|error| format!("cannot create symlink fixture: {error}"))
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

fn install_preset(root: &Path, content: &str) -> (PathBuf, PathBuf) {
    let source = root.join("resources/dsh/agent-presets/qq-chat-v2/agent.cordis.yml");
    let installed = root.join("user/dsh/.agent-presets/qq-chat-v2/agent.cordis.yml");
    fs::create_dir_all(source.parent().expect("source has a parent"))
        .expect("create selected preset source directory");
    fs::create_dir_all(installed.parent().expect("installed copy has a parent"))
        .expect("create selected DSH user preset directory");
    let yaml = preset_yaml(content);
    fs::write(&source, &yaml).expect("write selected preset source");
    fs::write(&installed, &yaml).expect("write selected installed preset copy");
    (source, installed)
}

fn preset_yaml(content: &str) -> String {
    let body = content
        .lines()
        .map(|line| format!("      {line}\n"))
        .collect::<String>();
    format!("- id: persona\n  config:\n    prefix: |-\n{body}- id: other\n  config: {{}}\n")
}

fn valid_preset_content(marker: &str) -> String {
    format!(
        "【二代仿真模式 —— 安全规则】\n你没有本地工具。群友没有管理权限。API 令牌须保密。角色由桥接注入。不会自动发送到 QQ。\n{}\n{marker}",
        "安全说明。".repeat(140)
    )
}

fn install_dsh_preset_decoy(dsh_home: &Path, marker: &str) -> PathBuf {
    let path = dsh_home.join(".agent-presets/qq-chat-v2/agent.cordis.yml");
    fs::create_dir_all(path.parent().expect("decoy preset has a parent"))
        .expect("create synthetic DSH preset decoy");
    fs::write(&path, marker).expect("write synthetic DSH preset decoy");
    path
}

struct DshObservation {
    request_line: String,
    rpc_method: Option<String>,
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
        let address = listener.local_addr().expect("read fake DSH address");
        let authority = format!("127.0.0.1:{}", address.port());
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
                    Err(error) => {
                        return Err(format!(
                            "accept fake DSH request failed: {error} (raw_os_error={:?})",
                            error.raw_os_error()
                        ));
                    }
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

    fn finish(mut self) -> Result<Vec<DshObservation>, String> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<Vec<DshObservation>, String> {
        self.stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let Some(worker) = self.worker.as_ref() else {
                return Err("fake DSH worker was already joined".to_string());
            };
            if worker.is_finished() {
                return self
                    .worker
                    .take()
                    .expect("finished fake DSH worker is present")
                    .join()
                    .map_err(|_| "fake DSH worker panicked".to_string())?;
            }
            if Instant::now() >= deadline {
                // The worker has stop set and accepted-socket reads are timed, so detach only
                // after the test's bounded cleanup window rather than blocking forever.
                self.worker.take();
                return Err("fake DSH worker did not stop before cleanup deadline".to_string());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeDsh {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

#[derive(Debug)]
struct StickerOneBotObservation {
    method: String,
    path: String,
    authorization: String,
    body: Value,
}

struct FakeStickerOneBot {
    base_url: String,
    listener: Option<TcpListener>,
    rows: Option<Vec<Value>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<Vec<StickerOneBotObservation>, String>>>,
}

impl FakeStickerOneBot {
    fn bind(rows: Vec<Value>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake sticker OneBot");
        listener
            .set_nonblocking(true)
            .expect("set fake sticker OneBot listener nonblocking");
        let address = listener
            .local_addr()
            .expect("read fake sticker OneBot address");
        let base_url = format!("http://127.0.0.1:{}", address.port());
        Self {
            base_url,
            listener: Some(listener),
            rows: Some(rows),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    fn start(&mut self) {
        let listener = self.listener.take().expect("fake sticker OneBot is bound");
        let rows = self
            .rows
            .take()
            .expect("fake sticker OneBot rows are available");
        let worker_stop = Arc::clone(&self.stop);
        self.worker = Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut observations = Vec::new();
            while !worker_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        observations.push(serve_fake_sticker_onebot_request(stream, &rows)?)
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(10035) =>
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        return Err(format!(
                            "accept fake sticker OneBot request failed: {error}"
                        ));
                    }
                }
            }
            Ok(observations)
        }));
    }

    fn drain_pending_connections(&self) -> Result<usize, String> {
        let listener = self
            .listener
            .as_ref()
            .ok_or_else(|| "fake sticker OneBot listener is already serving".to_string())?;
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut requests = 0;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    requests += 1;
                    drop(stream);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(10035) =>
                {
                    if Instant::now() >= deadline {
                        return Ok(requests);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(format!("drain fake sticker OneBot listener: {error}")),
            }
        }
    }

    fn finish(mut self) -> Result<Vec<StickerOneBotObservation>, String> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<Vec<StickerOneBotObservation>, String> {
        self.stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let Some(worker) = self.worker.as_ref() else {
                return Err("fake sticker OneBot worker was already joined".to_string());
            };
            if worker.is_finished() {
                return self
                    .worker
                    .take()
                    .expect("finished fake sticker OneBot worker is present")
                    .join()
                    .map_err(|_| "fake sticker OneBot worker panicked".to_string())?;
            }
            if Instant::now() >= deadline {
                self.worker.take();
                return Err(
                    "fake sticker OneBot worker did not stop before cleanup deadline".to_string(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeStickerOneBot {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn serve_fake_sticker_onebot_request(
    stream: TcpStream,
    rows: &[Value],
) -> Result<StickerOneBotObservation, String> {
    stream
        .set_nonblocking(false)
        .map_err(|_| "set fake sticker OneBot stream blocking".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "set fake sticker OneBot read timeout".to_string())?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|_| "read fake sticker OneBot request line".to_string())?;
    if request_line.trim().is_empty() {
        return Err("fake sticker OneBot received an empty request line".to_string());
    }
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or("").to_string();
    let path = request_parts.next().unwrap_or("").to_string();
    let mut authorization = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|_| "read fake sticker OneBot headers".to_string())?;
        if read == 0 {
            return Err("fake sticker OneBot request ended before headers".to_string());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_string();
            } else if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|_| "read fake sticker OneBot body".to_string())?;
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let response = if path == "/fetch_custom_face_detail" {
        json!({ "status": "ok", "retcode": 0, "data": rows })
    } else if path == "/modify_custom_face" {
        json!({ "status": "ok", "retcode": 0, "data": {} })
    } else {
        json!({ "status": "failed", "retcode": 404, "wording": "unexpected fake OneBot action" })
    }
    .to_string();
    let wire = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.as_bytes().len(),
        response
    );
    reader
        .get_mut()
        .write_all(wire.as_bytes())
        .map_err(|_| "write fake sticker OneBot response".to_string())?;
    Ok(StickerOneBotObservation {
        method,
        path,
        authorization,
        body,
    })
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
    let mut headers = std::collections::HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|_| "read fake DSH headers".to_string())?;
        if read == 0 {
            return Err("fake DSH request ended before headers".to_string());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|_| "read fake DSH request body".to_string())?;
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let rpc_method = request
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cookie_valid = headers
        .get("cookie")
        .map(|cookie| verify_dsh_cookie(authority, cookie, secret))
        .unwrap_or(false);
    let request_line = request_line.trim().to_string();
    let valid_call = cookie_valid
        && request_line.starts_with("POST /api/agentPresets/list ")
        && request.get("type").and_then(Value::as_str) == Some("client-request")
        && rpc_method.as_deref() == Some("agentPresets/list");
    let status = if valid_call {
        "200 OK"
    } else {
        "401 Unauthorized"
    };
    let response_body = if valid_call {
        json!({
            "type": "server-response",
            "rpcId": request.get("rpcId").cloned().unwrap_or(Value::Null),
            "result": {
                "ok": true,
                "value": { "presets": [{ "id": preset_id, "trust": "fixture" }] }
            }
        })
        .to_string()
    } else {
        String::new()
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.as_bytes().len()
    );
    reader
        .get_mut()
        .write_all(response.as_bytes())
        .map_err(|_| "write fake DSH response".to_string())?;
    Ok(DshObservation {
        request_line,
        rpc_method,
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
    let mut pieces = value.split('.');
    if pieces.next() != Some("v1") {
        return false;
    }
    let Some(payload) = pieces.next() else {
        return false;
    };
    let Some(signature) = pieces.next() else {
        return false;
    };
    if pieces.next().is_some() {
        return false;
    }
    let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(payload.as_bytes());
    if mac.verify_slice(&signature).is_err() {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(payload)
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

#[test]
fn memory_write_search_forget_and_index_stay_in_each_live_console_root() {
    let workspace = FixtureWorkspace::new("memory");
    let root_a = workspace.root("console A");
    let root_b = workspace.root("console B");
    let token_a = "public-memory-console-token-a";
    let token_b = "public-memory-console-token-b";
    let agent_a = "public-memory-agent-token-a";
    let agent_b = "public-memory-agent-token-b";
    let legacy_url = "http://127.0.0.1:1";
    let dsh_home_a = prepare_runtime_root(
        &root_a, token_a, agent_a, legacy_url, legacy_url, None, None,
    );
    let dsh_home_b = prepare_runtime_root(
        &root_b, token_b, agent_b, legacy_url, legacy_url, None, None,
    );
    initialize_memory_fixture(&root_a);
    initialize_memory_fixture(&root_b);

    let (port_a, port_b) = reserve_distinct_ports();
    let console_a = ConsoleProcess::start(&root_a, port_a, &dsh_home_a);
    let console_b = ConsoleProcess::start(&root_b, port_b, &dsh_home_b);
    assert_ne!(console_a.port, console_b.port);

    let content_a = "runtimealphaunique";
    let content_b = "runtimebetaunique";
    let write_a = http_request(
        console_a.port,
        "POST",
        "/api/memory/write",
        Some(token_a),
        Some(agent_a),
        Some(&json!({
            "key": SESSION_KEY,
            "content": content_a,
            "scope": "private",
            "importance": 0.8
        })),
    )
    .expect("write memory through console A");
    let write_a = expect_json(write_a, 200, "console A memory write");
    assert_eq!(write_a["ok"], true);
    let id_a = write_a["id"].as_i64().expect("A write returns a memory id");

    let write_b = http_request(
        console_b.port,
        "POST",
        "/api/memory/write",
        Some(token_b),
        Some(agent_b),
        Some(&json!({
            "key": SESSION_KEY,
            "content": content_b,
            "scope": "private",
            "importance": 0.8
        })),
    )
    .expect("write memory through console B");
    let write_b = expect_json(write_b, 200, "console B memory write");
    assert_eq!(write_b["ok"], true);
    let id_b = write_b["id"].as_i64().expect("B write returns a memory id");

    let search_a = http_request(
        console_a.port,
        "GET",
        "/api/memory/search?key=private:7001&query=runtimealphaunique",
        Some(token_a),
        Some(agent_a),
        None,
    )
    .expect("search A selected memory database");
    let search_a = expect_json(search_a, 200, "console A memory search");
    assert_eq!(search_a["count"], 1);
    assert!(search_a["memories"].to_string().contains(content_a));
    assert!(!search_a["memories"].to_string().contains(content_b));

    let search_b = http_request(
        console_b.port,
        "GET",
        "/api/memory/search?key=private:7001&query=runtimebetaunique",
        Some(token_b),
        Some(agent_b),
        None,
    )
    .expect("search B selected memory database");
    let search_b = expect_json(search_b, 200, "console B memory search");
    assert_eq!(search_b["count"], 1);
    assert!(search_b["memories"].to_string().contains(content_b));
    assert!(!search_b["memories"].to_string().contains(content_a));

    let cross_a = http_request(
        console_a.port,
        "GET",
        "/api/memory/search?key=private:7001&query=runtimebetaunique",
        Some(token_a),
        Some(agent_a),
        None,
    )
    .expect("search A for B-only fixture");
    let cross_a = expect_json(cross_a, 200, "console A cross-root search");
    // Empty FTS matches fall back to visible memories in the selected database. The
    // cross-root query must therefore return A's own fallback row without exposing B.
    assert_eq!(cross_a["count"], 1);
    assert!(cross_a["memories"].to_string().contains(content_a));
    assert!(!cross_a["memories"].to_string().contains(content_b));
    let cross_b = http_request(
        console_b.port,
        "GET",
        "/api/memory/search?key=private:7001&query=runtimealphaunique",
        Some(token_b),
        Some(agent_b),
        None,
    )
    .expect("search B for A-only fixture");
    let cross_b = expect_json(cross_b, 200, "console B cross-root search");
    assert_eq!(cross_b["count"], 1);
    assert!(cross_b["memories"].to_string().contains(content_b));
    assert!(!cross_b["memories"].to_string().contains(content_a));

    for (process, token, agent) in [
        (&console_a, token_a, agent_a),
        (&console_b, token_b, agent_b),
    ] {
        let index = http_request(
            process.port,
            "GET",
            "/api/memory/index?key=private:7001",
            Some(token),
            Some(agent),
            None,
        )
        .expect("index selected memory database");
        let index = expect_json(index, 200, "console memory index");
        assert!(index["total"].as_i64().unwrap_or(0) > 0);
    }

    let forget_a = http_request(
        console_a.port,
        "POST",
        "/api/memory/forget",
        Some(token_a),
        Some(agent_a),
        Some(&json!({ "key": SESSION_KEY, "id": id_a })),
    )
    .expect("forget A selected memory");
    let forget_a = expect_json(forget_a, 200, "console A memory forget");
    assert_eq!(forget_a["ok"], true);

    let after_a = http_request(
        console_a.port,
        "GET",
        "/api/memory/search?key=private:7001&query=runtimealphaunique",
        Some(token_a),
        Some(agent_a),
        None,
    )
    .expect("verify A forget through HTTP");
    let after_a = expect_json(after_a, 200, "console A post-forget search");
    assert_eq!(after_a["count"], 0);
    let after_b = http_request(
        console_b.port,
        "GET",
        "/api/memory/search?key=private:7001&query=runtimebetaunique",
        Some(token_b),
        Some(agent_b),
        None,
    )
    .expect("verify B survived A forget through HTTP");
    let after_b = expect_json(after_b, 200, "console B post-forget search");
    assert_eq!(after_b["count"], 1);
    assert!(after_b["memories"].to_string().contains(content_b));

    let forget_b = http_request(
        console_b.port,
        "POST",
        "/api/memory/forget",
        Some(token_b),
        Some(agent_b),
        Some(&json!({ "key": SESSION_KEY, "id": id_b })),
    )
    .expect("forget B selected memory");
    let forget_b = expect_json(forget_b, 200, "console B memory forget");
    assert_eq!(forget_b["ok"], true);
    let after_b_forget = http_request(
        console_b.port,
        "GET",
        "/api/memory/search?key=private:7001&query=runtimebetaunique",
        Some(token_b),
        Some(agent_b),
        None,
    )
    .expect("verify B forget through HTTP");
    assert_eq!(
        expect_json(after_b_forget, 200, "console B post-forget search")["count"],
        0
    );

    let config_path_a = root_a.join("config/config.json");
    let config_path_b = root_b.join("config/config.json");
    let config_b_before =
        fs::read(&config_path_b).expect("read selected B config before A failures");
    fs::remove_file(&config_path_a).expect("remove only the synthetic selected A config");
    let missing_config = http_request(
        console_a.port,
        "GET",
        "/api/status",
        Some(token_a),
        None,
        None,
    )
    .expect("request with selected A config missing");
    let missing_config = expect_json(missing_config, 503, "missing selected A config");
    assert_eq!(missing_config["error"], "runtime config is unavailable");
    assert!(!missing_config
        .to_string()
        .contains(&root_a.to_string_lossy().to_string()));
    assert!(
        !config_path_a.exists(),
        "request must not recreate missing selected config"
    );
    let b_while_a_missing = http_request(
        console_b.port,
        "GET",
        "/api/status",
        Some(token_b),
        None,
        None,
    )
    .expect("verify B remains available while A config is missing");
    assert_eq!(
        expect_json(
            b_while_a_missing,
            200,
            "B remains selected while A config is missing"
        )["ownerQQ"],
        OWNER_ID
    );
    assert_eq!(fs::read(&config_path_b).unwrap(), config_b_before);

    for invalid_config in ["{malformed", "null", "[]", "7", "\"scalar\"", "true"] {
        fs::write(&config_path_a, invalid_config.as_bytes())
            .expect("write invalid selected A config fixture");
        let invalid_config_response = http_request(
            console_a.port,
            "GET",
            "/api/status",
            Some(token_a),
            None,
            None,
        )
        .expect("request with invalid selected A config");
        let invalid_config_response =
            expect_json(invalid_config_response, 503, "invalid selected A config");
        assert_eq!(
            invalid_config_response["error"],
            "runtime config is unavailable"
        );
        assert!(!invalid_config_response
            .to_string()
            .contains(&root_a.to_string_lossy().to_string()));

        let rejected_write = http_request(
            console_a.port,
            "POST",
            "/api/memory/write",
            Some(token_a),
            Some(agent_a),
            Some(&json!({
                "key": SESSION_KEY,
                "content": "invalidconfigwriteprobe",
                "scope": "private",
                "importance": 0.8
            })),
        )
        .expect("attempt memory write while selected A config is invalid");
        let rejected_write = expect_json(rejected_write, 503, "invalid-config memory write");
        assert_eq!(rejected_write["error"], "runtime config is unavailable");
        assert_eq!(
            fs::read(&config_path_a).expect("invalid config remains selected and unchanged"),
            invalid_config.as_bytes()
        );

        let cross_root_token = http_request(
            console_a.port,
            "GET",
            "/api/status",
            Some(token_b),
            None,
            None,
        )
        .expect("verify A does not authenticate against B config");
        expect_json(
            cross_root_token,
            503,
            "A must reject config even with B token",
        );
        let b_status = http_request(
            console_b.port,
            "GET",
            "/api/status",
            Some(token_b),
            None,
            None,
        )
        .expect("verify B remains healthy during A invalid-config requests");
        assert_eq!(
            expect_json(
                b_status,
                200,
                "B remains selected during A invalid-config requests"
            )["ownerQQ"],
            OWNER_ID
        );
        assert_eq!(fs::read(&config_path_b).unwrap(), config_b_before);
    }

    let replacement_token = "public-memory-console-token-a-replaced";
    let replacement_config = json!({
        "consoleToken": replacement_token,
        "ownerQQ": OWNER_ID,
        "allow": { "groups": [], "private": ["7001"] },
        "dsh": { "baseUrl": legacy_url }
    });
    fs::write(
        &config_path_a,
        serde_json::to_vec_pretty(&replacement_config).expect("serialize replacement config"),
    )
    .expect("replace selected A config");
    let old_token = http_request(
        console_a.port,
        "GET",
        "/api/status",
        Some(token_a),
        None,
        None,
    )
    .expect("check old A token after selected config replacement");
    expect_json(
        old_token,
        401,
        "old A token after selected config replacement",
    );
    let replacement_status = http_request(
        console_a.port,
        "GET",
        "/api/status",
        Some(replacement_token),
        None,
        None,
    )
    .expect("read replacement A config through HTTP");
    let replacement_status =
        expect_json(replacement_status, 200, "replacement A config through HTTP");
    assert_eq!(replacement_status["ownerQQ"], OWNER_ID);

    let rejected_write_probe = http_request(
        console_a.port,
        "GET",
        "/api/memory/search?key=private:7001&query=invalidconfigwriteprobe",
        Some(replacement_token),
        Some(agent_a),
        None,
    )
    .expect("verify invalid-config memory writes did not persist after recovery");
    let rejected_write_probe = expect_json(
        rejected_write_probe,
        200,
        "no memory write persisted after config recovery",
    );
    assert_eq!(rejected_write_probe["count"], 0);
    assert!(!rejected_write_probe["memories"]
        .to_string()
        .contains("invalidconfigwriteprobe"));
    let b_write_probe = http_request(
        console_b.port,
        "GET",
        "/api/memory/search?key=private:7001&query=invalidconfigwriteprobe",
        Some(token_b),
        Some(agent_b),
        None,
    )
    .expect("verify invalid-config write did not leak to B");
    let b_write_probe = expect_json(b_write_probe, 200, "B has no A invalid-config write");
    assert_eq!(b_write_probe["count"], 0);
    assert!(!b_write_probe["memories"]
        .to_string()
        .contains("invalidconfigwriteprobe"));

    let status_b = http_request(
        console_b.port,
        "GET",
        "/api/status",
        Some(token_b),
        None,
        None,
    )
    .expect("verify B config remains selected after replacing A");
    let status_b = expect_json(status_b, 200, "console B status after A replacement");
    assert_eq!(status_b["ownerQQ"], OWNER_ID);
}

#[test]
fn preset_routes_use_selected_dsh_credentials_sources_backups_and_user_copies() {
    let workspace = FixtureWorkspace::new("presets");
    let root_a = workspace.root("console A");
    let root_b = workspace.root("console B");
    let token_a = "public-preset-console-token-a";
    let token_b = "public-preset-console-token-b";
    let agent_a = "public-preset-agent-token-a";
    let agent_b = "public-preset-agent-token-b";
    let selected_secret_a = [0x2a; 32];
    let selected_secret_b = [0x6b; 32];
    let legacy_secret_a = [0x31; 32];
    let legacy_secret_b = [0x72; 32];

    let selected_dsh_a = FakeDsh::start(selected_secret_a.to_vec(), "selected-preset-a");
    let selected_dsh_b = FakeDsh::start(selected_secret_b.to_vec(), "selected-preset-b");
    let legacy_dsh_a = FakeDsh::start(legacy_secret_a.to_vec(), "legacy-preset-a");
    let legacy_dsh_b = FakeDsh::start(legacy_secret_b.to_vec(), "legacy-preset-b");

    let dsh_home_a = prepare_runtime_root(
        &root_a,
        token_a,
        agent_a,
        &selected_dsh_a.base_url,
        &legacy_dsh_a.base_url,
        Some(&selected_secret_a),
        Some(&legacy_secret_a),
    );
    let dsh_home_b = prepare_runtime_root(
        &root_b,
        token_b,
        agent_b,
        &selected_dsh_b.base_url,
        &legacy_dsh_b.base_url,
        Some(&selected_secret_b),
        Some(&legacy_secret_b),
    );

    let original_a = valid_preset_content("preset-owner-A");
    let original_b = valid_preset_content("preset-owner-B");
    let updated_a = valid_preset_content("preset-updated-A");
    let updated_b = valid_preset_content("preset-updated-B");
    let (source_a, installed_a) = install_preset(&root_a, &original_a);
    let (source_b, installed_b) = install_preset(&root_b, &original_b);
    let dsh_preset_copy_a = install_dsh_preset_decoy(&dsh_home_a, "dsh-installed-copy-A");
    let dsh_preset_copy_b = install_dsh_preset_decoy(&dsh_home_b, "dsh-installed-copy-B");
    let dsh_preset_copy_a_before = fs::read_to_string(&dsh_preset_copy_a).expect("read synthetic A decoy");
    let dsh_preset_copy_b_before = fs::read_to_string(&dsh_preset_copy_b).expect("read synthetic B decoy");

    let (port_a, port_b) = reserve_distinct_ports();
    let console_a = ConsoleProcess::start(&root_a, port_a, &dsh_home_a);
    let console_b = ConsoleProcess::start(&root_b, port_b, &dsh_home_b);
    assert_ne!(console_a.port, console_b.port);

    let wrong_token = http_request(
        console_a.port,
        "GET",
        "/api/presets",
        Some(token_b),
        None,
        None,
    )
    .expect("reject B token at console A");
    expect_json(wrong_token, 401, "cross-console token");

    for (process, token, expected_id) in [
        (&console_a, token_a, "selected-preset-a"),
        (&console_b, token_b, "selected-preset-b"),
    ] {
        let response = http_request(process.port, "GET", "/api/presets", Some(token), None, None)
            .expect("list presets through selected fake DSH");
        let response = expect_json(response, 200, "selected DSH preset list");
        assert_eq!(response["presets"][0]["id"], expected_id);
        assert_eq!(response["presets"][0]["trust"], "fixture");
    }

    fs::remove_file(root_a.join("user/dsh/.credentials.yaml"))
        .expect("remove only selected A DSH credentials");
    let missing_credentials = http_request(
        console_a.port,
        "GET",
        "/api/presets",
        Some(token_a),
        None,
        None,
    )
    .expect("request presets with selected A credentials missing");
    let missing_credentials = expect_json(
        missing_credentials,
        500,
        "missing selected A DSH credentials",
    );
    assert!(!missing_credentials
        .to_string()
        .contains(&dsh_home_a.to_string_lossy().to_string()));
    assert!(!missing_credentials
        .to_string()
        .contains(&root_a.to_string_lossy().to_string()));

    fs::write(
        root_a.join("user/dsh/.credentials.yaml"),
        "secret: not-a-valid-signing-key\n",
    )
    .expect("write malformed selected A DSH credentials");
    let malformed_credentials = http_request(
        console_a.port,
        "GET",
        "/api/presets",
        Some(token_a),
        None,
        None,
    )
    .expect("request presets with malformed selected A credentials");
    expect_json(
        malformed_credentials,
        500,
        "malformed selected A DSH credentials",
    );

    let invalid_preset = http_request(
        console_a.port,
        "GET",
        "/api/preset/sim-prompt/backups?preset=../../qq-chat-v2",
        Some(token_a),
        None,
        None,
    )
    .expect("reject preset path traversal through HTTP");
    expect_json(invalid_preset, 400, "invalid preset path");

    let read_path = "/api/preset/sim-prompt?preset=qq-chat-v2";
    let read_a = http_request(console_a.port, "GET", read_path, Some(token_a), None, None)
        .expect("read A selected preset source");
    let read_a = expect_json(read_a, 200, "A selected preset source");
    assert_eq!(read_a["content"], original_a);
    assert_eq!(read_a["status"]["installed"], true);
    let read_b = http_request(console_b.port, "GET", read_path, Some(token_b), None, None)
        .expect("read B selected preset source");
    let read_b = expect_json(read_b, 200, "B selected preset source");
    assert_eq!(read_b["content"], original_b);
    assert_eq!(read_b["status"]["installed"], true);

    let write_a = http_request(
        console_a.port,
        "POST",
        "/api/preset/sim-prompt",
        Some(token_a),
        None,
        Some(&json!({ "preset": "qq-chat-v2", "content": updated_a })),
    )
    .expect("update A selected preset through HTTP");
    let write_a = expect_json(write_a, 200, "A selected preset update");
    assert_eq!(write_a["synced"], true);
    assert_eq!(write_a["requiresRestart"], true);
    assert_eq!(write_a["chars"], updated_a.encode_utf16().count());
    assert_eq!(
        fs::read_to_string(&installed_a).expect("read updated A installed copy"),
        preset_yaml(&updated_a)
    );
    assert_eq!(
        fs::read_to_string(&installed_b).expect("read B installed copy after A update"),
        preset_yaml(&original_b)
    );

    let unchanged_b = http_request(console_b.port, "GET", read_path, Some(token_b), None, None)
        .expect("verify B source after A update");
    let unchanged_b = expect_json(unchanged_b, 200, "B source after A update");
    assert_eq!(unchanged_b["content"], original_b);

    let write_b = http_request(
        console_b.port,
        "POST",
        "/api/preset/sim-prompt",
        Some(token_b),
        None,
        Some(&json!({ "preset": "qq-chat-v2", "content": updated_b })),
    )
    .expect("update B selected preset through HTTP");
    let write_b = expect_json(write_b, 200, "B selected preset update");
    assert_eq!(write_b["synced"], true);
    assert_eq!(write_b["requiresRestart"], true);
    assert_eq!(
        fs::read_to_string(&installed_a).expect("read A installed copy after B update"),
        preset_yaml(&updated_a)
    );
    assert_eq!(
        fs::read_to_string(&installed_b).expect("read updated B installed copy"),
        preset_yaml(&updated_b)
    );

    let updated_a_read = http_request(console_a.port, "GET", read_path, Some(token_a), None, None)
        .expect("verify A source after B update");
    let updated_a_read = expect_json(updated_a_read, 200, "A source after B update");
    assert_eq!(updated_a_read["content"], updated_a);

    let backups_a = http_request(
        console_a.port,
        "GET",
        "/api/preset/sim-prompt/backups?preset=qq-chat-v2",
        Some(token_a),
        None,
        None,
    )
    .expect("list A selected preset backups");
    let backups_a = expect_json(backups_a, 200, "A selected preset backups");
    assert_eq!(backups_a["backups"].as_array().map(Vec::len), Some(1));
    let backup_a = backups_a["backups"][0]["file"]
        .as_str()
        .expect("A backup response includes its filename")
        .to_string();

    let backups_b = http_request(
        console_b.port,
        "GET",
        "/api/preset/sim-prompt/backups?preset=qq-chat-v2",
        Some(token_b),
        None,
        None,
    )
    .expect("list B selected preset backups");
    let backups_b = expect_json(backups_b, 200, "B selected preset backups");
    assert_eq!(backups_b["backups"].as_array().map(Vec::len), Some(1));
    let backup_b = backups_b["backups"][0]["file"]
        .as_str()
        .expect("B backup response includes its filename")
        .to_string();

    let backup_dir_a = root_a.join("data/preset-backups");
    let backup_dir_b = root_b.join("data/preset-backups");
    assert_eq!(
        fs::read_to_string(backup_dir_a.join(&backup_a)).expect("read selected A backup"),
        preset_yaml(&original_a)
    );
    assert_eq!(
        fs::read_to_string(backup_dir_b.join(&backup_b)).expect("read selected B backup"),
        preset_yaml(&original_b)
    );

    let restore_a = http_request(
        console_a.port,
        "POST",
        "/api/preset/sim-prompt/restore",
        Some(token_a),
        None,
        Some(&json!({ "preset": "qq-chat-v2", "file": backup_a })),
    )
    .expect("restore A selected backup");
    let restore_a = expect_json(restore_a, 200, "A selected preset restore");
    assert_eq!(restore_a["restoredFrom"], backup_a);
    assert_eq!(
        fs::read_to_string(&installed_a).expect("read restored A installed copy"),
        preset_yaml(&original_a)
    );
    assert_eq!(
        fs::read_to_string(&installed_b).expect("read B installed copy after A restore"),
        preset_yaml(&updated_b)
    );

    let b_after_restore_a =
        http_request(console_b.port, "GET", read_path, Some(token_b), None, None)
            .expect("verify B after A restore");
    let b_after_restore_a = expect_json(b_after_restore_a, 200, "B source after A restore");
    assert_eq!(b_after_restore_a["content"], updated_b);

    let restore_b = http_request(
        console_b.port,
        "POST",
        "/api/preset/sim-prompt/restore",
        Some(token_b),
        None,
        Some(&json!({ "preset": "qq-chat-v2", "file": backup_b })),
    )
    .expect("restore B selected backup");
    let restore_b = expect_json(restore_b, 200, "B selected preset restore");
    assert_eq!(restore_b["restoredFrom"], backup_b);
    assert_eq!(
        fs::read_to_string(&installed_b).expect("read restored B installed copy"),
        preset_yaml(&original_b)
    );

    for (process, token, expected) in [
        (&console_a, token_a, &original_a),
        (&console_b, token_b, &original_b),
    ] {
        let final_read = http_request(process.port, "GET", read_path, Some(token), None, None)
            .expect("read restored preset through HTTP");
        let final_read = expect_json(final_read, 200, "restored preset");
        assert_eq!(&final_read["content"], expected);
    }
    assert_eq!(
        fs::read_to_string(&source_a).expect("read restored A source"),
        preset_yaml(&original_a)
    );
    assert_eq!(
        fs::read_to_string(&installed_a).expect("read updated A installed copy"),
        preset_yaml(&original_a)
    );
    assert_eq!(
        fs::read_to_string(&source_b).expect("read restored B source"),
        preset_yaml(&original_b)
    );
    assert_eq!(
        fs::read_to_string(&installed_b).expect("read updated B installed copy"),
        preset_yaml(&original_b)
    );
    assert_eq!(
        fs::read_to_string(&dsh_preset_copy_a).unwrap(),
        dsh_preset_copy_a_before
    );
    assert_eq!(
        fs::read_to_string(&dsh_preset_copy_b).unwrap(),
        dsh_preset_copy_b_before
    );

    fs::remove_file(&source_a).expect("remove only selected A preset source");
    let missing_source = http_request(console_a.port, "GET", read_path, Some(token_a), None, None)
        .expect("request missing selected A preset source");
    expect_json(missing_source, 500, "missing selected A preset source");

    fs::write(&source_a, "- id: other\n  config: {}\n")
        .expect("write malformed selected A preset source");
    let malformed_source =
        http_request(console_a.port, "GET", read_path, Some(token_a), None, None)
            .expect("request malformed selected A preset source");
    expect_json(malformed_source, 500, "malformed selected A preset source");
    let final_b = http_request(console_b.port, "GET", read_path, Some(token_b), None, None)
        .expect("verify B after selected A source errors");
    assert_eq!(
        expect_json(final_b, 200, "B after selected A source errors")["content"],
        original_b
    );

    let selected_requests_a = selected_dsh_a.finish().expect("join selected DSH A");
    let selected_requests_b = selected_dsh_b.finish().expect("join selected DSH B");
    let legacy_requests_a = legacy_dsh_a.finish().expect("join fallback DSH A");
    let legacy_requests_b = legacy_dsh_b.finish().expect("join fallback DSH B");
    for (requests, expected_path) in [
        (&selected_requests_a, "/api/agentPresets/list"),
        (&selected_requests_b, "/api/agentPresets/list"),
    ] {
        assert_eq!(
            requests.len(),
            1,
            "each selected DSH should receive one list call"
        );
        assert!(
            requests[0].cookie_valid,
            "selected credentials must sign the fake RPC"
        );
        assert_eq!(requests[0].rpc_method.as_deref(), Some("agentPresets/list"));
        assert!(requests[0]
            .request_line
            .starts_with(&format!("POST {expected_path} ")));
    }
    assert!(
        legacy_requests_a.is_empty(),
        "A must not use the legacy DSH endpoint"
    );
    assert!(
        legacy_requests_b.is_empty(),
        "B must not use the legacy DSH endpoint"
    );
}

#[test]
fn console_page_voice_and_tool_log_routes_stay_in_each_selected_root() {
    let workspace = FixtureWorkspace::new("resources");
    let root_a = workspace.root("console A");
    let root_b = workspace.root("console B");
    let token_a = "public-resources-console-token-a";
    let token_b = "public-resources-console-token-b";
    let agent_a = "public-resources-agent-token-a";
    let agent_b = "public-resources-agent-token-b";
    let fake_dependency = "http://127.0.0.1:1";
    let dsh_home_a = prepare_runtime_root(
        &root_a,
        token_a,
        agent_a,
        fake_dependency,
        fake_dependency,
        None,
        None,
    );
    let dsh_home_b = prepare_runtime_root(
        &root_b,
        token_b,
        agent_b,
        fake_dependency,
        fake_dependency,
        None,
        None,
    );
    install_console_resource_fixture(&root_a, "selected-a", agent_a);
    install_console_resource_fixture(&root_b, "selected-b", agent_b);

    let (port_a, port_b) = reserve_distinct_ports();
    let console_a = ConsoleProcess::start(&root_a, port_a, &dsh_home_a);
    let console_b = ConsoleProcess::start(&root_b, port_b, &dsh_home_b);
    assert_ne!(console_a.port, console_b.port);

    let page_a = http_request(console_a.port, "GET", "/", Some(token_a), None, None)
        .expect("request selected A console page");
    assert_eq!(page_a.status, 200, "selected A page: {}", page_a.body);
    assert!(page_a.body.contains("selected-a-page"));
    assert!(!page_a.body.contains("selected-b"));
    let page_b = http_request(console_b.port, "GET", "/", Some(token_b), None, None)
        .expect("request selected B console page");
    assert_eq!(page_b.status, 200, "selected B page: {}", page_b.body);
    assert!(page_b.body.contains("selected-b-page"));
    assert!(!page_b.body.contains("selected-a"));
    let cross_console_page = http_request(console_a.port, "GET", "/", Some(token_b), None, None)
        .expect("reject B console token at A page");
    expect_json(cross_console_page, 401, "cross-console page token");

    for (process, token, marker, own_agent, foreign_agent, root) in [
        (&console_a, token_a, "selected-a", agent_a, agent_b, &root_a),
        (&console_b, token_b, "selected-b", agent_b, agent_a, &root_b),
    ] {
        let voice_response = http_request(
            process.port,
            "GET",
            "/api/voice/list?probe=0",
            Some(token),
            None,
            None,
        )
        .expect("request selected console voice list");
        let voice_list = expect_json(voice_response, 200, "selected console voice list");
        assert_eq!(voice_list["ok"], true);
        assert_eq!(voice_list["count"], 2);
        let voice_names: Vec<String> = voice_list["voices"]
            .as_array()
            .expect("voice list is an array")
            .iter()
            .map(|voice| {
                voice["name"]
                    .as_str()
                    .expect("voice has a name")
                    .to_string()
            })
            .collect();
        assert_eq!(
            voice_names,
            vec![format!("nested/{marker}.wav"), format!("{marker}.MP3")],
            "console lists only sorted selected-root audio names"
        );
        let displayed_dir = voice_list["dir"].as_str().expect("console voice list includes dir");
        assert!(!displayed_dir.starts_with(r"\\?\"), "displayed paths must work in PowerShell");
        assert_eq!(
            fs::canonicalize(displayed_dir).expect("displayed directory resolves"),
            fs::canonicalize(root.join("resources/audio"))
                .expect("canonicalize selected voice root")
        );
        assert!(voice_list["voices"].to_string().contains(marker));

        let agent_path = "/api/socialV2/voices?key=private%3A7001&probe=0";
        let agent_response = http_request(
            process.port,
            "GET",
            agent_path,
            Some(token),
            Some(own_agent),
            None,
        )
        .expect("request selected agent voice list");
        let agent_list = expect_json(agent_response, 200, "selected agent voice list");
        assert_eq!(agent_list["count"], 2);
        assert!(
            agent_list.get("dir").is_none(),
            "agent response hides filesystem path"
        );
        assert!(agent_list["voices"].to_string().contains(marker));
        assert!(!agent_list
            .to_string()
            .contains(&root.to_string_lossy().to_string()));

        let foreign_agent_response = http_request(
            process.port,
            "GET",
            agent_path,
            Some(token),
            Some(foreign_agent),
            None,
        )
        .expect("reject another runtime's agent token");
        expect_json(foreign_agent_response, 403, "foreign agent token");
    }

    set_selected_voice_directory(&root_a, "missing-voices");
    let missing_voice_dir = http_request(
        console_a.port,
        "GET",
        "/api/voice/list?probe=0",
        Some(token_a),
        None,
        None,
    )
    .expect("request missing selected voice directory");
    // JS `listAudioFiles` swallows `readdir` errors: a missing library is an empty library, not a failure.
    let missing_voice_dir = expect_json(missing_voice_dir, 200, "missing selected voice directory");
    assert_eq!(missing_voice_dir["count"], 0);
    assert_eq!(missing_voice_dir["voices"], json!([]));

    set_selected_voice_directory(&root_a, "../data");
    let traversal = http_request(
        console_a.port,
        "GET",
        "/api/voice/list?probe=0",
        Some(token_a),
        None,
        None,
    )
    .expect("request traversing outside the selected resource directory");
    let traversal = expect_json(traversal, 503, "selected voice path traversal");
    assert_eq!(
        traversal["error"],
        "selected voice directory is unavailable"
    );
    assert!(!traversal
        .to_string()
        .contains(&root_a.to_string_lossy().to_string()));

    let outside_voice_dir = workspace.root("outside voice fixture");
    fs::create_dir_all(&outside_voice_dir).expect("create outside synthetic voice directory");
    let outside_voice = outside_voice_dir.join("must-not-be-listed.wav");
    fs::write(&outside_voice, b"outside sentinel").expect("write outside voice sentinel");
    let voice_junction = root_a.join("resources/voice-link");
    create_directory_reparse_point(&voice_junction, &outside_voice_dir)
        .expect("create synthetic resource-root junction");
    let _voice_junction_cleanup = DirectoryReparsePoint(voice_junction.clone());
    set_selected_voice_directory(&root_a, "voice-link");
    let junction_voice_dir = http_request(
        console_a.port,
        "GET",
        "/api/voice/list?probe=0",
        Some(token_a),
        None,
        None,
    )
    .expect("request through a selected-root voice junction");
    let junction_voice_dir = expect_json(junction_voice_dir, 503, "voice junction rejection");
    assert_eq!(
        junction_voice_dir["error"],
        "selected voice directory is unavailable"
    );
    assert!(!junction_voice_dir
        .to_string()
        .contains(&outside_voice_dir.to_string_lossy().to_string()));
    assert_eq!(
        fs::read(&outside_voice).expect("outside sentinel remains readable"),
        b"outside sentinel",
        "route must reject the junction before consuming outside voice data"
    );

    for (process, token, marker) in [
        (&console_a, token_a, "selected-a"),
        (&console_b, token_b, "selected-b"),
    ] {
        let log_response = http_request(
            process.port,
            "GET",
            "/api/socialV2/tool-log",
            Some(token),
            None,
            None,
        )
        .expect("request selected tool log");
        let tool_log = expect_json(log_response, 200, "selected tool-log read");
        assert_eq!(tool_log["ok"], true);
        assert_eq!(tool_log["entries"].as_array().map(Vec::len), Some(2));
        assert!(tool_log["entries"]
            .to_string()
            .contains(&format!("{marker}-first")));
        assert!(tool_log["entries"]
            .to_string()
            .contains(&format!("{marker}-last")));
        let foreign_marker = if marker == "selected-a" {
            "selected-b"
        } else {
            "selected-a"
        };
        assert!(!tool_log["entries"].to_string().contains(foreign_marker));
        assert!(!tool_log["entries"].to_string().contains("not-json"));
    }

    let rejected_clear = http_request(
        console_a.port,
        "POST",
        "/api/socialV2/tool-log/clear",
        Some(token_b),
        None,
        Some(&json!({})),
    )
    .expect("reject B token when clearing A tool log");
    expect_json(rejected_clear, 401, "cross-console tool-log clear token");
    assert!(fs::read_to_string(root_a.join("data/tool-calls.jsonl"))
        .expect("A log remains after rejected clear")
        .contains("selected-a"));

    let clear_a = http_request(
        console_a.port,
        "POST",
        "/api/socialV2/tool-log/clear",
        Some(token_a),
        None,
        Some(&json!({})),
    )
    .expect("clear selected A tool log");
    let clear_a = expect_json(clear_a, 200, "selected A tool-log clear");
    assert_eq!(clear_a["ok"], true);
    assert_eq!(fs::read(root_a.join("data/tool-calls.jsonl")).unwrap(), b"");

    let after_clear_a = http_request(
        console_a.port,
        "GET",
        "/api/socialV2/tool-log",
        Some(token_a),
        None,
        None,
    )
    .expect("read selected A tool log after clear");
    assert_eq!(
        expect_json(after_clear_a, 200, "selected A post-clear tool log")["entries"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    let b_after_a_clear = http_request(
        console_b.port,
        "GET",
        "/api/socialV2/tool-log",
        Some(token_b),
        None,
        None,
    )
    .expect("verify B tool log after clearing A");
    assert!(
        expect_json(b_after_a_clear, 200, "B tool log after A clear")["entries"]
            .to_string()
            .contains("selected-b-last")
    );

    fs::remove_file(root_b.join("data/tool-calls.jsonl"))
        .expect("remove selected B tool log to exercise optional read");
    let missing_log = http_request(
        console_b.port,
        "GET",
        "/api/socialV2/tool-log",
        Some(token_b),
        None,
        None,
    )
    .expect("read missing selected B tool log");
    let missing_log = expect_json(missing_log, 200, "missing selected B tool log");
    assert_eq!(missing_log["ok"], true);
    assert_eq!(missing_log["entries"].as_array().map(Vec::len), Some(0));
    assert!(
        !root_b.join("data/tool-calls.jsonl").exists(),
        "missing log read must not create it"
    );

    let clear_missing_log = http_request(
        console_b.port,
        "POST",
        "/api/socialV2/tool-log/clear",
        Some(token_b),
        None,
        Some(&json!({})),
    )
    .expect("clear missing selected B tool log");
    assert_eq!(
        expect_json(clear_missing_log, 200, "clear missing selected B tool log")["ok"],
        true
    );
    assert_eq!(fs::read(root_b.join("data/tool-calls.jsonl")).unwrap(), b"");

    for (process, label) in [(&console_a, "A"), (&console_b, "B")] {
        for path in ["/favicon.ico", "/apple-touch-icon.png"] {
            let icon = http_request(process.port, "GET", path, None, None, None)
                .unwrap_or_else(|error| panic!("request {label} no-op icon route {path}: {error}"));
            assert_eq!(
                icon.status, 204,
                "{label} no-token {path} response body: {}",
                icon.body
            );
            assert!(
                icon.body.is_empty(),
                "{label} {path} must have an empty body"
            );
        }
    }

    let config_a = root_a.join("config/config.json");
    let valid_config_a = fs::read(&config_a).expect("read A config before icon bypass checks");
    fs::remove_file(&config_a).expect("remove A config before icon bypass checks");
    for path in ["/favicon.ico", "/apple-touch-icon.png"] {
        let icon = http_request(console_a.port, "GET", path, None, None, None)
            .unwrap_or_else(|error| panic!("request A icon with missing config {path}: {error}"));
        assert_eq!(icon.status, 204, "missing-config {path}: {}", icon.body);
        assert!(icon.body.is_empty(), "missing-config {path} must be empty");
    }
    fs::write(&config_a, b"{malformed")
        .expect("write malformed A config before icon bypass checks");
    for path in ["/favicon.ico", "/apple-touch-icon.png"] {
        let icon = http_request(console_a.port, "GET", path, None, None, None)
            .unwrap_or_else(|error| panic!("request A icon with malformed config {path}: {error}"));
        assert_eq!(icon.status, 204, "malformed-config {path}: {}", icon.body);
        assert!(
            icon.body.is_empty(),
            "malformed-config {path} must have an empty body"
        );
    }
    assert_eq!(
        fs::read(&config_a).expect("malformed config remains unchanged after icon requests"),
        b"{malformed"
    );
    fs::write(&config_a, valid_config_a).expect("restore A config for later page checks");

    fs::remove_file(root_a.join("resources/public/console.html"))
        .expect("remove only selected A console page");
    let missing_page = http_request(console_a.port, "GET", "/", Some(token_a), None, None)
        .expect("request missing selected A console page");
    assert_eq!(
        missing_page.status, 500,
        "missing selected page: {}",
        missing_page.body
    );
    assert!(!missing_page.body.contains("selected-b"));
    let b_page_after_a_missing =
        http_request(console_b.port, "GET", "/", Some(token_b), None, None)
            .expect("verify B page after selected A page removal");
    assert_eq!(b_page_after_a_missing.status, 200);
    assert!(b_page_after_a_missing.body.contains("selected-b-page"));
}

#[test]
fn tool_log_management_rejects_agent_credentials_without_mutating_selected_data() {
    let workspace = FixtureWorkspace::new("tool-log-agent-auth");
    let root_a = workspace.root("console A");
    let root_b = workspace.root("console B");
    let token_a = "tool-log-agent-auth-console-token-a";
    let token_b = "tool-log-agent-auth-console-token-b";
    let agent_a = "tool-log-agent-auth-agent-token-a";
    let agent_b = "tool-log-agent-auth-agent-token-b";
    let fake_dependency = "http://127.0.0.1:1";
    let dsh_home_a = prepare_runtime_root(
        &root_a,
        token_a,
        agent_a,
        fake_dependency,
        fake_dependency,
        None,
        None,
    );
    let dsh_home_b = prepare_runtime_root(
        &root_b,
        token_b,
        agent_b,
        fake_dependency,
        fake_dependency,
        None,
        None,
    );
    install_console_resource_fixture(&root_a, "tool-log-a", agent_a);
    install_console_resource_fixture(&root_b, "tool-log-b", agent_b);

    for root in [&root_a, &root_b] {
        fs::write(root.join("data/mode.json"), br#"{"mode":"reserved2"}"#)
            .expect("write explicit selected reserved2 mode");
        let config_path = root.join("config/config.json");
        let mut config: Value = serde_json::from_slice(
            &fs::read(&config_path).expect("read selected config for tool-log auth fixture"),
        )
        .expect("parse selected config for tool-log auth fixture");
        config["socialV2"]["enabled"] = json!(true);
        config["socialV2"]["paused"] = json!(false);
        fs::write(
            config_path,
            serde_json::to_vec_pretty(&config).expect("serialize tool-log auth config fixture"),
        )
        .expect("write tool-log auth config fixture");
    }

    let log_a = root_a.join("data/tool-calls.jsonl");
    let log_b = root_b.join("data/tool-calls.jsonl");
    let state_a = root_a.join("data/social-v2.json");
    let state_b = root_b.join("data/social-v2.json");
    let config_a = root_a.join("config/config.json");
    let config_b = root_b.join("config/config.json");
    let original_a = [
        fs::read(&log_a).expect("snapshot selected A tool log"),
        fs::read(&state_a).expect("snapshot selected A social state"),
        fs::read(&config_a).expect("snapshot selected A config"),
    ];
    let original_b = [
        fs::read(&log_b).expect("snapshot selected B tool log"),
        fs::read(&state_b).expect("snapshot selected B social state"),
        fs::read(&config_b).expect("snapshot selected B config"),
    ];

    let (port_a, port_b) = reserve_distinct_ports();
    let console_a = ConsoleProcess::start(&root_a, port_a, &dsh_home_a);
    let console_b = ConsoleProcess::start(&root_b, port_b, &dsh_home_b);
    assert_ne!(console_a.port, console_b.port);

    // Legacy console authentication runs before agent-token policy. A valid or empty
    // agent header cannot make a request without the console token authorized, and
    // an incorrect console token remains a 401 even when an agent token is present.
    let clear_body = json!({});
    let auth_order_cases = [
        (
            "GET",
            "/api/socialV2/tool-log",
            None,
            Some(agent_a),
            None,
            "GET without console token and valid agent token",
        ),
        (
            "GET",
            "/api/socialV2/tool-log",
            None,
            Some(""),
            None,
            "GET without console token and empty agent token",
        ),
        (
            "GET",
            "/api/socialV2/tool-log",
            Some("tool-log-agent-auth-console-token-wrong"),
            Some(agent_a),
            None,
            "GET with incorrect console token and valid agent token",
        ),
        (
            "POST",
            "/api/socialV2/tool-log/clear",
            None,
            Some(agent_a),
            Some(&clear_body),
            "POST without console token and valid agent token",
        ),
        (
            "POST",
            "/api/socialV2/tool-log/clear",
            None,
            Some(""),
            Some(&clear_body),
            "POST without console token and empty agent token",
        ),
        (
            "POST",
            "/api/socialV2/tool-log/clear",
            Some("tool-log-agent-auth-console-token-wrong"),
            Some(agent_a),
            Some(&clear_body),
            "POST with incorrect console token and valid agent token",
        ),
    ];
    for (method, path, console_token, agent_token, body, context) in auth_order_cases {
        let response = http_request(
            console_a.port,
            method,
            path,
            console_token,
            agent_token,
            body,
        )
        .unwrap_or_else(|error| panic!("{context} failed at transport: {error}"));
        assert_eq!(
            expect_json(response, 401, context),
            json!({
                "ok": false,
                "error": "未授权：请提供控制台访问令牌"
            }),
            "{context} must fail at console authentication before agent policy"
        );
    }

    let get_agent_a = http_request(
        console_a.port,
        "GET",
        "/api/socialV2/tool-log",
        Some(token_a),
        Some(agent_a),
        None,
    )
    .expect("request A tool log with selected agent credentials");
    assert_eq!(
        expect_json(get_agent_a, 403, "agent credentials cannot read A tool log"),
        json!({ "ok": false, "error": "该接口仅控制台可用" })
    );

    let get_empty_agent = http_request(
        console_a.port,
        "GET",
        "/api/socialV2/tool-log",
        Some(token_a),
        Some(""),
        None,
    )
    .expect("request A tool log with an empty agent header");
    assert_eq!(
        expect_json(get_empty_agent, 403, "empty agent header is rejected first"),
        json!({ "ok": false, "error": "agent token 不能为空" })
    );

    let get_foreign_agent = http_request(
        console_a.port,
        "GET",
        "/api/socialV2/tool-log",
        Some(token_a),
        Some(agent_b),
        None,
    )
    .expect("request A tool log with B agent credentials");
    assert_eq!(
        expect_json(
            get_foreign_agent,
            403,
            "B agent credentials cannot read A tool log"
        ),
        json!({ "ok": false, "error": "该接口仅控制台可用" })
    );

    let clear_agent_a = http_request(
        console_a.port,
        "POST",
        "/api/socialV2/tool-log/clear",
        Some(token_a),
        Some(agent_a),
        Some(&json!({})),
    )
    .expect("clear A tool log with selected agent credentials");
    assert_eq!(
        expect_json(
            clear_agent_a,
            403,
            "agent credentials cannot clear A tool log"
        ),
        json!({ "ok": false, "error": "该接口仅控制台可用" })
    );

    let clear_empty_agent = http_request(
        console_a.port,
        "POST",
        "/api/socialV2/tool-log/clear",
        Some(token_a),
        Some(""),
        Some(&json!({})),
    )
    .expect("clear A tool log with an empty agent header");
    assert_eq!(
        expect_json(
            clear_empty_agent,
            403,
            "empty agent header precedes clear handler"
        ),
        json!({ "ok": false, "error": "agent token 不能为空" })
    );

    let clear_foreign_agent = http_request(
        console_a.port,
        "POST",
        "/api/socialV2/tool-log/clear",
        Some(token_a),
        Some(agent_b),
        Some(&json!({})),
    )
    .expect("clear A tool log with B agent credentials");
    assert_eq!(
        expect_json(
            clear_foreign_agent,
            403,
            "B agent credentials cannot clear A tool log"
        ),
        json!({ "ok": false, "error": "该接口仅控制台可用" })
    );

    for (path, original, label) in [
        (&log_a, &original_a[0], "A tool log"),
        (&state_a, &original_a[1], "A social state"),
        (&config_a, &original_a[2], "A config"),
        (&log_b, &original_b[0], "B tool log"),
        (&state_b, &original_b[1], "B social state"),
        (&config_b, &original_b[2], "B config"),
    ] {
        assert_eq!(
            fs::read(path)
                .unwrap_or_else(|error| panic!("read {label} after rejected request: {error}")),
            *original,
            "rejected agent requests must not mutate {label}"
        );
    }

    for (process, token, marker, foreign_marker) in [
        (&console_a, token_a, "tool-log-a", "tool-log-b"),
        (&console_b, token_b, "tool-log-b", "tool-log-a"),
    ] {
        let console_read = http_request(
            process.port,
            "GET",
            "/api/socialV2/tool-log",
            Some(token),
            None,
            None,
        )
        .expect("read tool log with console credentials only");
        let console_read = expect_json(console_read, 200, "console-only tool-log read");
        assert_eq!(console_read["ok"], true);
        assert!(console_read["entries"].to_string().contains(marker));
        assert!(!console_read["entries"].to_string().contains(foreign_marker));
    }

    let clear_console_a = http_request(
        console_a.port,
        "POST",
        "/api/socialV2/tool-log/clear",
        Some(token_a),
        None,
        Some(&json!({})),
    )
    .expect("clear A tool log with console credentials only");
    let clear_console_a = expect_json(clear_console_a, 200, "console-only A tool-log clear");
    assert_eq!(clear_console_a["ok"], true);
    assert_eq!(fs::read(&log_a).expect("read cleared selected A log"), b"");
    assert_eq!(
        fs::read(&log_b).expect("read unchanged selected B log"),
        original_b[0]
    );
    assert_eq!(
        fs::read(&state_a).expect("read unchanged selected A state"),
        original_a[1]
    );
    assert_eq!(
        fs::read(&state_b).expect("read unchanged selected B state"),
        original_b[1]
    );
    assert_eq!(
        fs::read(&config_a).expect("read unchanged selected A config"),
        original_a[2]
    );
    assert_eq!(
        fs::read(&config_b).expect("read unchanged selected B config"),
        original_b[2]
    );

    let b_after_a_clear = http_request(
        console_b.port,
        "GET",
        "/api/socialV2/tool-log",
        Some(token_b),
        None,
        None,
    )
    .expect("verify B tool log after console clears A");
    let b_after_a_clear = expect_json(b_after_a_clear, 200, "B tool log after A clear");
    assert!(b_after_a_clear["entries"]
        .to_string()
        .contains("tool-log-b-first"));
    assert!(!b_after_a_clear["entries"]
        .to_string()
        .contains("tool-log-a"));
}

#[test]
fn self_image_public_http_reads_selected_portrait_for_agent_after_restart() {
    let workspace = FixtureWorkspace::new("self-image-selected-restart");
    let root = workspace.root("selected");
    let token = "portrait-console-token";
    let agent = "portrait-agent-token";
    let dsh_home = prepare_runtime_root(&root, token, agent, "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let assets = root.join("resources/assets");
    fs::create_dir_all(&assets).unwrap();
    let portrait = assets.join("deepseek娘.png");
    let pixels = image::RgbImage::from_pixel(2, 1, image::Rgb([19, 123, 211]));
    pixels.save(&portrait).unwrap();
    let original = fs::read(&portrait).unwrap();
    let decoy = dsh_home.join("deepseek娘.png");
    fs::write(&decoy, b"synthetic legacy portrait sentinel").unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    let path = "/api/socialV2/self-image?key=private%3A7001";
    expect_json(http_request(port, "GET", path, Some(token), Some("wrong"), None).unwrap(), 403, "invalid portrait agent token");
    drop(console);
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    let result = expect_json(http_request(port, "GET", path, Some(token), Some(agent), None).unwrap(), 200, "selected portrait after restart");
    assert_eq!(result["ok"], true);
    assert_eq!(result["key"], SESSION_KEY);
    assert_eq!(result["image"]["mimeType"], "image/png");
    assert_eq!(result["images"][0], result["image"]);
    let bytes = base64::engine::general_purpose::STANDARD.decode(result["image"]["data"].as_str().unwrap()).unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert_eq!(decoded.dimensions(), (2, 1));
    assert_eq!(decoded.get_pixel(0, 0).0, [19, 123, 211]);
    assert_eq!(fs::read(&portrait).unwrap(), original);
    assert_eq!(fs::read(&decoy).unwrap(), b"synthetic legacy portrait sentinel");
    drop(console);
}

#[test]
fn missing_self_image_public_http_matches_js_not_found() {
    let workspace = FixtureWorkspace::new("missing-self-image");
    let root = workspace.root("selected");
    let token = "missing-self-image-console-token";
    let dsh_home = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    let response = http_request(port, "GET", "/api/socialV2/self-image?key=private%3A7001", Some(token), None, None).unwrap();
    let body = expect_json(response, 404, "missing selected self image");
    assert_eq!(body["ok"], false);
    assert!(body["error"].as_str().unwrap().contains("未找到 AI 形象图片"));
    drop(console);
}

#[test]
fn selected_self_image_invalid_paths_return_unavailable() {
    let workspace = FixtureWorkspace::new("invalid-self-image");
    let root = workspace.root("selected");
    let token = "invalid-self-image-console-token";
    let dsh_home = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    let request = || {
        let response = http_request(port, "GET", "/api/socialV2/self-image?key=private%3A7001", Some(token), None, None).unwrap();
        let body = expect_json(response, 503, "invalid selected self image path");
        assert_eq!(body["error"], "selected AI image is unavailable");
    };

    let assets = root.join("resources/assets");
    fs::write(&assets, b"not a directory").unwrap();
    request();
    fs::remove_file(&assets).unwrap();
    fs::create_dir(&assets).unwrap();
    fs::create_dir(assets.join("deepseek娘.png")).unwrap();
    request();
    fs::remove_dir(assets.join("deepseek娘.png")).unwrap();
    fs::remove_dir(&assets).unwrap();

    let outside = workspace.root("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("deepseek娘.png"), b"outside sentinel").unwrap();
    create_directory_reparse_point(&assets, &outside).expect("create synthetic resource junction");
    let _cleanup = DirectoryReparsePoint(assets);
    request();
    assert_eq!(fs::read(outside.join("deepseek娘.png")).unwrap(), b"outside sentinel");
    drop(console);
}

#[test]
fn sticker_model_send_and_collect_public_http_validate_required_fields() {
    let workspace = FixtureWorkspace::new("sticker-model-route-validation");
    let root = workspace.root("selected");
    let token = "sticker-model-route-console-token";
    let dsh_home = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let config_path = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["socialV2"] = json!({"sticker":{"enabled":true}});
    fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    for (path, error) in [
        ("/api/socialV2/send-sticker", "key 和 stickerId 不能为空"),
        ("/api/socialV2/collect-sticker", "key 和 messageId/seq 不能为空"),
    ] {
        let response = http_request(port, "POST", path, Some(token), None, Some(&json!({}))).unwrap();
        let body = expect_json(response, 400, path);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], error);
    }
    drop(console);
}

#[test]
fn sticker_note_public_http_persists_in_selected_root_after_restart() {
    let workspace = FixtureWorkspace::new("sticker-note-restart");
    let root = workspace.root("selected");
    let token = "sticker-note-console-token";
    let dsh_home = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    // Listing after a restart refreshes the gateway cache. A closed port has a
    // platform-dependent refusal delay; serve the selected dependency instead.
    let mut onebot = FakeStickerOneBot::bind(vec![json!({"emoji_id":"historic", "desc":"old"})]);
    let dependencies_path = root.join("dependencies.json");
    let mut dependencies: Value = serde_json::from_slice(&fs::read(&dependencies_path).unwrap()).unwrap();
    dependencies["onebot"] = json!(onebot.base_url);
    fs::write(dependencies_path, serde_json::to_vec(&dependencies).unwrap()).unwrap();
    onebot.start();
    let store = root.join("data/stickers.json");
    fs::write(&store, serde_json::to_vec(&json!([{
        "id":"historic", "desc":"old", "localNote":"before", "tags":["old"], "useCount":7
    }])).unwrap()).unwrap();
    let decoy = dsh_home.join("stickers.json");
    fs::write(&decoy, b"synthetic legacy sentinel").unwrap();
    let (port, _) = reserve_distinct_ports();
    let seeded = fs::read(&store).unwrap();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    wait_for_sticker_warmup(port, token, &store, &seeded);
    let body = json!({"stickerId":"historic", "note":"after", "tags":["new"], "usage":"reply"});
    let before = fs::read(&store).unwrap();
    expect_json(http_request(port, "POST", "/api/stickers/note", Some("wrong"), None, Some(&body)).unwrap(), 401, "unauthorized note");
    assert_eq!(fs::read(&store).unwrap(), before);
    let updated = expect_json(http_request(port, "POST", "/api/stickers/note", Some(token), None, Some(&body)).unwrap(), 200, "selected note");
    assert_eq!(updated["sticker"]["localNote"], "after");
    assert_eq!(updated["sticker"]["tags"], json!(["new"]));
    assert_eq!(updated["sticker"]["useCount"], 7);
    drop(console);
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    let list = expect_json(http_request(port, "GET", "/api/stickers", Some(token), None, None).unwrap(), 200, "list after restart");
    assert_eq!(list["stickers"][0]["localNote"], "after");
    assert_eq!(list["stickers"][0]["tags"], json!(["new"]));
    assert_eq!(list["stickers"][0]["useCount"], 7);
    assert_eq!(fs::read(&decoy).unwrap(), b"synthetic legacy sentinel");
    drop(console);
}

#[test]
fn sticker_remark_public_http_updates_selected_history() {
    let workspace = FixtureWorkspace::new("sticker-remark");
    let root = workspace.root("selected");
    let token = "sticker-remark-console-token";
    let mut onebot = FakeStickerOneBot::bind(vec![json!({"emoji_id":"historic", "desc":"old"})]);
    let dsh_home = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let dependencies_path = root.join("dependencies.json");
    let mut dependencies: Value = serde_json::from_slice(&fs::read(&dependencies_path).unwrap()).unwrap();
    dependencies["onebot"] = json!(onebot.base_url);
    fs::write(&dependencies_path, serde_json::to_vec(&dependencies).unwrap()).unwrap();
    let store = root.join("data/stickers.json");
    fs::write(&store, serde_json::to_vec(&json!([{
        "id":"historic", "source":"qq", "desc":"old", "localNote":"kept", "tags":["kept"], "useCount":7
    }])).unwrap()).unwrap();
    let decoy = dsh_home.join("stickers.json");
    fs::write(&decoy, b"synthetic legacy sentinel").unwrap();
    onebot.start();
    let (port, _) = reserve_distinct_ports();
    let seeded = fs::read(&store).unwrap();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    wait_for_sticker_warmup(port, token, &store, &seeded);
    let body = json!({"stickerId":"historic", "remark":"new remark"});
    let before = fs::read(&store).unwrap();
    expect_json(http_request(port, "POST", "/api/stickers/remark", Some("wrong"), None, Some(&body)).unwrap(), 401, "unauthorized remark");
    expect_json(http_request(port, "POST", "/api/stickers/remark", Some(token), Some("foreign"), Some(&body)).unwrap(), 403, "agent remark denied");
    assert_eq!(fs::read(&store).unwrap(), before);
    let result = expect_json(http_request(port, "POST", "/api/stickers/remark", Some(token), None, Some(&body)).unwrap(), 200, "selected remark");
    assert_eq!(result["sticker"]["desc"], "new remark");
    assert_eq!(result["sticker"]["localNote"], "kept");
    assert_eq!(result["sticker"]["tags"], json!(["kept"]));
    assert_eq!(result["sticker"]["useCount"], 7);
    drop(console);
    let requests = onebot.finish().unwrap();
    let remarks: Vec<_> = requests.iter().filter(|request| request.path == "/modify_custom_face").collect();
    assert_eq!(remarks.len(), 1);
    assert_eq!(remarks[0].body, json!({"emoji_id":"historic", "desc":"new remark"}));
    let entries: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
    assert_eq!(entries[0]["desc"], "new remark");
    assert_eq!(entries[0]["localNote"], "kept");
    assert_eq!(fs::read(&decoy).unwrap(), b"synthetic legacy sentinel");
}

#[test]
fn sticker_force_sync_public_http_merges_selected_history() {
    let workspace = FixtureWorkspace::new("sticker-force-sync");
    let root = workspace.root("selected");
    let token = "sticker-force-sync-console-token";
    let mut onebot = FakeStickerOneBot::bind(vec![json!({
        "emoji_id":"historic", "url":"https://example.invalid/new.png", "desc":"new"
    })]);
    let dsh_home = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let dependencies_path = root.join("dependencies.json");
    let mut dependencies: Value = serde_json::from_slice(&fs::read(&dependencies_path).unwrap()).unwrap();
    dependencies["onebot"] = json!(onebot.base_url);
    fs::write(&dependencies_path, serde_json::to_vec(&dependencies).unwrap()).unwrap();
    let store = root.join("data/stickers.json");
    fs::write(&store, serde_json::to_vec(&json!([{
        "id":"historic", "source":"qq", "desc":"old", "localNote":"kept", "tags":["kept"], "useCount":7
    }])).unwrap()).unwrap();
    let decoy = dsh_home.join("stickers.json");
    fs::write(&decoy, b"synthetic legacy sentinel").unwrap();
    onebot.start();
    let (port, _) = reserve_distinct_ports();
    let seeded = fs::read(&store).unwrap();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    wait_for_sticker_warmup(port, token, &store, &seeded);
    let before = fs::read(&store).unwrap();
    expect_json(http_request(port, "POST", "/api/stickers/sync", None, None, Some(&json!({}))).unwrap(), 401, "unauthenticated force sync");
    assert_eq!(fs::read(&store).unwrap(), before);
    let result = expect_json(http_request(port, "POST", "/api/stickers/sync", Some(token), None, Some(&json!({}))).unwrap(), 200, "force sync");
    assert_eq!(result["ok"], true);
    drop(console);
    let requests = onebot.finish().unwrap();
    assert!(requests.iter().any(|request| request.path == "/fetch_custom_face_detail"));
    let entries: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
    let entry = entries.as_array().unwrap().iter().find(|entry| entry["id"] == "historic").unwrap();
    assert_eq!(entry["desc"], "new");
    assert_eq!(entry["localNote"], "kept");
    assert_eq!(entry["tags"], json!(["kept"]));
    assert_eq!(entry["useCount"], 7);
    assert_eq!(fs::read(&decoy).unwrap(), b"synthetic legacy sentinel");
}

#[test]
fn sticker_public_http_cache_ignores_refresh_until_ttl_expires() {
    let workspace = FixtureWorkspace::new("sticker-cache-ttl");
    let root = workspace.root("selected");
    let token = "sticker-cache-console-token";
    let mut onebot = FakeStickerOneBot::bind(vec![json!({
        "emoji_id":"historic", "url":"https://example.invalid/a.png", "desc":"favorite"
    })]);
    let dsh_home = prepare_runtime_root(&root, token, "agent", "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    let config_path = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["socialV2"] = json!({"sticker":{"enabled":true,"syncTtlMs":5000}});
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let dependencies_path = root.join("dependencies.json");
    let mut dependencies: Value = serde_json::from_slice(&fs::read(&dependencies_path).unwrap()).unwrap();
    dependencies["onebot"] = json!(onebot.base_url);
    fs::write(&dependencies_path, serde_json::to_vec(&dependencies).unwrap()).unwrap();
    fs::write(root.join("data/stickers.json"), serde_json::to_vec(&json!([{
        "id":"historic", "source":"qq", "localNote":"kept", "useCount":4
    }])).unwrap()).unwrap();
    onebot.start();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start(&root, port, &dsh_home);
    expect_json(http_request(port, "POST", "/api/stickers/sync", Some(token), None, Some(&json!({}))).unwrap(), 200, "prime cache");
    let cached = expect_json(http_request(port, "GET", "/api/stickers", Some(token), None, None).unwrap(), 200, "cached list");
    let refresh = expect_json(http_request(port, "GET", "/api/stickers?refresh=true", Some(token), None, None).unwrap(), 200, "ignored refresh query");
    assert_eq!(cached["fromCache"], true);
    assert_eq!(refresh["fromCache"], true);
    assert_eq!(refresh["stickers"], cached["stickers"]);
    assert_eq!(cached["stickers"][0]["localNote"], "kept");
    assert_eq!(cached["stickers"][0]["useCount"], 4);
    thread::sleep(Duration::from_millis(5200));
    let expired = expect_json(http_request(port, "GET", "/api/stickers", Some(token), None, None).unwrap(), 200, "expired cache");
    assert_eq!(expired["fromCache"], false);
    assert_eq!(expired["stickers"][0]["localNote"], "kept");
    drop(console);
    let requests = onebot.finish().unwrap();
    assert_eq!(requests.len(), 3, "startup warm-up, force and expired GET fetch favorites");
    assert!(requests.iter().all(|request| request.path == "/fetch_custom_face_detail"));
}

#[test]
fn sticker_list_public_http_uses_selected_roots_and_js_console_auth() {
    let workspace = FixtureWorkspace::new("sticker-list-ab");
    let root_a = workspace.root("console A");
    let root_b = workspace.root("console B");
    let token_a = "sticker-list-console-token-a";
    let token_b = "sticker-list-console-token-b";
    let agent_a = "sticker-list-agent-token-a";
    let agent_b = "sticker-list-agent-token-b";
    let onebot_token_a = "sticker-list-onebot-token-a";
    let onebot_token_b = "sticker-list-onebot-token-b";
    let dead_dependency = "http://127.0.0.1:1";
    let dsh_home_a = prepare_runtime_root(
        &root_a,
        token_a,
        agent_a,
        dead_dependency,
        dead_dependency,
        None,
        None,
    );
    let dsh_home_b = prepare_runtime_root(
        &root_b,
        token_b,
        agent_b,
        dead_dependency,
        dead_dependency,
        None,
        None,
    );

    let make_entries = |marker: &str, base: i64| {
        let upper = marker.to_ascii_uppercase();
        let entries = [("one", true, 1_i64), ("two", true, 2), ("hidden", false, 3)]
            .into_iter()
            .map(|(suffix, matches_filter, offset)| {
                let tags = if matches_filter {
                    vec![format!("{marker}-filter"), suffix.to_string()]
                } else {
                    vec![format!("{marker}-other")]
                };
                json!({
                    "id": format!("{marker}-sticker-{suffix}"),
                    "resId": format!("{marker}-res-{suffix}"),
                    "url": format!("https://fixture.invalid/{marker}-{suffix}.gif"),
                    "md5": format!("{upper}-{}-MD5", suffix.to_ascii_uppercase()),
                    "desc": format!("{marker} description {suffix}"),
                    "localNote": format!("{marker} local note {suffix}"),
                    "tags": tags,
                    "usage": format!("{marker} usage {suffix}"),
                    "source": "qq",
                    "useCount": base + offset,
                    "lastUsedAt": base * 10 + offset + 10
                })
            })
            .collect::<Vec<_>>();
        Value::Array(entries)
    };
    let entries_a = make_entries("alpha", 10);
    let entries_b = make_entries("beta", 20);
    let onebot_rows = |entries: &Value, count: usize| {
        entries
            .as_array()
            .unwrap()
            .iter()
            .take(count)
            .map(|entry| {
                json!({
                    "emoji_id": entry["id"],
                    "resId": entry["resId"],
                    "url": entry["url"],
                    "md5": entry["md5"],
                    "desc": entry["desc"]
                })
            })
            .collect::<Vec<_>>()
    };
    let expected_sticker = |entry: &Value| {
        json!({
            "id": entry["id"],
            "resId": entry["resId"],
            "md5": entry["md5"],
            "url": entry["url"],
            "desc": entry["desc"],
            "localNote": entry["localNote"],
            "tags": entry["tags"],
            "usage": entry["usage"],
            "useCount": entry["useCount"],
            "lastUsedAt": entry["lastUsedAt"],
            "source": entry["source"]
        })
    };
    let expected_response = |entries: &Value| {
        json!({
            "ok": true,
            "key": SESSION_KEY,
            "total": 3,
            "matched": 2,
            "truncated": true,
            "stickers": [expected_sticker(&entries[0])],
            "fromCache": false
        })
    };
    // Reserve selected endpoints before starting the console processes, then start each
    // bounded fake worker only after their health checks have completed.
    let mut selected_onebot_a = FakeStickerOneBot::bind(onebot_rows(&entries_a, 3));
    let mut selected_onebot_b = FakeStickerOneBot::bind(onebot_rows(&entries_b, 1));
    let legacy_onebot_a = TcpListener::bind(("127.0.0.1", 0)).expect("bind A legacy OneBot decoy");
    legacy_onebot_a
        .set_nonblocking(true)
        .expect("set A legacy OneBot decoy nonblocking");
    let legacy_onebot_b = TcpListener::bind(("127.0.0.1", 0)).expect("bind B legacy OneBot decoy");
    legacy_onebot_b
        .set_nonblocking(true)
        .expect("set B legacy OneBot decoy nonblocking");
    let legacy_onebot_url_a = format!(
        "http://127.0.0.1:{}",
        legacy_onebot_a
            .local_addr()
            .expect("read A legacy decoy port")
            .port()
    );
    let legacy_onebot_url_b = format!(
        "http://127.0.0.1:{}",
        legacy_onebot_b
            .local_addr()
            .expect("read B legacy decoy port")
            .port()
    );

    for (root, selected_url, legacy_url, onebot_token, max_count, entries) in [
        (
            &root_a,
            selected_onebot_a.base_url.as_str(),
            legacy_onebot_url_a.as_str(),
            onebot_token_a,
            5_i64,
            &entries_a,
        ),
        (
            &root_b,
            selected_onebot_b.base_url.as_str(),
            legacy_onebot_url_b.as_str(),
            onebot_token_b,
            1_i64,
            &entries_b,
        ),
    ] {
        let config_path = root.join("config/config.json");
        let mut config: Value = serde_json::from_slice(
            &fs::read(&config_path).expect("read selected sticker-list config"),
        )
        .expect("parse selected sticker-list config");
        config["snowluma"] = json!({
            "httpUrl": legacy_url,
            "accessToken": onebot_token
        });
        config["socialV2"] = json!({
            "sticker": {
                "enabled": true,
                "syncTtlMs": 60000,
                "maxListCount": max_count
            },
            "tools": { "listStickers": true }
        });
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&config).expect("serialize selected sticker-list config"),
        )
        .expect("write selected sticker-list config");

        let dependencies_path = root.join("dependencies.json");
        let mut dependencies: Value = serde_json::from_slice(
            &fs::read(&dependencies_path).expect("read selected sticker-list dependencies"),
        )
        .expect("parse selected sticker-list dependencies");
        dependencies["onebot"] = json!(selected_url);
        fs::write(
            &dependencies_path,
            serde_json::to_vec(&dependencies).expect("serialize selected OneBot dependency"),
        )
        .expect("write selected OneBot dependency");
        fs::write(
            root.join("data/stickers.json"),
            serde_json::to_vec(entries).expect("serialize selected sticker entries"),
        )
        .expect("write selected sticker entries");
    }

    let stickers_a = root_a.join("data/stickers.json");
    let stickers_b = root_b.join("data/stickers.json");
    let state_a = root_a.join("data/social-v2.json");
    let state_b = root_b.join("data/social-v2.json");
    let config_a = root_a.join("config/config.json");
    let config_b = root_b.join("config/config.json");
    let original_denial_files_a = [
        fs::read(&stickers_a).expect("snapshot A stickers before auth denials"),
        fs::read(&state_a).expect("snapshot A state before auth denials"),
        fs::read(&config_a).expect("snapshot A config before auth denials"),
    ];
    let original_denial_files_b = [
        fs::read(&stickers_b).expect("snapshot B stickers before auth denials"),
        fs::read(&state_b).expect("snapshot B state before auth denials"),
        fs::read(&config_b).expect("snapshot B config before auth denials"),
    ];

    let (port_a, port_b) = reserve_distinct_ports();
    let console_a = ConsoleProcess::start(&root_a, port_a, &dsh_home_a);
    let console_b = ConsoleProcess::start(&root_b, port_b, &dsh_home_b);

    let missing_key = http_request(
        console_a.port,
        "GET",
        &format!("/api/socialV2/sticker-list?token={token_a}"),
        None,
        None,
        None,
    );
    let wrong_query_token = http_request(
        console_a.port,
        "GET",
        &format!(
            "/api/socialV2/sticker-list?token=wrong-query-token&key={SESSION_KEY}&query=alpha-filter&count=1"
        ),
        Some(token_a),
        None,
        None,
    );
    let cross_root_a = http_request(
        console_a.port,
        "GET",
        &format!("/api/socialV2/sticker-list?key={SESSION_KEY}"),
        Some(token_b),
        None,
        None,
    );
    let cross_root_b = http_request(
        console_b.port,
        "GET",
        &format!("/api/socialV2/sticker-list?key={SESSION_KEY}"),
        Some(token_a),
        None,
        None,
    );
    let denied_onebot_requests_a = selected_onebot_a.drain_pending_connections();
    let denied_onebot_requests_b = selected_onebot_b.drain_pending_connections();
    let denied_files_a = [
        fs::read(&stickers_a).ok(),
        fs::read(&state_a).ok(),
        fs::read(&config_a).ok(),
    ];
    let denied_files_b = [
        fs::read(&stickers_b).ok(),
        fs::read(&state_b).ok(),
        fs::read(&config_b).ok(),
    ];

    selected_onebot_a.start();
    selected_onebot_b.start();
    let selected_list_a = http_request(
        console_a.port,
        "GET",
        &format!(
            "/api/socialV2/sticker-list?token={token_a}&key={SESSION_KEY}&query=%20ALPHA-FILTER%20&count=1"
        ),
        None,
        None,
        None,
    );
    let selected_list_b = http_request(
        console_b.port,
        "GET",
        &format!("/api/socialV2/sticker-list?key={SESSION_KEY}&query=beta-filter&count=10"),
        Some(token_b),
        None,
        None,
    );

    let onebot_requests_a = selected_onebot_a.finish();
    let onebot_requests_b = selected_onebot_b.finish();

    let missing_key = expect_json(
        missing_key.expect("request A sticker list without key"),
        400,
        "sticker-list requires key",
    );
    assert_eq!(missing_key, json!({ "ok": false, "error": "key 不能为空" }));
    let wrong_query_token = expect_json(
        wrong_query_token.expect("request A with wrong query token and correct console header"),
        401,
        "wrong query token overrides correct console header",
    );
    assert_eq!(
        wrong_query_token,
        json!({ "ok": false, "error": "未授权：请提供控制台访问令牌" })
    );
    for (response, context) in [
        (
            cross_root_a.expect("request A with B console token"),
            "A rejects B console token",
        ),
        (
            cross_root_b.expect("request B with A console token"),
            "B rejects A console token",
        ),
    ] {
        assert_eq!(
            expect_json(response, 401, context),
            json!({ "ok": false, "error": "未授权：请提供控制台访问令牌" })
        );
    }
    assert_eq!(
        denied_onebot_requests_a.expect("drain A selected OneBot before authorized reads"),
        1,
        "only the startup warm-up contacts selected OneBot A; invalid or missing console credentials add none"
    );
    assert_eq!(
        denied_onebot_requests_b.expect("drain B selected OneBot before authorized reads"),
        1,
        "only the startup warm-up contacts selected OneBot B; cross-root console credentials add none"
    );
    for (denied, original, label) in [
        (&denied_files_a, &original_denial_files_a, "A"),
        (&denied_files_b, &original_denial_files_b, "B"),
    ] {
        for (index, path_label) in ["sticker store", "social state", "config"]
            .iter()
            .enumerate()
        {
            assert_eq!(
                denied[index].as_ref(),
                Some(&original[index]),
                "rejected {label} sticker-list request must not write its selected {path_label}"
            );
        }
    }

    let selected_list_a = expect_json(
        selected_list_a.expect("request A sticker list using query-token authentication"),
        200,
        "A selected sticker-list response",
    );
    let selected_list_b = expect_json(
        selected_list_b.expect("request B sticker list using console-header authentication"),
        200,
        "B selected sticker-list response",
    );
    let mut stable_a = selected_list_a.clone();
    let synced_at_a = stable_a
        .as_object_mut()
        .and_then(|fields| fields.remove("syncedAt"));
    assert!(
        synced_at_a
            .and_then(|value| value.as_i64())
            .unwrap_or_default()
            > 0
    );
    assert_eq!(stable_a, expected_response(&entries_a));
    let mut stable_b = selected_list_b.clone();
    let synced_at_b = stable_b
        .as_object_mut()
        .and_then(|fields| fields.remove("syncedAt"));
    assert!(
        synced_at_b
            .and_then(|value| value.as_i64())
            .unwrap_or_default()
            > 0
    );
    assert_eq!(stable_b, expected_response(&entries_b));
    assert!(!selected_list_a.to_string().contains("beta-sticker"));
    assert!(!selected_list_b.to_string().contains("alpha-sticker"));

    let onebot_requests_a = onebot_requests_a.expect("fake OneBot A worker completed");
    let onebot_requests_b = onebot_requests_b.expect("fake OneBot B worker completed");
    assert_eq!(
        onebot_requests_a.len(),
        1,
        "A must use only its selected OneBot"
    );
    assert_eq!(onebot_requests_a[0].method, "POST");
    assert_eq!(onebot_requests_a[0].path, "/fetch_custom_face_detail");
    assert_eq!(onebot_requests_a[0].body["count"], 5);
    assert_eq!(
        onebot_requests_a[0].authorization,
        format!("Bearer {onebot_token_a}")
    );
    assert_eq!(
        onebot_requests_b.len(),
        1,
        "B must use only its selected OneBot"
    );
    assert_eq!(onebot_requests_b[0].method, "POST");
    assert_eq!(onebot_requests_b[0].path, "/fetch_custom_face_detail");
    assert_eq!(onebot_requests_b[0].body["count"], 1);
    assert_eq!(
        onebot_requests_b[0].authorization,
        format!("Bearer {onebot_token_b}")
    );

    for (listener, label) in [
        (&legacy_onebot_a, "A legacy OneBot decoy"),
        (&legacy_onebot_b, "B legacy OneBot decoy"),
    ] {
        match listener.accept() {
            Ok((_stream, peer)) => panic!("unexpected request reached {label} from {peer}"),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(10035) => {}
            Err(error) => panic!("{label} listener failed unexpectedly: {error}"),
        }
    }

    let persisted_a = fs::read(&stickers_a).expect("read synchronized A sticker state");
    let persisted_b = fs::read(&stickers_b).expect("read synchronized B sticker state");
    assert!(String::from_utf8_lossy(&persisted_a).contains("alpha-sticker"));
    assert!(!String::from_utf8_lossy(&persisted_a).contains("beta-sticker"));
    assert!(String::from_utf8_lossy(&persisted_b).contains("beta-sticker"));
    assert!(!String::from_utf8_lossy(&persisted_b).contains("alpha-sticker"));
}
#[test]
fn backup_restore_preview_requires_admin_and_confirmation_without_stopping_the_console() {
    let workspace = FixtureWorkspace::new("restore-preview");
    let root = workspace.root("runtime");
    let install = workspace.root("protected-install");
    let token = "restore-preview-admin";
    let agent = "restore-preview-agent";
    let home = prepare_runtime_root(&root, token, agent, "http://127.0.0.1:1", "http://127.0.0.1:1", None, None);
    initialize_memory_fixture(&root);
    let config_file = root.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("ownerQQ");
    fs::write(&config_file, serde_json::to_vec(&config).unwrap()).unwrap();
    let (port, _) = reserve_distinct_ports();
    let console = ConsoleProcess::start_with_install_root(&root, port, &home, Some(&install));
    let started = expect_json(http_request(port, "POST", "/api/tasks/backup/run", Some(token), None, Some(&json!({"requestId":"restore-fixture-backup"}))).unwrap(), 202, "create restore fixture");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = expect_json(http_request(port, "GET", &format!("/api/tasks/runs/{}", started["runId"].as_str().unwrap()), Some(token), None, None).unwrap(), 200, "fixture backup status");
        if result["run"]["state"] == "succeeded" { break; }
        assert!(Instant::now() < deadline, "backup did not succeed: {result}");
        thread::sleep(Duration::from_millis(20));
    }
    let listed = expect_json(http_request(port, "GET", "/api/backups", Some(token), None, None).unwrap(), 200, "restorable archives");
    let name = listed["backups"][0]["name"].as_str().unwrap();
    let body = json!({"archive":name,"memoryOnly":true});
    let call = |path: &str, token, agent, body: &Value| http_request(port, "POST", path, token, agent, Some(body)).unwrap();
    assert_eq!(call("/api/backups/restore/preview", None, None, &body).status, 401);
    assert_eq!(call("/api/backups/restore/preview", Some(token), Some(agent), &body).status, 403);
    let p = expect_json(call("/api/backups/restore/preview", Some(token), None, &body), 200, "restore preview");
    assert_eq!(p["preview"]["replaces"], json!(["data/memory.db"]));
    assert!(p["preview"]["contents"].as_array().unwrap().iter().any(|p| p == "config.json"));
    assert!(p["preview"]["time"].is_i64());
    assert_eq!(call("/api/backups/restore", Some(token), None, &body).status, 400);
    let bad = json!({"archive":"../chosen.zip","memoryOnly":true});
    assert_eq!(call("/api/backups/restore/preview", Some(token), None, &bad).status, 400);
    let mut confirmed = body.clone();
    confirmed["confirmation"] = p["preview"]["confirmation"].clone();
    fs::create_dir_all(root.join("data/locks")).unwrap();
    let gate = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join("data/locks/update-restore.lock")).unwrap();
    gate.lock().unwrap();
    assert_eq!(call("/api/backups/restore", Some(token), None, &confirmed).status, 409);
    let audit = fs::read_to_string(root.join("data/audit/audit.jsonl")).unwrap_or_default();
    let restore_audits: Vec<Value> = audit.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["action"] == "backup.restore").collect();
    assert!(restore_audits.iter().any(|event| event["by"] == "console:administrator"
        && event["archive"] == json!(root.join("backups").join(name))
        && event["scope"] == "memory" && event["result"] == "failed"), "busy console restore audit: {audit}");
    drop(gate);
    fs::write(root.join("backups").join(name), b"broken").unwrap();
    assert_eq!(call("/api/backups/restore/preview", Some(token), None, &body).status, 400);
    assert!(!root.join("data/lifecycle/desired-state.json").exists());
    assert_eq!(http_request(port, "GET", "/api/health", None, None, None).unwrap().status, 200);
    let page = http_request(port, "GET", "/tasks", Some(token), None, None).unwrap();
    assert!(page.body.contains("/api/backups/restore/preview") && page.body.contains("restore-scope"));
    drop(console);
}
