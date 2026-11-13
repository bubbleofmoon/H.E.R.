//! Ticket 27: `her lifecycle` and `her tasks` through the real binary on a synthetic root.

#[path = "../src/install_root.rs"]
mod install_root;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn hidden_command(exe: &str) -> Command {
    let mut command = Command::new(exe);
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    command
}

fn root(name: &str) -> PathBuf {
    let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-lifecycle-cli-{name}-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data")).unwrap();
    dir
}

fn run(args: &[&str], root: &Path) -> (i32, Value, String) {
    run_image(env!("CARGO_BIN_EXE_her"), args, root)
}

fn run_image(image: &str, args: &[&str], root: &Path) -> (i32, Value, String) {
    let output = hidden_command(image)
        .args(args)
        .arg("--runtime-root")
        .arg(root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let json = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (output.status.code().unwrap_or(-1), json, String::from_utf8_lossy(&output.stderr).to_string())
}

#[test]
fn supervisor_owns_the_runtime_and_honours_a_stopped_state() {
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};
    struct KillOnDrop(Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let dir = root("supervisor");
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("resources")).unwrap();
    std::fs::write(dir.join("config/config.json"), r#"{"consoleToken":"fixture-token","backends":{"mcpRustBridge":true},"rollout":{"rustGroups":[]},"consolePort":31999}"#).unwrap();
    std::fs::write(dir.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(dir.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
    assert_eq!(run(&["lifecycle", "set", "stopped", "--confirm"], &dir).0, 0);
    let child = hidden_command(env!("CARGO_BIN_EXE_her"))
        .args(["supervise", "--runtime-root"]).arg(&dir)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let mut supervisor = KillOnDrop(child);
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        assert!(supervisor.0.try_wait().unwrap().is_none(), "supervisor exited before taking ownership");
        let (_, status, _) = run(&["lifecycle", "status"], &dir);
        if status["owners"]["supervisor"]["state"] == "held" { break; }
        assert!(Instant::now() < deadline, "supervisor did not take ownership: {status}");
        std::thread::sleep(Duration::from_millis(30));
    }
    let second = hidden_command(env!("CARGO_BIN_EXE_her"))
        .args(["supervise", "--runtime-root"]).arg(&dir)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let mut second = KillOnDrop(second);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(status) = second.0.try_wait().unwrap() {
            assert!(!status.success(), "a second supervisor must be refused");
            break;
        }
        assert!(Instant::now() < deadline, "second supervisor did not exit");
        std::thread::sleep(Duration::from_millis(30));
    }
    let (_, status, _) = run(&["lifecycle", "status"], &dir);
    assert_eq!(status["desired"]["state"], "stopped");
    assert_eq!(status["owners"]["live"]["state"], "free");
    assert_ne!(run(&["lifecycle", "stop-supervisor"], &dir).0, 0);
    assert!(supervisor.0.try_wait().unwrap().is_none());
    let (code, _, error) = run_image(env!("CARGO_BIN_EXE_her-cli"), &["lifecycle", "stop-supervisor", "--confirm"], &dir);
    assert_eq!(code, 0, "{error}");
    assert_eq!(run(&["lifecycle", "status"], &dir).1["owners"]["supervisor"]["state"], "free");
    supervisor.0.wait().unwrap();
}

struct ConsoleOwner(std::process::Child);
impl Drop for ConsoleOwner {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

fn console_owner(dir: &Path, image: &str) -> ConsoleOwner {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    for name in ["config", "resources"] { std::fs::create_dir_all(dir.join(name)).unwrap(); }
    std::fs::write(dir.join("config/config.json"), r#"{"consoleToken":"fixture-token"}"#).unwrap();
    std::fs::write(dir.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(dir.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    drop(listener);
    let mut child = ConsoleOwner(hidden_command(image)
        .args(["--console-serve", "--console-port", &port, "--runtime-root"]).arg(dir)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        assert!(child.0.try_wait().unwrap().is_none(), "console exited before ownership");
        let status = run(&["lifecycle", "status"], dir).1;
        let owner = &status["owners"]["console"];
        if owner["state"] == "held" && owner["owner"]["role"] == "console"
            && owner["owner"]["identity"]["pid"] == child.0.id() { break; }
        assert!(Instant::now() < deadline, "console did not take ownership");
        std::thread::sleep(Duration::from_millis(30));
    }
    child
}

#[test]
fn paired_images_stop_console_owners() {
    let gui = env!("CARGO_BIN_EXE_her");
    let cli = env!("CARGO_BIN_EXE_her-cli");
    for (case, caller, image) in [("cli-gui", cli, gui), ("gui-cli", gui, cli), ("gui-gui", gui, gui), ("cli-cli", cli, cli)] {
        let dir = root(case);
        let mut owner = console_owner(&dir, image);
        let (code, _, error) = run_image(caller, &["lifecycle", "set", "stopped", "--confirm"], &dir);
        assert_eq!(code, 0, "{case}: {error}");
        owner.0.wait().unwrap();
        let (_, status, _) = run(&["lifecycle", "status"], &dir);
        assert_eq!(status["owners"]["console"]["state"], "free");
        assert_eq!(status["desired"]["state"], "stopped");
        drop(owner);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn refused_stop_preserves_desired_state_and_leaves_the_owner_alive() {
    for case in ["foreign-directory", "wrong-pid", "wrong-start", "wrong-image"] {
        let dir = root(case);
        assert_eq!(run(&["lifecycle", "set", "running", "--reason", "keep running"], &dir).0, 0);
        let before = run(&["lifecycle", "status"], &dir).1["desired"].clone();
        let foreign = dir.join(std::path::Path::new(env!("CARGO_BIN_EXE_her")).file_name().unwrap());
        let image = if case == "foreign-directory" {
            std::fs::copy(env!("CARGO_BIN_EXE_her"), &foreign).unwrap();
            foreign.to_str().unwrap()
        } else { env!("CARGO_BIN_EXE_her") };
        let mut owner = console_owner(&dir, image);
        if case != "foreign-directory" {
            let marker_path = dir.join("data/logs/rbridge-console.lock.owner.json");
            let mut marker: Value = serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
            if case == "wrong-pid" {
                marker["identity"]["pid"] = std::process::id().into();
            } else if case == "wrong-start" {
                marker["identity"]["start"] = (marker["identity"]["start"].as_u64().unwrap() + 1).into();
            } else {
                marker["identity"]["exe"] = env!("CARGO_BIN_EXE_her-cli").into();
            }
            std::fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        }
        let (code, _, error) = run_image(env!("CARGO_BIN_EXE_her-cli"), &["lifecycle", "set", "stopped", "--confirm"], &dir);
        assert_eq!(code, 3, "{case}: {error}");
        assert!(owner.0.try_wait().unwrap().is_none(), "{case}: refused stop killed owner");
        assert_eq!(run(&["lifecycle", "status"], &dir).1["desired"], before,
            "{case}: refused stop must preserve desired state and revision");
        drop(owner);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn stopping_or_entering_maintenance_requires_explicit_confirmation() {
    let dir = root("confirmed-stop");
    assert_ne!(run(&["lifecycle", "stop-supervisor", "--confirm"], &dir).0, 0);
    assert_eq!(run(&["lifecycle", "status"], &dir).1["desired"]["state"], "running");
    for state in ["stopped", "maintenance"] {
        let (code, _, _) = run(&["lifecycle", "set", state], &dir);
        assert_ne!(code, 0, "{state} without confirmation must be refused");
        assert_eq!(run(&["lifecycle", "status"], &dir).1["desired"]["state"], "running");
        assert_eq!(run(&["lifecycle", "set", state, "--confirm"], &dir).0, 0);
        assert_eq!(run(&["lifecycle", "status"], &dir).1["desired"]["state"], state);
        assert_eq!(run(&["lifecycle", "set", "running"], &dir).0, 0);
    }
}

#[test]
fn confirmed_cli_state_changes_leave_an_audit_history() {
    let dir = root("lifecycle-audit");
    assert_eq!(run(&["lifecycle", "set", "stopped", "--confirm", "--by", "fixture", "--reason", "planned stop"], &dir).0, 0);
    assert_eq!(run(&["lifecycle", "set", "running", "--by", "fixture"], &dir).0, 0);
    let audit = std::fs::read_to_string(dir.join("data/lifecycle/audit.jsonl")).expect("lifecycle audit history");
    let entries: Vec<Value> = audit.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(entries.iter().any(|v| v["state"] == "stopped" && v["by"] == "fixture" && v["result"] == "applied"));
    assert!(entries.iter().any(|v| v["state"] == "running" && v["by"] == "fixture" && v["result"] == "applied"));
}

#[test]
fn task_list_reflects_explicit_enabled_configuration() {
    let dir = root("task-config");
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("resources")).unwrap();
    std::fs::write(dir.join("config/config.json"), r#"{"consoleToken":"fixture-token","tasks":{"upstream-check":{"enabled":true},"backup":{"enabled":false}}}"#).unwrap();
    std::fs::write(dir.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(dir.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
    let (code, list, _) = run(&["tasks", "list"], &dir);
    assert_eq!(code, 0);
    let tasks = list["tasks"].as_array().unwrap();
    assert_eq!(tasks.iter().find(|t| t["id"] == "upstream-check").unwrap()["enabled"], true);
    assert_eq!(tasks.iter().find(|t| t["id"] == "backup").unwrap()["enabled"], false);
}

#[test]
fn a_stopped_runtime_refuses_direct_console_startup() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let dir = root("stopped-console");
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("resources")).unwrap();
    std::fs::write(dir.join("config/config.json"), r#"{"consoleToken":"fixture-token"}"#).unwrap();
    std::fs::write(dir.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(dir.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
    assert_eq!(run(&["lifecycle", "set", "stopped", "--confirm"], &dir).0, 0);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    drop(listener);
    let mut child = hidden_command(env!("CARGO_BIN_EXE_her"))
        .args(["--console-serve", "--console-port", &port, "--runtime-root"]).arg(&dir)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("console started despite the stopped state");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
fn supervisor_reconciles_a_changed_desired_state_without_waiting_for_the_next_schedule() {
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};
    struct Group(Child);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL); }
            let _ = self.0.wait();
        }
    }
    let dir = root("reconcile");
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("resources")).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    std::fs::write(dir.join("config/config.json"), format!(r#"{{"consoleToken":"fixture-token","backends":{{"mcpRustBridge":true}},"rollout":{{"rustGroups":[]}},"consolePort":{port}}}"#)).unwrap();
    std::fs::write(dir.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(dir.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
    assert_eq!(run(&["lifecycle", "set", "stopped", "--confirm"], &dir).0, 0);
    let child = hidden_command(env!("CARGO_BIN_EXE_her"))
        .args(["supervise", "--runtime-root"]).arg(&dir).process_group(0)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let _group = Group(child);
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let (_, status, _) = run(&["lifecycle", "status"], &dir);
        if status["owners"]["supervisor"]["state"] == "held" { break; }
        assert!(Instant::now() < deadline, "supervisor did not take ownership");
        std::thread::sleep(Duration::from_millis(30));
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let (_, history, _) = run(&["tasks", "runs", "--task", "watchdog"], &dir);
        if history["runs"].as_array().is_some_and(|runs| runs.iter().any(|r| r["state"] == "succeeded" || r["state"] == "failed")) { break; }
        assert!(Instant::now() < deadline, "initial watchdog did not finish: {history}");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    assert_eq!(run(&["lifecycle", "set", "running"], &dir).0, 0);
    let deadline = Instant::now() + Duration::from_secs(12);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "console was not started after state changed");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(run(&["lifecycle", "set", "stopped", "--confirm"], &dir).0, 0);
    let deadline = Instant::now() + Duration::from_secs(12);
    while TcpStream::connect(("127.0.0.1", port)).is_ok() {
        assert!(Instant::now() < deadline, "console stayed running after stop");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn desired_state_persists_across_processes_and_unknown_is_reported() {
    let dir = root("desired");
    let (code, status, _) = run(&["lifecycle", "status"], &dir);
    assert_eq!(code, 0);
    assert_eq!(status["desired"]["state"], "running", "no record means running");
    assert_eq!(status["owners"]["live"]["state"], "free");

    let (code, set, err) = run(&["lifecycle", "set", "stopped", "--confirm", "--reason", "rollback drill", "--by", "operator"], &dir);
    assert_eq!(code, 0, "{err}");
    assert_eq!(set["record"]["revision"], 1);
    let (_, status, _) = run(&["lifecycle", "status"], &dir);
    assert_eq!(status["desired"]["state"], "stopped");
    assert_eq!(status["desired"]["wants"]["live"], false);
    assert_eq!(status["desired"]["record"]["reason"], "rollback drill");
    assert_eq!(status["desired"]["record"]["by"], "operator");

    let (code, _, _) = run(&["lifecycle", "set", "maintenance", "--confirm", "--minutes", "30"], &dir);
    assert_eq!(code, 0);
    let (_, status, _) = run(&["lifecycle", "status"], &dir);
    assert_eq!(status["desired"]["state"], "maintenance");
    assert_eq!(status["desired"]["wants"]["console"], true);
    assert!(status["desired"]["record"]["until"].is_number());

    std::fs::write(dir.join("data/lifecycle/desired-state.json"), b"\x00broken").unwrap();
    let (code, status, _) = run(&["lifecycle", "status"], &dir);
    assert_eq!(code, 3, "an unreadable desired state is reported, not guessed");
    assert_eq!(status["desired"]["state"], "unknown");

    let (code, set, _) = run(&["lifecycle", "set", "running", "--reason", "repaired"], &dir);
    assert_eq!(code, 0);
    assert_eq!(set["record"]["state"], "running");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn invalid_invocations_change_nothing() {
    let dir = root("usage");
    assert_eq!(run(&["lifecycle", "set", "paused"], &dir).0, 2);
    assert_eq!(run(&["lifecycle", "set", "stopped", "--confirm", "--minutes", "5"], &dir).0, 3, "only maintenance ends");
    assert_eq!(run(&["lifecycle", "set", "maintenance", "--confirm", "--minutes", "0"], &dir).0, 2);
    assert_eq!(run(&["lifecycle", "frobnicate"], &dir).0, 2);
    assert!(!dir.join("data/lifecycle/desired-state.json").exists());
    let missing = dir.join("no-such-root");
    assert_eq!(run(&["lifecycle", "status"], &missing).0, 2);
    let output = hidden_command(env!("CARGO_BIN_EXE_her")).args(["lifecycle", "status", "--runtime-root", "relative/root"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_held_owner_lock_is_reported_as_held() {
    let dir = root("owner");
    std::fs::create_dir_all(dir.join("data/logs")).unwrap();
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("data/logs/rbridge-live.lock")).unwrap();
    lock.try_lock().unwrap();
    let (_, status, _) = run(&["lifecycle", "status"], &dir);
    assert_eq!(status["owners"]["live"]["state"], "held");
    assert_eq!(status["owners"]["live"]["owner"], Value::Null, "an owner without a marker cannot be identified");
    assert_eq!(status["owners"]["supervisor"]["state"], "free");
    assert_eq!(run_image(env!("CARGO_BIN_EXE_her-cli"), &["lifecycle", "set", "stopped", "--confirm"], &dir).0, 3);
    assert_eq!(run(&["lifecycle", "status"], &dir).1["desired"]["state"], "running", "an unidentified owner cannot authorize a persisted stop");
    drop(lock);
    let (_, status, _) = run(&["lifecycle", "status"], &dir);
    assert_eq!(status["owners"]["live"]["state"], "free");
    assert!(dir.join("data/logs/rbridge-live.lock").exists(), "status never deletes a lock");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tasks_list_shows_the_registry_with_next_runs() {
    let dir = root("tasks");
    let (code, list, err) = run(&["tasks", "list"], &dir);
    assert_eq!(code, 0, "{err}");
    let tasks = list["tasks"].as_array().unwrap();
    let ids: Vec<&str> = tasks.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["watchdog", "memory-digest", "memory-digest-check", "backup", "upstream-check", "console-restart"]);
    let now = list["now"].as_i64().unwrap();
    let watchdog = &tasks[0];
    let next = watchdog["nextRunAt"].as_i64().unwrap();
    assert!(next > now && next - now <= 120_000, "the watchdog runs every two minutes");
    assert_eq!(tasks[4]["enabled"], false);
    let (code, runs, _) = run(&["tasks", "runs", "--task", "backup"], &dir);
    assert_eq!(code, 0);
    assert_eq!(runs["runs"].as_array().unwrap().len(), 0);
    assert_eq!(run(&["tasks", "runs", "--task", "nope"], &dir).0, 2);
    let _ = std::fs::remove_dir_all(dir);
}
