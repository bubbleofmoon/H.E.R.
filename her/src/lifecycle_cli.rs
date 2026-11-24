//! Selected-root lifecycle, assert-running health check and task CLI.
//!
//! Status and stop work without a valid config; maintenance operations hold the stopped owner gates.
//! Exit codes: 0 done, 1 unhealthy (assert-running), 2 usage/root error, 3 unknown/unwritten desired state.

use crate::lifecycle::{
    collect_verified_owners, effective, stop_verified_owners_transaction, DesiredRead,
    DesiredState, DesiredStore, Effective, ImagePolicy, LifecycleBackend as _, LiveLifecycle,
    OwnerProbe, OwnerRole, ProbeMode, VerifiedOwner,
};
use crate::task_runs::{credential_redactor, RunQuery, StoreLimits, TaskStore};
use crate::tasks::{Scheduler, TaskId, REGISTRY};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: her lifecycle status --runtime-root <root>\n       her-cli lifecycle assert-running --runtime-root <root> [--wait <duration>]\n       her-cli stop --runtime-root <root> --confirm [--with-dependencies]\n       her lifecycle set <running|stopped|maintenance> --runtime-root <root> [--confirm] [--by <who>] [--reason <text>] [--minutes <n>]\n       her lifecycle stop-supervisor --runtime-root <root> --confirm\n       her lifecycle reset-sessions --runtime-root <root> --confirm\n       her tasks list --runtime-root <root>\n       her tasks runs --runtime-root <root> [--task <id>] [--limit <n>]";
const CONVERSATION_USAGE: &str = "usage: her-cli conversation <admit|remove> <group:<id>|private:<id>> --runtime-root <root> --confirm";

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

fn roles_for_stop(supervisor: bool) -> &'static [OwnerRole] {
    if supervisor {
        &OwnerRole::ALL
    } else {
        &[OwnerRole::Live, OwnerRole::Console]
    }
}

pub(crate) fn stoppable_owners(data: &Path, supervisor: bool) -> Result<Vec<VerifiedOwner>, String> {
    collect_verified_owners(
        &mut LiveLifecycle,
        data,
        roles_for_stop(supervisor),
        &ImagePolicy::SameReleaseTwin,
        ProbeMode::Create,
        None,
    )
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
            let mut backend = LiveLifecycle;
            print(&json!({
                "desired": desired,
                "owners": {
                    "live": probe_json(backend.probe_owner(&data, OwnerRole::Live, ProbeMode::Create)),
                    "console": probe_json(backend.probe_owner(&data, OwnerRole::Console, ProbeMode::Create)),
                    "supervisor": probe_json(backend.probe_owner(&data, OwnerRole::Supervisor, ProbeMode::Create)),
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

fn stop_verified_owners_with_dependencies(data: &Path, _store: &DesiredStore, revision: u64, supervisor: bool, dependencies: Option<&crate::runtime::RuntimeContext>) -> Result<(), String> {
    stop_verified_owners_transaction(
        &mut LiveLifecycle,
        data,
        revision,
        roles_for_stop(supervisor),
        &ImagePolicy::SameReleaseTwin,
        ProbeMode::Create,
    )?;
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
            let mut scheduler = Scheduler::new(|d| crate::supervisor::enabled(&config, d), crate::local_time::configured_zone(&config), now);
            scheduler.refresh_schedules(&config, now);
            let latest = store.latest_by_task().unwrap_or_default();
            let tasks: Vec<Value> = REGISTRY
                .iter()
                .map(|d| {
                    json!({
                        "id": d.id.as_str(),
                        "title": d.title,
                        "schedule": scheduler.schedule(d.id).unwrap_or(d.schedule).describe(),
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

/// #186: `her-cli conversation <admit|remove> <group:<id>|private:<id>> --runtime-root <root> --confirm` admits or
/// removes one conversation in its allow list and `rollout.rustGroups` at once, through the console's
/// `POST /api/conversations/scope` handler (same gates, write and revocation). Its last stdout line is the JSON reply
/// (a failed background DSH session stop may print a diagnostic line before it).
/// Exit 0 done, 2 usage or a malformed request (400), 3 refused or failed.
pub fn run_conversation(args: &[String]) -> i32 {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("[conversation] {error}\n{CONVERSATION_USAGE}");
            return 2;
        }
    };
    let (action, conversation) = match parsed.positional.as_slice() {
        [action, conversation] if matches!(action.as_str(), "admit" | "remove") => (action.clone(), conversation.clone()),
        _ => {
            eprintln!("{CONVERSATION_USAGE}");
            return 2;
        }
    };
    if !parsed.confirm
        || parsed.with_dependencies
        || parsed.by.is_some()
        || parsed.reason.is_some()
        || parsed.minutes.is_some()
        || parsed.wait.is_some()
        || parsed.task.is_some()
        || parsed.limit.is_some()
        || parsed.expected_revision.is_some()
    {
        eprintln!("[conversation] --confirm is required and no other option applies\n{CONVERSATION_USAGE}");
        return 2;
    }
    let root = match root_path(parsed.root.as_deref()) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("[conversation] {error}");
            return 2;
        }
    };
    let context = match crate::runtime::RuntimeContext::from_root(root) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("[conversation] {error}");
            return 2;
        }
    };
    match crate::config_tx::change_conversation_scope(&context, &conversation, &action) {
        Ok(reply) => {
            println!("{reply}");
            0
        }
        Err(error) => {
            let status = error.http_status();
            eprintln!("[conversation] refused ({status}): {error}");
            if matches!(error, crate::social_state::SocialError::BadInput(_)) { 2 } else { 3 }
        }
    }
}
