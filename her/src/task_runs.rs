//! Ticket 27: background task run history and run logs.
//!
//! Layout under the store directory (phase B places it at `<data>/tasks`):
//! - `runs.jsonl`: append-only run events, one JSON object per line. The first event of a run is `queued` and carries
//!   the task, source, request id or schedule key and who triggered it; `running` adds the child pid; a terminal event
//!   (`succeeded`, `failed`, `cancelled`, `unknown`) adds exit code and reason. A run's state is the fold of its events;
//!   a terminal state is final.
//! - `store.lock`: OS gate for every append, de-duplication decision and compaction (bounded wait).
//! - `locks/<task>.lock`: held by the process running that task for the whole run. It de-duplicates concurrent triggers
//!   across processes and tells a restart which unfinished runs lost their process: those become `unknown`, never
//!   `succeeded` or re-run.
//! - `logs/<runId>.jsonl`: the run's output lines `{n, s: out|err|meta, t}`, redacted before they are written and bounded
//!   (head up to `max_log_bytes`, then the last `tail_lines` lines after an omission marker).
//!
//! The history is compacted to the newest `keep_runs` runs (unfinished runs always kept) when it exceeds
//! `max_history_bytes`; logs of dropped runs are deleted with them.
#![allow(dead_code)]

use crate::lifecycle::{lock_with_wait, write_replace};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const FORMAT_VERSION: u8 = 1;
pub const STORE_GATE_WAIT: Duration = Duration::from_secs(5);
pub const MAX_REASON_CHARS: usize = crate::run_record::MAX_REASON_CHARS;
pub const MAX_PAGE: usize = 200;
pub const DEFAULT_PAGE: usize = 50;
pub const MAX_LOG_PAGE: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunSource {
    Scheduled,
    Manual,
    Recovery,
    Login,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// The result was not observed (the process running the task ended first).
    Unknown,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, RunState::Queued | RunState::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogStream {
    Out,
    Err,
    Meta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunEvent {
    v: u8,
    run_id: String,
    state: RunState,
    at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<RunSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dedup_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// One run as the web page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunView {
    pub run_id: String,
    /// Increasing per store; the paging cursor.
    pub n: u64,
    pub task_id: String,
    pub source: RunSource,
    pub state: RunState,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub pid: Option<u32>,
    pub request_id: Option<String>,
    pub by: Option<String>,
    #[serde(skip)]
    dedup_key: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct StoreLimits {
    pub max_history_bytes: u64,
    pub keep_runs: usize,
    pub max_log_bytes: u64,
    pub tail_lines: usize,
    pub max_line_chars: usize,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self { max_history_bytes: 1024 * 1024, keep_runs: 500, max_log_bytes: 256 * 1024, tail_lines: 200, max_line_chars: 2000 }
    }
}

pub type Redactor = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Credentials, paths and the given secrets (console token, session tokens) masked as the JS `redactSensitiveText`.
pub fn credential_redactor(secrets: Vec<String>) -> Redactor {
    Arc::new(move |text: &str| crate::pipeline::redact_sensitive_text(text, &secrets))
}

pub struct TaskStore {
    dir: PathBuf,
    limits: StoreLimits,
    redact: Redactor,
    #[cfg(target_os = "linux")]
    pinned: Arc<PinnedDirs>,
}

/// How a trigger was settled.
#[derive(Debug)]
pub enum Claim {
    Started(RunHandle),
    /// The same request id (or schedule slot) already made a run: that run, not a new one.
    Duplicate(RunView),
    /// The task is running now (in this or another process).
    AlreadyRunning(Option<RunView>),
    /// The request id was used for another task.
    Conflict(String),
    /// The store could not decide (gate busy, I/O error): nothing was started.
    Unavailable(String),
}

/// The right to run one task once: holds the task lock until it is finished or dropped.
#[derive(Debug)]
pub struct RunHandle {
    view: RunView,
    task_lock: Option<File>,
    log: Option<File>,
    log_bytes: u64,
    line_no: u64,
    omitted: u64,
    tail: VecDeque<(LogStream, String)>,
    finished: bool,
    store: StoreRef,
}

#[derive(Clone)]
struct StoreRef {
    dir: PathBuf,
    limits: StoreLimits,
    redact: Redactor,
    #[cfg(target_os = "linux")]
    pinned: Arc<PinnedDirs>,
}

#[cfg(target_os = "linux")]
struct PinnedDirs { root: File, locks: File, logs: File }

impl std::fmt::Debug for StoreRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StoreRef({})", self.dir.display())
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finish {
    Succeeded,
    Failed { exit_code: Option<i32>, reason: String },
    Cancelled { reason: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone, Default)]
pub struct RunQuery {
    pub task: Option<String>,
    /// Only runs with `n` lower than this (the previous page's `next_before`).
    pub before: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunPage {
    pub runs: Vec<RunView>,
    pub next_before: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    pub n: u64,
    pub s: LogStream,
    pub t: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub lines: Vec<LogLine>,
    pub next_after: Option<u64>,
}

static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

fn redirected(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() { return true; }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 { return true; }
    }
    false
}

fn store_dir(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !redirected(&meta) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            let meta = std::fs::symlink_metadata(path)?;
            if meta.is_dir() && !redirected(&meta) { Ok(()) }
            else { Err(io::Error::new(io::ErrorKind::PermissionDenied, "task directory is redirected")) }
        }
        _ => Err(io::Error::new(io::ErrorKind::PermissionDenied, "task directory is redirected or unavailable")),
    }
}

fn store_file(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(meta) if meta.is_file() && !redirected(&meta) => Ok(()),
        _ => Err(io::Error::new(io::ErrorKind::PermissionDenied, "task file is redirected or unavailable")),
    }
}

fn task_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
}

#[cfg(target_os = "linux")]
fn create_pinned_child(root: &File, name: &str) -> io::Result<File> {
    let path = crate::lifecycle::pinned_directory_path(root).join(name);
    match std::fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    crate::lifecycle::pin_directory(&path)
}

/// `[a-z0-9-]{1,96}`: a run id that can name a file.
pub fn valid_run_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 96 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `[A-Za-z0-9_-]{8,64}`: a client-generated idempotency key.
pub fn valid_request_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_task_id(id: &str) -> bool {
    valid_run_id(id) && id.len() <= 48
}

fn cut_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        out.push(if c.is_control() { ' ' } else { c });
    }
    out.trim_end().to_string()
}

impl StoreRef {
    fn root_path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        { crate::lifecycle::pinned_directory_path(&self.pinned.root) }
        #[cfg(not(target_os = "linux"))]
        { self.dir.clone() }
    }
    fn locks_path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        { crate::lifecycle::pinned_directory_path(&self.pinned.locks) }
        #[cfg(not(target_os = "linux"))]
        { self.dir.join("locks") }
    }
    fn logs_path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        { crate::lifecycle::pinned_directory_path(&self.pinned.logs) }
        #[cfg(not(target_os = "linux"))]
        { self.dir.join("logs") }
    }
    fn validate(&self) -> io::Result<()> {
        for dir in [&self.dir, &self.dir.join("locks"), &self.dir.join("logs")] {
            let meta = std::fs::symlink_metadata(dir)?;
            if !meta.is_dir() || redirected(&meta) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "task directory is redirected"));
            }
        }
        store_file(&self.root_path().join("store.lock"))?;
        store_file(&self.history())
    }
    fn history(&self) -> PathBuf {
        self.root_path().join("runs.jsonl")
    }

    fn log_path(&self, run_id: &str) -> PathBuf {
        self.logs_path().join(format!("{run_id}.jsonl"))
    }

    fn task_lock_path(&self, task: &str) -> PathBuf {
        self.locks_path().join(format!("{task}.lock"))
    }

    fn gate(&self) -> Result<File, String> {
        self.validate().map_err(|error| format!("task store unavailable: {}", error.kind()))?;
        lock_with_wait(&self.root_path().join("store.lock"), STORE_GATE_WAIT).map_err(|e| format!("task store {e}"))
    }

    /// Ticket 25's run-record scrub (credentials, absolute paths, long opaque strings, one line, 300 chars) after
    /// this store's redactor.
    fn redacted_reason(&self, reason: &str) -> String {
        crate::run_record::sanitize_reason(reason, &*self.redact)
    }

    fn append(&self, event: &RunEvent) -> io::Result<()> {
        self.validate()?;
        let mut line = serde_json::to_vec(event).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        let mut file = task_file_options().read(true).append(true).create(true).open(self.history())?;
        if crate::runtime::ends_mid_line(&mut file)? {
            line.insert(0, b'\n'); // a crash-torn last line is never glued to this record
        }
        file.write_all(&line)?;
        file.sync_data()
    }

    fn load_events(&self) -> io::Result<Vec<RunEvent>> {
        self.validate()?;
        let file = match task_file_options().read(true).open(self.history()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut events = Vec::new();
        for line in io::BufReader::new(file).split(b'\n') {
            let line = line?;
            if let Ok(event) = serde_json::from_slice::<RunEvent>(&line) {
                if event.v == FORMAT_VERSION {
                    events.push(event);
                }
            }
        }
        Ok(events)
    }

    /// Runs in order of creation.
    fn fold(&self, events: &[RunEvent]) -> Vec<RunView> {
        let mut order: Vec<String> = Vec::new();
        let mut runs: HashMap<String, RunView> = HashMap::new();
        for event in events {
            if event.state == RunState::Queued {
                let (Some(task_id), Some(source), Some(n)) = (&event.task_id, event.source, event.n) else { continue };
                if runs.contains_key(&event.run_id) {
                    continue;
                }
                order.push(event.run_id.clone());
                runs.insert(
                    event.run_id.clone(),
                    RunView {
                        run_id: event.run_id.clone(),
                        n,
                        task_id: task_id.clone(),
                        source,
                        state: RunState::Queued,
                        queued_at: event.at,
                        started_at: None,
                        ended_at: None,
                        exit_code: None,
                        reason: None,
                        pid: None,
                        request_id: event.request_id.clone(),
                        by: event.by.clone(),
                        dedup_key: event.dedup_key.clone(),
                    },
                );
                continue;
            }
            let Some(run) = runs.get_mut(&event.run_id) else { continue };
            if run.state.is_terminal() {
                continue;
            }
            run.state = event.state;
            if event.state == RunState::Running {
                run.started_at = Some(event.at);
                run.pid = event.pid.or(run.pid);
            } else {
                run.ended_at = Some(event.at);
                run.exit_code = event.exit_code;
                run.reason = event.reason.clone();
            }
        }
        order.into_iter().filter_map(|id| runs.remove(&id)).collect()
    }

    fn terminal_event(&self, run_id: &str, finish: &Finish, now: i64) -> RunEvent {
        let (state, exit_code, reason) = match finish {
            Finish::Succeeded => (RunState::Succeeded, Some(0), None),
            Finish::Failed { exit_code, reason } => (RunState::Failed, *exit_code, Some(reason.as_str())),
            Finish::Cancelled { reason } => (RunState::Cancelled, None, Some(reason.as_str())),
            Finish::Unknown { reason } => (RunState::Unknown, None, Some(reason.as_str())),
        };
        RunEvent {
            v: FORMAT_VERSION,
            run_id: run_id.to_string(),
            state,
            at: now,
            n: None,
            task_id: None,
            source: None,
            request_id: None,
            dedup_key: None,
            by: None,
            pid: None,
            exit_code,
            reason: reason.map(|r| self.redacted_reason(r)),
        }
    }

    fn compact_locked(&self) -> io::Result<()> {
        let len = task_file_options().read(true).open(self.history()).and_then(|f| f.metadata()).map(|m| m.len()).unwrap_or(0);
        if len <= self.limits.max_history_bytes {
            return Ok(());
        }
        let events = self.load_events()?;
        let runs = self.fold(&events);
        let finished: Vec<&RunView> = runs.iter().filter(|r| r.state.is_terminal() && !surviving_task_group(r)).collect();
        let drop_count = finished.len().saturating_sub(self.limits.keep_runs);
        let dropped: std::collections::HashSet<&str> = finished.iter().take(drop_count).map(|r| r.run_id.as_str()).collect();
        let mut kept = Vec::new();
        for event in events.iter().filter(|e| !dropped.contains(e.run_id.as_str())) {
            kept.extend(serde_json::to_vec(event).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?);
            kept.push(b'\n');
        }
        write_replace(&self.history(), &kept)?;
        for run_id in dropped {
            let _ = std::fs::remove_file(self.log_path(run_id));
        }
        Ok(())
    }
}

impl TaskStore {
    pub fn open(dir: impl Into<PathBuf>, limits: StoreLimits, redact: Redactor) -> io::Result<Self> {
        let dir = dir.into();
        store_dir(&dir)?;
        #[cfg(target_os = "linux")]
        let pinned = {
            let root = crate::lifecycle::pin_directory(&dir)?;
            let locks = create_pinned_child(&root, "locks")?;
            let logs = create_pinned_child(&root, "logs")?;
            Arc::new(PinnedDirs { root, locks, logs })
        };
        #[cfg(not(target_os = "linux"))]
        {
        store_dir(&dir.join("locks"))?;
        store_dir(&dir.join("logs"))?;
        }
        store_file(&dir.join("store.lock"))?;
        store_file(&dir.join("runs.jsonl"))?;
        Ok(Self { dir, limits, redact, #[cfg(target_os = "linux")] pinned })
    }

    fn store_ref(&self) -> StoreRef {
        StoreRef { dir: self.dir.clone(), limits: self.limits, redact: self.redact.clone(),
            #[cfg(target_os = "linux")] pinned: self.pinned.clone() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Settle one trigger. `request_id` (manual) and `dedup_key` (a schedule slot) make repeated triggers return the
    /// first run instead of starting another; a task already running is never started twice.
    pub fn claim(
        &self,
        task: &str,
        source: RunSource,
        request_id: Option<&str>,
        dedup_key: Option<&str>,
        by: &str,
        now: i64,
    ) -> Claim {
        if !valid_task_id(task) {
            return Claim::Unavailable("invalid task id".to_string());
        }
        if request_id.is_some_and(|id| !valid_request_id(id)) {
            return Claim::Conflict("invalid request id".to_string());
        }
        let store = self.store_ref();
        let _gate = match store.gate() {
            Ok(gate) => gate,
            Err(error) => return Claim::Unavailable(error),
        };
        let events = match store.load_events() {
            Ok(events) => events,
            Err(error) => return Claim::Unavailable(format!("task history is unreadable: {}", error.kind())),
        };
        let runs = store.fold(&events);
        if let Some(id) = request_id {
            if let Some(run) = runs.iter().find(|r| r.request_id.as_deref() == Some(id)) {
                return if run.task_id == task {
                    Claim::Duplicate(run.clone())
                } else {
                    Claim::Conflict("request id was already used for another task".to_string())
                };
            }
        }
        if let Some(key) = dedup_key {
            if let Some(run) = runs.iter().find(|r| r.dedup_key.as_deref() == Some(key)) {
                return Claim::Duplicate(run.clone());
            }
        }
        if let Err(error) = store_file(&store.task_lock_path(task)) {
            return Claim::Unavailable(format!("task lock cannot be opened: {}", error.kind()));
        }
        let lock = match task_file_options().read(true).write(true).create(true).truncate(false).open(store.task_lock_path(task)) {
            Ok(file) => file,
            Err(error) => return Claim::Unavailable(format!("task lock cannot be opened: {}", error.kind())),
        };
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let running = runs.iter().rev().find(|r| r.task_id == task && !r.state.is_terminal()).cloned();
                return Claim::AlreadyRunning(running);
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Claim::Unavailable(format!("task lock cannot be taken: {}", error.kind()))
            }
        }
        // A free task lock proves the runner is gone, not that its child group exited.
        for stale in runs.iter().filter(|r| r.task_id == task) {
            if surviving_task_group(stale) { return Claim::AlreadyRunning(Some(stale.clone())); }
            if stale.state.is_terminal() { continue; }
            let event = store.terminal_event(&stale.run_id, &lost_process(), now);
            if let Err(error) = store.append(&event) {
                return Claim::Unavailable(format!("task history cannot be written: {}", error.kind()));
            }
        }
        let n = runs.iter().map(|r| r.n).max().unwrap_or(0) + 1;
        let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("{task}-{now}-{}-{seq}", std::process::id());
        let by = cut_chars(&one_line(by), 64);
        let event = RunEvent {
            v: FORMAT_VERSION,
            run_id: run_id.clone(),
            state: RunState::Queued,
            at: now,
            n: Some(n),
            task_id: Some(task.to_string()),
            source: Some(source),
            request_id: request_id.map(str::to_string),
            dedup_key: dedup_key.map(str::to_string),
            by: Some(by.clone()),
            pid: None,
            exit_code: None,
            reason: None,
        };
        if let Err(error) = store.append(&event) {
            return Claim::Unavailable(format!("task history cannot be written: {}", error.kind()));
        }
        let view = store.fold(std::slice::from_ref(&event)).remove(0);
        Claim::Started(RunHandle {
            view,
            task_lock: Some(lock),
            log: None,
            log_bytes: 0,
            line_no: 0,
            omitted: 0,
            tail: VecDeque::new(),
            finished: false,
            store,
        })
    }

    /// After a restart: unfinished runs whose task lock is free lost their process and become `unknown`. Runs still
    /// held by a living process (this or another) are left alone. Returns the run ids settled.
    pub fn recover(&self, now: i64) -> Result<Vec<String>, String> {
        let store = self.store_ref();
        let _gate = store.gate()?;
        let runs = store.fold(&store.load_events().map_err(|e| format!("task history is unreadable: {}", e.kind()))?);
        let mut settled = Vec::new();
        for run in runs.iter().filter(|r| !r.state.is_terminal()) {
            if store_file(&store.task_lock_path(&run.task_id)).is_err() { continue; }
            let Ok(lock) = task_file_options().read(true).write(true).create(true).truncate(false).open(store.task_lock_path(&run.task_id)) else {
                continue; // cannot tell: leave it
            };
            if lock.try_lock().is_ok() {
                if surviving_task_group(run) { continue; }
                store
                    .append(&store.terminal_event(&run.run_id, &lost_process(), now))
                    .map_err(|e| format!("task history cannot be written: {}", e.kind()))?;
                settled.push(run.run_id.clone());
            }
        }
        Ok(settled)
    }

    /// Locks can be free while descriptor-closing descendants still run. Stop
    /// must inspect every recorded ordinary group, including terminal records
    /// made by an older runner, before it can report complete quiescence.
    pub fn has_surviving_groups(&self) -> Result<bool, String> {
        let store = self.store_ref();
        let _gate = store.gate()?;
        let runs = store.fold(&store.load_events().map_err(|e| format!("task history is unreadable: {}", e.kind()))?);
        Ok(runs.iter().any(|run| {
            if matches!(crate::tasks::TaskId::parse(&run.task_id), Some(id) if crate::tasks::task_def(id).action == crate::tasks::TaskAction::Internal) { return false; }
            surviving_task_group(run)
        }))
    }

    pub fn list(&self, query: &RunQuery) -> Result<RunPage, String> {
        let store = self.store_ref();
        let runs = store.fold(&store.load_events().map_err(|e| format!("task history is unreadable: {}", e.kind()))?);
        let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
        let mut matching = runs
            .into_iter()
            .rev()
            .filter(|r| query.task.as_deref().is_none_or(|t| r.task_id == t) && query.before.is_none_or(|b| r.n < b));
        let page: Vec<RunView> = matching.by_ref().take(limit).collect();
        let next_before = if matching.next().is_some() { page.last().map(|r| r.n) } else { None };
        Ok(RunPage { runs: page, next_before })
    }

    pub fn get(&self, run_id: &str) -> Result<Option<RunView>, String> {
        if !valid_run_id(run_id) {
            return Ok(None);
        }
        let store = self.store_ref();
        let runs = store.fold(&store.load_events().map_err(|e| format!("task history is unreadable: {}", e.kind()))?);
        Ok(runs.into_iter().find(|r| r.run_id == run_id))
    }

    /// The newest finished or unfinished run of each task.
    pub fn latest_by_task(&self) -> Result<HashMap<String, RunView>, String> {
        let store = self.store_ref();
        let runs = store.fold(&store.load_events().map_err(|e| format!("task history is unreadable: {}", e.kind()))?);
        let mut latest = HashMap::new();
        for run in runs {
            latest.insert(run.task_id.clone(), run);
        }
        Ok(latest)
    }

    /// Log lines of a run known to the history, after line `after`, at most `limit` (bounded).
    pub fn read_log(&self, run_id: &str, after: Option<u64>, limit: Option<usize>) -> Result<Option<LogPage>, String> {
        if self.get(run_id)?.is_none() {
            return Ok(None);
        }
        let limit = limit.unwrap_or(MAX_LOG_PAGE).clamp(1, MAX_LOG_PAGE);
        let store = self.store_ref();
        store.validate().map_err(|error| format!("task store unavailable: {}", error.kind()))?;
        store_file(&store.log_path(run_id)).map_err(|error| format!("run log is unavailable: {}", error.kind()))?;
        let file = match task_file_options().read(true).open(store.log_path(run_id)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Some(LogPage { lines: Vec::new(), next_after: None }))
            }
            Err(error) => return Err(format!("run log is unreadable: {}", error.kind())),
        };
        let mut lines = Vec::new();
        let mut more = false;
        for raw in io::BufReader::new(file).split(b'\n') {
            let raw = raw.map_err(|e| format!("run log is unreadable: {}", e.kind()))?;
            let Ok(line) = serde_json::from_slice::<LogLine>(&raw) else { continue };
            if after.is_some_and(|a| line.n <= a) {
                continue;
            }
            if lines.len() == limit {
                more = true;
                break;
            }
            lines.push(line);
        }
        let next_after = if more { lines.last().map(|l| l.n) } else { None };
        Ok(Some(LogPage { lines, next_after }))
    }
}

fn lost_process() -> Finish {
    Finish::Unknown { reason: "the process running this task ended before its result was recorded".to_string() }
}

/// A lost runner does not prove that its ordinary child process group died with it.
/// A reused group id may cause a conservative refusal, never a second overlapping run.
fn surviving_task_group(run: &RunView) -> bool {
    run.pid.is_some_and(task_group_alive)
}

pub(crate) fn task_group_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 { return false; }
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(pid as i32), 0) };
        if result == 0 {
            #[cfg(target_os = "linux")]
            {
                // Reaped leaders can leave dead descendants awaiting their
                // adoptive parent's wait. A zombie main thread can still have
                // live sibling threads, so check those before declaring exit.
                let Ok(entries) = std::fs::read_dir("/proc") else { return true };
                for entry in entries {
                    let Ok(entry) = entry else { return true };
                    if entry.file_name().to_string_lossy().parse::<u32>().is_err() { continue; }
                    let text = match std::fs::read_to_string(entry.path().join("stat")) {
                        Ok(text) => text,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => return true,
                    };
                    let Some((_, fields)) = text.rsplit_once(')') else { return true };
                    let fields: Vec<&str> = fields.split_whitespace().collect();
                    if fields.get(2).and_then(|field| field.parse::<u32>().ok()) != Some(pid) { continue; }
                    if !matches!(fields.first(), Some(&"Z" | &"X")) { return true; }
                    let threads = match std::fs::read_dir(entry.path().join("task")) {
                        Ok(threads) => threads,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => return true,
                    };
                    for thread in threads {
                        let Ok(thread) = thread else { return true };
                        let text = match std::fs::read_to_string(thread.path().join("stat")) {
                            Ok(text) => text,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(_) => return true,
                        };
                        let Some((_, fields)) = text.rsplit_once(')') else { return true };
                        if !matches!(fields.split_whitespace().next(), Some("Z" | "X")) { return true; }
                    }
                }
                return false;
            }
            #[cfg(not(target_os = "linux"))]
            return true;
        }
        return std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    }
    #[cfg(windows)]
    { false } // the owning Windows job object terminates its members when the handle closes
    #[cfg(not(any(unix, windows)))]
    { let _ = pid; true }
}

impl RunHandle {
    pub fn run_id(&self) -> &str {
        &self.view.run_id
    }

    pub fn view(&self) -> &RunView {
        &self.view
    }

    /// The spawned Unix child inherits this same locked open-file description.
    /// Its lifetime then covers the interval before its PID is recorded as well
    /// as a runner crash, including descendants that keep the descriptor open.
    #[cfg(unix)]
    pub(crate) fn child_gate_fd(&self) -> io::Result<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.task_lock.as_ref().map(AsRawFd::as_raw_fd)
            .ok_or_else(|| io::Error::other("task gate is unavailable"))
    }

    /// The task process exists now (`pid` when there is one).
    pub fn mark_running(&mut self, pid: Option<u32>, now: i64) -> Result<(), String> {
        let store = self.store.clone();
        let _gate = store.gate()?;
        let event = RunEvent {
            v: FORMAT_VERSION,
            run_id: self.view.run_id.clone(),
            state: RunState::Running,
            at: now,
            n: None,
            task_id: None,
            source: None,
            request_id: None,
            dedup_key: None,
            by: None,
            pid,
            exit_code: None,
            reason: None,
        };
        store.append(&event).map_err(|e| format!("task history cannot be written: {}", e.kind()))?;
        self.view.state = RunState::Running;
        self.view.started_at = Some(now);
        self.view.pid = pid;
        Ok(())
    }

    /// One output line: redacted, cut, then written while under the byte bound or kept for the tail.
    pub fn log(&mut self, stream: LogStream, text: &str) {
        let text = cut_chars(&one_line(&(self.store.redact)(text)), self.store.limits.max_line_chars);
        let limit = self.store.limits.max_log_bytes;
        let estimate = text.len() as u64 + 32;
        if self.omitted == 0 && self.tail.is_empty() && self.log_bytes + estimate <= limit {
            self.write_line(stream, text);
            return;
        }
        self.tail.push_back((stream, text));
        if self.tail.len() > self.store.limits.tail_lines {
            self.tail.pop_front();
            self.omitted += 1;
        }
    }

    fn write_line(&mut self, stream: LogStream, text: String) {
        self.line_no += 1;
        let line = LogLine { n: self.line_no, s: stream, t: text };
        let Ok(mut bytes) = serde_json::to_vec(&line) else { return };
        bytes.push(b'\n');
        if self.log.is_none() {
            if self.store.validate().is_ok() && store_file(&self.store.log_path(&self.view.run_id)).is_ok() {
                self.log = task_file_options().create(true).append(true).open(self.store.log_path(&self.view.run_id)).ok();
            }
        }
        if let Some(file) = self.log.as_mut() {
            if file.write_all(&bytes).is_ok() {
                self.log_bytes += bytes.len() as u64;
            }
        }
    }

    fn flush_tail(&mut self) {
        if self.omitted > 0 {
            let marker = format!("… {} lines omitted …", self.omitted);
            self.write_line(LogStream::Meta, marker);
        }
        while let Some((stream, text)) = self.tail.pop_front() {
            self.write_line(stream, text);
        }
        if let Some(file) = self.log.as_mut() {
            let _ = file.sync_data();
        }
    }

    /// Record the result and release the task lock (after the record is written, so no second run can start before
    /// this one is settled).
    pub fn finish(mut self, finish: Finish, now: i64) -> Result<RunView, String> {
        self.flush_tail();
        let store = self.store.clone();
        let event = store.terminal_event(&self.view.run_id, &finish, now);
        let result = (|| {
            let _gate = store.gate()?;
            store.append(&event).map_err(|e| format!("task history cannot be written: {}", e.kind()))?;
            let _ = store.compact_locked();
            Ok::<(), String>(())
        })();
        self.finished = true;
        result?;
        self.view.state = event.state;
        self.view.ended_at = Some(now);
        self.view.exit_code = event.exit_code;
        self.view.reason = event.reason;
        drop(self.task_lock.take());
        Ok(self.view.clone())
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        if !self.finished {
            // Dropped without a result (a panic in the runner): the run's outcome was not observed.
            self.flush_tail();
            let event = self.store.terminal_event(&self.view.run_id, &Finish::Unknown { reason: "the runner ended without a result".into() }, crate::parity_clock::now_ms());
            if let Ok(_gate) = self.store.gate() {
                let _ = self.store.append(&event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-task-runs-{name}-{}-{}", std::process::id(), crate::parity_clock::now_ms())));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store(dir: &Path, limits: StoreLimits) -> TaskStore {
        TaskStore::open(dir, limits, credential_redactor(vec!["sess-token-0001".into()])).unwrap()
    }

    fn started(claim: Claim) -> RunHandle {
        match claim {
            Claim::Started(handle) => handle,
            other => panic!("expected a new run: {other:?}"),
        }
    }

    #[test]
    fn a_run_records_source_state_exit_code_and_reason() {
        let dir = temp_dir("lifecycle");
        let s = store(&dir, StoreLimits::default());
        let mut run = started(s.claim("backup", RunSource::Manual, Some("req-00000001"), None, "console", 100));
        assert_eq!(s.get(run.run_id()).unwrap().unwrap().state, RunState::Queued);
        run.mark_running(Some(4242), 110).unwrap();
        let id = run.run_id().to_string();
        assert_eq!(s.get(&id).unwrap().unwrap().state, RunState::Running);
        let done = run.finish(Finish::Failed { exit_code: Some(3), reason: "exit code 3".into() }, 150).unwrap();
        let stored = s.get(&id).unwrap().unwrap();
        assert_eq!(stored, done);
        assert_eq!(
            (stored.task_id.as_str(), stored.source, stored.state, stored.exit_code, stored.reason.as_deref()),
            ("backup", RunSource::Manual, RunState::Failed, Some(3), Some("exit code 3"))
        );
        assert_eq!((stored.queued_at, stored.started_at, stored.ended_at, stored.pid), (100, Some(110), Some(150), Some(4242)));
        assert_eq!((stored.request_id.as_deref(), stored.by.as_deref()), (Some("req-00000001"), Some("console")));
        let json = serde_json::to_value(&stored).unwrap();
        assert_eq!(json["state"], "failed");
        assert_eq!(json["source"], "manual");
        assert!(json.get("dedupKey").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_repeated_request_returns_the_first_run_and_a_running_task_is_not_started_twice() {
        let dir = temp_dir("dedup");
        let s = store(&dir, StoreLimits::default());
        let first = started(s.claim("backup", RunSource::Manual, Some("req-aaaaaaaa"), None, "console", 1));
        match s.claim("backup", RunSource::Manual, Some("req-aaaaaaaa"), None, "console", 2) {
            Claim::Duplicate(run) => assert_eq!(run.run_id, first.run_id()),
            other => panic!("{other:?}"),
        }
        match s.claim("backup", RunSource::Manual, Some("req-bbbbbbbb"), None, "console", 3) {
            Claim::AlreadyRunning(Some(run)) => assert_eq!(run.run_id, first.run_id()),
            other => panic!("{other:?}"),
        }
        assert!(matches!(s.claim("digest", RunSource::Manual, Some("req-aaaaaaaa"), None, "console", 4), Claim::Conflict(_)));
        assert!(matches!(s.claim("backup", RunSource::Manual, Some("short"), None, "console", 4), Claim::Conflict(_)));
        let first_id = first.run_id().to_string();
        first.finish(Finish::Succeeded, 5).unwrap();
        // finished: the same request id still answers with the first run, a new id starts a new run
        assert!(matches!(s.claim("backup", RunSource::Manual, Some("req-aaaaaaaa"), None, "console", 6), Claim::Duplicate(r) if r.run_id == first_id));
        let second = started(s.claim("backup", RunSource::Manual, Some("req-cccccccc"), None, "console", 7));
        assert_ne!(second.run_id(), first_id);
        // a schedule slot runs once
        drop(second);
        let slot = started(s.claim("digest", RunSource::Scheduled, None, Some("scheduled:digest:600000"), "scheduler", 8));
        slot.finish(Finish::Succeeded, 9).unwrap();
        assert!(matches!(s.claim("digest", RunSource::Scheduled, None, Some("scheduled:digest:600000"), "scheduler", 10), Claim::Duplicate(_)));
        assert_eq!(s.list(&RunQuery::default()).unwrap().runs.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_triggers_start_exactly_one_run() {
        let dir = temp_dir("concurrent");
        let s = Arc::new(store(&dir, StoreLimits::default()));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    match s.claim("backup", RunSource::Manual, Some(&format!("req-thread-{i}")), None, "console", i) {
                        Claim::Started(handle) => {
                            std::thread::sleep(Duration::from_millis(300));
                            handle.finish(Finish::Succeeded, 100).unwrap();
                            1
                        }
                        Claim::AlreadyRunning(_) => 0,
                        other => panic!("{other:?}"),
                    }
                })
            })
            .collect();
        let started: i32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(started, 1);
        assert_eq!(s.list(&RunQuery::default()).unwrap().runs.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unfinished_runs_of_a_lost_process_become_unknown_never_succeeded() {
        let dir = temp_dir("recover");
        let s = store(&dir, StoreLimits::default());
        let mut lost = started(s.claim("backup", RunSource::Scheduled, None, Some("k1"), "scheduler", 1));
        lost.mark_running(Some(i32::MAX as u32), 2).unwrap();
        let lost_id = lost.run_id().to_string();
        // a crash: neither the result nor the destructor's record happens, the OS releases the lock
        lost.finished = true;
        drop(lost);
        let alive = started(s.claim("digest", RunSource::Manual, Some("req-alive-01"), None, "console", 3));
        let settled = s.recover(10).unwrap();
        assert_eq!(settled, vec![lost_id.clone()], "the run still holding its lock is left alone");
        let view = s.get(&lost_id).unwrap().unwrap();
        assert_eq!(view.state, RunState::Unknown);
        assert!(view.reason.unwrap().contains("ended before its result"));
        assert_eq!(s.get(alive.run_id()).unwrap().unwrap().state, RunState::Queued);
        assert!(s.recover(11).unwrap().is_empty(), "recovery is idempotent");
        // a late result for a settled run does not overwrite it
        let mut events = std::fs::read_to_string(dir.join("runs.jsonl")).unwrap();
        events.push_str(&format!("{{\"v\":1,\"runId\":\"{lost_id}\",\"state\":\"succeeded\",\"at\":20}}\n"));
        std::fs::write(dir.join("runs.jsonl"), events).unwrap();
        assert_eq!(s.get(&lost_id).unwrap().unwrap().state, RunState::Unknown);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dropped_handle_records_unknown() {
        let dir = temp_dir("drop");
        let s = store(&dir, StoreLimits::default());
        let run = started(s.claim("backup", RunSource::Manual, Some("req-dropped1"), None, "console", 1));
        let id = run.run_id().to_string();
        drop(run);
        assert_eq!(s.get(&id).unwrap().unwrap().state, RunState::Unknown);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn logs_are_redacted_and_bounded_with_head_and_tail() {
        let dir = temp_dir("logs");
        let limits = StoreLimits { max_log_bytes: 400, tail_lines: 3, max_line_chars: 40, ..StoreLimits::default() };
        let s = store(&dir, limits);
        let mut run = started(s.claim("backup", RunSource::Manual, Some("req-logs-001"), None, "console", 1));
        run.log(LogStream::Out, "token=supersecret123 at C:\\qqbot\\config.json and sess-token-0001");
        run.log(LogStream::Err, &"x".repeat(100));
        for i in 0..20 {
            run.log(LogStream::Out, &format!("line {i}"));
        }
        let id = run.run_id().to_string();
        run.finish(Finish::Succeeded, 2).unwrap();
        let page = s.read_log(&id, None, None).unwrap().unwrap();
        let text: Vec<&str> = page.lines.iter().map(|l| l.t.as_str()).collect();
        assert!(!text.join("\n").contains("supersecret123") && !text.join("\n").contains("sess-token-0001") && !text.join("\n").contains("qqbot"), "{text:?}");
        assert_eq!(page.lines[1].s, LogStream::Err);
        assert_eq!(page.lines[1].t.chars().count(), 40, "long lines are cut");
        assert_eq!(&text[text.len() - 3..], ["line 17", "line 18", "line 19"], "the tail is kept");
        let marker = text[text.len() - 4];
        let omitted: usize = marker.strip_prefix("… ").and_then(|m| m.strip_suffix(" lines omitted …")).unwrap().parse().unwrap();
        assert_eq!(text.len() - 4 + omitted + 3, 22, "every one of the 22 lines is written or counted: {text:?}");
        assert!(omitted > 0 && text.contains(&"line 0"), "the head is kept");
        let bytes = std::fs::metadata(dir.join("logs").join(format!("{id}.jsonl"))).unwrap().len();
        assert!(bytes < 400 + 4 * 100, "bounded: {bytes}");
        // paging
        let first = s.read_log(&id, None, Some(2)).unwrap().unwrap();
        assert_eq!(first.lines.len(), 2);
        let next = s.read_log(&id, first.next_after, Some(500)).unwrap().unwrap();
        assert_eq!(next.lines[0].n, 3);
        assert_eq!(next.next_after, None);
        // unknown or malformed run ids name no file
        assert_eq!(s.read_log("../runs", None, None).unwrap(), None);
        assert_eq!(s.read_log("backup-0-0-0", None, None).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn history_pages_newest_first_and_compacts_with_its_logs() {
        let dir = temp_dir("compact");
        let limits = StoreLimits { max_history_bytes: 4_000, keep_runs: 5, ..StoreLimits::default() };
        let s = store(&dir, limits);
        let mut ids = Vec::new();
        for i in 0..30 {
            let mut run = started(s.claim(if i % 2 == 0 { "backup" } else { "digest" }, RunSource::Manual, Some(&format!("req-page-{i:04}")), None, "console", i));
            run.log(LogStream::Out, "hello");
            ids.push(run.run_id().to_string());
            run.finish(Finish::Succeeded, i).unwrap();
        }
        assert!(std::fs::metadata(dir.join("runs.jsonl")).unwrap().len() <= 4_000 + 1_000);
        let all = s.list(&RunQuery { limit: Some(200), ..RunQuery::default() }).unwrap();
        assert!(all.runs.len() >= 5 && all.runs.len() < 30, "{}", all.runs.len());
        assert_eq!(all.runs[0].run_id, ids[29], "newest first");
        assert!(s.get(&ids[0]).unwrap().is_none());
        assert!(!dir.join("logs").join(format!("{}.jsonl", ids[0])).exists(), "logs of dropped runs are deleted");
        assert!(dir.join("logs").join(format!("{}.jsonl", ids[29])).exists());
        let page = s.list(&RunQuery { task: Some("digest".into()), limit: Some(2), before: None }).unwrap();
        assert_eq!(page.runs.iter().map(|r| r.run_id.clone()).collect::<Vec<_>>(), vec![ids[29].clone(), ids[27].clone()]);
        let next = s.list(&RunQuery { task: Some("digest".into()), limit: Some(2), before: page.next_before }).unwrap();
        assert_eq!(next.runs[0].run_id, ids[25]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_torn_history_line_is_skipped_and_not_glued() {
        let dir = temp_dir("torn");
        let s = store(&dir, StoreLimits::default());
        started(s.claim("backup", RunSource::Manual, Some("req-torn-001"), None, "console", 1)).finish(Finish::Succeeded, 2).unwrap();
        let mut file = OpenOptions::new().append(true).open(dir.join("runs.jsonl")).unwrap();
        file.write_all(b"{\"v\":1,\"runId\":\"x").unwrap();
        drop(file);
        started(s.claim("backup", RunSource::Manual, Some("req-torn-002"), None, "console", 3)).finish(Finish::Succeeded, 4).unwrap();
        assert_eq!(s.list(&RunQuery::default()).unwrap().runs.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
