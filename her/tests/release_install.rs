//! Supervisor spec seam 1: paired binaries and a loopback release source.
use support::install_root;
mod support;
#[path = "../src/process_identity.rs"]
mod process_identity;
#[path = "support/release_signing.rs"]
mod release_signing;
#[path = "support/release_signature_tests.rs"]
mod signed_lists;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn windows_junction_command(link: &Path, target: &Path) -> String {
    format!(
        r#"mklink /J "{}" "{}""#,
        link.to_string_lossy().replace('/', "\\"),
        target.to_string_lossy().replace('/', "\\")
    )
}

#[cfg(windows)]
fn create_junction(link: &Path, target: &Path) {
    use std::os::windows::process::CommandExt;

    let out = Command::new("cmd.exe")
        .creation_flags(0x08000000)
        .args(["/d", "/c"])
        // Preserve the explicit cmd quotes without Rust's argument re-quoting.
        .raw_arg(windows_junction_command(link, target))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn junction_command_uses_quoted_native_windows_paths() {
    for (link, target, expected) in [
        (
            "C:/Temp/her-release/install/current",
            "C:/Temp/her-release/install/releases/v1.0.0",
            r#"mklink /J "C:\Temp\her-release\install\current" "C:\Temp\her-release\install\releases\v1.0.0""#,
        ),
        (
            "C:/Temp root/her-release/install/current",
            r"C:\Temp root\her-release/install/releases/v1.0.0",
            r#"mklink /J "C:\Temp root\her-release\install\current" "C:\Temp root\her-release\install\releases\v1.0.0""#,
        ),
        (
            r"C:\Temp\her-release\install\releases",
            r"C:\Temp\her-release\outside",
            r#"mklink /J "C:\Temp\her-release\install\releases" "C:\Temp\her-release\outside""#,
        ),
    ] {
        assert_eq!(
            windows_junction_command(link.as_ref(), target.as_ref()),
            expected
        );
    }
}

struct Fixture {
    root: PathBuf,
    url: String,
    requests: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    hold: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let root = install_root::development_root(std::env::temp_dir().join(format!(
                "her-release-13-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        for dir in ["runtime/config", "runtime/data", "runtime/resources"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        // Installation is authorized by the actual release image, independently of runtime.
        let installed_cli = root.join("install/releases/v0.0.0/her-cli.exe");
        fs::create_dir_all(installed_cli.parent().unwrap()).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_her-cli"), installed_cli).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let mut replies = HashMap::new();
        let mut assets = Vec::new();
        let mut sums = format!("# HER-RELEASE v1.2.3 1\n{}", release_signing::keys("primary"));
        for name in ["her.exe", "her-cli.exe", "herupdate.exe"] {
            let body = format!("synthetic {name}").into_bytes();
            sums.push_str(&format!("{:x}  {name}\n", Sha256::digest(&body)));
            replies.insert(format!("/{name}"), body);
            assets.push(json!({"name":name,"browser_download_url":format!("{url}/{name}")}));
        }
        replies.insert("/SHA256SUMS.minisig".into(), release_signing::sign(sums.as_bytes(), "primary"));
        replies.insert("/SHA256SUMS".into(), sums.into_bytes());
        assets
            .push(json!({"name":"SHA256SUMS","browser_download_url":format!("{url}/SHA256SUMS")}));
        assets.push(json!({"name":"SHA256SUMS.minisig","browser_download_url":format!("{url}/SHA256SUMS.minisig")}));
        replies.insert(
            "/feed".into(),
            serde_json::to_vec(
                &json!([{"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":assets}]),
            )
            .unwrap(),
        );
        let replies = Arc::new(Mutex::new(replies));
        let responses = replies.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let received = seen.clone();
        let hold = Arc::new(Mutex::new(None::<String>));
        let held = hold.clone();
        let server = thread::spawn(move || {
            let mut path_counts = HashMap::<String, usize>::new();
            while !done.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 8192];
                loop {
                    let n = socket.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    if request.len() > 65536 {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&request);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                count.fetch_add(1, Ordering::SeqCst);
                received.lock().unwrap().push(req.to_string());
                while !done.load(Ordering::SeqCst) && held.lock().unwrap().as_deref() == Some(path)
                {
                    thread::sleep(Duration::from_millis(5));
                }
                let attempt = path_counts.entry(path.to_string()).or_default();
                *attempt += 1;
                let replies = responses.lock().unwrap();
                let body = replies.get(&format!("{path}#{attempt}"))
                    .or_else(|| replies.get(path)).cloned();
                let status = if body.is_none() && path == "/SHA256SUMS.minisig" {
                    "404 Not Found"
                } else { "200 OK" };
                let body = body.unwrap_or_else(|| b"{}".to_vec());
                drop(replies);
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(&body);
            }
        });
        let f = Self {
            root,
            url,
            requests,
            seen,
            replies,
            hold,
            stop,
            server: Some(server),
        };
        // This is an established signed installation, even before current is selected.
        let old = f.root.join("install/releases/v0.0.0");
        fs::write(old.join("her.exe"), b"old resident").unwrap();
        fs::copy(env!("CARGO_BIN_EXE_herupdate"), old.join("herupdate.exe")).unwrap();
        f.sign_directory("v0.0.0", 1, "backup");
        f.config("notify");
        f
    }
    fn config(&self, policy: &str) {
        fs::write(self.root.join("runtime/config/config.json"),json!({"consoleToken":"fixture-release-token","update":{"policy":policy,"feedUrl":format!("{}/feed",self.url)}}).to_string()).unwrap();
        fs::write(self.root.join("runtime/data/social-v2.json"), "{}").unwrap();
        fs::write(self.root.join("runtime/dependencies.json"),r#"{"dsh":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","model":"http://127.0.0.1:1"}"#).unwrap();
    }
    fn feed(&self) -> Value {
        serde_json::from_slice(self.replies.lock().unwrap().get("/feed").unwrap()).unwrap()
    }
    fn sign(&self, key: &str) {
        let mut replies = self.replies.lock().unwrap();
        let signature = release_signing::sign(&replies["/SHA256SUMS"], key);
        replies.insert("/SHA256SUMS.minisig".into(), signature);
    }
    fn sign_directory(&self, tag: &str, generation: u64, key: &str) {
        let dir = self.root.join("install/releases").join(tag);
        let mut sums = format!("# HER-RELEASE {tag} {generation}\n{}", release_signing::keys("primary"));
        for name in ["her.exe", "her-cli.exe", "herupdate.exe"] {
            sums.push_str(&format!("{:x}  {name}\n", Sha256::digest(fs::read(dir.join(name)).unwrap())));
        }
        fs::write(dir.join("SHA256SUMS.minisig"), release_signing::sign(sums.as_bytes(), key)).unwrap();
        fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    }
    fn select_current(&self, tag: &str) {
        let root = self.root.join("install");
        let next = root.join(".fixture-current-next");
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("releases").join(tag), &next).unwrap();
        #[cfg(windows)]
        create_junction(&next, &root.join("releases").join(tag));
        let current = root.join("current");
        #[cfg(windows)]
        if current.exists() { fs::remove_dir(&current).unwrap(); }
        fs::rename(next, current).unwrap();
    }
    fn set_feed(&self, value: Value) {
        self.replies
            .lock()
            .unwrap()
            .insert("/feed".into(), serde_json::to_vec(&value).unwrap());
    }
    fn command(&self, args: &[&str]) -> Command {
        let image = if args.first() == Some(&"supervise") {
            PathBuf::from(env!("CARGO_BIN_EXE_her"))
        } else {
            self.root.join("install/releases/v0.0.0/her-cli.exe")
        };
        let mut c = Command::new(image);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c.env_remove("HER_INSTALL_ROOT")
            .env_remove("HER_RUNTIME_ROOT")
            .env_remove("HER_PARITY_EPOCH_MS")
            .env("HER_TASK_SCHEDULER_FILE", self.root.join("scheduler.xml"))
            .args(args)
            .arg("--install-root")
            .arg(self.root.join("install"));
        c
    }
    fn run(&self, action: &str) -> Output {
        self.command(&[
            "release",
            action,
            "--runtime-root",
            self.root.join("runtime").to_str().unwrap(),
        ])
        .output()
        .unwrap()
    }
    fn count(&self, path: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.split_whitespace().nth(1) == Some(path))
            .count()
    }
}
fn cleanup_warning(message: std::fmt::Arguments<'_>) {
    // Even a closed stderr must not turn cleanup during unwinding into an abort.
    let _ = writeln!(std::io::stderr(), "release fixture cleanup: {message}");
}

#[cfg(feature = "parity-clock")]
fn cleanup_owned_process(
    child: &mut std::process::Child,
    identity: &process_identity::ProcessIdentity,
) {
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(error) => cleanup_warning(format_args!("poll {identity:?}: {error}")),
    }
    match process_identity::kill_verified(identity) {
        process_identity::KillOutcome::Killed | process_identity::KillOutcome::Gone => {}
        outcome => cleanup_warning(format_args!("terminate {identity:?}: {outcome:?}")),
    }
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < end => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                cleanup_warning(format_args!("{identity:?} did not exit within 5 seconds"));
                return;
            }
            Err(error) => {
                cleanup_warning(format_args!("reap {identity:?}: {error}"));
                return;
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            if server.join().is_err() {
                cleanup_warning(format_args!("release server thread panicked"));
            }
        }
        let current = self.root.join("install/current");
        if fs::symlink_metadata(&current).is_ok() {
            #[cfg(windows)]
            let result = fs::remove_dir(&current);
            #[cfg(unix)]
            let result = fs::remove_file(&current);
            if let Err(error) = result {
                cleanup_warning(format_args!("remove {}: {error}", current.display()));
            }
        }
        if let Err(error) = fs::remove_dir_all(&self.root) {
            cleanup_warning(format_args!("remove {}: {error}", self.root.display()));
        }
    }
}

// Focused platform fault driver. The portable_handover module below exercises
// seam 1 with real candidate CLI, retirement, supervisors and fake dependencies.
#[cfg(feature = "parity-clock")]
fn handover_fixture(trial: bool, healthy: bool) -> (Fixture, PathBuf) {
    handover_fixture_generation(trial, healthy, 1)
}
#[cfg(feature = "parity-clock")]
fn handover_fixture_generation(trial: bool, healthy: bool, generation: u64) -> (Fixture, PathBuf) {
    let f = Fixture::new();
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
        replies.insert("/SHA256SUMS".into(), sums.replace("v1.2.3 1", &format!("v1.2.3 {generation}")).into_bytes());
    }
    f.sign("primary");
    assert!(f.run("install").status.success());
    let old = f.root.join("install/releases/v0.0.0");
    fs::write(old.join("her.exe"), "old resident").unwrap();
    fs::copy(env!("CARGO_BIN_EXE_herupdate"), old.join("herupdate.exe")).unwrap();
    f.sign_directory("v0.0.0", 1, "backup");
    fs::copy(
        env!("CARGO_BIN_EXE_herupdate"),
        f.root.join("install/herupdate.exe"),
    )
    .unwrap();
    let platform = f.root.join("update-platform.json");
    fs::write(&platform, json!({"trial":trial,"healthy":healthy,"old":"gone","now":1000000,
        "current":"v0.0.0","lastGood":"v0.0.0","events":[],
        "dependencies":[{"pid":120,"start":31,"exe":"fake-snowluma"},{"pid":121,"start":32,"exe":"fake-dsh"}]}).to_string()).unwrap();
    (f, platform)
}

#[cfg(feature = "parity-clock")]
fn handover(f: &Fixture, platform: &PathBuf) -> Output {
    let mut command = Command::new(f.root.join("install/herupdate.exe"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .env_remove("HER_INSTALL_ROOT")
        .args([
            "--target",
            "v1.2.3",
            "--old-pid",
            "4242",
            "--old-start",
            "17",
            "--old-exe",
            f.root
                .join("install/releases/v0.0.0/her.exe")
                .to_str()
                .unwrap(),
        ])
        .arg("--install-root")
        .arg(f.root.join("install"));
    command
        .env("HER_TEST_UPDATE_PLATFORM", platform)
        .output()
        .unwrap()
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_recovers_interrupted_links_before_trial() {
    let (f, _) = handover_fixture(true, true);
    let install = f.root.join("install");
    for name in ["current", "last-good"] {
        let previous = install.join(format!(".{name}-previous"));
        let pending = install.join(format!(".{name}-next"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(install.join("releases/v0.0.0"), &previous).unwrap();
            std::os::unix::fs::symlink(install.join("releases/v1.2.3"), &pending).unwrap();
        }
        #[cfg(windows)]
        {
            create_junction(&previous, &install.join("releases/v0.0.0"));
            create_junction(&pending, &install.join("releases/v1.2.3"));
        }
    }
    let mut command = Command::new(install.join("herupdate.exe"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let out = command
        .env_remove("HER_TEST_UPDATE_PLATFORM")
        .env_remove("HER_INSTALL_ROOT")
        .args([
            "--target",
            "v1.2.3",
            "--old-pid",
            "4242",
            "--old-start",
            "17",
            "--old-exe",
        ])
        .arg(install.join("releases/v0.0.0/her.exe"))
        .arg("--install-root")
        .arg(&install)
        .output()
        .unwrap();
    // The synthetic candidate cannot execute a checker: recovery must precede
    // that clean refusal, rather than leaving the scheduled current path absent.
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(
        fs::canonicalize(install.join("current")).unwrap(),
        fs::canonicalize(install.join("releases/v0.0.0")).unwrap()
    );
    assert!(!install.join(".current-previous").exists());
    assert!(!install.join(".current-next").exists());
    assert_eq!(
        fs::canonicalize(install.join("last-good")).unwrap(),
        fs::canonicalize(install.join("releases/v0.0.0")).unwrap()
    );
    assert!(!install.join(".last-good-previous").exists());
    assert!(!install.join(".last-good-next").exists());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("current release unavailable"),
        "{out:?}"
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_success_checks_trial_before_switch_and_preserves_dependencies() {
    let (f, platform) = handover_fixture(true, true);
    let dependencies = handover_dependencies(&platform);
    let before: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    let out = handover(&f, &platform);
    assert!(out.status.success(), "{out:?}");
    let after: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(after["current"], "v1.2.3");
    assert_eq!(after["lastGood"], "v1.2.3");
    assert_eq!(after["dependencies"], before["dependencies"]);
    for dependency in &dependencies {
        dependency.assert_same_instance();
    }
    assert_eq!(
        after["events"],
        json!([
            "trial:v1.2.3",
            "wait:4242:17",
            "switch:v1.2.3",
            "start:v1.2.3",
            "health:120000",
            "last-good:v1.2.3"
        ])
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_trial_failure_preserves_current_and_last_good() {
    let (f, platform) = handover_fixture(false, true);
    let out = handover(&f, &platform);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let after: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(after["current"], "v0.0.0");
    assert_eq!(after["lastGood"], "v0.0.0");
    assert_eq!(after["events"], json!(["trial:v1.2.3"]));
}

#[test]
fn handover_helper_outside_install_root_refuses_before_creating_installation() {
    let f = Fixture::new();
    let absent = f.root.join("absent-install");
    let mut command = Command::new(env!("CARGO_BIN_EXE_herupdate"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let out = command
        .env_remove("HER_INSTALL_ROOT")
        .args([
            "--target",
            "v1.2.3",
            "--old-pid",
            "4242",
            "--old-start",
            "17",
            "--old-exe",
        ])
        .arg(absent.join("releases/v0.0.0/her.exe"))
        .arg("--install-root")
        .arg(&absent)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        !absent.exists(),
        "authorization must precede every install-root mutation"
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_post_switch_failure_rolls_back_and_records_alert_without_advancing_last_good() {
    let (f, platform) = handover_fixture(true, false);
    let dependencies = handover_dependencies(&platform);
    let before: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    let out = handover(&f, &platform);
    assert_eq!(
        out.status.code(),
        Some(2),
        "failed update stays visible: {out:?}"
    );
    let after: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    assert_eq!(after["current"], "v0.0.0", "must roll back");
    assert_eq!(after["lastGood"], "v0.0.0");
    assert_eq!(after["dependencies"], before["dependencies"]);
    for dependency in &dependencies {
        dependency.assert_same_instance();
    }
    let events = after["events"].as_array().unwrap();
    assert!(events.contains(&json!("health:120000")));
    assert!(events.contains(&json!("stop:v1.2.3")));
    assert_eq!(events.last(), Some(&json!("start:v0.0.0")));
    let alert: Value =
        serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
            .unwrap();
    assert_eq!(alert["failed"], "v1.2.3");
    assert_eq!(alert["restored"], "v0.0.0");
    assert!(alert["reason"].as_str().unwrap().contains("120"));
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_three_unhealthy_installed_starts_within_ten_minutes_fall_back() {
    let (f, platform) = handover_fixture(true, false);
    let target = handover_executable_release(&f);
    let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    state["current"] = "v1.2.3".into();
    for (attempt, now) in [1_000_000, 1_240_000, 1_540_000].into_iter().enumerate() {
        state["now"] = now.into();
        fs::write(&platform, state.to_string()).unwrap();
        let mut command = Command::new(target.join("her.exe"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let out = command
            .args(["supervise", "--runtime-root"])
            .arg(f.root.join("install/runtime"))
            .arg("--install-root")
            .arg(f.root.join("install"))
            .env_remove("HER_INSTALL_ROOT")
            .env("HER_TEST_UPDATE_PLATFORM", &platform)
            .output()
            .unwrap();
        // Missing runtime config deliberately prevents all service startup.
        assert_eq!(
            out.status.code(),
            Some(if attempt == 2 { 3 } else { 2 }),
            "{out:?}"
        );
    }
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let state = fs::read(&platform)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null);
        if state["current"] == "v0.0.0" {
            break;
        }
        assert!(
            Instant::now() < end,
            "crash loop did not fall back: {state}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let alert: Value =
        serde_json::from_slice(&fs::read(f.root.join("install/update-alert.json")).unwrap())
            .unwrap();
    assert!(alert["reason"].as_str().unwrap().contains("three starts"));
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_pruning_keeps_newest_three_and_both_protected_older_releases() {
    let (f, platform) = handover_fixture(true, true);
    let releases = f.root.join("install/releases");
    for tag in ["v0.0.1", "v2.0.0", "v3.0.0", "v4.0.0", "v10.0.0", "notes"] {
        fs::create_dir(releases.join(tag)).unwrap();
        fs::write(releases.join(tag).join("sentinel"), tag).unwrap();
    }
    fs::write(releases.join("v99.0.0"), "not a release directory").unwrap();
    #[cfg(unix)]
    {
        let outside = f.root.join("outside-release");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "outside").unwrap();
        std::os::unix::fs::symlink(&outside, releases.join("v99.9.9")).unwrap();
    }
    let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    state["lastGood"] = "v0.0.1".into();
    fs::write(&platform, state.to_string()).unwrap();
    let mut command = Command::new(f.root.join("install/herupdate.exe"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let out = command
        .env_remove("HER_INSTALL_ROOT")
        .env("HER_TEST_UPDATE_PLATFORM", &platform)
        .args(["--prune", "--install-root"])
        .arg(f.root.join("install"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    for tag in [
        "v0.0.0", "v0.0.1", "v3.0.0", "v4.0.0", "v10.0.0", "notes", "v99.0.0",
    ] {
        assert!(releases.join(tag).exists(), "must keep {tag}");
    }
    for tag in ["v1.2.3", "v2.0.0"] {
        assert!(!releases.join(tag).exists(), "must prune {tag}");
    }
    #[cfg(unix)]
    assert_eq!(
        fs::read_to_string(f.root.join("outside-release/sentinel")).unwrap(),
        "outside"
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_success_prunes_automatically_with_new_current_protected() {
    let (f, platform) = handover_fixture(true, true);
    let releases = f.root.join("install/releases");
    for tag in ["v2.0.0", "v3.0.0", "v4.0.0", "v10.0.0"] {
        fs::create_dir(releases.join(tag)).unwrap();
    }
    let out = handover(&f, &platform);
    assert!(out.status.success(), "{out:?}");
    for tag in ["v1.2.3", "v3.0.0", "v4.0.0", "v10.0.0"] {
        assert!(releases.join(tag).is_dir(), "must keep {tag}");
    }
    for tag in ["v0.0.0", "v2.0.0"] {
        assert!(!releases.join(tag).exists(), "must prune {tag}");
    }
}
#[test]
fn unsigned_release_is_refused_before_any_image_download_or_staging() {
    let f = Fixture::new();
    let mut feed = f.feed();
    feed[0]["assets"].as_array_mut().unwrap()
        .retain(|asset| asset["name"] != "SHA256SUMS.minisig");
    f.set_feed(feed);
    f.replies.lock().unwrap().remove("/SHA256SUMS.minisig");
    for action in ["check", "install"] {
        let out = f.run(action);
        assert_eq!(out.status.code(), Some(2), "{action}: {out:?}");
    }
    for image in ["her.exe", "her-cli.exe", "herupdate.exe"] {
        assert_eq!(f.count(&format!("/{image}")), 0);
    }
    assert!(!f.root.join("install/releases/v1.2.3").exists());
    assert!(fs::read_dir(f.root.join("install/releases"))
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".staging-")));
}

#[test]
fn check_uses_configured_feed_and_reports_new_release_without_installing() {
    let f = Fixture::new();
    let out = f.run("check");
    assert!(out.status.success(), "{out:?}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["available"]["tag"], "v1.2.3");
    assert!(f.requests.load(Ordering::SeqCst) > 0);
    assert!(!f.root.join("install/releases/v1.2.3").exists());
}

#[test]
fn incomplete_or_nonstable_releases_are_not_offered() {
    for case in ["draft", "prerelease", "missing sums"] {
        let f = Fixture::new();
        let mut feed = f.feed();
        match case {
            "draft" => feed[0]["draft"] = true.into(),
            "prerelease" => feed[0]["prerelease"] = true.into(),
            _ => feed[0]["assets"]
                .as_array_mut()
                .unwrap()
                .retain(|a| a["name"] != "SHA256SUMS"),
        }
        f.set_feed(feed);
        let out = f.run("check");
        assert!(out.status.success(), "{case}: {out:?}");
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(result["available"].is_null(), "{case}: {result}");
        assert!(!f.root.join("install/releases/v1.2.3").exists());
    }
}

#[test]
fn verified_release_installs_beside_old_images_and_failures_publish_nothing() {
    for case in [
        "valid",
        "mismatch",
        "malformed sums",
        "missing checksum",
        "draft",
        "prerelease",
    ] {
        let f = Fixture::new();
        let old = f.root.join("install/releases/v1.0.0");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("her.exe"), b"old image in use").unwrap();
        fs::write(old.join("her-cli.exe"), b"old CLI").unwrap();
        fs::write(old.join("herupdate.exe"), b"old helper").unwrap();
        f.sign_directory("v1.0.0", 1, "primary");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&old, f.root.join("install/current")).unwrap();
        #[cfg(windows)]
        create_junction(&f.root.join("install/current"), &old);
        fs::write(f.root.join("install/last-good.json"), b"last-good sentinel").unwrap();
        let config = fs::read(f.root.join("runtime/config/config.json")).unwrap();
        match case {
            "mismatch" => {
                f.replies
                    .lock()
                    .unwrap()
                    .insert("/her-cli.exe".into(), b"tampered".to_vec());
            }
            "malformed sums" => {
                f.replies
                    .lock()
                    .unwrap()
                    .insert("/SHA256SUMS".into(), format!("# HER-RELEASE v1.2.3 1\n{}not a checksum file", release_signing::keys("primary")).into_bytes());
            }
            "missing checksum" => {
                let mut replies = f.replies.lock().unwrap();
                let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
                replies.insert(
                    "/SHA256SUMS".into(),
                    sums.lines()
                        .filter(|line| !line.ends_with("herupdate.exe"))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into_bytes(),
                );
            }
            "draft" | "prerelease" => {
                let mut feed = f.feed();
                feed[0][case] = true.into();
                f.set_feed(feed);
            }
            _ => {}
        }
        if matches!(case, "malformed sums" | "missing checksum") {
            // Keep these checksum-parser tests authenticated, rather than failing earlier.
            f.sign("primary");
        }
        let out = f.run("install");
        let target = f.root.join("install/releases/v1.2.3");
        if case == "valid" {
            assert!(out.status.success(), "{out:?}");
            let value: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(value["installed"]["tag"], "v1.2.3");
            for name in ["her.exe", "her-cli.exe", "herupdate.exe", "SHA256SUMS", "SHA256SUMS.minisig"] {
                assert_eq!(
                    fs::read(target.join(name)).unwrap(),
                    f.replies.lock().unwrap()[&format!("/{name}")]
                );
            }
            let again = f.run("install");
            assert!(
                !again.status.success(),
                "existing release must not be overwritten: {again:?}"
            );
            assert_eq!(
                fs::read(target.join("her.exe")).unwrap(),
                b"synthetic her.exe"
            );
        } else {
            assert!(!out.status.success(), "{case}: {out:?}");
            assert!(!target.exists(), "{case} was published");
        }
        assert_eq!(fs::read(old.join("her.exe")).unwrap(), b"old image in use");
        assert_eq!(
            fs::read(f.root.join("install/last-good.json")).unwrap(),
            b"last-good sentinel"
        );
        assert_eq!(
            fs::read(f.root.join("runtime/config/config.json")).unwrap(),
            config
        );
        assert_eq!(
            fs::canonicalize(f.root.join("install/current")).unwrap(),
            fs::canonicalize(&old).unwrap()
        );
        assert!(!f.root.join("install/herupdate.exe").exists());
        assert!(fs::read_dir(f.root.join("install/releases"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".staging-")));
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            cleanup_warning(format_args!("kill console child {}: {error}", self.0.id()));
        }
        if let Err(error) = self.0.wait() {
            cleanup_warning(format_args!("reap console child {}: {error}", self.0.id()));
        }
    }
}
impl Fixture {
    fn edit_config(&self, edit: impl FnOnce(&mut Value)) {
        let path = self.root.join("runtime/config/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut config);
        fs::write(path, config.to_string()).unwrap();
    }
    fn console(&self) -> (Child, u16) {
        fs::create_dir_all(self.root.join("runtime/resources/public")).unwrap();
        fs::write(
            self.root.join("runtime/resources/public/console.html"),
            "<!doctype html><body>fixture console</body>",
        )
        .unwrap();
        let (child, port) = crate::support::ports::start_console(Duration::from_secs(15), |port| {
            self.command(&[
                "--console-serve",
                "--runtime-root",
                self.root.join("runtime").to_str().unwrap(),
                "--console-port",
                &port.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(fs::File::create(self.root.join("console.stderr")).unwrap())
            .spawn()
            .unwrap()
        });
        let mut child = Child(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(self.root.join("console.stderr")).unwrap()
            );
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return (child, port);
            }
            assert!(Instant::now() < deadline, "console readiness timeout");
            thread::sleep(Duration::from_millis(30));
        }
    }
    fn supervisor(&self, epoch: i64) -> Child {
        self.edit_config(|cfg| {
            cfg["ownerQQ"] = 70001.into();
            cfg["tasks"] = json!({});
            for name in [
                "watchdog",
                "start-all",
                "backup",
                "memory-digest",
                "memory-digest-check",
                "upstream-check",
            ] {
                cfg["tasks"][name] = json!({"enabled":false});
            }
        });
        fs::write(
            self.root.join("runtime/dependencies.json"),
            json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":self.url})
                .to_string(),
        )
        .unwrap();
        self.replies.lock().unwrap().insert(
            "/send_private_msg".into(),
            br#"{"status":"ok","retcode":0,"data":{"message_id":1}}"#.to_vec(),
        );
        let task = self.command(&["install"]).output().unwrap();
        assert!(task.status.success(), "{task:?}");
        Child(
            self.command(&[
                "supervise",
                "--runtime-root",
                self.root.join("runtime").to_str().unwrap(),
            ])
            .env("HER_PARITY_EPOCH_MS", epoch.to_string())
            .stdout(Stdio::null())
            .stderr(fs::File::create(self.root.join("supervisor.stderr")).unwrap())
            .spawn()
            .unwrap(),
        )
    }
    fn wait_count(&self, child: &mut Child, path: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.count(path) < count {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(self.root.join("supervisor.stderr")).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "waiting for {count} {path}, observed {}",
                self.count(path)
            );
            thread::sleep(Duration::from_millis(30));
        }
    }
}
fn http(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    authorized: bool,
    agent: bool,
) -> (u16, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let token = if authorized {
        "x-console-token: fixture-release-token\r\n"
    } else {
        ""
    };
    let agent = if agent { "x-agent-call: true\r\n" } else { "" };
    write!(socket,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\n{token}{agent}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).unwrap();
    let response = String::from_utf8(bytes).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body.into(),
    )
}
#[test]
fn console_policy_defaults_to_notify_and_only_administrators_can_save_it() {
    let f = Fixture::new();
    f.edit_config(|cfg| {
        cfg.as_object_mut().unwrap().remove("update");
        cfg["sentinel"] = json!({"keep":true});
    });
    let (_console, port) = f.console();
    let (status, body) = http(port, "GET", "/api/releases/settings", "", true, false);
    assert_eq!(status, 200, "{body}");
    let settings: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(settings["settings"]["policy"], "notify");
    assert_eq!(
        settings["settings"]["feedUrl"],
        "https://api.github.com/repos/bubbleofmoon/H.E.R./releases"
    );
    assert_eq!(
        f.count("/feed"),
        0,
        "settings reads must not check the feed"
    );
    for policy in ["off", "auto", "notify"] {
        let request = json!({"policy":policy,"feedUrl":format!("{}/feed",f.url)}).to_string();
        let (status, body) = http(
            port,
            "POST",
            "/api/releases/settings",
            &request,
            true,
            false,
        );
        assert_eq!(status, 200, "{body}");
        let cfg: Value =
            serde_json::from_slice(&fs::read(f.root.join("runtime/config/config.json")).unwrap())
                .unwrap();
        assert_eq!(cfg["update"]["policy"], policy);
        assert_eq!(cfg["sentinel"], json!({"keep":true}));
    }
    let before = fs::read(f.root.join("runtime/config/config.json")).unwrap();
    for (body, authorized, agent, expected) in [
        (r#"{"policy":"invalid"}"#, true, false, 400),
        (r#"{"policy":"off"}"#, false, false, 401),
        (r#"{"policy":"off"}"#, true, true, 403),
    ] {
        let (status, response) = http(
            port,
            "POST",
            "/api/releases/settings",
            body,
            authorized,
            agent,
        );
        assert_eq!(status, expected, "{response}");
        assert_eq!(
            fs::read(f.root.join("runtime/config/config.json")).unwrap(),
            before
        );
    }
    assert!(http(port, "GET", "/tasks", "", true, false)
        .1
        .contains("id=\"update-settings\""));
    assert!(http(port, "GET", "/", "", true, false)
        .1
        .contains("her-update-banner"));
    let tasks = http(port,"GET","/tasks","",true,false).1;
    assert!(tasks.contains("id=\"approve-update\""),"tasks page needs an approval button");
    assert!(tasks.contains("/api/releases/approve"));
    assert!(http(port,"GET","/","",true,false).1.contains("/api/releases/approve"),"the app banner must approve too");
}
#[cfg(feature = "parity-clock")]
#[test]
fn supervisor_checks_every_six_hours_and_notifies_once_across_restarts() {
    let f = Fixture::new();
    let start = 1_800_000_000_000i64;
    let mut first = f.supervisor(start);
    f.wait_count(&mut first, "/feed", 1);
    f.wait_count(&mut first, "/send_private_msg", 1);
    let (_console, port) = f.console();
    let (status, body) = http(port, "GET", "/api/releases", "", true, false);
    assert_eq!(status, 200, "{body}");
    let view: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["available"]["tag"], "v1.2.3");
    assert!(
        !f.root.join("install/releases/v1.2.3").exists(),
        "notify must not install"
    );
    drop(first);
    let mut early = f.supervisor(start + 6 * 60 * 60 * 1000 - 30_000);
    thread::sleep(Duration::from_millis(1800));
    assert!(early.0.try_wait().unwrap().is_none());
    assert_eq!(f.count("/feed"), 1);
    assert_eq!(f.count("/send_private_msg"), 1);
    drop(early);
    let mut due = f.supervisor(start + 6 * 60 * 60 * 1000 + 30_000);
    f.wait_count(&mut due, "/feed", 2);
    thread::sleep(Duration::from_millis(250));
    assert_eq!(f.count("/send_private_msg"), 1);
    drop(due);
    f.config("off");
    let mut off = f.supervisor(start + 12 * 60 * 60 * 1000 + 60_000);
    thread::sleep(Duration::from_millis(1800));
    assert!(off.0.try_wait().unwrap().is_none());
    assert_eq!(f.count("/feed"), 2);
    assert_eq!(f.count("/send_private_msg"), 1);
    assert_eq!(f.run("check").status.code(), Some(0));
    assert_eq!(f.run("install").status.code(), Some(2));
    assert_eq!(f.count("/feed"), 2, "off CLI must never check");
    let pid = off.0.id();
    let (code, body) = http(
        port,
        "POST",
        "/api/releases/settings",
        r#"{"policy":"notify"}"#,
        true,
        false,
    );
    assert_eq!(code, 200, "{body}");
    f.wait_count(&mut off, "/feed", 3);
    assert_eq!(off.0.id(), pid);
    assert!(off.0.try_wait().unwrap().is_none());
    drop(off);
    let mut feed = f.feed();
    feed[0]["tag_name"] = "v1.2.4".into();
    f.set_feed(feed);
    {
        let mut replies = f.replies.lock().unwrap();
        let sums = String::from_utf8(replies["/SHA256SUMS"].clone()).unwrap();
        replies.insert("/SHA256SUMS".into(), sums.replace("v1.2.3 1", "v1.2.4 1").into_bytes());
    }
    f.sign("primary");
    let mut next = f.supervisor(start + 18 * 60 * 60 * 1000 + 90_000);
    f.wait_count(&mut next, "/feed", 4);
    f.wait_count(&mut next, "/send_private_msg", 2);
    thread::sleep(Duration::from_millis(250));
    assert_eq!(f.count("/send_private_msg"), 2);
    let messages = f.seen.lock().unwrap();
    for tag in ["v1.2.3", "v1.2.4"] {
        assert_eq!(
            messages
                .iter()
                .filter(
                    |request| request.split_whitespace().nth(1) == Some("/send_private_msg")
                        && request.contains(tag)
                )
                .count(),
            1,
            "one private notice for {tag}"
        );
    }
}

#[cfg(windows)]
#[test]
fn native_install_leaves_a_running_old_image_locked_and_untouched() {
    use std::os::windows::process::CommandExt;
    let f = Fixture::new();
    let old = f.root.join("install/releases/v1.0.0/her-cli.exe");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_her-cli"), &old).unwrap();
    drop(fs::OpenOptions::new().write(true).open(&old).unwrap());
    let before = fs::read(&old).unwrap();
    let (child, port) = crate::support::ports::start_console(Duration::from_secs(15), |port| {
        Command::new(&old)
            .creation_flags(0x08000000)
            .env_remove("HER_INSTALL_ROOT")
            .env_remove("HER_RUNTIME_ROOT")
            .args(["--console-serve", "--runtime-root"])
            .arg(f.root.join("runtime"))
            .args(["--console-port", &port.to_string(), "--install-root"])
            .arg(f.root.join("install"))
            .stdout(Stdio::null())
            .stderr(fs::File::create(f.root.join("native.stderr")).unwrap())
            .spawn()
            .unwrap()
    });
    let mut resident = Child(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            resident.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(f.root.join("native.stderr")).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "native old image readiness timeout"
        );
        thread::sleep(Duration::from_millis(30));
    }
    let locked = fs::OpenOptions::new().write(true).open(&old).unwrap_err();
    assert!(
        matches!(locked.raw_os_error(), Some(5 | 32 | 33)),
        "{locked:?}"
    );
    let out = f.run("install");
    assert!(out.status.success(), "{out:?}");
    assert!(resident.0.try_wait().unwrap().is_none());
    assert_eq!(fs::read(&old).unwrap(), before);
    assert_eq!(
        fs::read(f.root.join("install/releases/v1.2.3/her.exe")).unwrap(),
        b"synthetic her.exe"
    );
}

#[test]
fn killed_installer_staging_is_recovered_without_removing_other_entries() {
    let f = Fixture::new();
    let releases = f.root.join("install/releases");
    let old = releases.join("v1.0.0");
    fs::create_dir(&old).unwrap();
    fs::write(old.join("her.exe"), b"current image").unwrap();
    fs::write(old.join("her-cli.exe"), b"current CLI").unwrap();
    fs::write(old.join("herupdate.exe"), b"current helper").unwrap();
    f.sign_directory("v1.0.0", 1, "primary");
    let last_good = releases.join("v0.9.0");
    fs::create_dir(&last_good).unwrap();
    fs::write(last_good.join("her.exe"), b"last-good image").unwrap();
    fs::write(
        f.root.join("install/last-good.json"),
        br#"{"tag":"v0.9.0"}"#,
    )
    .unwrap();
    for name in [
        ".staging-unrelated",
        ".staging-123-456-0",
        ".staging-her-release-v1-not-owned",
    ] {
        fs::create_dir(releases.join(name)).unwrap();
        fs::write(releases.join(name).join("sentinel"), b"unrelated").unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&old, f.root.join("install/current")).unwrap();
    #[cfg(windows)]
    create_junction(&f.root.join("install/current"), &old);
    *f.hold.lock().unwrap() = Some("/her-cli.exe".into());
    let mut installer = Child(
        f.command(&[
            "release",
            "install",
            "--runtime-root",
            f.root.join("runtime").to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    println!(
        "fixture installer pid={} launched_at={:?}",
        installer.0.id(),
        SystemTime::now()
    );
    f.wait_count(&mut installer, "/her-cli.exe", 1);
    let staged = fs::read_dir(&releases)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".staging-")
                && path.join("her.exe").is_file()
        })
        .expect("installer staged its first image");
    assert_eq!(
        fs::read(staged.join("her.exe")).unwrap(),
        b"synthetic her.exe"
    );
    installer.0.kill().unwrap();
    let status = installer.0.wait().unwrap();
    assert!(!status.success());
    assert!(staged.exists(), "killing the process bypasses destructors");
    *f.hold.lock().unwrap() = None;

    // Recovery must wait for the installation lock, even with an abandoned directory.
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(releases.join(".install.lock"))
        .unwrap();
    gate.lock().unwrap();
    assert!(!f.run("install").status.success());
    assert!(staged.exists());
    drop(gate);
    let out = f.run("install");
    assert!(out.status.success(), "{out:?}");
    assert!(
        !staged.exists(),
        "abandoned installer staging must be recovered"
    );
    for name in [
        ".staging-unrelated",
        ".staging-123-456-0",
        ".staging-her-release-v1-not-owned",
    ] {
        assert_eq!(
            fs::read(releases.join(name).join("sentinel")).unwrap(),
            b"unrelated"
        );
    }
    assert_eq!(fs::read(old.join("her.exe")).unwrap(), b"current image");
    assert_eq!(
        fs::read(last_good.join("her.exe")).unwrap(),
        b"last-good image"
    );
    assert_eq!(
        fs::read(f.root.join("install/last-good.json")).unwrap(),
        br#"{"tag":"v0.9.0"}"#
    );
    assert_eq!(
        fs::canonicalize(f.root.join("install/current")).unwrap(),
        fs::canonicalize(old).unwrap()
    );
    assert_eq!(
        fs::read(releases.join("v1.2.3/her.exe")).unwrap(),
        b"synthetic her.exe"
    );
}

#[test]
fn install_refuses_the_restore_gate_without_publishing() {
    let f = Fixture::new();
    // The configuration runtime is external. Coordination must still use this installation.
    let locks = f.root.join("install/runtime/data/locks");
    fs::create_dir_all(&locks).unwrap();
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(locks.join("update-restore.lock"))
        .unwrap();
    gate.lock().unwrap();
    let out = f.run("install");
    assert!(!out.status.success(), "{out:?}");
    assert!(!f.root.join("install/releases/v1.2.3").exists());
    drop(gate);
    let out = f.run("install");
    assert!(out.status.success(), "{out:?}");
}
#[test]
fn install_refuses_redirected_release_directories() {
    for relative in ["install", "install/releases"] {
        let f = Fixture::new();
        let redirected = f.root.join(relative);
        let outside = f.root.join("outside");
        // Keep the installed CLI available through the alias, and witness that the
        // redirected destination (including its existing image) remains untouched.
        let before = fs::read(f.root.join("install/releases/v0.0.0/her-cli.exe")).unwrap();
        fs::rename(&redirected, &outside).unwrap();
        let entries = fs::read_dir(&outside).unwrap().count();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &redirected).unwrap();
        #[cfg(windows)]
        create_junction(&redirected, &outside);
        let out = f.run("install");
        assert!(!out.status.success(), "{out:?}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), entries);
        assert_eq!(
            fs::read(f.root.join("install/releases/v0.0.0/her-cli.exe")).unwrap(),
            before
        );
        #[cfg(windows)]
        fs::remove_dir(&redirected).unwrap();
        #[cfg(unix)]
        fs::remove_file(&redirected).unwrap();
    }
}

#[test]
#[ignore = "owned child process for the handover fixture"]
fn handover_dependency_process_fixture() {
    assert_eq!(std::env::var("HER_TEST_HANDOVER_CHILD").as_deref(), Ok("1"));
    thread::sleep(Duration::from_secs(120));
}
#[cfg(feature = "parity-clock")]
struct HandoverDependency {
    child: std::process::Child,
    identity: process_identity::ProcessIdentity,
}
#[cfg(feature = "parity-clock")]
impl HandoverDependency {
    fn new() -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let child = command
            .args([
                "--exact",
                "handover_dependency_process_fixture",
                "--ignored",
            ])
            .env("HER_TEST_HANDOVER_CHILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let identity = process_identity::identity_of(child.id()).unwrap().unwrap();
        Self { child, identity }
    }
    fn assert_same_instance(&self) {
        assert_eq!(
            process_identity::identity_of(self.child.id()).unwrap(),
            Some(self.identity.clone())
        );
    }
}
#[cfg(feature = "parity-clock")]
impl Drop for HandoverDependency {
    fn drop(&mut self) {
        cleanup_owned_process(&mut self.child, &self.identity);
    }
}
#[cfg(feature = "parity-clock")]
fn handover_dependencies(platform: &PathBuf) -> [HandoverDependency; 2] {
    let children = [HandoverDependency::new(), HandoverDependency::new()];
    let mut state: Value = serde_json::from_slice(&fs::read(platform).unwrap()).unwrap();
    state["dependencies"] = serde_json::to_value(
        children
            .iter()
            .map(|child| &child.identity)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(platform, state.to_string()).unwrap();
    children
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_success_waits_out_a_brief_start_history_writer() {
    let (f, platform) = handover_fixture(true, true);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.join("install/runtime/data/locks/update-starts.lock"))
        .unwrap();
    lock.lock().unwrap();
    thread::scope(|scope| {
        let waiting = scope.spawn(|| handover(&f, &platform));
        thread::sleep(Duration::from_millis(200));
        drop(lock);
        let out = waiting.join().unwrap();
        assert!(
            out.status.success(),
            "healthy switch must survive a concurrent state writer: {out:?}"
        );
    });
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_passing_health_clears_unhealthy_start_history() {
    let (f, platform) = handover_fixture(true, true);
    let history = f.root.join("install/update-starts.json");
    fs::write(
        &history,
        json!({"v":1,"releases":{"v1.2.3":[900000,950000]}}).to_string(),
    )
    .unwrap();
    assert!(handover(&f, &platform).status.success());
    let saved: Value = serde_json::from_slice(&fs::read(history).unwrap()).unwrap();
    assert!(saved["releases"].get("v1.2.3").is_none());
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_unverified_old_exit_never_switches() {
    for observation in ["alive", "unknown"] {
        let (f, platform) = handover_fixture(true, true);
        let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        state["old"] = observation.into();
        fs::write(&platform, state.to_string()).unwrap();
        let out = handover(&f, &platform);
        assert_eq!(out.status.code(), Some(2));
        let state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
        assert_eq!(state["current"], "v0.0.0");
        assert_eq!(state["lastGood"], "v0.0.0");
        assert_eq!(state["events"], json!(["trial:v1.2.3", "wait:4242:17"]));
    }
}

#[cfg(feature = "parity-clock")]
fn handover_executable_release(f: &Fixture) -> PathBuf {
    let target = f.root.join("install/releases/v1.2.3");
    // A verified candidate retires its own owners through its CLI, so it must run too.
    for (name, source) in [
        ("her.exe", env!("CARGO_BIN_EXE_her")),
        ("her-cli.exe", env!("CARGO_BIN_EXE_her-cli")),
        ("herupdate.exe", env!("CARGO_BIN_EXE_herupdate")),
    ] {
        fs::remove_file(target.join(name)).unwrap();
        fs::hard_link(source, target.join(name)).unwrap();
    }
    let sums = ["her.exe", "her-cli.exe", "herupdate.exe"]
        .into_iter()
        .map(|name| {
            format!(
                "{:x}  {name}\n",
                Sha256::digest(fs::read(target.join(name)).unwrap())
            )
        })
        .collect::<String>();
    let sums = format!("# HER-RELEASE v1.2.3 1\n{}{sums}", release_signing::keys("primary"));
    fs::write(target.join("SHA256SUMS.minisig"), release_signing::sign(sums.as_bytes(), "primary")).unwrap();
    fs::write(target.join("SHA256SUMS"), sums).unwrap();
    target
}

#[cfg(feature = "parity-clock")]
#[test]
fn handover_starts_outside_ten_minutes_do_not_trigger_fallback() {
    let (f, platform) = handover_fixture(true, false);
    let target = handover_executable_release(&f);
    let mut state: Value = serde_json::from_slice(&fs::read(&platform).unwrap()).unwrap();
    state["current"] = "v1.2.3".into();
    for now in [1_000_000, 1_300_000, 1_600_001] {
        state["now"] = now.into();
        fs::write(&platform, state.to_string()).unwrap();
        let mut command = Command::new(target.join("her.exe"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let out = command
            .env_remove("HER_INSTALL_ROOT")
            .env("HER_TEST_UPDATE_PLATFORM", &platform)
            .args(["supervise", "--runtime-root"])
            .arg(f.root.join("install/runtime"))
            .arg("--install-root")
            .arg(f.root.join("install"))
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "expired starts must not cause fallback: {out:?}"
        );
    }
    let history: Value =
        serde_json::from_slice(&fs::read(f.root.join("install/update-starts.json")).unwrap())
            .unwrap();
    assert_eq!(history["releases"]["v1.2.3"], json!([1300000, 1600001]));
    let state: Value = serde_json::from_slice(&fs::read(platform).unwrap()).unwrap();
    assert_eq!(state["current"], "v1.2.3");
    assert_eq!(state["events"], json!([]));
}

#[cfg(feature = "parity-clock")]
#[path = "fixtures/release_handover.rs"]
mod portable_handover;
