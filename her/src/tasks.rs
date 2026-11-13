//! Ticket 27: the Rust-internal scheduler and the fixed registry of background tasks.
//!
//! Windows starts Rust once (one hidden start task); everything that used to be a separate scheduled task is a timer
//! here. The registry is a closed list: the web page and the CLI can trigger only these ids, and each id maps to a fixed
//! program and arguments resolved under the installation root (phase B supplies the resolver). There is no route that
//! runs an arbitrary command or path.
//!
//! Time is injected ([`Clock`]); schedules are wall-clock local times (UTC+8 by default, see [`local_offset_ms`]). A
//! missed slot (the process was down) is not caught up; a late tick runs the slot once. Every trigger goes through
//! [`crate::task_runs::TaskStore::claim`], so a schedule slot runs once and a running task is never started twice.
#![allow(dead_code)]

use crate::lifecycle::Effective;
use crate::task_runs::{Claim, Finish, LogStream, RunHandle, RunSource, RunView, TaskStore};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// The bridge clock shared with ticket 25's retry policy (`parity_clock::now_ms` in production, injected in tests).
pub use crate::dependency::{Clock, SystemClock};

/// The production zone offset: `UTC`/`GMT` in `TZ` mean 0, everything else the host's UTC+8 (as `prompt.rs`).
pub fn local_offset_ms() -> i64 {
    match std::env::var("TZ").ok().as_deref().map(str::trim) {
        Some("UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z") => 0,
        _ => 8 * 3_600_000,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// Every `minutes` (a divisor of a day), at `offset` minutes past local midnight and every period after.
    Every { minutes: u32, offset: u32 },
    Daily { hour: u32, minute: u32 },
    /// Only by hand.
    Manual,
}

impl Schedule {
    pub fn describe(self) -> String {
        match self {
            Schedule::Every { minutes, offset } if minutes % 60 == 0 => {
                format!("每 {} 小时（第 {} 分）", minutes / 60, offset % 60)
            }
            Schedule::Every { minutes, offset } => format!("每 {minutes} 分钟（偏移 {offset} 分）"),
            Schedule::Daily { hour, minute } => format!("每天 {hour:02}:{minute:02}"),
            Schedule::Manual => "仅手动".to_string(),
        }
    }
}

const DAY_MS: i64 = 86_400_000;

/// The first slot strictly after `after_ms` (UTC ms), with local time `UTC + offset_ms`.
pub fn next_due(schedule: Schedule, after_ms: i64, offset_ms: i64) -> Option<i64> {
    let local = after_ms + offset_ms;
    let next_local = match schedule {
        Schedule::Manual => return None,
        Schedule::Every { minutes, offset } => {
            let period = minutes as i64 * 60_000;
            if period <= 0 || DAY_MS % period != 0 {
                return None;
            }
            let shift = (offset as i64 * 60_000) % period;
            (local - shift).div_euclid(period) * period + period + shift
        }
        Schedule::Daily { hour, minute } => {
            if hour > 23 || minute > 59 {
                return None;
            }
            let day = local.div_euclid(DAY_MS) * DAY_MS;
            let slot = day + hour as i64 * 3_600_000 + minute as i64 * 60_000;
            if slot > local {
                slot
            } else {
                slot + DAY_MS
            }
        }
    };
    Some(next_local - offset_ms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskId {
    Watchdog,
    ConsoleRestart,
    MemoryDigest,
    MemoryDigestCheck,
    Backup,
    UpstreamCheck,
}

impl TaskId {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskId::Watchdog => "watchdog",
            TaskId::ConsoleRestart => "console-restart",
            TaskId::MemoryDigest => "memory-digest",
            TaskId::MemoryDigestCheck => "memory-digest-check",
            TaskId::Backup => "backup",
            TaskId::UpstreamCheck => "upstream-check",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        REGISTRY.iter().map(|t| t.id).find(|id| id.as_str() == text)
    }
}

/// What a task runs. Programs are fixed here; only their installation root is resolved at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    /// A Rust job inside this binary.
    Internal,
    /// `node <root>/<script> <args>` with the installation root as working directory.
    Node { script: &'static str, args: &'static [&'static str] },
}

#[derive(Debug, Clone, Copy)]
pub struct TaskDef {
    pub id: TaskId,
    pub title: &'static str,
    /// What running it does, shown before a manual trigger.
    pub effect: &'static str,
    pub schedule: Schedule,
    pub action: TaskAction,
    pub enabled_by_default: bool,
    /// Runs while the desired state holds jobs (stopped, maintenance): the watchdog enforces the hold itself.
    pub runs_when_held: bool,
    /// Either output stream can carry chat text; both streams are withheld from the run log.
    pub chat_output: bool,
    pub timeout: Duration,
    /// The Windows scheduled task it replaces.
    pub replaces: Option<&'static str>,
}

pub const REGISTRY: &[TaskDef] = &[
    TaskDef {
        id: TaskId::Watchdog,
        title: "看门狗",
        effect: "按期望状态检查控制台与 live 处理权；只启动缺失实例，只停止身份已核实的实例",
        schedule: Schedule::Every { minutes: 2, offset: 0 },
        action: TaskAction::Internal,
        enabled_by_default: true,
        runs_when_held: true,
        chat_output: false,
        timeout: Duration::from_secs(90),
        replaces: Some("QQBot-GrayWatchdog"),
    },
    TaskDef {
        id: TaskId::MemoryDigest,
        title: "记忆整理",
        effect: "整理过去一天活跃会话的长期记忆",
        schedule: Schedule::Daily { hour: 3, minute: 0 },
        action: TaskAction::Internal,
        enabled_by_default: true,
        runs_when_held: false,
        chat_output: true,
        timeout: Duration::from_secs(2 * 3600),
        replaces: Some("QQBot-MemoryDigest"),
    },
    TaskDef {
        id: TaskId::MemoryDigestCheck,
        title: "记忆整理检查",
        effect: "整理上下文超过阈值且继续增长的会话记忆",
        schedule: Schedule::Every { minutes: 30, offset: 21 },
        action: TaskAction::Internal,
        enabled_by_default: true,
        runs_when_held: false,
        chat_output: true,
        timeout: Duration::from_secs(30 * 60),
        replaces: Some("QQBot-MemoryDigestCheck"),
    },
    TaskDef {
        id: TaskId::Backup,
        title: "备份",
        effect: "备份配置、状态与记忆库，并校验备份完整性",
        schedule: Schedule::Daily { hour: 4, minute: 0 },
        action: TaskAction::Internal,
        enabled_by_default: true,
        runs_when_held: false,
        chat_output: false,
        timeout: Duration::from_secs(3600),
        replaces: Some("QQBot-Backup"),
    },
    TaskDef {
        id: TaskId::UpstreamCheck,
        title: "上游检查",
        effect: "检查上游版本，发现更新时私聊管理员（qq-bridge/check-upstream.mjs）",
        schedule: Schedule::Daily { hour: 5, minute: 0 },
        action: TaskAction::Node { script: "qq-bridge/check-upstream.mjs", args: &[] },
        enabled_by_default: false,
        runs_when_held: false,
        chat_output: false,
        timeout: Duration::from_secs(10 * 60),
        replaces: Some("QQBot-UpstreamCheck"),
    },
    TaskDef {
        id: TaskId::ConsoleRestart,
        title: "重启控制台",
        effect: "由监督进程核验所有者后重启控制台",
        schedule: Schedule::Manual,
        action: TaskAction::Internal,
        enabled_by_default: true,
        runs_when_held: false,
        chat_output: false,
        timeout: Duration::from_secs(60),
        replaces: None,
    },
];

pub fn task_def(id: TaskId) -> &'static TaskDef {
    REGISTRY.iter().find(|t| t.id == id).expect("every TaskId is registered")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Migrated,
    Retired,
}

/// The ten Windows scheduled tasks found on 2026-10-23 (`ops-investigation.md`) and what replaces them.
#[derive(Debug, Clone, Copy)]
pub struct LegacyTask {
    pub name: &'static str,
    pub disposition: Disposition,
    pub replacement: Option<&'static str>,
    pub note: &'static str,
}

pub const LEGACY_TASKS: &[LegacyTask] = &[
    LegacyTask { name: "QQBot", disposition: Disposition::Retired, replacement: None, note: "每 15 分钟启动旧 JS bridge，会与 Rust 唯一处理权冲突" },
    LegacyTask { name: "QQBot-ABReport", disposition: Disposition::Retired, replacement: None, note: "影子 A/B 与日报执行入口已退役" },
    LegacyTask { name: "QQBot-Backup", disposition: Disposition::Migrated, replacement: Some("backup"), note: "" },
    LegacyTask { name: "QQBot-GrayWatchdog", disposition: Disposition::Migrated, replacement: Some("watchdog"), note: "Node 看门狗由 Rust 看门狗取代" },
    LegacyTask { name: "QQBot-MemoryDigest", disposition: Disposition::Migrated, replacement: Some("memory-digest"), note: "" },
    LegacyTask { name: "QQBot-MemoryDigestCheck", disposition: Disposition::Migrated, replacement: Some("memory-digest-check"), note: "" },
    LegacyTask { name: "QQBot-RBridgeShadow", disposition: Disposition::Retired, replacement: None, note: "影子 A/B 已退役（2026-10-27 重复发言事故后停用），不得重新启用" },
    LegacyTask { name: "QQBot-RestartDsh", disposition: Disposition::Retired, replacement: None, note: "2026-10-23 已删除，不得恢复；DSH 重启改由生命周期入口处理" },
    LegacyTask { name: "QQBot-StartAll", disposition: Disposition::Retired, replacement: None, note: "依赖由监督进程直接管理" },
    LegacyTask { name: "QQBot-UpstreamCheck", disposition: Disposition::Migrated, replacement: Some("upstream-check"), note: "" },
];

// ---- scheduler -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Due {
    pub task: TaskId,
    pub slot: i64,
}

impl Due {
    /// The de-duplication key of this slot: one run per task and slot, however often it is triggered.
    pub fn dedup_key(&self) -> String {
        format!("scheduled:{}:{}", self.task.as_str(), self.slot)
    }
}

#[derive(Debug)]
struct Entry {
    task: TaskId,
    schedule: Schedule,
    enabled: bool,
    next: Option<i64>,
}

#[derive(Debug)]
pub struct Scheduler {
    entries: Vec<Entry>,
    offset_ms: i64,
}

impl Scheduler {
    /// Schedule every registered task from `now`; slots before `now` are not caught up.
    pub fn new(enabled: impl Fn(&TaskDef) -> bool, offset_ms: i64, now: i64) -> Self {
        let entries = REGISTRY
            .iter()
            .map(|def| {
                let on = enabled(def);
                Entry { task: def.id, schedule: def.schedule, enabled: on, next: on.then(|| next_due(def.schedule, now, offset_ms)).flatten() }
            })
            .collect();
        Self { entries, offset_ms }
    }

    /// Slots that are due at `now`, each task at most once however late the tick is.
    pub fn tick(&mut self, now: i64) -> Vec<Due> {
        let mut due = Vec::new();
        for entry in self.entries.iter_mut().filter(|e| e.enabled) {
            if let Some(slot) = entry.next.filter(|slot| *slot <= now) {
                due.push(Due { task: entry.task, slot });
                entry.next = next_due(entry.schedule, now, self.offset_ms);
            }
        }
        due
    }

    pub fn next_run(&self, task: TaskId) -> Option<i64> {
        self.entries.iter().find(|e| e.task == task).and_then(|e| e.next)
    }

    pub fn enabled(&self, task: TaskId) -> bool {
        self.entries.iter().any(|e| e.task == task && e.enabled)
    }

    pub fn schedule(&self, task: TaskId) -> Option<Schedule> {
        self.entries.iter().find(|e| e.task == task).map(|e| e.schedule)
    }

    /// Unchanged refreshes preserve pending slots, including when already due.
    pub fn set_schedule(&mut self, task: TaskId, schedule: Schedule, now: i64) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.task == task && e.schedule != schedule) {
            entry.schedule = schedule;
            entry.next = if entry.enabled { next_due(schedule, now, self.offset_ms) } else { None };
        }
    }

    pub fn set_enabled(&mut self, task: TaskId, on: bool, now: i64) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.task == task) {
            entry.enabled = on;
            entry.next = if on { next_due(entry.schedule, now, self.offset_ms) } else { None };
        }
    }
}

// ---- dispatcher ------------------------------------------------------------------------------

/// Runs one task to its end. It receives the run (for logging) and a cancel flag it must honour.
pub trait Executor: Send + Sync {
    fn execute(&self, task: &TaskDef, run: &mut RunHandle, cancel: &AtomicBool) -> Finish;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// A new run was created; its result is in the store when it ends (never reported as done here).
    Started(RunView),
    Duplicate(RunView),
    AlreadyRunning(Option<RunView>),
    /// The desired state holds jobs (stopped, maintenance) or cannot be read.
    Held(String),
    Refused(String),
    Unavailable(String),
}

pub struct Dispatcher {
    store: Arc<TaskStore>,
    executor: Arc<dyn Executor>,
    clock: Arc<dyn Clock>,
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Dispatcher {
    pub fn new(store: Arc<TaskStore>, executor: Arc<dyn Executor>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self { store, executor, clock, cancels: Mutex::new(HashMap::new()) })
    }

    pub fn store(&self) -> &Arc<TaskStore> {
        &self.store
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// A scheduled slot. Jobs are held by a stopped/maintenance or unreadable desired state (the watchdog still runs and
    /// enforces the hold itself).
    pub fn on_due(self: &Arc<Self>, due: Due, desired: &Effective) -> Trigger {
        let def = task_def(due.task);
        if !def.runs_when_held {
            match desired {
                Effective::Unknown(reason) => return Trigger::Held(format!("desired state is unknown: {reason}")),
                Effective::Act { wants, state } if !wants.jobs => {
                    return Trigger::Held(format!("background jobs are held while {}", state.as_str()))
                }
                Effective::Act { .. } => {}
            }
        }
        self.start(def, RunSource::Scheduled, None, Some(&due.dedup_key()), "scheduler")
    }

    /// A manual trigger. `request_id` is the client's idempotency key: repeating it returns the first run.
    pub fn manual(self: &Arc<Self>, task: TaskId, request_id: &str, by: &str) -> Trigger {
        self.start(task_def(task), RunSource::Manual, Some(request_id), None, by)
    }

    pub fn start(
        self: &Arc<Self>,
        def: &'static TaskDef,
        source: RunSource,
        request_id: Option<&str>,
        dedup_key: Option<&str>,
        by: &str,
    ) -> Trigger {
        let now = self.clock.now_ms();
        let handle = match self.store.claim(def.id.as_str(), source, request_id, dedup_key, by, now) {
            Claim::Started(handle) => handle,
            Claim::Duplicate(run) => return Trigger::Duplicate(run),
            Claim::AlreadyRunning(run) => return Trigger::AlreadyRunning(run),
            Claim::Conflict(reason) => return Trigger::Refused(reason),
            Claim::Unavailable(reason) => return Trigger::Unavailable(reason),
        };
        let view = handle.view().clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap_or_else(|p| p.into_inner()).insert(view.run_id.clone(), cancel.clone());
        let this = self.clone();
        std::thread::spawn(move || {
            let mut handle = handle;
            let finish = this.executor.execute(def, &mut handle, &cancel);
            let run_id = handle.run_id().to_string();
            // Publishing the terminal state and retiring cancellation are one operation to callers:
            // a cancellation that observes a completed run must not still be accepted.
            let mut cancels = this.cancels.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(error) = handle.finish(finish, this.clock.now_ms()) {
                crate::safe_println!("[tasks] run {run_id} result could not be recorded: {error}");
            }
            cancels.remove(&run_id);
        });
        Trigger::Started(view)
    }

    /// Ask a run of this process to stop. `false` when it is not running here.
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.cancels.lock().unwrap_or_else(|p| p.into_inner()).get(run_id) {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }
}

/// Drive `scheduler` until `stop`: every `period`, hand due slots to the dispatcher with the current desired state.
pub fn run_scheduler(
    scheduler: Arc<Mutex<Scheduler>>,
    dispatcher: Arc<Dispatcher>,
    desired: impl Fn(i64) -> Effective,
    sleep: impl Fn(Duration),
    period: Duration,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        let now = dispatcher.clock().now_ms();
        let due = scheduler.lock().unwrap_or_else(|p| p.into_inner()).tick(now);
        if !due.is_empty() {
            let effective = desired(now);
            for slot in due {
                match dispatcher.on_due(slot, &effective) {
                    Trigger::Started(_) | Trigger::Duplicate(_) => {}
                    other => crate::safe_println!("[tasks] {} slot {} not run: {other:?}", slot.task.as_str(), slot.slot),
                }
            }
        }
        sleep(period);
    }
}

// ---- command executor ------------------------------------------------------------------------

/// A resolved program invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

pub type Resolver = Box<dyn Fn(&TaskDef) -> Result<CommandSpec, String> + Send + Sync>;
pub type InternalJob = Box<dyn Fn(&TaskDef, &mut RunHandle, &AtomicBool) -> Finish + Send + Sync>;

/// The production resolver: `node` (an explicit path, normally the installed Node) with the registry's script under
/// `root`.
pub fn node_resolver(node: PathBuf, root: PathBuf, _runtime: PathBuf) -> Resolver {
    Box::new(move |def: &TaskDef| match def.action {
        TaskAction::Node { script, args } => Ok(CommandSpec {
            program: crate::subprocess::external_path(&node)?.into(),
            args: std::iter::once(crate::subprocess::external_path(root.join(script))?)
                .chain(args.iter().map(|a| a.to_string()))
                .collect(),
            cwd: crate::subprocess::external_path(&root)?.into(),
        }),
        TaskAction::Internal => Err("internal task has no program".to_string()),
    })
}

pub struct CommandExecutor {
    resolve: Resolver,
    internal: InternalJob,
    clock: Arc<dyn Clock>,
    admission: Option<Admission>,
    running_allowed: Option<Box<dyn Fn(&TaskDef) -> bool + Send + Sync>>,
}

pub type Admission = Box<dyn Fn(&TaskDef, &RunHandle) -> Result<Option<std::fs::File>, String> + Send + Sync>;

struct TaskChild {
    #[cfg(windows)]
    tree: crate::backend::windows_cli_lifecycle::CliProcessTree,
    #[cfg(not(windows))]
    child: std::process::Child,
}

impl TaskChild {
    fn spawn(mut command: std::process::Command, timeout: Duration) -> Result<Self, String> {
        #[cfg(windows)]
        {
            let tree = crate::backend::windows_cli_lifecycle::CliProcessTree::spawn_task(command, timeout)?;
            Ok(Self { tree })
        }
        #[cfg(not(windows))]
        {
            let _ = timeout;
            command.spawn().map(|child| Self { child }).map_err(|e| e.kind().to_string())
        }
    }

    fn child(&mut self) -> &mut std::process::Child {
        #[cfg(windows)] { self.tree.child_mut() }
        #[cfg(not(windows))] { &mut self.child }
    }

    fn stop(&mut self) -> Result<(), String> {
        #[cfg(windows)]
        { self.tree.stop_task() }
        #[cfg(not(windows))]
        {
            #[cfg(unix)]
            {
                let pid = i32::try_from(self.child.id()).map_err(|_| "invalid child pid".to_string())?;
                if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) { return Err(format!("cannot stop process group: {}", error.kind())); }
                }
            }
            self.child.kill().or_else(|error| {
                if self.child.try_wait()?.is_some() { Ok(()) } else { Err(error) }
            }).map_err(|e| format!("cannot stop task process: {}", e.kind()))
        }
    }
}

impl CommandExecutor {
    pub fn new(resolve: Resolver, internal: InternalJob, clock: Arc<dyn Clock>) -> Self {
        Self { resolve, internal, clock, admission: None, running_allowed: None }
    }

    pub fn with_admission(mut self, admission: Admission) -> Self {
        self.admission = Some(admission);
        self
    }

    pub fn with_running_check(mut self, check: Box<dyn Fn(&TaskDef) -> bool + Send + Sync>) -> Self {
        self.running_allowed = Some(check);
        self
    }
}

impl Executor for CommandExecutor {
    fn execute(&self, def: &TaskDef, run: &mut RunHandle, cancel: &AtomicBool) -> Finish {
        let admission = match &self.admission {
            Some(check) => match check(def, run) {
                Ok(guard) => guard,
                Err(reason) => return Finish::Cancelled { reason },
            },
            None => None,
        };
        if def.action == TaskAction::Internal {
            if let Err(error) = run.mark_running(None, self.clock.now_ms()) {
                return Finish::Unknown { reason: error };
            }
            // Long internal jobs recheck desired state themselves; release the configuration gate
            // before waiting for dependencies, queuing memory_write work or streaming a backup.
            if matches!(def.id, TaskId::MemoryDigest | TaskId::MemoryDigestCheck | TaskId::Backup) { drop(admission); }
            return (self.internal)(def, run, cancel);
        }
        let spec = match (self.resolve)(def) {
            Ok(spec) => spec,
            Err(reason) => return Finish::Failed { exit_code: None, reason: format!("task program unavailable: {reason}") },
        };
        #[cfg(unix)]
        if spec.program.is_absolute() || spec.program.components().count() > 1 {
            let program = if spec.program.is_absolute() { spec.program.clone() } else { spec.cwd.join(&spec.program) };
            if let Err(error) = std::fs::metadata(program) {
                return Finish::Failed { exit_code: None, reason: format!("task could not be started: {}", error.kind()) };
            }
        }
        #[cfg(unix)]
        let mut command = {
            // The bootstrap execs the job in the same PID/process group only after
            // its running record is durable. EOF (runner death) starts no job.
            // Program and arguments are argv entries, never shell source.
            let mut command = crate::subprocess::command_without_console("/bin/sh");
            command.args(["-c", "IFS= read -r ready && [ \"$ready\" = her-start ] || exit 125; exec \"$@\" </dev/null", "her-task"])
                .arg(&spec.program).args(&spec.args).stdin(Stdio::piped());
            command
        };
        #[cfg(not(unix))]
        let mut command = {
            let mut command = crate::subprocess::command_without_console(&spec.program);
            command.args(&spec.args).stdin(Stdio::null());
            command
        };
        command.current_dir(&spec.cwd).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            // The child inherits the locked task gate before it can execute.
            // A runner dying before it records the child PID cannot free the
            // gate while that child or an inheriting descendant is still alive.
            use std::os::unix::process::CommandExt as _;
            let task_gate = match run.child_gate_fd() {
                Ok(fd) => fd,
                Err(error) => return Finish::Unknown { reason: format!("task gate is unavailable: {}", error.kind()) },
            };
            #[cfg(target_os = "linux")]
            let parent = unsafe { libc::getpid() };
            unsafe {
                command.pre_exec(move || {
                    #[cfg(target_os = "linux")]
                    {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 { return Err(std::io::Error::last_os_error()); }
                        if libc::getppid() != parent { return Err(std::io::Error::other("task parent has exited")); }
                    }
                    let flags = libc::fcntl(task_gate, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(task_gate, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command.process_group(0);
        }
        let mut tree = match TaskChild::spawn(command, def.timeout) {
            Ok(child) => child,
            Err(error) => return Finish::Failed { exit_code: None, reason: format!("task could not be started: {error}") },
        };
        if let Err(error) = run.mark_running(Some(tree.child().id()), self.clock.now_ms()) {
            if tree.stop().is_ok() { let _ = tree.child().wait(); }
            return Finish::Unknown { reason: error };
        }
        #[cfg(unix)]
        {
            if cancel.load(Ordering::SeqCst) || self.running_allowed.as_ref().is_some_and(|check| !check(def)) {
                if tree.stop().is_ok() { let _ = tree.child().wait(); }
                return Finish::Cancelled { reason: "task was cancelled before execution".to_string() };
            }
            let started = tree.child().stdin.take().ok_or_else(|| std::io::Error::other("task startup pipe is unavailable"))
                .and_then(|mut pipe| std::io::Write::write_all(&mut pipe, b"her-start\n"));
            if let Err(error) = started {
                if tree.stop().is_ok() { let _ = tree.child().wait(); }
                return Finish::Unknown { reason: format!("task startup could not be released: {}", error.kind()) };
            }
        }
        drop(admission);
        // Bound both the queue and a single unterminated line. A noisy child must not grow the
        // supervisor's memory while its output is being redacted and persisted.
        let (tx, rx) = mpsc::sync_channel::<(LogStream, String)>(128);
        let mut readers = Vec::new();
        for (stream, pipe) in [
            (LogStream::Out, tree.child().stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)),
            (LogStream::Err, tree.child().stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)),
        ] {
            let Some(pipe) = pipe else { continue };
            let tx = tx.clone();
            readers.push(std::thread::spawn(move || {
                let mut reader = BufReader::new(pipe);
                let mut line = Vec::with_capacity(8192);
                loop {
                    let Ok(chunk) = reader.fill_buf() else { break };
                    if chunk.is_empty() {
                        if !line.is_empty() {
                            let text = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
                            let _ = tx.send((stream, text));
                        }
                        break;
                    }
                    let mut disconnected = false;
                    for &byte in chunk {
                        if byte == b'\n' {
                            let text = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
                            line.clear();
                            if tx.send((stream, text)).is_err() { disconnected = true; break; }
                        } else if line.len() < 8192 {
                            line.push(byte);
                        }
                    }
                    let count = chunk.len();
                    reader.consume(count);
                    if disconnected { break; }
                }
            }));
        }
        drop(tx);
        let deadline = Instant::now() + def.timeout;
        let mut stopped: Option<Finish> = None;
        let mut stop_at: Option<Instant> = None;
        let mut pipes_closed = false;
        let mut output_withheld = false;
        let mut exited: Option<(Instant, std::io::Result<std::process::ExitStatus>)> = None;
        loop {
            if pipes_closed {
                std::thread::sleep(Duration::from_millis(25));
            } else {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok((stream, text)) if !def.chat_output => run.log(stream, &text),
                    Ok(_) if !output_withheld => {
                        run.log(LogStream::Meta, "task output withheld because it may contain chat content");
                        output_withheld = true;
                    }
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => pipes_closed = true,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            if stopped.is_none() && exited.is_none() {
                let held = self.running_allowed.as_ref().is_some_and(|check| !check(def));
                if cancel.load(Ordering::SeqCst) || held {
                    stop_at = Some(Instant::now());
                    stopped = Some(match tree.stop() {
                        Ok(()) => Finish::Cancelled { reason: if held { "desired state no longer permits this task" } else { "cancelled by an administrator" }.to_string() },
                        Err(reason) => Finish::Unknown { reason },
                    });
                } else if Instant::now() >= deadline {
                    stop_at = Some(Instant::now());
                    stopped = Some(match tree.stop() {
                        Ok(()) => Finish::Failed { exit_code: None, reason: format!("timed out after {} s", def.timeout.as_secs()) },
                        Err(reason) => Finish::Unknown { reason },
                    });
                }
            }
            if exited.is_none() {
                match tree.child().try_wait() {
                    Ok(Some(status)) => exited = Some((Instant::now(), Ok(status))),
                    Ok(None) => {}
                    Err(error) => exited = Some((Instant::now(), Err(error))),
                }
            }
            // A descendant that outlives the task keeps its output pipes open: stop reading two seconds after the
            // task process itself ended (the reader threads end when those pipes close).
            if matches!(stopped, Some(Finish::Unknown { .. }))
                || exited.as_ref().is_some_and(|(at, _)| pipes_closed || at.elapsed() >= Duration::from_secs(2))
                || stop_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
            {
                break;
            }
        }
        let finished_readers = readers.into_iter().filter(|r| r.is_finished()).collect::<Vec<_>>();
        for reader in finished_readers {
            let _ = reader.join();
        }
        let status = match exited {
            Some((_, status)) => status,
            None => return match stopped {
                Some(finish @ Finish::Unknown { .. }) => finish,
                _ => Finish::Unknown { reason: "task termination could not be confirmed".to_string() },
            },
        };
        #[cfg(unix)]
        {
            // SIGKILL and the leader's wait do not witness descendant exit.
            // Give a signalled tree a bounded interval to finish dying before
            // retaining an unresolved group; ordinary completion never kills it.
            let pid = tree.child().id();
            let settled = Instant::now() + Duration::from_secs(1);
            while stopped.is_some() && crate::task_runs::task_group_alive(pid) && Instant::now() < settled {
                std::thread::sleep(Duration::from_millis(20));
            }
            if crate::task_runs::task_group_alive(pid) {
                return Finish::Unknown { reason: "task leader exited but its process group is still running; admission remains held".into() };
            }
        }
        #[cfg(windows)]
        if let Err(reason) = tree.stop() { return Finish::Unknown { reason }; }
        if let Some(finish) = stopped {
            return finish;
        }
        match status {
            Ok(status) if status.success() => Finish::Succeeded,
            Ok(status) => match status.code() {
                Some(code) => Finish::Failed { exit_code: Some(code), reason: format!("exit code {code}") },
                None => Finish::Failed { exit_code: None, reason: "terminated by a signal".to_string() },
            },
            Err(error) => Finish::Unknown { reason: format!("task exit status unavailable: {}", error.kind()) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency::ManualClock;
    use crate::lifecycle::{DesiredState, Wants};
    use crate::task_runs::{credential_redactor, RunQuery, RunState, StoreLimits};

    const H: i64 = 3_600_000;
    const M: i64 = 60_000;
    /// 2026-10-30 00:00 at UTC+8, as UTC ms.
    const MIDNIGHT: i64 = 1_793_289_600_000;

    fn running() -> Effective {
        Effective::Act { state: DesiredState::Running, wants: Wants { live: true, console: true, jobs: true } }
    }

    #[test]
    fn next_due_follows_local_wall_clock() {
        let tz = 8 * H;
        let daily3 = Schedule::Daily { hour: 3, minute: 0 };
        assert_eq!(next_due(daily3, MIDNIGHT, tz), Some(MIDNIGHT + 3 * H));
        assert_eq!(next_due(daily3, MIDNIGHT + 3 * H, tz), Some(MIDNIGHT + 27 * H), "strictly after");
        assert_eq!(next_due(daily3, MIDNIGHT + 3 * H - 1, tz), Some(MIDNIGHT + 3 * H));
        let check = Schedule::Every { minutes: 30, offset: 21 };
        assert_eq!(next_due(check, MIDNIGHT, tz), Some(MIDNIGHT + 21 * M));
        assert_eq!(next_due(check, MIDNIGHT + 21 * M, tz), Some(MIDNIGHT + 51 * M));
        assert_eq!(next_due(check, MIDNIGHT + 23 * H + 52 * M, tz), Some(MIDNIGHT + 24 * H + 21 * M), "wraps to the next day");
        let hourly33 = Schedule::Every { minutes: 60, offset: 33 };
        assert_eq!(next_due(hourly33, MIDNIGHT + 10 * H + 40 * M, tz), Some(MIDNIGHT + 11 * H + 33 * M));
        assert_eq!(next_due(Schedule::Every { minutes: 2, offset: 0 }, MIDNIGHT + 61_000, tz), Some(MIDNIGHT + 2 * M));
        assert_eq!(next_due(Schedule::Manual, MIDNIGHT, tz), None);
        assert_eq!(next_due(Schedule::Every { minutes: 7, offset: 0 }, MIDNIGHT, tz), None, "must divide a day");
        // UTC zone: 03:00 UTC is 11:00 at UTC+8
        assert_eq!(next_due(daily3, MIDNIGHT, 0), Some(MIDNIGHT + 11 * H));
    }

    #[test]
    fn registry_is_closed_and_every_schedule_is_valid() {
        for def in REGISTRY {
            assert_eq!(TaskId::parse(def.id.as_str()), Some(def.id));
            assert!(crate::task_runs::valid_run_id(def.id.as_str()));
            if def.schedule != Schedule::Manual {
                assert!(next_due(def.schedule, MIDNIGHT, 8 * H).is_some(), "{}", def.id.as_str());
            }
        }
        assert_eq!(TaskId::parse("rm -rf"), None);
        assert_eq!(TaskId::parse("../backup"), None);
        assert_eq!(LEGACY_TASKS.len(), 10);
        for legacy in LEGACY_TASKS {
            if let Some(id) = legacy.replacement {
                assert!(TaskId::parse(id).is_some(), "{}", legacy.name);
            }
        }
    }

    #[test]
    fn scheduler_runs_each_slot_once_and_does_not_catch_up() {
        let mut s = Scheduler::new(|d| d.enabled_by_default, 8 * H, MIDNIGHT + 2 * H + 59 * M);
        assert_eq!(s.next_run(TaskId::MemoryDigest), Some(MIDNIGHT + 3 * H));
        assert_eq!(TaskId::parse("ab-report"), None, "retired tasks cannot be enabled or dispatched");
        assert!(!s.enabled(TaskId::UpstreamCheck) && s.next_run(TaskId::UpstreamCheck).is_none(), "upstream checks are disabled by default");
        s.set_enabled(TaskId::UpstreamCheck, true, MIDNIGHT + 2 * H + 59 * M);
        assert!(s.tick(MIDNIGHT + 2 * H + 59 * M + 30_000).is_empty());
        let due = s.tick(MIDNIGHT + 3 * H + 500);
        assert!(due.contains(&Due { task: TaskId::MemoryDigest, slot: MIDNIGHT + 3 * H }));
        assert!(due.contains(&Due { task: TaskId::Watchdog, slot: MIDNIGHT + 3 * H }));
        assert!(!s.tick(MIDNIGHT + 3 * H + 600).iter().any(|d| d.task == TaskId::MemoryDigest), "a slot is given once");
        // The process slept for five hours: each task is due once, with its first missed slot.
        let late = s.tick(MIDNIGHT + 8 * H);
        assert_eq!(late.iter().filter(|d| d.task == TaskId::Watchdog).count(), 1);
        assert!(late.contains(&Due { task: TaskId::Backup, slot: MIDNIGHT + 4 * H }));
        assert!(late.contains(&Due { task: TaskId::UpstreamCheck, slot: MIDNIGHT + 5 * H }));
        assert_eq!(s.next_run(TaskId::Backup), Some(MIDNIGHT + 28 * H));
        s.set_enabled(TaskId::MemoryDigestCheck, false, MIDNIGHT + 8 * H);
        s.set_enabled(TaskId::MemoryDigestCheck, true, MIDNIGHT + 8 * H);
        assert_eq!(s.next_run(TaskId::MemoryDigestCheck), Some(MIDNIGHT + 8 * H + 21 * M));
        s.set_enabled(TaskId::Backup, false, MIDNIGHT + 8 * H);
        assert!(!s.tick(MIDNIGHT + 30 * H).iter().any(|d| d.task == TaskId::Backup));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-tasks-{name}-{}-{}", std::process::id(), crate::parity_clock::now_ms())));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Executes by running a shell script per task id, for tests only.
    #[cfg(unix)]
    fn shell_executor(dir: &std::path::Path, clock: Arc<dyn Clock>) -> Arc<dyn Executor> {
        // These tests exercise external process execution independently of which production
        // task IDs currently use Rust actions. A task with a shell fixture in `dir` runs it as an
        // external action; a task without one keeps its real action.
        struct Shell(CommandExecutor, std::path::PathBuf);
        impl Executor for Shell {
            fn execute(&self, def: &TaskDef, run: &mut RunHandle, cancel: &AtomicBool) -> Finish {
                if !self.1.join(format!("{}.sh", def.id.as_str())).is_file() {
                    return self.0.execute(def, run, cancel);
                }
                let external = TaskDef { action: TaskAction::Node { script: "fixture", args: &[] }, ..*def };
                self.0.execute(&external, run, cancel)
            }
        }
        let fixtures = dir.to_path_buf();
        let dir = dir.to_path_buf();
        Arc::new(Shell(CommandExecutor::new(
            Box::new(move |def: &TaskDef| {
                Ok(CommandSpec { program: "/bin/sh".into(), args: vec![dir.join(format!("{}.sh", def.id.as_str())).to_string_lossy().into_owned()], cwd: dir.clone() })
            }),
            Box::new(|_, run: &mut RunHandle, _| {
                run.log(LogStream::Out, "internal tick");
                Finish::Succeeded
            }),
            clock,
        ), fixtures))
    }

    fn wait_terminal(store: &TaskStore, run_id: &str) -> crate::task_runs::RunView {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = store.get(run_id).unwrap().unwrap();
            if view.state.is_terminal() {
                return view;
            }
            assert!(Instant::now() < deadline, "run did not end: {view:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_child_processes_report_exit_codes_logs_timeouts_and_cancellation() {
        let dir = temp_dir("exec");
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        std::fs::write(dir.join("upstream-check.sh"), "echo starting\necho 'token=hunter2hunter2' >&2\nexit 0\n").unwrap();
        std::fs::write(dir.join("memory-digest.sh"), "echo working\nexit 7\n").unwrap();
        std::fs::write(dir.join("memory-digest-check.sh"), "echo before; sleep 30; echo never\n").unwrap();
        let store = Arc::new(TaskStore::open(dir.join("store"), StoreLimits::default(), credential_redactor(vec![])).unwrap());
        let dispatcher = Dispatcher::new(store.clone(), shell_executor(&dir, clock.clone()), clock);

        let Trigger::Started(run) = dispatcher.manual(TaskId::UpstreamCheck, "req-exec-0001", "console") else { panic!() };
        assert_eq!(run.state, RunState::Queued, "a trigger answers before the result exists");
        let done = wait_terminal(&store, &run.run_id);
        assert_eq!((done.state, done.exit_code), (RunState::Succeeded, Some(0)));
        assert!(done.pid.is_some() && done.started_at.is_some());
        let log = store.read_log(&run.run_id, None, None).unwrap().unwrap();
        let lines: Vec<(LogStream, &str)> = log.lines.iter().map(|l| (l.s, l.t.as_str())).collect();
        assert!(lines.contains(&(LogStream::Out, "starting")), "{lines:?}");
        assert!(lines.iter().any(|(s, t)| *s == LogStream::Err && !t.contains("hunter2")), "{lines:?}");

        let Trigger::Started(run) = dispatcher.manual(TaskId::MemoryDigest, "req-exec-0002", "console") else { panic!() };
        let done = wait_terminal(&store, &run.run_id);
        assert_eq!((done.state, done.exit_code, done.reason.as_deref()), (RunState::Failed, Some(7), Some("exit code 7")));

        let Trigger::Started(run) = dispatcher.manual(TaskId::MemoryDigestCheck, "req-exec-0003", "console") else { panic!() };
        std::thread::sleep(Duration::from_millis(300));
        assert!(matches!(dispatcher.manual(TaskId::MemoryDigestCheck, "req-exec-0004", "console"), Trigger::AlreadyRunning(Some(r)) if r.run_id == run.run_id));
        assert!(dispatcher.cancel(&run.run_id));
        let done = wait_terminal(&store, &run.run_id);
        assert_eq!(done.state, RunState::Cancelled);
        assert!(!dispatcher.cancel(&run.run_id), "a finished run is no longer cancellable");

        let Trigger::Started(run) = dispatcher.manual(TaskId::Watchdog, "req-exec-0005", "console") else { panic!() };
        assert_eq!(wait_terminal(&store, &run.run_id).state, RunState::Succeeded);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_terminal_run_cannot_still_accept_cancellation() {
        struct Fast;
        impl Executor for Fast {
            fn execute(&self, _: &TaskDef, run: &mut RunHandle, _: &AtomicBool) -> Finish {
                run.mark_running(None, 0).unwrap();
                Finish::Succeeded
            }
        }
        let dir = temp_dir("cancel-terminal-race");
        let store = Arc::new(TaskStore::open(dir.join("store"), StoreLimits::default(), credential_redactor(vec![])).unwrap());
        let dispatcher = Dispatcher::new(store.clone(), Arc::new(Fast), Arc::new(SystemClock));
        for i in 0..500 {
            let request = format!("race-request-{i:04}");
            let deadline = Instant::now() + Duration::from_secs(5);
            let run = loop {
                match dispatcher.manual(TaskId::Watchdog, &request, "test") {
                    Trigger::Started(run) => break run,
                    Trigger::AlreadyRunning(_) if Instant::now() < deadline => std::thread::yield_now(),
                    other => panic!("task did not start: {other:?}"),
                }
            };
            loop {
                if store.get(&run.run_id).unwrap().unwrap().state.is_terminal() { break; }
                assert!(Instant::now() < deadline, "task did not finish");
                std::thread::yield_now();
            }
            assert!(!dispatcher.cancel(&run.run_id), "a terminal run accepted cancellation");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "synthetic child launched by the Windows process-tree test"]
    fn windows_task_tree_grandchild() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "synthetic parent launched by the Windows process-tree test"]
    fn windows_task_tree_parent() {
        let mut command = crate::subprocess::command_without_console(std::env::current_exe().unwrap());
        let child = command.args(["--ignored", "--exact", "tasks::tests::windows_task_tree_grandchild", "--nocapture"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        println!("grandchild-pid={}", child.id());
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(windows)]
    #[test]
    fn cancellation_stops_the_windows_task_and_its_grandchild() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
        let dir = temp_dir("windows-task-tree");
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let store = Arc::new(TaskStore::open(dir.join("store"), StoreLimits::default(), credential_redactor(vec![])).unwrap());
        let exe = std::env::current_exe().unwrap();
        let cwd = dir.clone();
        let resolve: Resolver = Box::new(move |_| Ok(CommandSpec {
            program: exe.clone(), cwd: cwd.clone(),
            args: ["--ignored", "--exact", "tasks::tests::windows_task_tree_parent", "--nocapture"].iter().map(|s| s.to_string()).collect(),
        }));
        let internal: InternalJob = Box::new(|_, _, _| Finish::Succeeded);
        let dispatcher = Dispatcher::new(store.clone(), Arc::new(CommandExecutor::new(resolve, internal, clock.clone())), clock);
        let Trigger::Started(run) = dispatcher.manual(TaskId::UpstreamCheck, "windows-tree-request", "test") else { panic!() };
        let deadline = Instant::now() + Duration::from_secs(15);
        let grandchild = loop {
            if let Some(page) = store.read_log(&run.run_id, None, None).unwrap() {
                if let Some(pid) = page.lines.iter().find_map(|line| line.t.split("grandchild-pid=").nth(1).and_then(|s| s.trim().parse::<u32>().ok())) { break pid; }
            }
            assert!(Instant::now() < deadline, "grandchild was not reported");
            std::thread::sleep(Duration::from_millis(20));
        };
        let handle = unsafe { OpenProcess(0x0010_0000, 0, grandchild) };
        assert!(!handle.is_null(), "grandchild must still be running before cancellation");
        assert!(dispatcher.cancel(&run.run_id));
        assert_eq!(wait_terminal(&store, &run.run_id).state, RunState::Cancelled);
        assert_eq!(unsafe { WaitForSingleObject(handle, 3000) }, WAIT_OBJECT_0, "grandchild must stop too");
        unsafe { CloseHandle(handle); }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_task_that_overruns_its_timeout_fails_and_a_missing_program_fails_to_start() {
        static SLOW: TaskDef = TaskDef { timeout: Duration::from_millis(300), ..REGISTRY[2] };
        let dir = temp_dir("timeout");
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        std::fs::write(dir.join("memory-digest-check.sh"), "sleep 30\n").unwrap();
        let store = Arc::new(TaskStore::open(dir.join("store"), StoreLimits::default(), credential_redactor(vec![])).unwrap());
        let dispatcher = Dispatcher::new(store.clone(), shell_executor(&dir, clock.clone()), clock.clone());
        let Trigger::Started(run) = dispatcher.start(&SLOW, RunSource::Manual, Some("req-slow-0001"), None, "console") else { panic!() };
        let done = wait_terminal(&store, &run.run_id);
        assert_eq!(done.state, RunState::Failed);
        assert!(done.reason.unwrap().contains("timed out"));

        let missing = Arc::new(CommandExecutor::new(
            node_resolver(dir.join("no-such-node"), dir.clone(), dir.clone()),
            Box::new(|_, _: &mut RunHandle, _| Finish::Succeeded),
            clock.clone(),
        ));
        let dispatcher = Dispatcher::new(store.clone(), missing, clock);
        let Trigger::Started(run) = dispatcher.manual(TaskId::UpstreamCheck, "req-missing01", "console") else { panic!() };
        let done = wait_terminal(&store, &run.run_id);
        assert_eq!((done.state, done.exit_code), (RunState::Failed, None));
        assert!(done.reason.unwrap().contains("could not be started"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_closes_output_pipes_is_still_timed_out() {
        static QUICK_TIMEOUT: TaskDef = TaskDef { timeout: Duration::from_millis(300), ..REGISTRY[2] };
        let dir = temp_dir("closed-pipes");
        std::fs::write(dir.join("memory-digest-check.sh"), "exec 1>&- 2>&-\nsleep 3\n").unwrap();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let store = Arc::new(TaskStore::open(dir.join("store"), StoreLimits::default(), credential_redactor(vec![])).unwrap());
        let dispatcher = Dispatcher::new(store.clone(), shell_executor(&dir, clock.clone()), clock);
        let start = Instant::now();
        let Trigger::Started(run) = dispatcher.start(&QUICK_TIMEOUT, RunSource::Manual, Some("req-closed-pipes"), None, "test") else { panic!() };
        let done = wait_terminal(&store, &run.run_id);
        assert_eq!(done.state, RunState::Failed, "closed output pipes do not imply the child exited");
        assert!(done.reason.unwrap_or_default().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2), "timeout must remain bounded when both pipes close");
        let _ = std::fs::remove_dir_all(dir);
    }

    struct Gate(Mutex<bool>, std::sync::Condvar);

    struct BlockingExecutor(Arc<Gate>);

    impl Executor for BlockingExecutor {
        fn execute(&self, _: &TaskDef, run: &mut RunHandle, _: &AtomicBool) -> Finish {
            run.mark_running(None, 0).unwrap();
            let mut open = self.0 .0.lock().unwrap();
            while !*open {
                open = self.0 .1.wait(open).unwrap();
            }
            Finish::Succeeded
        }
    }

    #[test]
    fn scheduled_slots_are_deduplicated_and_held_by_the_desired_state() {
        let dir = temp_dir("dispatch");
        let clock = Arc::new(ManualClock::new(MIDNIGHT + 3 * H));
        let store = Arc::new(TaskStore::open(&dir, StoreLimits::default(), credential_redactor(vec![])).unwrap());
        let gate = Arc::new(Gate(Mutex::new(true), std::sync::Condvar::new()));
        let dispatcher = Dispatcher::new(store.clone(), Arc::new(BlockingExecutor(gate)), clock.clone());
        let slot = Due { task: TaskId::Backup, slot: MIDNIGHT + 4 * H };
        let Trigger::Started(run) = dispatcher.on_due(slot, &running()) else { panic!() };
        wait_terminal(&store, &run.run_id);
        // the same slot again (a second tick, a restart within the slot) is the same run
        assert!(matches!(dispatcher.on_due(slot, &running()), Trigger::Duplicate(r) if r.run_id == run.run_id));
        let stopped = Effective::Act { state: DesiredState::Stopped, wants: Wants { live: false, console: false, jobs: false } };
        let next = Due { task: TaskId::Backup, slot: MIDNIGHT + 28 * H };
        assert!(matches!(dispatcher.on_due(next, &stopped), Trigger::Held(_)));
        assert!(matches!(dispatcher.on_due(next, &Effective::Unknown("corrupt".into())), Trigger::Held(_)));
        // the watchdog runs while held: it is what keeps a stopped bot stopped
        let wd = Due { task: TaskId::Watchdog, slot: MIDNIGHT + 28 * H };
        assert!(matches!(dispatcher.on_due(wd, &stopped), Trigger::Started(_)));
        assert_eq!(store.list(&RunQuery::default()).unwrap().runs.iter().filter(|r| r.task_id == "backup").count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_scheduler_loop_triggers_due_slots_with_an_injected_clock() {
        let dir = temp_dir("loop");
        let clock = Arc::new(ManualClock::new(MIDNIGHT + 3 * H + 59 * M));
        let store = Arc::new(TaskStore::open(&dir, StoreLimits::default(), credential_redactor(vec![])).unwrap());
        let gate = Arc::new(Gate(Mutex::new(true), std::sync::Condvar::new()));
        let dispatcher = Dispatcher::new(store.clone(), Arc::new(BlockingExecutor(gate)), clock.clone());
        let scheduler = Arc::new(Mutex::new(Scheduler::new(|d| d.id == TaskId::Backup, 8 * H, clock.now_ms())));
        let stop = AtomicBool::new(false);
        let ticks = std::cell::Cell::new(0);
        run_scheduler(
            scheduler.clone(),
            dispatcher,
            |_| running(),
            |_| {
                ticks.set(ticks.get() + 1);
                clock.advance(30_000);
                if ticks.get() == 6 {
                    stop.store(true, Ordering::SeqCst);
                }
            },
            Duration::from_secs(30),
            &stop,
        );
        std::thread::sleep(Duration::from_millis(200));
        let runs = store.list(&RunQuery::default()).unwrap().runs;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!((runs[0].task_id.as_str(), runs[0].source), ("backup", RunSource::Scheduled));
        assert_eq!(scheduler.lock().unwrap().next_run(TaskId::Backup), Some(MIDNIGHT + 28 * H));
        let _ = std::fs::remove_dir_all(dir);
    }
}
