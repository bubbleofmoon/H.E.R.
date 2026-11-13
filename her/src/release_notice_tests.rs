//! Release worker/outbox seam with process-isolated budgets and a loopback OneBot.
use super::*;
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Fixture {
    root: PathBuf,
    hold: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<String>>>,
    server: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(queue: bool) -> Self {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-release-notice-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )));
        for dir in ["config", "data", "resources"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let feed = format!("{url}/unused-feed");
        fs::write(
            root.join("config/config.json"),
            json!({
                "consoleToken":"fixture-token", "ownerQQ":70001,
                "update":{"policy":"notify","feedUrl":feed}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            root.join("dependencies.json"),
            json!({
                "dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:1", "onebot":url
            })
            .to_string(),
        )
        .unwrap();
        fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        fs::write(
            root.join("data/releases.json"),
            json!({
                // This outbox seam starts from a post-authentication discovery cache.
                "version":3,"trust":crate::update::authority(&crate::install_root::InstallRoot::configured().unwrap()).unwrap().stamp,
                "feedUrl":feed,"lastCheckedAt":crate::parity_clock::now_ms(),
                "available":{"tag":"v1.2.3","assets":[]},"notified":[],"error":null
            })
            .to_string(),
        )
        .unwrap();
        let hold = Arc::new(AtomicBool::new(queue));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (held, done, received, base) = (hold.clone(), stop.clone(), seen.clone(), root.clone());
        let server = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = socket.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                received
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&bytes).into_owned());
                fs::write(base.join("request-observed"), b"ready").unwrap();
                while held.load(Ordering::SeqCst) && !done.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
                let body = r#"{"status":"ok","retcode":0,"data":{"message_id":1}}"#;
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            root,
            hold,
            stop,
            seen,
            server: Some(server),
        }
    }
    fn child(&self, phase: &str) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let child = command
            .args([
                "--exact",
                "admin_alert::release_notice_tests::notice_worker_child",
                "--nocapture",
            ])
            .env("HER_13_NOTICE_ROOT", &self.root)
            .env("HER_13_NOTICE_PHASE", phase)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        println!(
            "notice fixture phase={phase} pid={} launched_at={:?}",
            child.id(),
            SystemTime::now()
        );
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{phase}: {output:?}");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.hold.store(false, Ordering::SeqCst);
        self.server.take().unwrap().join().unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "release notice fixture timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn retry_after_restart(queue: bool) {
    let fixture = Fixture::new(queue);
    fixture.child(if queue { "queue" } else { "hourly" });
    fixture.hold.store(false, Ordering::SeqCst);
    fixture.child("admit");
    fixture.child("dedup");
    let seen = fixture.seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|request| request.contains("v1.2.3"))
            .count(),
        1,
        "pending notice is delivered exactly once across restarts: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|request| request.starts_with("POST /send_private_msg ")),
        "retry must use durable availability without a new feed request: {seen:?}"
    );
}
#[test]
fn full_queue_keeps_release_notice_pending_across_restart() {
    retry_after_restart(true);
}
#[test]
fn hourly_budget_keeps_release_notice_pending_across_restart() {
    retry_after_restart(false);
}

#[test]
fn notice_worker_child() {
    let Some(root) = std::env::var_os("HER_13_NOTICE_ROOT").map(PathBuf::from) else {
        return;
    };
    let phase = std::env::var("HER_13_NOTICE_PHASE").unwrap();
    let context = RuntimeContext::from_root(&root).unwrap();
    let cfg = context.load_config().unwrap();
    if phase == "hourly" {
        let mut limiter = notice_limiter().lock().unwrap();
        for n in 0..HOURLY_CAP {
            assert!(limiter
                .check(
                    "fixture",
                    &format!("fill-{n}"),
                    crate::parity_clock::now_ms()
                )
                .is_send());
        }
    } else if phase == "queue" {
        assert!(send_private(
            &context,
            &cfg,
            70001,
            "filler".into(),
            "fill".into()
        ));
        wait(|| root.join("request-observed").is_file());
        for _ in 0..32 {
            assert!(send_private(
                &context,
                &cfg,
                70001,
                "filler".into(),
                "fill".into()
            ));
        }
    }
    crate::release::start(context.clone());
    let state = || -> Value {
        serde_json::from_slice(&fs::read(root.join("data/releases.json")).unwrap()).unwrap()
    };
    if phase == "hourly" || phase == "queue" {
        let reason = if phase == "hourly" {
            "hourly cap of 10 reached"
        } else {
            "the alert queue is full"
        };
        wait(|| {
            fs::read_to_string(root.join("data/run-record.jsonl"))
                .unwrap_or_default()
                .lines()
                .any(|line| line.contains("owner-alert-release-available") && line.contains(reason))
        });
        // Observe the completed poll, after any definite-refusal claim release.
        let mut gate = None;
        wait(|| {
            gate = context
                .open_locked_file(
                    crate::runtime::RuntimeArea::Data,
                    "locks/release-check.lock",
                )
                .ok();
            gate.is_some()
        });
        assert_eq!(
            state()["notified"],
            json!([]),
            "definite non-admission must stay pending"
        );
        assert_eq!(state()["available"]["tag"], "v1.2.3");
    } else if phase == "admit" {
        wait(|| {
            state()["notified"] == json!(["v1.2.3"])
                && fs::read_to_string(root.join("data/run-record.jsonl"))
                    .unwrap_or_default()
                    .contains("sent to the administrator")
        });
    } else {
        thread::sleep(Duration::from_millis(1300));
        assert_eq!(state()["notified"], json!(["v1.2.3"]));
    }
}
