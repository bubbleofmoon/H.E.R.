//! Selected-root supervisor and the real effects used by the watchdog task.

use crate::lifecycle::{
    self, Acquire, DesiredRead, DesiredState, DesiredStore, Effective, ImagePolicy,
    LifecycleBackend as _, LiveLifecycle, OwnerGate, OwnerObservation, OwnerProbe, OwnerRole,
    ProbeMode, SUPERVISOR_LOCK,
};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use crate::task_runs::{Finish, RunHandle, RunSource, StoreLimits, TaskStore};
use crate::tasks::{self, Clock, CommandExecutor, Dispatcher, Executor, Scheduler, SystemClock, TaskDef, TaskId};
use crate::watchdog::{self, ConsoleProbe, Effects, Observation, WatchState};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const RESTART_REQUEST: &str = "lifecycle/console-restart.json";

// Windows can take about two seconds to report WSAECONNREFUSED even on
// loopback. Leave margin for that response; a timeout still proves nothing.
#[cfg(windows)]
const LOOPBACK_CONNECT_BUDGET: Duration = Duration::from_secs(5);
#[cfg(not(windows))]
const LOOPBACK_CONNECT_BUDGET: Duration = Duration::from_millis(500);

const RESTART_WINDOW_MS: i64 = 5 * 60 * 1000;

struct RestartRequest {
    id: String,
    at: i64,
    not_before: i64,
    authority: Option<crate::console_auth::ConsoleAuthority>,
}

fn remove_expired_bad_restart(context: &RuntimeContext) -> Result<(), String> {
    let path = context.resolve_path(RuntimeArea::Data, RESTART_REQUEST, PathIntent::ReadFile)?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| "restart request age is unavailable")?;
    if !metadata.is_file() || crate::guarded_file::is_link(&metadata) {
        return Err("restart request path is unavailable".into());
    }
    let age = metadata.modified().ok().and_then(|modified| modified.elapsed().ok())
        .ok_or("restart request age is unavailable")?;
    if age <= Duration::from_millis(RESTART_WINDOW_MS as u64) {
        return Err("restart request is invalid".into());
    }
    let current = context.resolve_path(RuntimeArea::Data, RESTART_REQUEST, PathIntent::ReadFile)?;
    if current != path || std::fs::symlink_metadata(&current).ok().and_then(|meta| meta.modified().ok()) != metadata.modified().ok() {
        return Err("restart request changed".into());
    }
    context.remove_file(RuntimeArea::Data, RESTART_REQUEST)?;
    crate::guarded_file::sync_directory(path.parent().ok_or("restart request path is invalid")?)
        .map_err(|_| "restart request cleanup is unavailable")?;
    crate::safe_eprintln!("[supervisor] removed expired unreadable restart request");
    Ok(())
}

/// Called with the configuration gate held from authorization to publication.
pub(crate) fn queue_console_restart(context: &RuntimeContext, body: &str) -> Result<Value, (u16, String)> {
    let request: Value = serde_json::from_str(body).map_err(|_| (400, "body must be a JSON object".into()))?;
    let id = request.get("requestId").and_then(Value::as_str).filter(|id| crate::task_runs::valid_request_id(id))
        .ok_or((400, "a valid requestId is required".into()))?;
    if request.get("confirm").and_then(Value::as_bool) != Some(true) { return Err((400, "explicit confirmation is required".into())); }
    let authority = context.console_authority.as_ref().ok_or((401, "console credential is unavailable".into()))?;
    if !matches!(desired(context), Effective::Act { wants, .. } if wants.console && wants.jobs) {
        return Err((409, "desired state does not permit restart".into()));
    }
    if !matches!(
        lifecycle::verify_owner(
            &mut LiveLifecycle,
            context.data_dir(),
            OwnerRole::Supervisor,
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        ),
        OwnerObservation::Verified(_)
    ) {
        return Err((503, "supervisor ownership is unavailable".into()));
    }
    match inspect_restart_request(context).map_err(|e| (503, e))? {
        Some(pending) if pending.id == id => {
            return Ok(serde_json::json!({"ok":true,"state":"queued","requestId":id,"duplicate":true}));
        }
        Some(_) => return Err((409, "another restart is pending".into())),
        None => {}
    }
    let now = crate::parity_clock::now_ms();
    let record = serde_json::json!({"v":2,"requestId":id,"authority":authority,"at":now,"notBefore":now+1500});
    let bytes = serde_json::to_vec(&record).map_err(|_| (503, "restart request could not be serialized".into()))?;
    let unavailable = || (503, "restart request could not be published".into());
    let (target, staged) = context.stage_atomic_from(RuntimeArea::Data, RESTART_REQUEST, &mut bytes.as_slice())
        .map_err(|_| unavailable())?;
    #[cfg(feature = "parity-clock")]
    if std::env::var_os("HER_TEST_RESTART_QUEUE_CUT").is_some_and(|path| Path::new(&path).is_file()) {
        return Err(unavailable());
    }
    staged.commit(&mut || context.revalidate_atomic_target(RuntimeArea::Data, RESTART_REQUEST, &target))
        .map_err(|_| unavailable())?;
    Ok(serde_json::json!({"ok":true,"state":"queued","requestId":id}))
}

fn read_restart_request(context: &RuntimeContext) -> Result<Option<RestartRequest>, String> {
    let Some(bytes) = context.read_protected_data_file(RESTART_REQUEST, 4096)? else {
        return Ok(None);
    };
    let invalid = || "restart request is invalid".to_string();
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let id = value["requestId"].as_str().filter(|id| crate::task_runs::valid_request_id(id)).ok_or_else(invalid)?;
    let at = value["at"].as_i64().filter(|at| *at >= 0).ok_or_else(invalid)?;
    let not_before = value["notBefore"].as_i64().filter(|next| *next >= at).ok_or_else(invalid)?;
    let authority = match value["v"].as_u64() {
        Some(1) if value["tokenDigest"].as_str().and_then(crate::console_auth::decode_digest::<32>).is_some() => None,
        Some(2) => Some(serde_json::from_value(value["authority"].clone()).map_err(|_| invalid())?),
        _ => return Err(invalid()),
    };
    Ok(Some(RestartRequest { id: id.into(), at, not_before, authority }))
}

/// Queue inspection and cleanup serialize with enqueue and execution.
fn inspect_restart_request(context: &RuntimeContext) -> Result<Option<RestartRequest>, String> {
    match read_restart_request(context) {
        Ok(record) => Ok(record),
        Err(_) => remove_expired_bad_restart(context).map(|()| None),
    }
}

fn clear_restart_request(context: &RuntimeContext, id: &str) {
    let Ok(_guard) = crate::conversation_state::acquire(context) else { return };
    if read_restart_request(context).ok().flatten().is_some_and(|record| record.id == id) {
        if let Err(error) = context.remove_protected_data_file(RESTART_REQUEST) {
            crate::safe_eprintln!("[supervisor] restart request cleanup failed: {error}");
        }
    }
}

fn dispatch_restart(context: &RuntimeContext, dispatcher: &Arc<Dispatcher>) {
    #[cfg(feature = "parity-clock")]
    if std::env::var_os("HER_TEST_RESTART_PAUSE_FILE").is_some_and(|path| Path::new(&path).is_file()) {
        return;
    }
    let request = {
        let Ok(_guard) = crate::conversation_state::acquire(context) else { return };
        let Ok(Some(request)) = inspect_restart_request(context) else { return };
        request
    };
    if request.not_before > crate::parity_clock::now_ms() { return; }
    let id = request.id.as_str();
    match dispatcher.start(tasks::task_def(TaskId::ConsoleRestart), RunSource::Manual, Some(id), None, "console-restart") {
        tasks::Trigger::Duplicate(run) if run.state.is_terminal() => clear_restart_request(context, id),
        tasks::Trigger::Refused(_) => clear_restart_request(context, id),
        _ => {}
    }
}

fn restart_console(context: &RuntimeContext, state: &Mutex<WatchState>, run: &mut RunHandle) -> Finish {
    let Some(id) = run.view().request_id.clone() else { return Finish::Failed { exit_code: None, reason: "restart request id is missing".into() } };
    let result = (|| -> Result<(), String> {
        let _watch = state.lock().unwrap_or_else(|p| p.into_inner());
        let social = crate::conversation_state::acquire(context).map_err(|(_, e)| e)?;
        let request = read_restart_request(context)?.ok_or("restart request is missing")?;
        if request.id != id { return Err("restart request changed".into()); }
        let now = crate::parity_clock::now_ms();
        if request.at > now.saturating_add(60_000) || request.not_before > now
            || now.saturating_sub(request.at) > RESTART_WINDOW_MS
        {
            return Err("restart request expired".into());
        }
        let proof = request.authority.ok_or("a fresh restart request is required")?;
        let cfg = context.load_config().map_err(str::to_string)?;
        let desired_gate = lifecycle::lock_with_wait(&context.data_dir().join("lifecycle/lifecycle.lock"), Duration::from_secs(5))?;
        if !matches!(desired(context), Effective::Act { wants, .. } if wants.console && wants.jobs) { return Err("desired state no longer permits restart".into()); }
        let me = lifecycle::current_identity()?;
        match lifecycle::verify_owner(
            &mut LiveLifecycle,
            context.data_dir(),
            OwnerRole::Supervisor,
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        ) {
            OwnerObservation::Verified(owner)
                if lifecycle::same_identity(&owner.marker.identity, &me) => {}
            _ => return Err("only the supervisor can restart the console".into()),
        }
        let target = watchdog::live_target_from_config(&cfg)?;
        let pid = match console_probe(context, target.console_port) {
            ConsoleProbe::Managed { pid } => pid,
            _ => return Err("console owner cannot be verified".into()),
        };
        let owner = match lifecycle::verify_owner(
            &mut LiveLifecycle,
            context.data_dir(),
            OwnerRole::Console,
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        ) {
            OwnerObservation::Verified(owner) if owner.marker.identity.pid == pid => owner,
            _ => return Err("console owner changed".into()),
        };
        proof.recheck(&context.clone().with_console_port(target.console_port), &cfg).map_err(|(_, error)| error)?;
        lifecycle::retire_verified_owners(
            &mut LiveLifecycle,
            context.data_dir(),
            &[owner],
            ProbeMode::Create,
            Duration::from_secs(10),
            Duration::from_millis(50),
            None,
        )
        .map_err(|failure| failure.reason)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !matches!(console_probe(context, target.console_port), ConsoleProbe::Absent) {
            if std::time::Instant::now() >= deadline { return Err("console did not release its owner and port".into()); }
            std::thread::sleep(Duration::from_millis(50));
        }
        let live_gate = owner_gate(context, lifecycle::LIVE_LOCK)?;
        let effects = RealEffects { context: context.clone(), live_gate };
        let child = effects.launch_console(target.console_port)?;
        drop(social);
        drop(desired_gate);
        effects.await_console(child, target.console_port)?;
        run.log(crate::task_runs::LogStream::Meta, "replacement console is ready");
        Ok(())
    })();
    clear_restart_request(context, &id);
    match result {
        Ok(()) => Finish::Succeeded,
        Err(reason) => Finish::Failed { exit_code: None, reason },
    }
}

fn task_config<'a>(cfg: &'a Value, name: &str) -> Option<&'a Value> {
    cfg.get("tasks").and_then(|tasks| tasks.get(name))
}

pub fn enabled(cfg: &Value, def: &TaskDef) -> bool {
    task_config(cfg, def.id.as_str()).and_then(|task| task.get("enabled"))
        .and_then(Value::as_bool).unwrap_or(def.enabled_by_default)
}

/// `enabled` and not paused by the owner on the Schedule page.
pub fn enabled_in(context: &RuntimeContext, cfg: &Value, def: &TaskDef) -> bool {
    enabled(cfg, def) && !crate::schedule::system_job_paused(context, def.id.as_str())
}

fn limits(cfg: &Value) -> StoreLimits {
    let config = task_config(cfg, "limits");
    let n = |key: &str, default: u64, min: u64, max: u64| {
        config.and_then(|v| v.get(key)).and_then(Value::as_u64).unwrap_or(default).clamp(min, max)
    };
    let defaults = StoreLimits::default();
    StoreLimits {
        max_history_bytes: n("maxHistoryBytes", defaults.max_history_bytes, 4096, 64 * 1024 * 1024),
        keep_runs: n("keepRuns", defaults.keep_runs as u64, 1, 10_000) as usize,
        max_log_bytes: n("maxLogBytes", defaults.max_log_bytes, 1024, 16 * 1024 * 1024),
        tail_lines: n("tailLines", defaults.tail_lines as u64, 1, 2000) as usize,
        max_line_chars: n("maxLineChars", defaults.max_line_chars as u64, 80, 10_000) as usize,
    }
}

fn config_secrets(cfg: &Value, out: &mut Vec<String>) {
    match cfg {
        Value::Object(fields) => for (key, value) in fields {
            let key = key.to_ascii_lowercase().replace(['-', '_'], "");
            if key.contains("token") || key.contains("secret") || key.contains("password") || key == "apikey" {
                if let Some(value) = value.as_str().filter(|s| !s.is_empty()) { out.push(value.to_string()); }
            }
            config_secrets(value, out);
        },
        Value::Array(items) => for value in items { config_secrets(value, out); },
        _ => {}
    }
}

fn watchdog_states() -> &'static Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<WatchState>>>> {
    static STATES: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<WatchState>>>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

struct RuntimeExecutor {
    context: RuntimeContext,
    inner: CommandExecutor,
}

impl Executor for RuntimeExecutor {
    fn execute(&self, def: &TaskDef, run: &mut RunHandle, cancel: &AtomicBool) -> Finish {
        if !def.runs_when_held {
            match desired(&self.context) {
                Effective::Act { wants, .. } if wants.jobs => {}
                _ => return Finish::Cancelled { reason: "desired state no longer permits background jobs".into() },
            }
        }
        let result = self.inner.execute(def, run, cancel);
        let failure = match &result {
            Finish::Failed { reason, .. } => Some((crate::run_record::Outcome::Failed, reason)),
            Finish::Unknown { reason } => Some((crate::run_record::Outcome::Unknown, reason)),
            _ => None,
        };
        if let Some((outcome, reason)) = failure {
            let trigger = match run.view().source {
                RunSource::Scheduled => crate::run_record::Trigger::Timer,
                RunSource::Manual => crate::run_record::Trigger::Manual,
                RunSource::Recovery => crate::run_record::Trigger::Recovery,
                RunSource::Login => crate::run_record::Trigger::Startup,
            };
            if let Some(writer) = crate::recovery_log::writer(&self.context) {
                let summary = format!("task {} failed: {reason}", def.id.as_str());
                let record = crate::run_record::RecordInput {
                    run_id: Some(run.run_id()), trigger, component: crate::run_record::Component::Process,
                    event: "task-failed", outcome, reason: &summary,
                };
                if let Err(error) = writer.record(&record) { crate::safe_eprintln!("[tasks] recovery record failed: {error}"); }
                if let Ok(cfg) = self.context.load_config() {
                    if def.id != TaskId::Backup {
                        let key = if def.id == TaskId::Watchdog { "watchdog".to_string() } else { format!("task-{}", def.id.as_str()) };
                        crate::admin_alert::notify_notice(&self.context, &cfg, &summary, &key);
                    }
                }
            }
            if def.id == TaskId::Backup {
                if let Ok(cfg) = self.context.load_config() {
                    crate::backup::notify_failure(&self.context, &cfg, run.run_id(), reason);
                }
            }
        }
        result
    }
}

/// Construct the same fixed registry and store for the supervisor and the authenticated console.
#[cfg(test)]
thread_local! {
    pub(crate) static TASK_ADMISSION_TEST: std::cell::RefCell<Option<Box<dyn Fn() + Send + Sync>>> =
        std::cell::RefCell::new(None);
}

pub fn task_api(context: &RuntimeContext, cfg: &Value) -> Result<crate::console_tasks::TasksApi, String> {
    let root = context.runtime_root().ok_or("tasks require a selected runtime root")?.to_path_buf();
    let store_dir = context.resolve_path(RuntimeArea::Data, "tasks", PathIntent::CreateDirectory)
        .map_err(str::to_string)?;
    let redact_context = context.clone();
    let mut initial_secrets = Vec::new();
    config_secrets(cfg, &mut initial_secrets);
    initial_secrets.push(context.effective_console_token(cfg));
    let store = Arc::new(TaskStore::open(store_dir, limits(cfg), Arc::new(move |s| {
        let mut secrets = initial_secrets.clone();
        secrets.extend(crate::activity::known_session_tokens(&redact_context));
        if let Ok(current) = redact_context.load_config() {
            config_secrets(&current, &mut secrets);
            secrets.push(redact_context.effective_console_token(&current));
        }
        crate::pipeline::redact_sensitive_text(s, &secrets)
    }))
        .map_err(|e| format!("task store unavailable: {}", e.kind()))?);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    store.recover(clock.now_ms())?;
    let state = watchdog_states().lock().unwrap_or_else(|p| p.into_inner())
        .entry(root.clone()).or_insert_with(|| Arc::new(Mutex::new(WatchState::default()))).clone();
    let mut watchdog_context = context.clone();
    watchdog_context.console_authority = None;
    let internal = Box::new(move |def: &TaskDef, run: &mut RunHandle, cancel: &AtomicBool| {
        if cancel.load(Ordering::SeqCst) {
            return Finish::Cancelled { reason: "cancelled before watchdog observation".into() };
        }
        if matches!(def.id, TaskId::MemoryDigest | TaskId::MemoryDigestCheck) { crate::digest::run(&watchdog_context, def, run, cancel) }
        else if def.id == TaskId::Backup { crate::backup::run(&watchdog_context, run, cancel, def.timeout) }
        else if def.id == TaskId::ConsoleRestart { restart_console(&watchdog_context, &state, run) }
        else { watchdog_tick(&watchdog_context, &state, run) }
    });
    let node = task_config(cfg, "nodePath").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        .map(PathBuf::from).unwrap_or_else(|| PathBuf::from("node"));
    let install = task_config(cfg, "installRoot").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        .map(PathBuf::from).unwrap_or_else(|| root.clone());
    if !install.is_absolute() { return Err("tasks.installRoot must be absolute".into()); }
    let admission_context = context.clone();
    #[cfg(test)]
    let test_admission = TASK_ADMISSION_TEST.with(|hook| hook.borrow_mut().take());
    let admission: tasks::Admission = Box::new(move |def, run| {
        if run.view().source == RunSource::Scheduled && crate::schedule::PAUSABLE_JOBS.contains(&def.id.as_str()) {
            // Ticket 33: a fresh read of the owner pause, holding the Schedule store lock until the run is marked
            // running (the executor keeps the returned guard), so a pause that has answered is never overtaken by a
            // due slot. An unreadable store refuses the start.
            return crate::schedule::ScheduleStore::new(&admission_context)
                .admit_scheduled(def.id.as_str())
                .map(|lock| Some(tasks::AdmissionGuard::scheduled(lock)));
        }
        if run.view().source != RunSource::Manual || run.view().by.as_deref() != Some("console") {
            return Ok(None);
        }
        #[cfg(test)]
        if let Some(hook) = &test_admission { hook(); }
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("HER_TEST_TASK_ADMISSION_PAUSE") {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while Path::new(&path).is_file() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let guard = crate::conversation_state::acquire(&admission_context)
            .map_err(|(_, message)| message)?;
        if admission_context.console_authority.is_none() {
            return Err("HTTP task authority is unavailable".into());
        }
        if !def.runs_when_held && !matches!(desired(&admission_context), Effective::Act { wants, .. } if wants.jobs) {
            return Err("desired state no longer permits background jobs".into());
        }
        Ok(Some(tasks::AdmissionGuard::manual(guard)))
    });
    let running_context = context.clone();
    let running_check = Box::new(move |def: &TaskDef| def.runs_when_held || !matches!(desired(&running_context), Effective::Act { wants, .. } if !wants.jobs));
    let executor = Arc::new(RuntimeExecutor { context: context.clone(), inner: CommandExecutor::new(tasks::node_resolver(node, install, root), internal, clock.clone()).with_admission(admission).with_running_check(running_check) });
    let dispatcher = Dispatcher::new(store, executor, clock.clone());
    let mut scheduler = Scheduler::new(|def| enabled_in(context, cfg, def), crate::local_time::configured_zone(cfg), clock.now_ms());
    scheduler.refresh_schedules(cfg, clock.now_ms());
    let scheduler = Arc::new(Mutex::new(scheduler));
    Ok(crate::console_tasks::TasksApi { dispatcher, scheduler, desired: DesiredStore::new(context.data_dir().join("lifecycle")) })
}

fn owner_gate(context: &RuntimeContext, name: &str) -> Result<OwnerGate, String> {
    let path = context.resolve_path(RuntimeArea::Data, &format!("logs/{name}"), PathIntent::WriteFile)
        .map_err(str::to_string)?;
    Ok(OwnerGate::new(path))
}

pub(crate) fn desired(context: &RuntimeContext) -> Effective {
    desired_observation(context).0
}

fn desired_observation(context: &RuntimeContext) -> (Effective, Option<u64>) {
    let store = DesiredStore::new(context.data_dir().join("lifecycle"));
    let read = store.read();
    let revision = match &read {
        lifecycle::DesiredRead::Present(record) => Some(record.revision),
        _ => None,
    };
    (lifecycle::effective(&read, crate::parity_clock::now_ms()), revision)
}

fn checked_live(context: &RuntimeContext) -> watchdog::LiveObservation {
    lifecycle::live_observation(
        &mut LiveLifecycle,
        context.data_dir(),
        &ImagePolicy::SameReleaseTwin,
        ProbeMode::Create,
    )
}

/// A listening socket is managed only if its health pid matches the held console gate and OS identity.
fn console_probe(context: &RuntimeContext, port: u16) -> ConsoleProbe {
    console_probe_with(context, port, false, None)
}

pub(crate) fn health_console_probe(context: &RuntimeContext, port: u16, deadline: std::time::Instant) -> ConsoleProbe {
    console_probe_with(context, port, true, Some(deadline))
}

fn console_probe_with(context: &RuntimeContext, port: u16, allow_sibling: bool, deadline: Option<std::time::Instant>) -> ConsoleProbe {
    let remaining = |limit: Duration| -> Result<Duration, ConsoleProbe> {
        match deadline {
            None => Ok(limit),
            Some(deadline) => deadline.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero())
                .map(|d| d.min(limit)).ok_or_else(|| ConsoleProbe::Unknown("console health deadline exceeded".into())),
        }
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let budget = match remaining(LOOPBACK_CONNECT_BUDGET) { Ok(b) => b, Err(p) => return p };
    let mut stream = match TcpStream::connect_timeout(&addr, budget) {
        Ok(stream) => stream,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return ConsoleProbe::Absent,
        Err(e) => return ConsoleProbe::Unknown(format!("console connect failed: {}", e.kind())),
    };
    let request = format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    let mut pending = request.as_bytes();
    while !pending.is_empty() {
        let budget = match remaining(Duration::from_secs(2)) { Ok(b) => b, Err(p) => return p };
        if stream.set_write_timeout(Some(budget)).is_err() { return ConsoleProbe::Unknown("console health timeout cannot be set".into()); }
        match stream.write(pending) {
            Ok(0) => return ConsoleProbe::Unknown("console health request closed".into()),
            Ok(n) => pending = &pending[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return ConsoleProbe::Unknown("console health request failed".into()),
        }
    }
    let mut bytes = Vec::new();
    let mut block = [0; 4096];
    loop {
        let budget = match remaining(Duration::from_secs(2)) { Ok(b) => b, Err(p) => return p };
        if stream.set_read_timeout(Some(budget)).is_err() { return ConsoleProbe::Unknown("console health timeout cannot be set".into()); }
        match stream.read(&mut block) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&block[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return ConsoleProbe::Unknown("console health response is unreadable or timed out".into()),
        }
        if bytes.len() > 16_384 { return ConsoleProbe::Unknown("console health response is too large".into()); }
    }
    let text = String::from_utf8_lossy(&bytes);
    let Some((headers, body)) = text.split_once("\r\n\r\n") else { return ConsoleProbe::Unknown("invalid console health response".into()) };
    if !headers.starts_with("HTTP/1.1 200 ") && !headers.starts_with("HTTP/1.0 200 ") {
        return ConsoleProbe::Unknown("console health was refused".into());
    }
    let body = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let closed = body["ok"] == false && body["administratorReady"] == false
        && body["recoveryReason"] == crate::restore::RECOVERY_REASON;
    if body["port"].as_u64() != Some(port as u64) || (body["ok"] != true && !closed) {
        return ConsoleProbe::Unknown("console health did not report success on the selected port".into());
    }
    let Some(pid) = body.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()) else {
        return ConsoleProbe::Unknown("console health has no valid pid".into());
    };
    let (policy, mode) = if allow_sibling {
        (ImagePolicy::HealthReadOnly, ProbeMode::Existing)
    } else {
        (ImagePolicy::SameReleaseTwin, ProbeMode::Create)
    };
    match lifecycle::verify_owner(
        &mut LiveLifecycle,
        context.data_dir(),
        OwnerRole::Console,
        &policy,
        mode,
    ) {
        OwnerObservation::Verified(owner) if owner.marker.identity.pid == pid => {
            ConsoleProbe::Managed { pid }
        }
        OwnerObservation::Verified(_) | OwnerObservation::Free { .. } => {
            ConsoleProbe::Foreign { pid }
        }
        OwnerObservation::Mismatch {
            marker: Some(marker),
            liveness: Some(other),
            ..
        } if marker.role == OwnerRole::Console.as_str() && marker.identity.pid == pid => {
            ConsoleProbe::Unknown(format!("console owner identity is {other:?}"))
        }
        OwnerObservation::Mismatch {
            marker: Some(_), ..
        } => ConsoleProbe::Foreign { pid },
        OwnerObservation::Mismatch { marker: None, .. } => {
            ConsoleProbe::Unknown("console owner cannot be verified".into())
        }
        OwnerObservation::Unknown(reason) => ConsoleProbe::Unknown(reason),
    }
}

struct RealEffects {
    context: RuntimeContext,
    live_gate: OwnerGate,
}

impl RealEffects {
    fn spawn(&self, args: &[String], name: &str, ready: impl FnMut(u32) -> bool) -> Result<(), String> {
        Self::await_child(self.launch(args, name)?, ready)
    }

    fn launch(&self, args: &[String], name: &str) -> Result<std::process::Child, String> {
        let exe = std::env::current_exe().map_err(|e| format!("executable unavailable: {}", e.kind()))?;
        let out = self.context.resolve_path(RuntimeArea::Data, &format!("logs/{name}.out.log"), PathIntent::WriteFile)
            .map_err(str::to_string)?;
        let err = self.context.resolve_path(RuntimeArea::Data, &format!("logs/{name}.err.log"), PathIntent::WriteFile)
            .map_err(str::to_string)?;
        let open = |path: &Path| std::fs::OpenOptions::new().create(true).append(true).open(path)
            .map_err(|e| format!("process log unavailable: {}", e.kind()));
        let mut cmd = crate::subprocess::command_without_console(crate::subprocess::external_path(exe)?);
        cmd.args(args.iter().map(crate::subprocess::external_path).collect::<Result<Vec<_>, _>>()?).stdin(Stdio::null()).stdout(Stdio::from(open(&out)?)).stderr(Stdio::from(open(&err)?));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        cmd.spawn().map_err(|e| format!("process spawn failed: {}", e.kind()))
    }

    fn await_child(mut child: std::process::Child, mut ready: impl FnMut(u32) -> bool) -> Result<(), String> {
        let pid = child.id();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Err(format!("process exited before readiness: {status}")),
                Ok(None) => {}
                Err(error) => return Err(format!("process status is unavailable: {}", error.kind())),
            }
            if ready(pid) {
                std::thread::spawn(move || { let _ = child.wait(); });
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                if child.kill().is_err() { return Err("readiness timed out; child termination is unknown".into()); }
                let stop_deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return Err("process did not become ready before the deadline".into()),
                        Ok(None) if std::time::Instant::now() < stop_deadline => std::thread::sleep(Duration::from_millis(50)),
                        _ => return Err("readiness timed out; child termination is unknown".into()),
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl RealEffects {
    fn launch_console(&self, port: u16) -> Result<std::process::Child, String> {
        let root = self.context.runtime_root().ok_or("runtime root unavailable")?;
        self.launch(&["--runtime-root".into(), root.to_string_lossy().into_owned(), "--console-port".into(),
            port.to_string(), "--console-serve".into()], "rbridge-console")
    }

    fn await_console(&self, child: std::process::Child, port: u16) -> Result<(), String> {
        Self::await_child(child, |pid| {
            matches!(console_probe(&self.context, port), ConsoleProbe::Managed { pid: actual } if actual == pid)
        })
    }
}

impl Effects for RealEffects {
    fn kill_verified(&mut self, identity: &lifecycle::ProcessIdentity, restart: Option<&[String]>) -> lifecycle::KillOutcome {
        let _transition = match lifecycle::lock_with_wait(&self.context.data_dir().join("lifecycle/lifecycle.lock"), Duration::from_secs(5)) {
            Ok(gate) => gate,
            Err(error) => return lifecycle::KillOutcome::Unknown(error),
        };
        let wants = match desired(&self.context) {
            Effective::Act { wants, .. } => wants,
            Effective::Unknown(reason) => return lifecycle::KillOutcome::Refused(reason),
        };
        let target = match self.context.load_config().ok().and_then(|cfg| watchdog::live_target_from_config(&cfg).ok()) {
            Some(target) => Some(target),
            None if !wants.live && restart.is_none() => None,
            None => return lifecycle::KillOutcome::Refused("live target is no longer known".into()),
        };
        let still_needed = match restart {
            Some(groups) => wants.live && target.as_ref().is_some_and(|target| target.groups.as_slice() == groups),
            None => !wants.live || target.as_ref().is_some_and(|target| target.groups.is_empty()),
        };
        if !still_needed { return lifecycle::KillOutcome::Refused("desired state or live target changed before stop".into()); }
        match self.live_gate.probe() {
            OwnerProbe::Held { marker: Some(marker) } if marker.role == "live" && lifecycle::same_identity(&marker.identity, identity) => {}
            _ => return lifecycle::KillOutcome::Refused("live owner changed before stop".into()),
        }
        lifecycle::kill_verified(identity)
    }
    fn probe_live(&mut self) -> OwnerProbe { self.live_gate.probe() }
    fn start_live(&mut self, groups: &[String]) -> Result<(), String> {
        match desired(&self.context) {
            Effective::Act { wants, .. } if wants.live => {},
            _ => return Err("desired state no longer permits live".into()),
        }
        if !matches!(self.live_gate.probe(), OwnerProbe::Free { .. }) { return Err("live owner is not known free".into()); }
        let cfg = self.context.load_config().map_err(str::to_string)?;
        let target = watchdog::live_target_from_config(&cfg)?;
        if target.groups != groups { return Err("configured groups changed before startup".into()); }
        let root = self.context.runtime_root().ok_or("runtime root unavailable")?;
        self.spawn(&["--runtime-root".into(), root.to_string_lossy().into_owned(), "--console-port".into(), target.console_port.to_string(), "--live".into(), "--live-groups".into(), groups.join(",")], "rbridge-live", |pid| {
            let OwnerObservation::Verified(owner) = lifecycle::verify_owner(
                &mut LiveLifecycle,
                self.context.data_dir(),
                OwnerRole::Live,
                &ImagePolicy::SameReleaseTwin,
                ProbeMode::Create,
            ) else {
                return false;
            };
            owner.marker.identity.pid == pid
                && lifecycle::verify_live_heartbeat(
                    &mut LiveLifecycle,
                    self.context.data_dir(),
                    &owner.marker,
                    groups,
                )
                .is_ok()
        })
    }
    fn start_console(&mut self, port: u16) -> Result<(), String> {
        match desired(&self.context) {
            Effective::Act { wants, .. } if wants.console => {},
            _ => return Err("desired state no longer permits the console".into()),
        }
        if !matches!(console_probe(&self.context, port), ConsoleProbe::Absent) { return Err("console port is not known free".into()); }
        let child = self.launch_console(port)?;
        self.await_console(child, port)
    }
    fn stop_console(&mut self, pid: u32) -> Result<(), String> {
        let data = self.context.data_dir();
        let expected_revision = match DesiredStore::new(data.join("lifecycle")).read() {
            DesiredRead::Present(record) if record.state == DesiredState::Stopped => record.revision,
            _ => return Err("desired state no longer permits stopping the console".into()),
        };
        match lifecycle::verify_owner(
            &mut LiveLifecycle,
            data,
            OwnerRole::Console,
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        ) {
            OwnerObservation::Verified(owner) if owner.marker.identity.pid == pid => {}
            _ => return Err("console owner is no longer verified".into()),
        }
        lifecycle::stop_verified_owners_transaction(
            &mut LiveLifecycle,
            data,
            expected_revision,
            &[OwnerRole::Console],
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        )
    }
    fn wait_ms(&mut self, ms: u64) { std::thread::sleep(Duration::from_millis(ms)); }
}

fn watchdog_tick(context: &RuntimeContext, state: &Mutex<WatchState>, run: &mut RunHandle) -> Finish {
    let cfg = match context.load_config().map_err(str::to_string).and_then(|cfg| {
        watchdog::live_target_from_config(&cfg).map(|_| cfg)
    }) {
        Ok(cfg) => cfg,
        Err(error) => {
            // Wait for tasks without holding the desired gate; they may need that
            // gate to complete. Recheck the desired state under the gate before
            // retiring any owner, including after a concurrent resume.
            let data = context.data_dir();
            let task_gates = if matches!(desired(context), Effective::Act { wants, .. } if !wants.jobs) {
                lifecycle::hold_stopped_task_gates(data).ok()
            } else { None };
            match LiveLifecycle.lock_desired_gate(data, Duration::from_secs(5)) {
                Ok(_desired_gate) => if let Effective::Act { wants, .. } = desired(context) {
                    for (role, stop) in [
                        (OwnerRole::Live, !wants.live),
                        (OwnerRole::Console, !wants.console),
                    ] {
                        if !stop { continue; }
                        if role == OwnerRole::Console && task_gates.is_none() {
                            run.log(crate::task_runs::LogStream::Meta, "running tasks did not stop; console left running");
                            continue;
                        }
                        match lifecycle::verify_owner(
                            &mut LiveLifecycle,
                            data,
                            role,
                            &ImagePolicy::SameReleaseTwin,
                            ProbeMode::Create,
                        ) {
                            OwnerObservation::Free { .. } => {}
                            OwnerObservation::Verified(owner) => {
                                match lifecycle::kill_verified(&owner.marker.identity) {
                                    lifecycle::KillOutcome::Killed | lifecycle::KillOutcome::Gone =>
                                        run.log(crate::task_runs::LogStream::Meta, &format!("stopped verified {} owner while configuration was unreadable", role.as_str())),
                                    _ => run.log(crate::task_runs::LogStream::Meta, &format!("could not verify stopping {} owner", role.as_str())),
                                }
                            }
                            _ => run.log(crate::task_runs::LogStream::Meta, &format!("{} owner is unknown; no process stopped", role.as_str())),
                        }
                    }
                },
                Err(_) => run.log(crate::task_runs::LogStream::Meta, "desired-state gate is unavailable; no process stopped"),
            }
            return Finish::Failed { exit_code: None, reason: format!("configuration unavailable: {error}") };
        }
    };
    let gate = match owner_gate(context, lifecycle::LIVE_LOCK) {
        Ok(gate) => gate,
        Err(error) => return Finish::Unknown { reason: error },
    };
    let now = crate::parity_clock::now_ms();
    let target = watchdog::live_target_from_config(&cfg);
    let console = target.as_ref().map(|t| console_probe(context, t.console_port))
        .unwrap_or_else(|e| ConsoleProbe::Unknown(e.clone()));
    let observation = Observation {
        desired: desired(context), target, console,
        live: checked_live(context),
        heartbeat: match context.read_optional_text(RuntimeArea::Data, "rust-heartbeat-live.json") {
            Ok(Some(text)) => watchdog::read_heartbeat(Ok(text.into_bytes()), now),
            Ok(None) => watchdog::Heartbeat::Missing,
            Err(_) => watchdog::Heartbeat::Unknown("heartbeat is unreadable".into()),
        },
    };
    let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
    let decision = watchdog::decide(&observation, &state);
    *state = decision.state.clone();
    let mut fx = RealEffects { context: context.clone(), live_gate: gate };
    let applied = watchdog::apply(&decision, &mut fx);
    for note in decision.notes.iter().chain(applied.notes.iter()) { run.log(crate::task_runs::LogStream::Meta, note); }
    for alert in decision.alerts.iter().chain(applied.alerts.iter()) {
        crate::admin_alert::notify_notice(context, &cfg, alert, "watchdog");
    }
    if applied.failed {
        crate::recovery_log::record(context, crate::run_record::Trigger::Timer, crate::run_record::Component::Process,
            "watchdog", crate::run_record::Outcome::Failed, "watchdog reconciliation failed");
        Finish::Failed { exit_code: None, reason: "watchdog reconciliation failed; see task log".into() }
    } else { Finish::Succeeded }
}

pub fn run_cli(args: &[String]) -> i32 {
    if args.len() != 2 || args[0] != "--runtime-root" {
        eprintln!("usage: her supervise --runtime-root <absolute root>");
        return 2;
    }
    let root = Path::new(&args[1]);
    if let Err(error) = crate::install_root::check_runtime(root, true) {
        eprintln!("[supervisor] {error}"); return 2;
    }
    if !root.is_absolute() { eprintln!("[supervisor] runtime root must be absolute"); return 2; }
    match crate::release::before_supervisor_start(root) {
        Ok(true) => { eprintln!("[supervisor] unhealthy start limit reached; handing back to last-good"); return 3; }
        Ok(false) => {},
        Err(error) => { eprintln!("[supervisor] {error}"); return 3; }
    }
    let context = match RuntimeContext::from_root(root) {
        Ok(context) => context,
        Err(error) => { eprintln!("[supervisor] {error}"); return 2; }
    };
    let cfg = match context.load_config() {
        Ok(cfg) => cfg,
        Err(error) => { eprintln!("[supervisor] {error}"); return 2; }
    };
    let gate = match owner_gate(&context, SUPERVISOR_LOCK) {
        Ok(gate) => gate,
        Err(error) => { eprintln!("[supervisor] {error}"); return 3; }
    };
    let identity = match lifecycle::current_identity() {
        Ok(identity) => identity,
        Err(error) => { eprintln!("[supervisor] {error}"); return 3; }
    };
    let _owner = match gate.acquire("supervisor", identity, None, crate::parity_clock::now_ms(), Duration::from_millis(500)) {
        Acquire::Acquired(owner) => owner,
        other => { eprintln!("[supervisor] ownership unavailable: {other:?}"); return 3; }
    };
    crate::recovery_log::bind_default(&context);
    crate::scheduled_task::check_at_start(&context, &cfg);
    let api = match task_api(&context, &cfg) {
        Ok(api) => api,
        Err(error) => { eprintln!("[supervisor] {error}"); return 3; }
    };
    let initial_state = desired_observation(&context);
    if enabled(&cfg, tasks::task_def(TaskId::Watchdog)) {
        let _ = api.dispatcher.start(tasks::task_def(TaskId::Watchdog), RunSource::Login, None, None, "supervisor");
    }
    crate::supervisor_dependencies::start(context.clone());
    crate::release::start(context.clone());
    crate::qq_login::start(context.clone());
    let scheduler = api.scheduler.clone();
    let refresh_scheduler = scheduler.clone();
    let refresh_context = context.clone();
    let desired_context = context.clone();
    let change_context = context.clone();
    let change_dispatcher = api.dispatcher.clone();
    let restart_dispatcher = api.dispatcher.clone();
    let restart_context = context.clone();
    let last_state = Mutex::new(initial_state);
    let backup_notice_at = Mutex::new(None::<std::time::Instant>);
    let stop = AtomicBool::new(false);
    tasks::run_scheduler(scheduler, api.dispatcher, move |now| {
        let store = DesiredStore::new(desired_context.data_dir().join("lifecycle"));
        lifecycle::effective(&store.read(), now)
    }, move |period| {
        std::thread::sleep(period);
        dispatch_restart(&restart_context, &restart_dispatcher);
        let current = desired_observation(&change_context);
        let mut last = last_state.lock().unwrap_or_else(|p| p.into_inner());
        if *last != current {
            if matches!(change_dispatcher.start(tasks::task_def(TaskId::Watchdog), RunSource::Recovery, None, None, "desired-state"),
                tasks::Trigger::Started(_)) {
                *last = current;
            }
        }
        drop(last);
        if let Ok(cfg) = refresh_context.load_config() {
            let now = crate::parity_clock::now_ms();
            let mut checked = backup_notice_at.lock().unwrap_or_else(|p| p.into_inner());
            if checked.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
                if let Err(error) = crate::backup::monitor_notices(&refresh_context, &cfg, change_dispatcher.store(), now) {
                    crate::safe_eprintln!("[backup] administrator notice check failed: {error}");
                }
                *checked = Some(std::time::Instant::now());
            }
            drop(checked);
            let mut schedule = refresh_scheduler.lock().unwrap_or_else(|p| p.into_inner());
            for def in tasks::REGISTRY {
                let on = enabled_in(&refresh_context, &cfg, def);
                if schedule.enabled(def.id) != on { schedule.set_enabled(def.id, on, now); }
            }
            schedule.set_zone(crate::local_time::configured_zone(&cfg), now);
            schedule.refresh_schedules(&cfg, now);
        }
    }, Duration::from_secs(1), &stop);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-probe-{}-{}-{}", std::process::id(), crate::parity_clock::now_ms(), NEXT.fetch_add(1, Ordering::Relaxed))));
            for name in ["config", "data", "resources"] { std::fs::create_dir_all(root.join(name)).unwrap(); }
            std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-probe"}"#).unwrap();
            std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","model":"http://127.0.0.1:1"}"#).unwrap();
            Self(root)
        }
        fn context(&self) -> RuntimeContext { RuntimeContext::from_root(&self.0).unwrap() }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
    }

    #[cfg(windows)]
    #[test]
    fn backup_daily_time_is_read_from_selected_config() {
        let fixture = Fixture::new();
        let mut cfg: Value = serde_json::from_slice(&std::fs::read(fixture.0.join("config/config.json")).unwrap()).unwrap();
        cfg["backup"] = serde_json::json!({"time":"06:30"});
        std::fs::write(fixture.0.join("config/config.json"), cfg.to_string()).unwrap();
        let context = fixture.context();
        let cfg = context.load_config().unwrap();
        let api = task_api(&context, &cfg).unwrap();
        let now = api.dispatcher.clock().now_ms();
        let next = api.scheduler.lock().unwrap().next_run(TaskId::Backup).unwrap();
        assert_eq!(next, tasks::next_due(tasks::Schedule::Daily { hour: 6, minute: 30 }, now, &crate::local_time::configured_zone(&cfg)).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn canonical_task_and_mcp_paths_are_ordinary_and_unsupported_names_fail() {
        let fixture = Fixture::new();
        let root = std::fs::canonicalize(&fixture.0).unwrap();
        let resolve = tasks::node_resolver(root.join("node.exe"), root.clone(), root.clone());
        for task in tasks::REGISTRY.iter().filter(|def| matches!(def.action, tasks::TaskAction::Node { .. })) {
            let command = resolve(task).unwrap();
            for path in std::iter::once(command.program.to_str().unwrap()).chain(command.args.iter().map(String::as_str)).chain(std::iter::once(command.cwd.to_str().unwrap())) {
                assert!(!path.starts_with(r"\\?\"), "task {} leaked {path}", task.id.as_str());
            }
        }
        let block = crate::dsh_mcp_config::apply("", root.join("her.exe").to_str().unwrap(), Some((root.to_str().unwrap(), 12345))).unwrap();
        assert!(!block.contains(r"\\?\"), "generated MCP config leaks verbatim paths");
        assert_eq!(crate::subprocess::external_path(r"\\?\UNC\server\share\dir").unwrap(), r"\\server\share\dir");
        for unsupported in [r"\\?\GLOBALROOT\Device\HarddiskVolume1", r"\\?\C:\dir.\file", r"\\?\C:\NUL.txt", r"\\?\C:\file:stream", r"\\?\UNC\server"] {
            assert!(crate::subprocess::external_path(unsupported).is_err(), "unsafe conversion accepted {unsupported}");
        }
    }

    #[test]
    fn closed_console_port_is_absent() {
        let root = Fixture::new();
        let context = root.context();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        assert!(![3000, 3001, 3080, 3100, 5099].contains(&address.port()));
        drop(listener);

        let probe = console_probe(&context, address.port());
        assert!(matches!(probe, ConsoleProbe::Absent), "closed console must be absent: {probe:?}");
    }

    #[test]
    fn listening_console_with_unreadable_health_is_unknown() {
        let root = Fixture::new();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(![3000, 3001, 3080, 3100, 5099].contains(&port));
        // Keep a real listening socket open without answering HTTP. A successful
        // TCP connection does not establish a managed or absent console.
        let probe = console_probe(&root.context(), port);
        assert!(matches!(probe, ConsoleProbe::Unknown(_)), "live unknown listener must stay unknown: {probe:?}");
    }
}
