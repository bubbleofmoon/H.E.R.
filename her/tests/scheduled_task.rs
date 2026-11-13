//! Spec seams 1/2: actual install CLI and a disposable file scheduler.
#[path = "../src/install_root.rs"]
mod install_root;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn hidden(image: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(image);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = install_root::development_root(std::env::temp_dir().join(format!(
            "her-task-12-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_her-cli"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        cmd.env_remove("HER_INSTALL_ROOT")
            .env_remove("HER_RUNTIME_ROOT")
            .env("HER_TASK_SCHEDULER_FILE", self.0.join("task.xml"))
            .args(args)
            .arg("--install-root")
            .arg(self.0.join("install 中文 & space"))
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let current = self.0.join("install 中文 & space/current");
        if std::fs::symlink_metadata(&current).is_ok() {
            #[cfg(windows)]
            std::fs::remove_dir(&current).unwrap();
            #[cfg(unix)]
            std::fs::remove_file(&current).unwrap();
        }
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Supervisor(Child);
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        self.0.wait().unwrap();
    }
}

impl Fixture {
    fn installed_command(&self, image: &str, args: &[&str]) -> Command {
        let filename = if cfg!(windows) {
            format!("{image}.exe")
        } else {
            image.to_string()
        };
        let mut cmd = hidden(self.0.join("install 中文 & space/current").join(filename));
        cmd.current_dir(self.0.join("install 中文 & space/current"));
        cmd.env_remove("HER_INSTALL_ROOT")
            .env_remove("HER_RUNTIME_ROOT")
            .env("HER_TASK_SCHEDULER_FILE", self.0.join("task.xml"))
            .args(args)
            .arg("--install-root")
            .arg(self.0.join("install 中文 & space"));
        cmd
    }

    fn start_supervisor(&self, endpoint: &str) -> Supervisor {
        let install = self.0.join("install 中文 & space");
        let release = install.join("releases/test");
        let runtime = install.join("runtime");
        std::fs::create_dir_all(&release).unwrap();
        if !install.join("current").exists() {
            for (source, name) in [
                (env!("CARGO_BIN_EXE_her"), "her"),
                (env!("CARGO_BIN_EXE_her-cli"), "her-cli"),
            ] {
                let name = if cfg!(windows) {
                    format!("{name}.exe")
                } else {
                    name.to_string()
                };
                std::fs::copy(source, release.join(name)).unwrap();
            }
            #[cfg(windows)]
            {
                let output = hidden("cmd.exe")
                    .args(["/d", "/c", "mklink", "/J"])
                    .arg(install.join("current"))
                    .arg(&release)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&release, install.join("current")).unwrap();
        }
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(runtime.join(dir)).unwrap();
        }
        let mut config = serde_json::json!({"consoleToken":"task-fixture-token", "ownerQQ":70001, "update":{"policy":"off"},
            "backends":{"mcpRustBridge":true}, "rollout":{"rustGroups":[]}, "consolePort":31999, "tasks":{}});
        for task in [
            "watchdog",
            "start-all",
            "backup",
            "memory-digest",
            "memory-digest-check",
            "upstream-check",
        ] {
            config["tasks"][task] = serde_json::json!({"enabled":false});
        }
        std::fs::write(runtime.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(runtime.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            runtime.join("dependencies.json"),
            serde_json::json!({"dsh":endpoint,"model":endpoint,"onebot":endpoint}).to_string(),
        )
        .unwrap();
        let root = runtime.to_str().unwrap();
        let child = self
            .installed_command("her", &["supervise", "--runtime-root", root])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.0.join("supervisor.stderr")).unwrap())
            .spawn()
            .unwrap();
        Supervisor(child)
    }

    fn assert_alive(&self, child: &mut Supervisor) {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "supervisor exited: {}",
            std::fs::read_to_string(self.0.join("supervisor.stderr")).unwrap()
        );
    }

    fn wait_owned(&self, child: &mut Supervisor) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let root = self.0.join("install 中文 & space/runtime");
        loop {
            self.assert_alive(child);
            let output = self
                .installed_command(
                    "her-cli",
                    &[
                        "lifecycle",
                        "status",
                        "--runtime-root",
                        root.to_str().unwrap(),
                    ],
                )
                .output()
                .unwrap();
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if value["owners"]["supervisor"]["state"] == "held"
                    && value["owners"]["supervisor"]["owner"]["identity"]["pid"] == child.0.id()
                {
                    println!(
                        "owned fixture supervisor: {}",
                        value["owners"]["supervisor"]["owner"]["identity"]
                    );
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "supervisor never owned its fixture: {output:?}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn wait_record(&self, child: &mut Supervisor, event: &str, outcome: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.assert_alive(child);
            let text = std::fs::read_to_string(
                self.0
                    .join("install 中文 & space/runtime/data/run-record.jsonl"),
            )
            .unwrap_or_default();
            if text
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|v| v["event"] == event && v["outcome"] == outcome)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {event}/{outcome}: {text}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

// Login polling is independent of administrator notices, including when no task drifts.
fn answer_login_status(
    reader: &mut std::io::BufReader<std::net::TcpStream>,
    first: &str,
) -> bool {
    use std::io::{BufRead, Read, Write};
    if !first.starts_with("POST /get_status ") {
        return false;
    }
    let mut length = None;
    loop {
        let mut header = String::new();
        assert!(reader.read_line(&mut header).unwrap() > 0);
        if header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
    }
    let length = length.expect("login status request length");
    assert!(length < 16_384);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let _: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let reply = r#"{"status":"ok","retcode":0,"data":{"online":true,"good":true}}"#;
    write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply).unwrap();
    true
}

// Observe actual administrator HTTP messages across fresh supervisor processes.
fn check_restart(fixture: &Fixture, xml: &str, expected_alerts: usize) {
    use std::io::{BufRead, BufReader, Read, Write};
    let task = fixture.0.join("task.xml");
    std::fs::write(&task, xml).unwrap();
    let record = fixture
        .0
        .join("install 中文 & space/runtime/data/run-record.jsonl");
    let offset = std::fs::read(&record).unwrap_or_default().len();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = fixture.start_supervisor(&format!("http://{}", listener.local_addr().unwrap()));
    fixture.wait_owned(&mut child);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut alerts = 0;
    let mut settled = None;
    loop {
        fixture.assert_alive(&mut child);
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut first = String::new();
                if reader.read_line(&mut first).unwrap() != 0
                    && !answer_login_status(&mut reader, &first)
                {
                    assert!(first.starts_with("POST /send_private_msg "), "{first}");
                    let mut length = None;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header == "\r\n" {
                            break;
                        }
                        assert!(!header.is_empty());
                        if let Some((name, value)) = header.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                length = Some(value.trim().parse::<usize>().unwrap());
                            }
                        }
                    }
                    let length = length.unwrap();
                    assert!(length < 16_384);
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body["user_id"], 70001);
                    assert!(
                        body.to_string().contains("HER scheduled task drift"),
                        "{body}"
                    );
                    alerts += 1;
                    let reply = r#"{"status":"ok","retcode":0,"data":{"message_id":12002}}"#;
                    write!(
                        reader.into_inner(),
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.len(),
                        reply
                    )
                    .unwrap();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("fake gateway: {error}"),
        }
        let records = std::fs::read(&record).unwrap_or_default();
        let events: Vec<serde_json::Value> = records
            .get(offset..)
            .unwrap_or_default()
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice(line).ok())
            .collect();
        let checked = events.iter().any(|v| v["event"] == "scheduled-task-check");
        let delivered = events
            .iter()
            .any(|v| v["event"] == "owner-alert-scheduled-task-drift" && v["outcome"] == "ok");
        if checked && (expected_alerts == 0 || delivered) {
            let since = settled.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(500) {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "startup did not settle: {events:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        alerts, expected_alerts,
        "administrator alerts for this start"
    );
    assert_eq!(
        std::fs::read_to_string(task).unwrap(),
        xml,
        "check must not repair drift"
    );
    // Child retains its native process handle until kill/wait; only this started child is stopped.
    drop(child);
}

#[test]
fn repeated_supervisor_starts_report_the_same_drift_only_once() {
    let fixture = Fixture::new();
    assert!(fixture.run(&["install"]).status.success());
    let xml = std::fs::read_to_string(fixture.0.join("task.xml")).unwrap();
    let drift = xml.replace("<Count>3</Count>", "<Count>4</Count>");
    check_restart(&fixture, &drift, 1);
    check_restart(&fixture, &drift, 0);
}

#[test]
fn changed_drift_at_the_same_field_alerts_again() {
    let fixture = Fixture::new();
    assert!(fixture.run(&["install"]).status.success());
    let xml = std::fs::read_to_string(fixture.0.join("task.xml")).unwrap();
    check_restart(
        &fixture,
        &xml.replace("<Count>3</Count>", "<Count>4</Count>"),
        1,
    );
    check_restart(
        &fixture,
        &xml.replace("<Count>3</Count>", "<Count>5</Count>"),
        1,
    );
}

#[test]
fn repaired_task_can_report_the_same_drift_if_it_recurs() {
    let fixture = Fixture::new();
    assert!(fixture.run(&["install"]).status.success());
    let xml = std::fs::read_to_string(fixture.0.join("task.xml")).unwrap();
    let drift = xml.replace("<Count>3</Count>", "<Count>4</Count>");
    check_restart(&fixture, &drift, 1);
    check_restart(&fixture, &xml, 0);
    check_restart(&fixture, &drift, 1);
}

#[test]
fn supervisor_reports_drift_to_the_administrator_and_keeps_a_matching_task_quiet() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    fn request(listener: &TcpListener) -> Option<(BufReader<TcpStream>, String)> {
        let (stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) => panic!("fake administrator gateway: {e}"),
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut first = String::new();
        // The retained supervisor also performs TCP-only dependency reachability probes.
        // A closed probe carries no HTTP request and is not an administrator message.
        if reader.read_line(&mut first).unwrap() == 0
            || answer_login_status(&mut reader, &first)
        {
            return None;
        }
        Some((reader, first))
    }
    for case in ["changed", "missing", "unreadable", "matching"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let fixture = Fixture::new();
        assert!(fixture.run(&["install"]).status.success());
        let task = fixture.0.join("task.xml");
        match case {
            "changed" => {
                let xml = std::fs::read_to_string(&task).unwrap();
                std::fs::write(&task, xml.replace("HighestAvailable", "LeastPrivilege")).unwrap();
            }
            "missing" => std::fs::remove_file(&task).unwrap(),
            "unreadable" => {
                std::fs::remove_file(&task).unwrap();
                std::fs::create_dir(&task).unwrap();
            }
            _ => {}
        }
        let before = std::fs::read(&task).ok();
        let was_directory = task.is_dir();
        let mut child =
            fixture.start_supervisor(&format!("http://{}", listener.local_addr().unwrap()));
        fixture.wait_owned(&mut child);
        if case == "matching" {
            fixture.wait_record(&mut child, "scheduled-task-check", "ok");
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                assert!(
                    request(&listener).is_none(),
                    "a matching task must not alert"
                );
                fixture.assert_alive(&mut child);
                std::thread::sleep(Duration::from_millis(20));
            }
            continue;
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut reader, first) = loop {
            fixture.assert_alive(&mut child);
            if let Some(request) = request(&listener) {
                break request;
            }
            assert!(
                Instant::now() < deadline,
                "{case} task did not produce an administrator alert"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(first.starts_with("POST /send_private_msg "), "{first}");
        let mut length = None;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" {
                break;
            }
            assert!(!header.is_empty(), "truncated fake gateway request");
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
        }
        let length = length.expect("gateway request length");
        assert!(length < 16_384);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        let mut stream = reader.into_inner();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["user_id"], 70001);
        assert!(
            body.to_string().contains("HER scheduled task drift"),
            "{body}"
        );
        let reply = r#"{"status":"ok","retcode":0,"data":{"message_id":12001}}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply).unwrap();
        drop(stream);
        fixture.wait_record(&mut child, "scheduled-task-check", "failed");
        fixture.wait_record(&mut child, "owner-alert-scheduled-task-drift", "ok");
        assert_eq!(
            std::fs::read(&task).ok(),
            before,
            "alerting must not repair the task"
        );
        assert_eq!(task.is_dir(), was_directory);
    }
}

#[test]
fn install_registers_the_hidden_logon_task_for_the_selected_current_link() {
    let fixture = Fixture::new();
    let output = fixture.run(&["install"]);
    assert!(output.status.success(), "{output:?}");
    let xml = std::fs::read_to_string(fixture.0.join("task.xml")).unwrap();
    assert!(xml.contains("<LogonTrigger>"), "{xml}");
    assert!(
        xml.contains("<RunLevel>HighestAvailable</RunLevel>"),
        "{xml}"
    );
    assert!(
        xml.contains("<LogonType>InteractiveToken</LogonType>"),
        "{xml}"
    );
    assert!(
        xml.contains(
            "<RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>"
        ),
        "{xml}"
    );
    assert!(
        xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
        "{xml}"
    );
    assert!(xml.contains("<Hidden>true</Hidden>"), "{xml}");
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["task"], "HER");
    assert_eq!(
        Path::new(doc["command"].as_str().unwrap()),
        fixture.0.join("install 中文 & space/current/her.exe")
    );
    assert!(doc["arguments"]
        .as_str()
        .unwrap()
        .starts_with("supervise --runtime-root \""));
    assert!(doc["arguments"]
        .as_str()
        .unwrap()
        .contains("--install-root"));
    assert!(!doc["userId"].as_str().unwrap().is_empty());
    let install = fixture.0.join("install 中文 & space");
    let expected_arguments = format!(
        "supervise --runtime-root \"{}\" --install-root \"{}\"",
        install.join("runtime").display(),
        install.display()
    );
    assert_eq!(doc["arguments"], expected_arguments);
    let parsed = roxmltree::Document::parse(&xml).unwrap();
    let text = |name: &str| {
        parsed
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == name)
            .unwrap()
            .text()
            .unwrap()
    };
    assert_eq!(Path::new(text("Command")), install.join("current/her.exe"));
    assert_eq!(Path::new(text("WorkingDirectory")), install.join("current"));
    assert_eq!(text("Arguments"), expected_arguments);
    let users: Vec<_> = parsed
        .descendants()
        .filter(|n| n.has_tag_name("UserId"))
        .map(|n| n.text().unwrap())
        .collect();
    assert_eq!(
        users,
        [doc["userId"].as_str().unwrap(); 2],
        "logon trigger and principal must use the installing account"
    );
    for (container, child) in [("Triggers", "LogonTrigger"), ("Actions", "Exec")] {
        let nodes: Vec<_> = parsed
            .descendants()
            .find(|n| n.has_tag_name(container))
            .unwrap()
            .children()
            .filter(|n| n.is_element())
            .collect();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].has_tag_name(child));
    }
}

#[test]
fn task_readback_compares_behavior_and_removes_only_the_selected_file_task() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("foreign-task.xml"), "foreign task").unwrap();
    assert!(fixture.run(&["install"]).status.success());
    let original = std::fs::read_to_string(fixture.0.join("task.xml")).unwrap();
    let read = fixture.run(&["scheduled-task", "read"]);
    assert!(read.status.success(), "{read:?}");
    let read: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read["xml"], original);
    let matches = fixture.run(&["scheduled-task", "compare"]);
    assert!(matches.status.success(), "{matches:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&matches.stdout).unwrap()["matches"],
        true
    );

    // Native Scheduler may reorder/indent XML, add registration metadata, omit defaults,
    // and spell the same duration differently. Those changes preserve this task's behavior.
    let equivalent = original
        .replace(
            "<Triggers>",
            "<RegistrationInfo><Author>fixture</Author></RegistrationInfo><Triggers>",
        )
        .replace("<Enabled>true</Enabled>", "")
        .replace("PT1M", "PT60S")
        .replace("PT0S", "PT0M")
        .replace("><", ">\n  <");
    std::fs::write(fixture.0.join("task.xml"), equivalent).unwrap();
    let same = fixture.run(&["scheduled-task", "compare"]);
    assert!(same.status.success(), "{same:?}");
    let mut utf16 = vec![0xff, 0xfe];
    for word in original.replace("UTF-8", "UTF-16").encode_utf16() {
        utf16.extend_from_slice(&word.to_le_bytes());
    }
    std::fs::write(fixture.0.join("task.xml"), utf16).unwrap();
    let native_encoding = fixture.run(&["scheduled-task", "compare"]);
    assert!(native_encoding.status.success(), "{native_encoding:?}");
    for (from, to) in [
        ("HighestAvailable", "LeastPrivilege"),
        ("InteractiveToken", "Password"),
        ("<Count>3</Count>", "<Count>4</Count>"),
        ("<Interval>PT1M</Interval>", "<Interval>PT5M</Interval>"),
        (
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<ExecutionTimeLimit>PT72H</ExecutionTimeLimit>",
        ),
        (
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<ExecutionTimeLimit>P</ExecutionTimeLimit>",
        ),
        ("<Hidden>true</Hidden>", "<Hidden>false</Hidden>"),
        ("<Enabled>true</Enabled>", "<Enabled>false</Enabled>"),
        ("her.exe", "her-cli.exe"),
        ("supervise --runtime-root", "app --runtime-root"),
        ("<LogonTrigger>", "<LogonTrigger><Delay>PT1H</Delay>"),
        (
            "</Exec>",
            "</Exec><Exec><Command>foreign.exe</Command></Exec>",
        ),
        ("</Triggers>", "<BootTrigger/></Triggers>"),
        ("<UserId>", "<UserId>foreign-"),
    ] {
        let drifted = original.replace(from, to);
        assert_ne!(drifted, original, "fixture mutation must change {from}");
        std::fs::write(fixture.0.join("task.xml"), &drifted).unwrap();
        let drift = fixture.run(&["scheduled-task", "compare"]);
        assert_eq!(drift.status.code(), Some(3), "{from}: {drift:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("task.xml")).unwrap(),
            drifted,
            "comparison must never repair drift"
        );
    }
    assert!(fixture.run(&["scheduled-task", "remove"]).status.success());
    assert!(!fixture.0.join("task.xml").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("foreign-task.xml")).unwrap(),
        "foreign task"
    );
    let missing = fixture.run(&["scheduled-task", "read"]);
    assert!(missing.status.success(), "{missing:?}");
    assert!(serde_json::from_slice::<serde_json::Value>(&missing.stdout).unwrap()["xml"].is_null());
    assert_eq!(
        fixture.run(&["scheduled-task", "compare"]).status.code(),
        Some(3)
    );
    std::fs::write(fixture.0.join("task.xml"), "not XML").unwrap();
    assert_eq!(
        fixture.run(&["scheduled-task", "compare"]).status.code(),
        Some(3)
    );
    assert!(
        fixture.run(&["install"]).status.success(),
        "reinstall repairs only when explicitly requested"
    );
    assert!(fixture.run(&["scheduled-task", "compare"]).status.success());
}
