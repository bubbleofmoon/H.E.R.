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
        let mut zip = zip::ZipWriter::new(fs::File::create(path.join("chosen.zip")).unwrap());
        let mut files = serde_json::Map::new();
        for (name, source) in [
            ("memory.db", "data/memory.db"),
            ("conversations.db", "data/conversations.db"),
            ("config.json", "config/config.json"),
        ] {
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
        Self(path)
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_her-cli"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c.env_remove("HER_RUNTIME_ROOT");
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
