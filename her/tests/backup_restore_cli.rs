#[path = "../src/install_root.rs"]
mod install_root;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let mut id = [0; 8];
        getrandom::getrandom(&mut id).unwrap();
        let path = install_root::development_root(
            std::env::temp_dir().join(format!("her-restore-cli-{:x}", u64::from_ne_bytes(id))),
        );
        // The CLI's own guard also checks these roots before use.
        for dir in ["config", "data", "resources/roles"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        fs::write(
            path.join("config/config.json"),
            br#"{"consoleToken":"fixture","backup":{}}"#,
        )
        .unwrap();
        fs::write(path.join("dependencies.json"), br#"{"dsh":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","model":"http://127.0.0.1:1"}"#).unwrap();
        fs::write(path.join("data/social-v2.json"), "{}").unwrap();
        let db = rusqlite::Connection::open(path.join("data/memory.db")).unwrap();
        db.execute_batch("CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES('fixture');")
            .unwrap();
        drop(db);
        // Close the WAL fixture before reading its checkpointed bytes into the archive.
        let conversations = rusqlite::Connection::open(path.join("data/conversations.db")).unwrap();
        conversations.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES('conversation');").unwrap();
        drop(conversations);
        let fixture = Self(path);
        fixture.write_archive(None);
        fixture
    }

    fn write_archive(&self, settings_snapshot: Option<&str>) {
        let path = &self.0;
        let mut zip = zip::ZipWriter::new(fs::File::create(path.join("chosen.zip")).unwrap());
        let mut files = serde_json::Map::new();
        for (name, source) in [
            ("memory.db", "data/memory.db"),
            ("conversations.db", "data/conversations.db"),
            ("state/social-v2.json", "data/social-v2.json"),
            ("config.json", "config/config.json"),
        ]
        .into_iter()
        .chain(settings_snapshot.map(|source| ("settings.db", source)))
        {
            let bytes = fs::read(path.join(source)).unwrap();
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
    }

    fn cli() -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_her-cli"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c.env_remove("HER_RUNTIME_ROOT").env_remove("HER_TEST_RESTORE_CONTROL");
        c
    }

    fn command(&self) -> Command {
        let mut c = Self::cli();
        c.args(["backup", "restore"])
            .arg(self.0.join("chosen.zip"))
            .arg("--runtime-root")
            .arg(&self.0)
            .stdin(Stdio::null());
        c
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn cli_preview_and_terminal_confirmation_leave_running_files_untouched() {
    let f = Fixture::new();
    let before = fs::read(f.0.join("data/memory.db")).unwrap();
    let conversation_before = fs::read(f.0.join("data/conversations.db")).unwrap();
    let output = f
        .command()
        .args(["--memory-only", "--preview"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .unwrap()
        .to_string();
    let view: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(view["preview"]["scope"], "memory");
    assert_eq!(view["preview"]["replaces"], json!(["data/memory.db"]));
    assert!(view["preview"]["contents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "config.json"));
    assert!(view["preview"]["contents"]
        .as_array()
        .unwrap()
        .contains(&json!("conversations.db")));
    let full = f.command().arg("--preview").output().unwrap();
    assert!(
        full.status.success(),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    let text = String::from_utf8_lossy(&full.stdout);
    let full: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert!(full["preview"]["replaces"]
        .as_array()
        .unwrap()
        .contains(&json!("data/conversations.db")));
    assert!(full["preview"]["replaces"].as_array().unwrap().contains(&json!("data/console-token")));
    let output = f.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not confirmed"));
    assert!(!f.0.join("data/lifecycle/desired-state.json").exists());
    assert_eq!(fs::read(f.0.join("data/memory.db")).unwrap(), before);
    assert_eq!(
        fs::read(f.0.join("data/conversations.db")).unwrap(),
        conversation_before
    );
    fs::write(f.0.join("chosen.zip"), b"broken").unwrap();
    let output = f.command().args(["--confirm", "invalid"]).output().unwrap();
    assert!(!output.status.success());
    assert!(!f.0.join("data/lifecycle/desired-state.json").exists());
    assert_eq!(fs::read(f.0.join("data/memory.db")).unwrap(), before);
    assert_eq!(
        fs::read(f.0.join("data/conversations.db")).unwrap(),
        conversation_before
    );
}

#[test]
fn restore_failure_waits_for_a_slow_administrator_notification_before_cli_exit() {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let f = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let blocked = f.0.join("backup-is-a-file");
    fs::write(&blocked, "not a directory").unwrap();
    fs::write(
        f.0.join("config/config.json"),
        json!({
            "consoleToken":"fixture", "ownerQQ":12345,
            "backup":{"dir":blocked}
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        f.0.join("dependencies.json"),
        json!({"onebot":url,
        "dsh":"http://127.0.0.1:1", "model":"http://127.0.0.1:1"})
        .to_string(),
    )
    .unwrap();
    let plan = f
        .command()
        .args(["--memory-only", "--preview"])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let text = String::from_utf8_lossy(&plan.stdout);
    let plan: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let limit = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < limit => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => panic!("failure alert did not reach the gateway: {other:?}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0, "notifier exited before submitting the alert");
            bytes.extend_from_slice(&buf[..n]);
            if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..end]);
                let size: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + size {
                    break;
                }
            }
        }
        let request = String::from_utf8_lossy(&bytes);
        assert!(request.starts_with("POST /send_private_msg "), "{request}");
        assert!(
            request.contains("Restore refused before stopping"),
            "{request}"
        );
        started_tx.send(()).unwrap();
        // A delayed gateway acknowledgement must outlive the old detached-thread window.
        std::thread::sleep(Duration::from_millis(700));
        let mut probe = [0];
        stream
            .set_read_timeout(Some(Duration::from_millis(30)))
            .unwrap();
        let read = stream.read(&mut probe);
        assert!(
            matches!(read, Err(ref e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
            "CLI closed the notification connection before acknowledgement: {read:?}"
        );
        let body = r#"{"status":"ok","retcode":0,"data":{"message_id":42}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let mut child = f
        .command()
        .args([
            "--memory-only",
            "--confirm",
            plan["preview"]["confirmation"].as_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let received = started_rx.recv_timeout(Duration::from_secs(6));
    let alive = child.try_wait().unwrap().is_none();
    let server_result = server.join();
    let output = child.wait_with_output().unwrap();
    assert!(
        received.is_ok() && alive,
        "CLI must wait for delivery: started={received:?}, alive={alive}, stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server_result.unwrap();
    assert!(
        !output.status.success(),
        "the original restore refusal still fails"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("已私聊管理员"),
        "notification must complete before exit"
    );
    assert!(!f.0.join("data/lifecycle/desired-state.json").exists());
}

#[cfg(feature = "parity-clock")]
mod password_restore {
    use super::*;
    use rusqlite::OptionalExtension;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Output};
    use std::sync::mpsc;
    use std::time::Duration;

    fn password_cli(fixture: &Fixture, action: &str) -> Command {
        let mut cli = Fixture::cli();
        cli.args(["console", action, "--runtime-root"]).arg(&fixture.0);
        cli
    }

    fn set_password(fixture: &Fixture, password: &str) {
        let mut child = password_cli(fixture, "set-password")
            .stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let wrote = write!(input, "fixture\n{password}\n{password}\n");
        drop(input);
        let status = child.wait().unwrap();
        wrote.unwrap();
        assert!(status.success());
    }

    fn reset_password(fixture: &Fixture) -> Output {
        password_cli(fixture, "reset-password").stdin(Stdio::null()).output().unwrap()
    }

    fn current_hash(fixture: &Fixture) -> Option<String> {
        rusqlite::Connection::open_with_flags(
            fixture.0.join("data/settings.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ).unwrap().query_row(
            "SELECT value FROM settings WHERE key='console.administratorPassword'", [], |row| row.get(0),
        ).optional().unwrap()
    }

    struct RestoreChild(Child);

    impl Drop for RestoreChild {
        fn drop(&mut self) {
            // EOF releases either fixture barrier so the owned child can exit and be reaped.
            drop(self.0.stdin.take());
            let _ = self.0.wait();
        }
    }

    fn exercise_rollback(reset_before_snapshot: bool) {
        let f = Fixture::new();
        set_password(&f, "fixture archived password");
        let archived = current_hash(&f);
        // Full restore accepts an archive of the credential without the live auth state.
        let snapshot_name = "archived-settings.db";
        let snapshot = f.0.join(snapshot_name);
        let live = rusqlite::Connection::open(f.0.join("data/settings.db")).unwrap();
        live.execute("VACUUM INTO ?1", [snapshot.to_str().unwrap()])
            .unwrap();
        drop(live);
        let copy = rusqlite::Connection::open(&snapshot).unwrap();
        copy.execute_batch(
            "PRAGMA secure_delete=ON;
             DELETE FROM console_browser_sessions;
             DELETE FROM console_auth_state;",
        )
        .unwrap();
        copy.close().unwrap();
        f.write_archive(Some(snapshot_name));
        fs::remove_file(snapshot).unwrap();
        assert!(reset_password(&f).status.success());
        set_password(&f, "fixture destination password");
        if reset_before_snapshot {
            assert!(reset_password(&f).status.success());
        }
        let previous = current_hash(&f);
        assert_eq!(previous.is_none(), reset_before_snapshot);
        let preview = f.command().arg("--preview").output().unwrap();
        assert!(
            preview.status.success(),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        let stdout = String::from_utf8(preview.stdout).unwrap();
        let plan: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
        let mut child = RestoreChild(f.command()
            .env("HER_TEST_RESTORE_CONTROL", "stdio")
            .args(["--confirm", plan["preview"]["confirmation"].as_str().unwrap()])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap());
        let stdout = child.0.stdout.take().unwrap();
        let (ready, stages) = mpsc::channel();
        let drain = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(stage) = line.strip_prefix("restore fixture ") {
                    let _ = ready.send(stage.to_string());
                }
            }
        });
        let probes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (stage, expected) in [("snapshot", &previous), ("health", &archived)] {
                assert_eq!(stages.recv_timeout(Duration::from_secs(20)).unwrap(), stage);
                let reset = reset_password(&f);
                assert_eq!(reset.status.code(), Some(3),
                    "offline reset must refuse while full restore holds the snapshot and health boundary");
                assert_eq!(&current_hash(&f), expected, "refused reset must leave the stored hash alone");
                writeln!(child.0.stdin.as_mut().unwrap(), "continue").unwrap();
            }
        }));
        drop(child.0.stdin.take());
        let status = child.0.wait().unwrap();
        drain.join().unwrap();
        if let Err(panic) = probes {
            std::panic::resume_unwind(panic);
        }
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.0.stderr.take().unwrap(), &mut stderr).unwrap();
        assert_eq!(status.code(), Some(1));
        assert!(stderr.contains("rolled back"), "{stderr}");
        assert_eq!(current_hash(&f), previous, "rollback must reflect the credential at snapshot selection");
        assert!(reset_password(&f).status.success());
        assert!(current_hash(&f).is_none());
    }

    #[test]
    fn full_restore_rollback_serializes_offline_password_reset() {
        exercise_rollback(false);
    }

    #[test]
    fn full_restore_rollback_keeps_a_password_reset_before_snapshot_selection() {
        exercise_rollback(true);
    }
    const SAVED_GENERATED: &str = "fixture-saved-generated-credential";
    const ARCHIVED_CONFIGURED: &str = "fixture-archived-configured-credential";

    fn pair_fixture(generated_archive: bool) -> (Fixture, Value) {
        let f = Fixture::new();
        let archive = json!({"consoleToken":if generated_archive { "" } else { ARCHIVED_CONFIGURED },
            "backup":{"dir":f.0.join("archive backups")},"fixtureMode":"archive"});
        fs::write(f.0.join("config/config.json"), archive.to_string()).unwrap();
        f.write_archive(None);
        let destination = json!({"consoleToken":"","backup":{"dir":f.0.join("destination backups")},"fixtureMode":"destination"});
        fs::write(f.0.join("config/config.json"), destination.to_string()).unwrap();
        fs::write(f.0.join("data/console-token"), SAVED_GENERATED).unwrap();
        (f, destination)
    }

    struct PairWorker {
        child: RestoreChild,
        stages: mpsc::Receiver<String>,
        drain: Option<std::thread::JoinHandle<()>>,
    }

    impl PairWorker {
        fn spawn(mut command: Command) -> Self {
            let mut child = RestoreChild(command
                .env("HER_TEST_RESTORE_CONTROL", "stdio")
                .env("HER_TEST_RESTORE_PAIR_STEPS", "1")
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
                .spawn().unwrap());
            let stdout = child.0.stdout.take().unwrap();
            let (ready, stages) = mpsc::channel();
            let drain = std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(stage) = line.strip_prefix("restore fixture ") {
                        let _ = ready.send(stage.to_string());
                    }
                }
            });
            Self { child, stages, drain: Some(drain) }
        }

        fn stage(&self, expected: &str) {
            assert_eq!(self.stages.recv_timeout(Duration::from_secs(30)).unwrap(), expected);
        }

        fn proceed(&mut self) {
            writeln!(self.child.0.stdin.as_mut().unwrap(), "continue").unwrap();
        }

        fn wait(mut self) -> bool {
            drop(self.child.0.stdin.take());
            let ok = self.child.0.wait().unwrap().success();
            self.drain.take().unwrap().join().unwrap();
            ok
        }

        fn cut(mut self) {
            self.child.0.kill().unwrap();
            drop(self.child.0.stdin.take());
            self.child.0.wait().unwrap();
            self.drain.take().unwrap().join().unwrap();
        }
    }

    fn begin_pair(f: &Fixture, healthy: bool, memory_only: bool) -> PairWorker {
        let mut preview = f.command();
        preview.arg("--preview");
        if memory_only { preview.arg("--memory-only"); }
        let output = preview.output().unwrap();
        assert!(output.status.success(), "restore preview: {}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let plan: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
        let mut command = f.command();
        command.args(["--confirm", plan["preview"]["confirmation"].as_str().unwrap()]);
        if memory_only { command.arg("--memory-only"); }
        if healthy { command.env("HER_TEST_RESTORE_HEALTH", "pass"); }
        PairWorker::spawn(command)
    }

    fn recover_pair(f: &Fixture) -> PairWorker {
        let mut command = Fixture::cli();
        command.args(["backup", "restore", "--recover", "--runtime-root"]).arg(&f.0);
        PairWorker::spawn(command)
    }

    fn settings_config(f: &Fixture) -> Value {
        serde_json::from_slice(&fs::read(f.0.join("config/config.json")).unwrap()).unwrap()
    }

    fn protected_file(f: &Fixture) -> String {
        fs::read_to_string(f.0.join("data/console-token")).unwrap()
    }

    fn place_running(f: &Fixture) {
        let output = Fixture::cli().args(["lifecycle", "set", "running", "--runtime-root"]).arg(&f.0)
            .stdout(Stdio::null()).output().unwrap();
        assert!(output.status.success(), "begin fixture console: {}", String::from_utf8_lossy(&output.stderr));
    }

    struct PairConsole(Child);
    impl Drop for PairConsole {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() { let _ = self.0.kill(); }
            let _ = self.0.wait();
        }
    }

    fn pair_http(port: u16, method: &str, path: &str, headers: &str) -> Option<(u16, Value)> {
        use std::io::Read as _;
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: her.localhost:{port}\r\n{headers}Connection: close\r\n\r\n").ok()?;
        let mut result = String::new();
        stream.read_to_string(&mut result).ok()?;
        let (head, body) = result.split_once("\r\n\r\n")?;
        let status = head.split_whitespace().nth(1)?.parse().ok()?;
        Some((status, serde_json::from_str(body).ok()?))
    }

    fn start_pair_console(f: &Fixture) -> (PairConsole, u16) {
        place_running(f);
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = port.local_addr().unwrap().port();
        let mut cmd = Fixture::cli();
        cmd.args(["--console-serve", "--console-port", &address.to_string(), "--runtime-root"]).arg(&f.0)
            .stdout(Stdio::null()).stderr(Stdio::null());
        drop(port);
        let mut console = PairConsole(cmd.spawn().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((200, reply)) = pair_http(address, "GET", "/api/health", "") {
                if reply["pid"] == console.0.id() { return (console, address); }
            }
            assert!(console.0.try_wait().unwrap().is_none(), "fixture console exited before health");
            assert!(std::time::Instant::now() < deadline, "fixture console did not report liveness");
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn native_pair_status(port: u16, token: &str) -> u16 {
        pair_http(port, "GET", "/api/status", &format!("X-Console-Token: {token}\r\n")).unwrap().0
    }

    #[test]
    fn full_restore_and_rollback_reconcile_console_token_pair() {
        for (generated_archive, healthy) in [(false, true), (false, false), (true, true)] {
            let (f, _) = pair_fixture(generated_archive);
            if generated_archive {
                let mut config = settings_config(&f);
                config["consoleToken"] = json!(SAVED_GENERATED);
                fs::write(f.0.join("config/config.json"), config.to_string()).unwrap();
            }
            let mut worker = begin_pair(&f, healthy, false);
            worker.stage("snapshot");
            worker.proceed();
            worker.stage("forward-pair");
            assert_eq!(protected_file(&f), if generated_archive { SAVED_GENERATED } else { ARCHIVED_CONFIGURED },
                "forward restore publishes its corresponding token file");
            worker.proceed();
            worker.stage("health");
            assert_eq!(settings_config(&f)["consoleToken"], if generated_archive { "" } else { ARCHIVED_CONFIGURED });
            worker.proceed();
            if !healthy {
                worker.stage("rollback-before-token");
                worker.proceed();
                worker.stage("rollback-pair");
                assert_eq!(protected_file(&f), SAVED_GENERATED, "rollback republishes the destination generated token");
                worker.proceed();
            }
            assert_eq!(worker.wait(), healthy);
            let (_console, port) = start_pair_console(&f);
            let live = if healthy && !generated_archive { ARCHIVED_CONFIGURED } else { SAVED_GENERATED };
            assert_eq!(native_pair_status(port, live), 200);
            assert_eq!(settings_config(&f)["consoleToken"], if healthy && !generated_archive { ARCHIVED_CONFIGURED } else { "" });
        }
        let (f, before) = pair_fixture(false);
        let mut worker = begin_pair(&f, true, true);
        worker.stage("snapshot");
        worker.proceed();
        worker.stage("health");
        worker.proceed();
        assert!(worker.wait());
        assert_eq!(protected_file(&f), SAVED_GENERATED);
        assert_eq!(settings_config(&f), before);
    }

    #[test]
    fn interrupted_rollback_recovers_destination_generated_token() {
        let (f, before) = pair_fixture(false);
        let mut worker = begin_pair(&f, false, false);
        worker.stage("snapshot");
        worker.proceed();
        worker.stage("forward-pair");
        worker.proceed();
        worker.stage("health");
        worker.proceed();
        worker.stage("rollback-before-token");
        worker.cut();
        assert_eq!(protected_file(&f), ARCHIVED_CONFIGURED);
        assert!(f.0.join("data/console-restore-recovery/destination-token").is_file());
        let (closed, port) = start_pair_console(&f);
        assert_eq!(native_pair_status(port, ARCHIVED_CONFIGURED), 503);
        drop(closed);
        let worker = recover_pair(&f);
        worker.stage("rollback-before-token");
        worker.cut();
        let mut worker = recover_pair(&f);
        worker.stage("rollback-before-token");
        worker.proceed();
        worker.stage("rollback-pair");
        assert_eq!(protected_file(&f), SAVED_GENERATED, "local recovery republishes the saved generated token");
        worker.proceed();
        assert!(worker.wait());
        assert_eq!(settings_config(&f), before);
        assert!(!f.0.join("data/console-restore-recovery/intent").exists());
        assert!(!f.0.join("data/console-restore-recovery/destination-token").exists());
        let (_console, port) = start_pair_console(&f);
        assert_eq!(native_pair_status(port, SAVED_GENERATED), 200);
        assert_eq!(native_pair_status(port, ARCHIVED_CONFIGURED), 401);
    }

    #[test]
    fn unreadable_full_restore_intent_keeps_process_health_and_refuses_replay() {
        let (f, _) = pair_fixture(false);
        let dir = f.0.join("data/console-restore-recovery");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("intent"), b"{torn").unwrap();
        fs::write(f.0.join("config/config.json"), b"{torn").unwrap();
        let (closed, port) = start_pair_console(&f);
        let (status, health) = pair_http(port, "GET", "/api/health", "").unwrap();
        assert_eq!(status, 200);
        assert_eq!(health["administratorReady"], false);
        assert_eq!(native_pair_status(port, SAVED_GENERATED), 503);
        drop(closed);
        assert!(!recover_pair(&f).wait());
        assert_eq!(fs::read(dir.join("intent")).unwrap().as_slice(), b"{torn");
        assert_eq!(fs::read(f.0.join("config/config.json")).unwrap().as_slice(), b"{torn");
    }

    #[test]
    fn pending_full_restore_closes_console_until_entire_config_recovers() {
        for rollback in [false, true] {
            let (f, before) = pair_fixture(false);
            let mut worker = begin_pair(&f, false, false);
            worker.stage("snapshot");
            worker.proceed();
            worker.stage("forward-pair");
            if rollback {
                worker.proceed();
                worker.stage("health");
                worker.proceed();
                worker.stage("rollback-before-token");
                worker.proceed();
                worker.stage("rollback-pair");
            }
            worker.cut();
            let config_path = f.0.join("config/config.json");
            let intent_path = f.0.join("data/console-restore-recovery/intent");
            let interrupted = fs::read(&config_path).unwrap();
            let intent = fs::read(&intent_path).unwrap();
            let audit_path = f.0.join("data/audit/audit.jsonl");
            let audit = fs::read(&audit_path).unwrap();
            let (closed, port) = start_pair_console(&f);
            let (status, health) = pair_http(port, "GET", "/api/health", "").unwrap();
            assert_eq!(status, 200);
            assert_eq!(health["ok"], false);
            assert_eq!(health["administratorReady"], false);
            assert_eq!(health["recoveryReason"], "full restore requires recovery");
            assert_eq!(native_pair_status(port, SAVED_GENERATED), 503, "pending restore must close administrator routes");
            assert_eq!(pair_http(port, "POST", "/api/desktop/sign-in", "Content-Type: application/json\r\n").unwrap().0, 503);
            let cookie = format!("Cookie: her_browser_{port}_fixture=fixture\r\n");
            assert_eq!(pair_http(port, "GET", "/api/status", &cookie).unwrap().0, 503);
            assert_eq!(fs::read(&audit_path).unwrap(), audit);
            assert_eq!(fs::read(&config_path).unwrap(), interrupted);
            assert_eq!(fs::read(&intent_path).unwrap(), intent);
            drop(closed);
            let mut worker = recover_pair(&f);
            worker.stage("rollback-before-token");
            worker.proceed();
            worker.stage("rollback-pair");
            worker.proceed();
            assert!(worker.wait());
            assert_eq!(settings_config(&f), before, "recovery must restore the complete staged configuration");
            let (_console, port) = start_pair_console(&f);
            assert_eq!(native_pair_status(port, SAVED_GENERATED), 200);
        }
    }

}
