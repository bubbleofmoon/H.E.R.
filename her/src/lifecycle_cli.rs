//! Selected-root lifecycle, assert-running health check and task CLI.
//!
//! Status and stop work without a valid config; maintenance operations hold the stopped owner gates.
//! Exit codes: 0 done, 1 unhealthy (assert-running), 2 usage/root error, 3 unknown/unwritten desired state.

use crate::lifecycle::{effective, DesiredRead, DesiredState, DesiredStore, Effective, OwnerGate, OwnerProbe, LIVE_LOCK, SUPERVISOR_LOCK};
use crate::task_runs::{credential_redactor, RunQuery, StoreLimits, TaskStore};
use crate::tasks::{local_offset_ms, Scheduler, TaskId, REGISTRY};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: her lifecycle status --runtime-root <root>\n       her-cli lifecycle assert-running --runtime-root <root> [--wait <duration>]\n       her-cli stop --runtime-root <root> --confirm [--with-dependencies]\n       her lifecycle set <running|stopped|maintenance> --runtime-root <root> [--confirm] [--by <who>] [--reason <text>] [--minutes <n>]\n       her lifecycle stop-supervisor --runtime-root <root> --confirm\n       her lifecycle reset-sessions --runtime-root <root> --confirm\n       her tasks list --runtime-root <root>\n       her tasks runs --runtime-root <root> [--task <id>] [--limit <n>]";

struct Args {
    positional: Vec<String>,
    root: Option<String>,
    by: Option<String>,
    reason: Option<String>,
    minutes: Option<String>,
    wait: Option<String>,
    task: Option<String>,
    limit: Option<String>,
    expected_revision: Option<String>,
    confirm: bool,
    with_dependencies: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args { positional: Vec::new(), root: None, by: None, reason: None, minutes: None, wait: None, task: None, limit: None, expected_revision: None, confirm: false, with_dependencies: false };
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i].as_str() {
            "--with-dependencies" => {
                if out.with_dependencies { return Err("--with-dependencies given twice".into()); }
                out.with_dependencies = true;
                i += 1;
                continue;
            }
            "--confirm" => {
                if out.confirm { return Err("--confirm given twice".to_string()); }
                out.confirm = true;
                i += 1;
                continue;
            }
            "--runtime-root" => &mut out.root,
            "--by" => &mut out.by,
            "--reason" => &mut out.reason,
            "--minutes" => &mut out.minutes,
            "--wait" => &mut out.wait,
            "--task" => &mut out.task,
            "--limit" => &mut out.limit,
            "--expected-revision" => &mut out.expected_revision,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            word => {
                out.positional.push(word.to_string());
                i += 1;
                continue;
            }
        };
        if slot.is_some() {
            return Err(format!("{} given twice", args[i]));
        }
        *slot = Some(args.get(i + 1).ok_or_else(|| format!("{} needs a value", args[i]))?.clone());
        i += 2;
    }
    Ok(out)
}

/// Validate the CLI root syntax without inspecting runtime contents.
fn root_path(root: Option<&str>) -> Result<&Path, String> {
    let root = root.filter(|r| !r.trim().is_empty()).ok_or("--runtime-root is required")?;
    let root = Path::new(root);
    crate::install_root::check_runtime(root, false)?;
    if !root.is_absolute() {
        return Err("--runtime-root must be an absolute path".to_string());
    }
    Ok(root)
}

/// `<root>/data`, which must be a real directory (not a link) of an existing root.
fn data_dir(root: Option<&str>) -> Result<PathBuf, String> {
    let data = root_path(root)?.join("data");
    let meta = std::fs::symlink_metadata(&data).map_err(|_| "runtime data directory is unavailable".to_string())?;
    if !meta.is_dir() {
        return Err("runtime data directory is unavailable".to_string());
    }
    Ok(data)
}

fn probe_json(probe: OwnerProbe) -> Value {
    match probe {
        OwnerProbe::Free { last } => json!({ "state": "free", "last": last }),
        OwnerProbe::Held { marker: Some(marker) } => json!({ "state": "held", "owner": marker }),
        OwnerProbe::Held { marker: None } => json!({ "state": "held", "owner": null, "note": "owner cannot be identified" }),
        OwnerProbe::Unknown(reason) => json!({ "state": "unknown", "reason": reason }),
    }
}

fn print(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

/// The console and GUI images may control each other only inside the same release directory.
pub(crate) fn authorized_image(caller: &str, owner: &str) -> bool {
    let equal = |a: &str, b: &str| if cfg!(windows) {
        a.replace('/', "\\").eq_ignore_ascii_case(&b.replace('/', "\\"))
    } else { a == b };
    if equal(caller, owner) { return true; }
    let caller = Path::new(caller);
    let sibling = match caller.file_name().and_then(|s| s.to_str()) {
        Some(name) if equal(name, if cfg!(windows) { "her.exe" } else { "her" }) =>
            caller.with_file_name(if cfg!(windows) { "her-cli.exe" } else { "her-cli" }),
        Some(name) if equal(name, if cfg!(windows) { "her-cli.exe" } else { "her-cli" }) =>
            caller.with_file_name(if cfg!(windows) { "her.exe" } else { "her" }),
        _ => return false,
    };
    let (Ok(caller), Ok(sibling), Ok(owner)) = (
        std::fs::canonicalize(caller), std::fs::canonicalize(sibling), std::fs::canonicalize(owner)
    ) else { return false };
    equal(&caller.parent().unwrap().to_string_lossy(), &sibling.parent().unwrap().to_string_lossy())
        && equal(&sibling.to_string_lossy(), &owner.to_string_lossy())
}

pub(crate) fn stoppable_owners(data: &Path, supervisor: bool) -> Result<Vec<(OwnerGate, crate::lifecycle::OwnerMarker)>, String> {
    crate::lifecycle::validate_owner_gate_paths(data)?;
    let me = crate::process_identity::current_identity()?;
    let mut owners = Vec::new();
    let roles = [(LIVE_LOCK, "live"), ("rbridge-console.lock", "console"), (SUPERVISOR_LOCK, "supervisor")];
    for (name, role) in roles.into_iter().take(if supervisor { 3 } else { 2 }) {
        let gate = OwnerGate::new(data.join("logs").join(name));
        let owner = match gate.probe() {
            OwnerProbe::Free { .. } => continue,
            OwnerProbe::Held { marker: Some(marker) } if marker.role == role => marker,
            _ => return Err(format!("{role} owner cannot be identified")),
        };
        if !authorized_image(&me.exe, &owner.identity.exe) {
            return Err(format!("{role} owner belongs to another executable"));
        }
        if crate::process_identity::check_identity(&owner.identity) != crate::process_identity::Liveness::Matches {
            return Err(format!("{role} owner process identity cannot be verified"));
        }
        owners.push((gate, owner));
    }
    Ok(owners)
}

pub fn run_lifecycle(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("guard") {
        return guard_stopped(args);
    }
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("[lifecycle] {error}\n{USAGE}");
            return 2;
        }
    };
    if parsed.with_dependencies && (parsed.positional.len() != 1 || parsed.positional[0] != "stop") {
        eprintln!("[lifecycle] --with-dependencies requires stop\n{USAGE}"); return 2;
    }
    if parsed.positional.first().map(String::as_str) == Some("assert-running") {
        if parsed.positional.len() != 1 || parsed.confirm || parsed.by.is_some()
            || parsed.reason.is_some() || parsed.minutes.is_some() || parsed.task.is_some()
            || parsed.limit.is_some() || parsed.expected_revision.is_some()
        {
            eprintln!("[lifecycle] invalid assert-running arguments\n{USAGE}");
            return 2;
        }
        let root = match root_path(parsed.root.as_deref()) {
            Ok(root) => root,
            Err(error) => { eprintln!("[lifecycle] {error}"); return 2; }
        };
        return crate::health_check::assert_running(root, parsed.wait.as_deref());
    }
    let data = match data_dir(parsed.root.as_deref()) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("[lifecycle] {error}");
            return 2;
        }
    };
    let store = DesiredStore::new(data.join("lifecycle"));
    let now = crate::parity_clock::now_ms();
    match parsed.positional.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["stop"] if parsed.confirm && parsed.minutes.is_none() && parsed.wait.is_none()
            && parsed.task.is_none() && parsed.limit.is_none() && parsed.expected_revision.is_none() => {
            let context = if parsed.with_dependencies {
                match crate::runtime::RuntimeContext::from_root(Path::new(parsed.root.as_deref().unwrap())) {
                    Ok(context) => Some(context),
                    Err(error) => { eprintln!("[lifecycle] dependency stop refused: {error}"); return 3; }
                }
            } else { None };
            if let Err(error) = stoppable_owners(&data, false) {
                eprintln!("[lifecycle] stop refused without changing desired state: {error}"); return 3;
            }
            let record = match store.write(DesiredState::Stopped, parsed.by.as_deref().unwrap_or("cli"), parsed.reason.as_deref().unwrap_or("explicit stop"), None, now) {
                Ok(record) => record,
                Err(error) => { eprintln!("[lifecycle] {error}"); return 3; }
            };
            match stop_verified_owners_with_dependencies(&data, &store, record.revision, false, context.as_ref()) {
                Ok(()) => { print(&json!({"ok":true,"record":record})); 0 }
                Err(error) => { eprintln!("[lifecycle] stop was recorded but not completed: {error}"); 3 }
            }
        }
        ["status"] => {
            let read = store.read();
            let desired = match (&read, effective(&read, now)) {
                (_, Effective::Unknown(reason)) => json!({ "state": "unknown", "reason": reason }),
                (DesiredRead::Present(record), Effective::Act { state, wants }) => {
                    json!({ "state": state.as_str(), "wants": wants, "record": record })
                }
                (_, Effective::Act { state, wants }) => json!({ "state": state.as_str(), "wants": wants, "record": null }),
            };
            let unknown = desired["state"] == "unknown";
            print(&json!({
                "desired": desired,
                "owners": {
                    "live": probe_json(OwnerGate::new(data.join("logs").join(LIVE_LOCK)).probe()),
                    "console": probe_json(OwnerGate::new(data.join("logs").join("rbridge-console.lock")).probe()),
                    "supervisor": probe_json(OwnerGate::new(data.join("logs").join(SUPERVISOR_LOCK)).probe()),
                },
            }));
            if unknown {
                3
            } else {
                0
            }
        }
        ["set", state] => {
            let Some(state) = DesiredState::parse(state) else {
                eprintln!("[lifecycle] unknown state {state}\n{USAGE}");
                return 2;
            };
            if matches!(state, DesiredState::Stopped | DesiredState::Maintenance) && !parsed.confirm {
                eprintln!("[lifecycle] --confirm is required for stopped or maintenance");
                return 2;
            }
            let until = match parsed.minutes.as_deref().map(str::parse::<u32>) {
                None => None,
                Some(Ok(minutes)) if minutes > 0 => Some(now + minutes as i64 * 60_000),
                Some(_) => {
                    eprintln!("[lifecycle] --minutes must be a positive integer");
                    return 2;
                }
            };
            let by = parsed.by.unwrap_or_else(|| "cli".to_string());
            let expected = match parsed.expected_revision.as_deref().map(str::parse::<u64>) {
                None => None,
                Some(Ok(revision)) => Some(revision),
                Some(Err(_)) => { eprintln!("[lifecycle] --expected-revision must be an unsigned integer"); return 2; }
            };
            // Refused authorization must not publish a stop intent to the supervisor.
            // Owners are checked again after the write, before the pinned verified kills.
            if state == DesiredState::Stopped {
                if let Err(error) = stoppable_owners(&data, false) {
                    eprintln!("[lifecycle] stop refused without changing desired state: {error}");
                    return 3;
                }
            }
            match store.write_if_revision(state, &by, parsed.reason.as_deref().unwrap_or(""), until, now, expected) {
                Ok(record) => {
                    if state == DesiredState::Stopped {
                        if let Err(error) = stop_verified_owners(&data, &store, record.revision, false) {
                            eprintln!("[lifecycle] stopped state was recorded but owners were not confirmed stopped: {error}");
                            return 3;
                        }
                    }
                    print(&json!({ "ok": true, "record": record }));
                    0
                }
                Err(error) => {
                    eprintln!("[lifecycle] {error}");
                    3
                }
            }
        }
        ["stop-supervisor"] if parsed.confirm => {
            let DesiredRead::Present(record) = store.read() else {
                eprintln!("[lifecycle] an explicit stopped state is required"); return 3;
            };
            match stop_verified_owners(&data, &store, record.revision, true) {
                Ok(()) => { print(&json!({"ok":true,"supervisorStopped":true})); 0 }
                Err(error) => { eprintln!("[lifecycle] supervisor stop refused: {error}"); 3 }
            }
        }
        ["reset-sessions"] if parsed.confirm => {
            let _guard = match crate::lifecycle::acquire_quiescent(&data) {
                Ok(guard) => guard,
                Err(error) => { eprintln!("[lifecycle] {error}"); return 3; }
            };
            let Some(root) = parsed.root.as_deref() else { return 2 };
            let context = match crate::runtime::RuntimeContext::from_root(Path::new(root)) {
                Ok(context) => context,
                Err(error) => { eprintln!("[lifecycle] {error}"); return 2; }
            };
            match crate::session_map::reset_all(&context) {
                Ok(backup) => { print(&json!({"ok":true,"backup":backup})); 0 }
                Err(error) => { eprintln!("[lifecycle] session reset refused: {error}"); 3 }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

/// A local confirmed stop must work even if the supervisor or its configuration is unavailable.
/// The desired gate prevents a resume while verified owners are being retired.
pub(crate) fn stop_verified_owners(data: &Path, store: &DesiredStore, revision: u64, supervisor: bool) -> Result<(), String> {
    stop_verified_owners_with_dependencies(data, store, revision, supervisor, None)
}

fn stop_verified_owners_with_dependencies(data: &Path, store: &DesiredStore, revision: u64, supervisor: bool, dependencies: Option<&crate::runtime::RuntimeContext>) -> Result<(), String> {
    use crate::lifecycle::OwnerProbe;
    use crate::process_identity::KillOutcome;
    use std::time::{Duration, Instant};

    crate::lifecycle::validate_owner_gate_paths(data)?;
    let _gate = crate::lifecycle::lock_with_wait(&data.join("lifecycle/lifecycle.lock"), Duration::from_secs(5))?;
    if !matches!(store.read(), DesiredRead::Present(record) if record.state == DesiredState::Stopped && record.revision == revision) {
        return Err("the stop was superseded by another lifecycle transition".into());
    }
    // A running task may itself need the lifecycle gate. Never hold it while
    // waiting for that task to observe the stopped state and release its gate.
    drop(_gate);
    let _tasks = crate::lifecycle::hold_stopped_task_gates(data)?;
    let _gate = crate::lifecycle::lock_with_wait(&data.join("lifecycle/lifecycle.lock"), Duration::from_secs(5))?;
    if !matches!(store.read(), DesiredRead::Present(record) if record.state == DesiredState::Stopped && record.revision == revision) {
        return Err("the stop was superseded by another lifecycle transition".into());
    }
    for (owner_gate, owner) in stoppable_owners(data, supervisor)? {
        let role = &owner.role;
        match crate::process_identity::kill_verified(&owner.identity) {
            KillOutcome::Killed | KillOutcome::Gone => {}
            other => return Err(format!("{role} owner could not be stopped: {other:?}")),
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match owner_gate.probe() {
                OwnerProbe::Free { .. } => break,
                OwnerProbe::Held { .. } if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => return Err(format!("{role} owner did not release its gate")),
            }
        }
    }
    if let Some(context) = dependencies {
        crate::supervisor_dependencies::stop(context)?;
    }
    Ok(())
}

/// Run a local maintenance command only while the desired state and all three owner gates remain
/// exclusively held. This is the transaction boundary for restoring old scheduler definitions.
fn guard_stopped(args: &[String]) -> i32 {
    if args.len() < 5 || args[1] != "--runtime-root" || args[3] != "--" || !Path::new(&args[4]).is_absolute() {
        eprintln!("usage: her lifecycle guard --runtime-root <root> -- <absolute executable> [arguments...]");
        return 2;
    }
    let data = match data_dir(Some(&args[2])) {
        Ok(data) => data,
        Err(error) => { eprintln!("[lifecycle] {error}"); return 2; }
    };
    let _guard = match crate::lifecycle::acquire_quiescent(&data) {
        Ok(gate) => gate,
        Err(error) => { eprintln!("[lifecycle] {error}"); return 3; }
    };
    let external = match args[4..].iter().map(crate::subprocess::external_path).collect::<Result<Vec<_>, _>>() {
        Ok(args) => args,
        Err(error) => { eprintln!("[lifecycle] {error}"); return 3; }
    };
    let mut child = match crate::subprocess::command_without_console(&external[0]).args(&external[1..]).spawn() {
        Ok(child) => child,
        Err(error) => { eprintln!("[lifecycle] guarded command could not start: {}", error.kind()); return 3; }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(3),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
            Ok(None) => {
                if child.kill().is_err() || child.wait().is_err() {
                    eprintln!("[lifecycle] timed-out guarded command could not be confirmed stopped");
                    break 3;
                }
                break 124;
            }
            Err(error) => { eprintln!("[lifecycle] guarded command status is unknown: {}", error.kind()); break 3; }
        }
    };
    code
}

pub fn run_tasks(args: &[String]) -> i32 {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("[tasks] {error}\n{USAGE}");
            return 2;
        }
    };
    if parsed.with_dependencies { eprintln!("[tasks] --with-dependencies requires stop"); return 2; }
    let data = match data_dir(parsed.root.as_deref()) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("[tasks] {error}");
            return 2;
        }
    };
    let store = match TaskStore::open(data.join("tasks"), StoreLimits::default(), credential_redactor(Vec::new())) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("[tasks] task store unavailable: {}", error.kind());
            return 2;
        }
    };
    let now = crate::parity_clock::now_ms();
    match parsed.positional.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["list"] => {
            let config = parsed.root.as_deref()
                .and_then(|root| crate::runtime::RuntimeContext::from_root(Path::new(root)).ok())
                .and_then(|context| context.load_config().ok())
                .unwrap_or(Value::Null);
            let scheduler = Scheduler::new(|d| crate::supervisor::enabled(&config, d), local_offset_ms(), now);
            let latest = store.latest_by_task().unwrap_or_default();
            let tasks: Vec<Value> = REGISTRY
                .iter()
                .map(|d| {
                    json!({
                        "id": d.id.as_str(),
                        "title": d.title,
                        "schedule": d.schedule.describe(),
                        "enabled": scheduler.enabled(d.id),
                        "nextRunAt": scheduler.next_run(d.id),
                        "lastRun": latest.get(d.id.as_str()),
                    })
                })
                .collect();
            print(&json!({ "now": now, "tasks": tasks }));
            0
        }
        ["runs"] => {
            let task = match parsed.task.as_deref() {
                None => None,
                Some(id) => match TaskId::parse(id) {
                    Some(id) => Some(id.as_str().to_string()),
                    None => {
                        eprintln!("[tasks] unknown task {id}");
                        return 2;
                    }
                },
            };
            let limit = match parsed.limit.as_deref().map(str::parse::<usize>) {
                None => None,
                Some(Ok(limit)) => Some(limit),
                Some(Err(_)) => {
                    eprintln!("[tasks] --limit must be a non-negative integer");
                    return 2;
                }
            };
            match store.list(&RunQuery { task, before: None, limit }) {
                Ok(page) => {
                    print(&json!({ "runs": page.runs, "nextBefore": page.next_before }));
                    0
                }
                Err(error) => {
                    eprintln!("[tasks] {error}");
                    3
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}
