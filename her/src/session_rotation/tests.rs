use super::*;
use crate::digest::{DigestModel, ModelError};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    ModelError,
    Refusal,
    Unparseable,
    Checkpoint,
}

/// The in-process digest model for these fixtures: each call is recorded, and the answer follows the fault.
struct FixtureModel {
    root: PathBuf,
    fault: Fault,
}

impl DigestModel for FixtureModel {
    fn complete(&self, _system: &str, _input: &str) -> Result<String, ModelError> {
        use std::fs::OpenOptions;
        let mut calls = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("model-called"))
            .unwrap();
        writeln!(calls, "call").unwrap();
        if self.fault == Fault::Checkpoint {
            std::fs::write(self.root.join("data/sessions.json"), "invalid-json").unwrap();
        }
        match self.fault {
            Fault::ModelError => Err(ModelError("backend")),
            Fault::Refusal => Ok("content_filter".into()),
            Fault::Unparseable => Ok("好的".into()),
            _ => Ok(r#"[{"subject":"爬山","content":"甲周六要去爬山"}]"#.into()),
        }
    }
}

struct Fixture {
    root: PathBuf,
    context: RuntimeContext,
    plan: Plan,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(fault: Fault) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "hf135-unit-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        for d in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"hf135-fixture-console"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("user/dsh/.credentials.yaml"),
            "secret: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            json!({"dsh":url,"model":url,"onebot":url}).to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("data/social-v2.json"),
            json!({"conversations":{"group:1":{"agentToken":"epoch","recentMessages":[
                {"seq":1,"sender":"甲","userId":"5001","text":"周六去爬山","isSelf":false},
                {"seq":2,"sender":"乙","userId":"5002","text":"我也去","isSelf":false}]}}}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("data/sessions.json"), json!({"sessions":{"group:1":"sid"},"briefings":{"group:1":{
            "sid":"sid","epoch":"epoch","format":1,"lastTurnSeq":5,"rotation":json!({})
        }}}).to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        crate::digest::install_test_model(
            context.runtime_root().unwrap(),
            Arc::new(FixtureModel { root: root.clone(), fault }),
        );
        let plan = Plan {
            key: "group:1".into(),
            sid: "sid".into(),
            epoch: "epoch".into(),
            guard: json!({}),
            turn_seq: 5,
            input: 0,
            recovering: false,
            policy: json!({}),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let server_root = root.clone();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let root = server_root.clone();
                        thread::spawn(move || connection(stream, &root));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            root,
            context,
            plan,
            stop,
            server: Some(server),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.take().unwrap().join().unwrap();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn connection(mut stream: TcpStream, root: &std::path::Path) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    let body = loop {
        let Ok(n) = stream.read(&mut buf) else { return };
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(split) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..split]);
            let len = head
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= split + 4 + len {
                break bytes[split + 4..split + 4 + len].to_vec();
            }
        }
    };
    let request: Value = serde_json::from_slice(&body).unwrap();
    if request["user_id"] == 9001 {
        use std::fs::OpenOptions;
        let mut notices = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("owner-notices"))
            .unwrap();
        writeln!(notices, "{}", request).unwrap();
        let body = r#"{"status":"ok","retcode":0,"data":{"message_id":1}}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        return;
    }
    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn model_calls(f: &Fixture) -> usize {
    std::fs::read_to_string(f.root.join("model-called"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn digest_rows(f: &Fixture) -> i64 {
    rusqlite::Connection::open(f.root.join("data/memory.db"))
        .ok()
        .and_then(|c| {
            c.query_row(
                "SELECT count(*) FROM memory WHERE source LIKE 'digest:%'",
                [],
                |r| r.get(0),
            )
            .ok()
        })
        .unwrap_or(0)
}

fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_digest_failures_carry_a_fixed_step_and_kind_and_write_nothing() {
    for (fault, step, kind) in [
        (Fault::ModelError, "model", "backend"),
        (Fault::Refusal, "digest-result", "refusal-or-error"),
        (Fault::Unparseable, "digest-result", "unparseable"),
    ] {
        let f = Fixture::new(fault);
        let error = digest_inner(&f.context, &json!({}), &f.plan, &no_cancel())
            .await
            .unwrap_err();
        assert_eq!((error.step, error.kind, error.settled), (step, kind, true), "{error:?}");
        assert!(!format!("{error:?}").contains("private"));
        assert_eq!(digest_rows(&f), 0);
    }
}

#[tokio::test]
async fn hf135_received_error_frame_wins_over_later_socket_reset() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (received, read) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let Message::Text(open) = ws.next().await.unwrap().unwrap() else {
            panic!("open frame")
        };
        let open: Value = serde_json::from_str(&open).unwrap();
        ws.send(Message::Text(
            json!({"type":"error","streamId":open["streamId"],
            "error":{"code":"timeout-private-error-text"}})
            .to_string(),
        ))
        .await
        .unwrap();
        // Only reset after the client has returned the received error frame.
        tokio::time::timeout(Duration::from_secs(3), read)
            .await
            .unwrap()
            .unwrap();
        #[allow(deprecated)]
        ws.get_ref().set_linger(Some(Duration::ZERO)).unwrap();
    });
    let error = crate::dsh::FollowStream::open(&base, "", "sid").await.err().unwrap();
    assert!(
        error.starts_with("订阅被拒："),
        "the error frame must be read first"
    );
    received.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_digest_writes_new_messages_with_bridge_provenance_once() {
    let f = Fixture::new(Fault::None);
    assert_eq!(
        digest_inner(&f.context, &json!({}), &f.plan, &no_cancel()).await.unwrap(),
        Some(true)
    );
    let (owner, kind, source): (String, String, String) = rusqlite::Connection::open(f.root.join("data/memory.db"))
        .unwrap()
        .query_row(
            "SELECT owner_id,kind,source FROM memory WHERE source LIKE 'digest:%'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (owner.as_str(), kind.as_str(), source.as_str()),
        ("group:1", "inference", "digest:group:1:msg:1-2")
    );
    assert_eq!(
        digest_inner(&f.context, &json!({}), &f.plan, &no_cancel()).await.unwrap(),
        Some(true)
    );
    assert_eq!(model_calls(&f), 1, "a digested range is never asked again");
    std::fs::write(f.root.join("data/sessions.json"), "bad").unwrap();
    assert_eq!(
        digest_inner(&f.context, &json!({}), &f.plan, &no_cancel())
            .await
            .unwrap_err()
            .step,
        "context-read"
    );
}

#[test]
fn rotation_notice_is_once_per_conversation_per_day_across_session_replacement() {
    let f = Fixture::new(Fault::None);
    assert!(crate::session_map::mark_rotation_alert(&f.context, "group:1", "sid-one").unwrap());
    assert!(!crate::session_map::mark_rotation_alert(&f.context, "group:1", "sid-two").unwrap());
    let doc = crate::session_map::read(&f.context).unwrap();
    let day = doc["rotationAlerts"]["group:1"]["day"].as_i64().unwrap();
    assert!(crate::session_map::mark_rotation_alert_day(&f.context, "group:1", day + 1).unwrap());
    assert!(!crate::session_map::mark_rotation_alert_day(&f.context, "group:1", day + 1).unwrap());
}

#[tokio::test]
async fn checkpoint_failure_does_not_claim_success_or_repeat_a_prompt() {
    let f = Fixture::new(Fault::Checkpoint);
    let original = std::fs::read(f.root.join("data/sessions.json")).unwrap();
    let result = digest(
        f.context.clone(),
        json!({}),
        f.plan.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    assert!(matches!(result, DigestResult::Pending));
    assert_eq!(model_calls(&f), 1);
    assert_eq!(digest_rows(&f), 1);
    std::fs::write(f.root.join("data/sessions.json"), original).unwrap();
    let result = digest(
        f.context.clone(),
        json!({}),
        f.plan.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    assert!(matches!(result, DigestResult::Done { ran: true, .. }));
    assert_eq!(model_calls(&f), 1, "a settled digest range is not asked again");
}

#[tokio::test]
async fn hf135_concurrent_digest_and_recovery_failures_send_exactly_one_notice() {
    let f = Fixture::new(Fault::None);
    let cfg = json!({"ownerQQ":9001});
    let context = f.context.clone();
    let recovery_cfg = cfg.clone();
    let (worker_tx, worker_rx) = std::sync::mpsc::channel();
    NOTICE_CLAIM_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |sent| {
            // Pause the winning claimant. If the memory guard is still free,
            // let the losing caller finish first, reproducing the lost notice.
            // With serialization it must wait until this caller decides.
            let can_overtake = match sent.try_lock() {
                Ok(guard) => {
                    drop(guard);
                    true
                }
                Err(std::sync::TryLockError::WouldBlock) => false,
                Err(error) => panic!("notice guard poisoned: {error}"),
            };
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                ready_tx.send(()).unwrap();
                for pass in 0..12 {
                    let _: Result<(), _> = rotation_read(
                        &context,
                        &recovery_cfg,
                        "group:1",
                        "context-read",
                        pass * 5_000,
                        Err("private-read-error".into()),
                    );
                }
                let _ = done_tx.send(());
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            worker_tx.send(worker).unwrap();
            if can_overtake {
                done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }));
    });
    failure(&f.context, &cfg, &f.plan, "step=memory-write kind=storage");
    worker_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .join()
        .unwrap();
    assert!(crate::admin_alert::notify_notice(
        &f.context,
        &cfg,
        "hf135-concurrent-barrier",
        "hf135-barrier"
    ));
    wait_for_notice(&f.context, "hf135-concurrent-barrier").await;
    let notices = std::fs::read_to_string(f.root.join("owner-notices")).unwrap();
    assert_eq!(
        notices
            .lines()
            .filter(|line| line.contains("group:1 session rotation"))
            .count(),
        1,
        "concurrent digest and recovery failures must deliver exactly one owner notice"
    );
    assert!(notices.contains("step=memory-write kind=storage"));
    assert!(!notices.contains("private-read-error"));
}

type NoticeClaimHook = Box<dyn FnOnce(&Mutex<HashMap<ReadKey, i64>>)>;

thread_local! {
    static NOTICE_CLAIM_HOOK: std::cell::RefCell<Option<NoticeClaimHook>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn after_notice_claim(sent: &Mutex<HashMap<ReadKey, i64>>) {
    let hook = NOTICE_CLAIM_HOOK.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook(sent);
    }
}

#[tokio::test]
async fn hf135_notice_survives_unreadable_store_repair_and_process_restart() {
    const TEST: &str = "session_rotation::tests::hf135_notice_survives_unreadable_store_repair_and_process_restart";
    if let Some(root) = std::env::var_os("HF135_NOTICE_TEST_ROOT") {
        let context = RuntimeContext::from_root(&PathBuf::from(root)).unwrap();
        let cfg = json!({"ownerQQ":9001});
        rotation_notice(&context, &cfg, "group:1", "step=context-read kind=storage");
        let phase = std::env::var("HF135_NOTICE_TEST_PHASE").unwrap();
        let barrier = format!("hf135-notice-barrier-{phase}");
        assert!(crate::admin_alert::notify_notice(
            &context,
            &cfg,
            &barrier,
            "hf135-barrier"
        ));
        wait_for_notice(&context, &barrier).await;
        return;
    }
    let f = Fixture::new(Fault::None);
    let sessions = f.root.join("data/sessions.json");
    let original = std::fs::read(&sessions).unwrap();
    std::fs::write(&sessions, "private-invalid-state").unwrap();
    rotation_notice(
        &f.context,
        &json!({"ownerQQ":9001}),
        "group:1",
        "step=context-read kind=storage",
    );
    wait_for_notice(&f.context, "group:1 session rotation").await;
    for phase in ["unreadable", "repaired", "unreadable-again"] {
        if phase == "repaired" {
            std::fs::write(&sessions, &original).unwrap();
            rotation_notice(
                &f.context,
                &json!({"ownerQQ":9001}),
                "group:1",
                "step=context-read kind=storage",
            );
        } else if phase == "unreadable-again" {
            std::fs::write(&sessions, "private-invalid-state").unwrap();
        }
        let output = crate::subprocess::command_without_console(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--test-threads=1"])
            .env("HF135_NOTICE_TEST_ROOT", &f.root)
            .env("HF135_NOTICE_TEST_PHASE", phase)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let notices = std::fs::read_to_string(f.root.join("owner-notices")).unwrap();
        assert_eq!(
            notices
                .lines()
                .filter(|line| line.contains("group:1 session rotation"))
                .count(),
            1,
            "{phase}: fresh process must not resend the day's notice"
        );
        assert!(!notices.contains("private-invalid-state"));
        if phase == "repaired" {
            let doc = crate::session_map::read(&f.context).unwrap();
            assert!(
                doc["rotationAlerts"]["group:1"]["day"].is_i64(),
                "repair must persist the day in sessions.json"
            );
        }
    }
}

async fn wait_for_notice(context: &RuntimeContext, text: &str) {
    let path = context.runtime_root().unwrap().join("owner-notices");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&path)
            .unwrap_or_default()
            .split_inclusive('\n')
            .any(|line| line.ends_with('\n') && line.contains(text))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "owner notice not delivered: {text}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn hf135_memory_fallback_is_adopted_durably_after_repair() {
    let f = Fixture::new(Fault::None);
    let sessions = f.root.join("data/sessions.json");
    let original = std::fs::read(&sessions).unwrap();
    let independent = f.root.join("data/rotation-alerts.json");
    std::fs::write(&sessions, "private-invalid-state").unwrap();
    std::fs::write(&independent, "private-invalid-state").unwrap();
    let cfg = json!({"ownerQQ":9001});
    rotation_notice(
        &f.context,
        &cfg,
        "group:1",
        "step=context-read kind=storage",
    );
    assert!(crate::admin_alert::notify_notice(
        &f.context,
        &cfg,
        "hf135-fallback-barrier",
        "hf135-barrier"
    ));
    wait_for_notice(&f.context, "hf135-fallback-barrier").await;
    // The FIFO barrier also witnesses the first send's diagnostic. A second
    // notify attempt would add a skip record even inside the general 60s limit.
    let claims = || {
        std::fs::read_to_string(f.root.join("data/run-record.jsonl"))
            .unwrap()
            .lines()
            .filter(|line| line.contains("owner-alert-session-rotation-group-1"))
            .count()
    };
    assert_eq!(claims(), 1);
    std::fs::write(&sessions, original).unwrap();
    std::fs::remove_file(independent).unwrap();
    rotation_notice(
        &f.context,
        &cfg,
        "group:1",
        "step=context-read kind=storage",
    );
    assert_eq!(claims(), 1, "repair must not even attempt a second notice");
    assert!(
        !crate::session_map::mark_rotation_alert(&f.context, "group:1", "replacement").unwrap()
    );
    let doc = crate::session_map::read(&f.context).unwrap();
    assert!(doc["rotationAlerts"]["group:1"]["day"].is_i64());
}

#[tokio::test]
async fn replaced_authority_releases_old_maintenance_without_submitting() {
    let f = Fixture::new(Fault::None);
    std::fs::write(
        f.root.join("data/social-v2.json"),
        r#"{"conversations":{"group:1":{"agentToken":"replacement"}}}"#,
    )
    .unwrap();
    assert!(matches!(
        digest(
            f.context.clone(),
            json!({}),
            f.plan.clone(),
            Arc::new(AtomicBool::new(false))
        )
        .await,
        DigestResult::Done { ran: false, .. }
    ));
    assert_eq!(model_calls(&f), 0);
}

#[tokio::test]
async fn hf135_persistent_sessions_read_failure_alerts_once_and_recovers() {
    persistent_sessions_read_failure(false).await;
}

#[tokio::test]
async fn hf135_interleaved_read_failures_alert_once_without_resetting_on_success() {
    persistent_sessions_read_failure(true).await;
}

async fn persistent_sessions_read_failure(interleaved: bool) {
    let f = Fixture::new(Fault::None);
    let path = f.root.join("data/sessions.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, "private-invalid-state").unwrap();
    let social = f.root.join("data/social-v2.json");
    let original_social = std::fs::read(&social).unwrap();
    let cfg = json!({"ownerQQ":9001});
    for attempt in 0..24 {
        // sessions.json stays unreadable; token reads fail on alternating passes.
        if interleaved {
            std::fs::write(
                &social,
                if attempt % 2 == 0 {
                    b"private-invalid-state".as_slice()
                } else {
                    &original_social
                },
            )
            .unwrap();
        }
        assert!(matches!(
            digest(
                f.context.clone(),
                cfg.clone(),
                f.plan.clone(),
                Arc::new(AtomicBool::new(false))
            )
            .await,
            DigestResult::Pending
        ));
        assert_eq!(
            read_retry_ms(&f.context, &f.plan.key),
            if attempt < 11 { 5_000 } else { 60_000 },
            "every failed recovery pass counts, even when another read succeeds"
        );
    }
    assert_eq!(read_retry_ms(&f.context, &f.plan.key), 60_000);
    let log = crate::activity::tail(&f.context, 100);
    assert_eq!(
        log.matches("step=context-read kind=storage").count(),
        1,
        "persistent sessions read failure needs one content-free diagnostic"
    );
    assert!(!log.contains("private-invalid-state"));
    wait_for_notice(&f.context, "step=context-read kind=storage").await;
    let notices = std::fs::read_to_string(f.root.join("owner-notices")).unwrap();
    assert_eq!(notices.lines().count(), 1);
    assert!(notices.contains("step=context-read kind=storage"));
    assert!(!notices.contains("private-invalid-state"));
    assert_eq!(model_calls(&f), 0);
    std::fs::write(path, original).unwrap();
    std::fs::write(social, original_social).unwrap();
    assert!(matches!(
        digest(
            f.context.clone(),
            cfg,
            f.plan.clone(),
            Arc::new(AtomicBool::new(false))
        )
        .await,
        DigestResult::Done { ran: true, .. }
    ));
    assert_eq!(
        read_retry_ms(&f.context, &f.plan.key),
        60_000,
        "a completed digest does not yet confirm revocation or release maintenance"
    );
    assert_eq!(model_calls(&f), 1);
}
