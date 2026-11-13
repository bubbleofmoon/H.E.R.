//! Ticket 27: the single message-handling owner, process identity and the persistent desired state.
//!
//! Owner gate: one OS file lock per role (`rbridge-live.lock`, `rbridge-supervisor.lock`). The lock is the only proof of
//! ownership: a crashed owner releases it with its handle, a hung but living owner keeps it, and nothing here ever
//! deletes or replaces a lock file, so ownership is never taken by age or by unlinking. Who holds the lock is published in
//! a sidecar marker (`<lock>.owner.json`, written atomically by the holder while it holds the lock) because Windows
//! byte-range locks make the locked file itself unreadable to other processes. A marker is only a claim: before any
//! process is stopped its identity (pid, OS start stamp, executable) is re-read from the OS through a handle that pins the
//! process ([`kill_verified`]). An unreadable marker or process means "unknown", and unknown never stops or starts
//! anything.
//!
//! Desired state: `desired-state.json` (`running` | `stopped` | `maintenance`, who, why, when, optional expiry) is written
//! first by a stop, maintenance or rollback and read by every automatic starter (watchdog, scheduler, login start). A
//! missing file means `running` (fresh install); an unreadable one means unknown, which holds everything as it is.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FORMAT_VERSION: u8 = 1;
pub const LIVE_LOCK: &str = "rbridge-live.lock";
pub const SUPERVISOR_LOCK: &str = "rbridge-supervisor.lock";
pub const DESIRED_FILE: &str = "desired-state.json";
const DESIRED_GATE: &str = "lifecycle.lock";
const MAX_BY_CHARS: usize = 64;
const MAX_REASON_CHARS: usize = 300;
/// How long a writer of the desired state waits for its gate.
pub const DESIRED_GATE_WAIT: Duration = Duration::from_secs(5);

// ---- process identity ------------------------------------------------------------------------

// Compatibility exports keep owner markers and existing callers on the shared identity module.
#[allow(unused_imports)]
pub use crate::process_identity::{ProcessIdentity, Liveness, KillOutcome, identity_of, current_identity, same_identity, check_identity, kill_verified};

/// Hold one host-wide gate for every selected conversation of this gateway.
/// The gates persist for the live owner's lifetime and are released by the OS on crash.
pub fn acquire_message_source(endpoint: &str, groups: &[String]) -> Result<SourceGuard, String> {
    use sha2::{Digest, Sha256};
    let authority = endpoint.strip_prefix("ws://").ok_or("message source is not a WebSocket endpoint")?;
    let address: std::net::SocketAddr = authority.parse().map_err(|_| "message source endpoint is invalid")?;
    if !address.ip().is_loopback() || address.port() == 0 || groups.is_empty() {
        return Err("message source endpoint or scope is invalid".into());
    }
    let mut keys: Vec<String> = groups.iter().map(|group| {
        let value = format!("v1\0{address}\0{group}");
        format!("{:x}", Sha256::digest(value.as_bytes()))
    }).collect();
    keys.sort();
    keys.dedup();
    let source = source_transition(&address, true)?;
    let conversations = source_gate::acquire(&keys)?;
    Ok(SourceGuard { _source: source, _conversations: conversations })
}

pub struct SourceGuard { _source: File, _conversations: ConversationGuard }

fn source_transition(address: &std::net::SocketAddr, shared: bool) -> Result<File, String> {
    use sha2::{Digest, Sha256};
    #[cfg(unix)]
    let directory = PathBuf::from("/tmp/qqbot-source-transitions-v1");
    #[cfg(not(unix))]
    let directory = std::env::temp_dir().join("qqbot-source-transitions-v1");
    crate::install_root::check_development_path(&directory)?;
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
    match builder.create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("message source transition directory is unavailable".into()),
    }
    let meta = std::fs::symlink_metadata(&directory).map_err(|_| "message source transition directory is unavailable")?;
    if !meta.is_dir() || crate::guarded_file::is_link(&meta) { return Err("message source transition directory is redirected".into()); }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err("message source transition directory is unsafe".into());
        }
    }
    let key = format!("{:x}", Sha256::digest(address.to_string().as_bytes()));
    use crate::guarded_file::{open_gate, Gate, GateError};
    let file = open_gate(&directory.join(format!("{key}.lock")), Gate::Private).map_err(|error| match error {
        GateError::Open(_) => "message source transition gate is unavailable",
        GateError::Redirected => "message source transition gate is redirected",
        GateError::Unsafe => "message source transition gate is unsafe",
    })?;
    let result = if shared { file.try_lock_shared() } else { file.try_lock() };
    result.map_err(|_| "message source is owned by a live runtime or a rollback is in progress")?;
    Ok(file)
}

#[cfg(unix)]
pub struct ConversationGuard { _files: Vec<File> }

#[cfg(unix)]
mod source_gate {
    use super::{File, ConversationGuard};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::Path;

    pub(super) fn acquire(keys: &[String]) -> Result<ConversationGuard, String> {
        let dir = Path::new("/tmp/qqbot-source-gates-v1");
        crate::install_root::check_development_path(dir)?;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("message source gate directory unavailable: {}", error.kind())),
        }
        let metadata = std::fs::symlink_metadata(dir).map_err(|_| "message source gate directory unavailable")?;
        if !metadata.is_dir() || crate::guarded_file::is_link(&metadata) || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0 {
            return Err("message source gate directory is unsafe".into());
        }
        let mut files = Vec::<File>::new();
        for key in keys {
            let file = crate::guarded_file::open_gate(&dir.join(format!("{key}.lock")), crate::guarded_file::Gate::Private)
                .map_err(|error| match error {
                    crate::guarded_file::GateError::Open(error) => format!("message source gate unavailable: {}", error.kind()),
                    _ => "message source gate is unsafe".to_string(),
                })?;
            match file.try_lock() {
                Ok(()) => files.push(file),
                Err(std::fs::TryLockError::WouldBlock) => return Err("another live process owns this message source and conversation".into()),
                Err(std::fs::TryLockError::Error(error)) => return Err(format!("message source gate cannot be locked: {}", error.kind())),
            }
        }
        Ok(ConversationGuard { _files: files })
    }
}

#[cfg(windows)]
pub struct ConversationGuard {
    release: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl Drop for ConversationGuard {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

#[cfg(windows)]
mod source_gate {
    use super::ConversationGuard;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

    pub(super) fn acquire(keys: &[String]) -> Result<ConversationGuard, String> {
        let names: Vec<String> = keys.iter().map(|key| format!("Global\\QQBot-Source-v1-{key}")).collect();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut handles = Vec::new();
            let mut error = None;
            for name in names {
                let wide: Vec<u16> = std::ffi::OsStr::new(&name).encode_wide().chain(Some(0)).collect();
                let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
                if handle.is_null() { error = Some("message source mutex is unavailable"); break; }
                let result = unsafe { WaitForSingleObject(handle, 0) };
                if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
                    unsafe { CloseHandle(handle); }
                    error = Some("another live process owns this message source and conversation"); break;
                }
                handles.push(handle);
            }
            let _ = ready_tx.send(error.map_or(Ok(()), |message| Err(message.to_string())));
            if error.is_none() { let _ = release_rx.recv(); }
            for handle in handles.into_iter().rev() { unsafe { ReleaseMutex(handle); CloseHandle(handle); } }
        });
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(ConversationGuard { release, worker: Some(worker) }),
            Ok(Err(error)) => { let _ = worker.join(); Err(error) }
            Err(_) => Err("message source mutex acquisition timed out".into()),
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub struct ConversationGuard;

#[cfg(not(any(unix, windows)))]
mod source_gate {
    use super::ConversationGuard;
    pub(super) fn acquire(_: &[String]) -> Result<ConversationGuard, String> { Err("message source gates are unavailable".into()) }
}

// ---- owner gate --------------------------------------------------------------------------------

/// Published by the holder of an owner lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerMarker {
    pub v: u8,
    pub role: String,
    pub identity: ProcessIdentity,
    pub acquired_at: i64,
    pub refreshed_at: i64,
    /// Conversations a live owner serves (`--live-groups`), when it has a fixed list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
}

/// What a non-owner can learn about an owner lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerProbe {
    /// Nobody holds the lock. `last` is the marker of the previous holder, if readable.
    Free { last: Option<OwnerMarker> },
    /// Another process holds the lock. `marker` is `None` when the sidecar is missing or unreadable.
    Held { marker: Option<OwnerMarker> },
    /// The lock file could not be opened or locked for another reason.
    Unknown(String),
}

pub enum Acquire {
    Acquired(OwnerGuard),
    Held { marker: Option<OwnerMarker> },
    Unknown(String),
}

impl std::fmt::Debug for Acquire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Acquire::Acquired(guard) => write!(f, "Acquired({:?})", guard.marker),
            Acquire::Held { marker } => write!(f, "Held({marker:?})"),
            Acquire::Unknown(error) => write!(f, "Unknown({error})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OwnerGate {
    lock_path: PathBuf,
    marker_path: PathBuf,
}

/// Holding this keeps the OS lock. Dropping it releases the lock; the lock file and marker are left in place.
#[derive(Debug)]
pub struct OwnerGuard {
    file: File,
    marker: OwnerMarker,
    marker_path: PathBuf,
}

/// A stopped runtime with desired state and every Rust owner gate held for a maintenance operation.
pub struct QuiescentGuard {
    _desired: File,
    _owners: Vec<OwnerGuard>,
}

pub(crate) fn validate_owner_gate_paths(data: &Path) -> Result<(), String> {
    validate_owner_gate_paths_with(data, true)
}

/// Validate existing gate paths without preparing a runtime for startup or control.
pub(crate) fn validate_existing_owner_gate_paths(data: &Path) -> Result<(), String> {
    validate_owner_gate_paths_with(data, false)
}

fn validate_owner_gate_paths_with(data: &Path, create: bool) -> Result<(), String> {
    // These commands work even with a broken config, so validate the small set of gate paths
    // directly instead of constructing a RuntimeContext that could reject the operator's stop.
    for name in ["lifecycle", "logs"] {
        let dir = data.join(name);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() && !crate::guarded_file::is_link(&meta) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                std::fs::create_dir(&dir).map_err(|e| format!("gate directory is unavailable: {}", e.kind()))?;
            }
            _ => return Err("gate directory is redirected or unavailable".into()),
        }
        let base = std::fs::canonicalize(data).map_err(|_| "runtime data directory is unavailable")?;
        if std::fs::canonicalize(&dir).ok().as_deref().and_then(Path::parent) != Some(base.as_path()) {
            return Err("gate directory is redirected".into());
        }
    }
    for path in [data.join("lifecycle/lifecycle.lock"), data.join("logs/rbridge-live.lock"),
        data.join("logs/rbridge-console.lock"), data.join("logs/rbridge-supervisor.lock")] {
        let mut marker = path.as_os_str().to_os_string();
        marker.push(".owner.json");
        for candidate in [path.clone(), PathBuf::from(marker)] {
            match std::fs::symlink_metadata(&candidate) {
                Ok(meta) if meta.is_file() && !crate::guarded_file::is_link(&meta) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err("gate file is redirected or unavailable".into()),
            }
        }
    }
    Ok(())
}

pub fn acquire_quiescent(data: &Path) -> Result<QuiescentGuard, String> {
    validate_owner_gate_paths(data)?;
    let gate = lock_with_wait(&data.join("lifecycle/lifecycle.lock"), Duration::from_secs(1))?;
    if !matches!(DesiredStore::new(data.join("lifecycle")).read(), DesiredRead::Present(record) if record.state == DesiredState::Stopped) {
        return Err("maintenance requires an explicit stopped state".into());
    }
    let identity = current_identity()?;
    let mut owners = Vec::new();
    for name in [LIVE_LOCK, "rbridge-console.lock", SUPERVISOR_LOCK] {
        match OwnerGate::new(data.join("logs").join(name)).acquire(
            "maintenance-guard", identity.clone(), None, crate::parity_clock::now_ms(), Duration::ZERO,
        ) {
            Acquire::Acquired(owner) => owners.push(owner),
            _ => return Err(format!("owner is held or cannot be established: {name}")),
        }
    }
    Ok(QuiescentGuard { _desired: gate, _owners: owners })
}

/// Hold each ordinary task gate after the desired stop has made its runner cancel.
/// Callers keep these handles while retiring owners, preventing a queued task from
/// crossing the stop boundary before the owning process exits.
pub(crate) fn hold_stopped_task_gates(data: &Path) -> Result<Vec<File>, String> {
    let tasks = data.join("tasks");
    let locks = tasks.join("locks");
    for dir in [&tasks, &locks] {
        match std::fs::symlink_metadata(dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound && dir == &tasks => return Ok(Vec::new()),
            Ok(meta) if meta.is_dir() && !crate::guarded_file::is_link(&meta) => {}
            _ => return Err("task gate directory is redirected or unavailable".into()),
        }
    }
    let store = crate::task_runs::TaskStore::open(&tasks, crate::task_runs::StoreLimits::default(), crate::task_runs::credential_redactor(Vec::new()))
        .map_err(|error| format!("task history cannot be inspected: {}", error.kind()))?;
    #[cfg(target_os = "linux")]
    let locks_handle = {
        let tasks_handle = pin_directory(&tasks).map_err(|_| "task gate directory is unavailable")?;
        pin_directory(&pinned_directory_path(&tasks_handle).join("locks"))
            .map_err(|_| "task gate directory is unavailable")?
    };
    #[cfg(target_os = "linux")]
    let locks_path = pinned_directory_path(&locks_handle);
    #[cfg(not(target_os = "linux"))]
    let locks_path = locks;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut held = Vec::new();
    for task in crate::tasks::REGISTRY.iter().filter(|task| !task.runs_when_held) {
        let path = locks_path.join(format!("{}.lock", task.id.as_str()));
        loop {
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Ok(meta) if meta.is_file() && !crate::guarded_file::is_link(&meta) => {}
                _ => return Err("task gate is redirected or unavailable".into()),
            }
            let file = crate::guarded_file::open_gate(&path, crate::guarded_file::Gate::Existing)
                .map_err(|error| match error {
                    crate::guarded_file::GateError::Open(error) => format!("task gate cannot be opened: {}", error.kind()),
                    _ => "task gate is redirected or unavailable".to_string(),
                })?;
            match file.try_lock() {
                Ok(()) => { held.push(file); break; }
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Err(std::fs::TryLockError::WouldBlock) => return Err("running tasks did not stop before the deadline".into()),
                Err(std::fs::TryLockError::Error(error)) => return Err(format!("task gate cannot be taken: {}", error.kind())),
            }
        }
    }
    if store.has_surviving_groups()? {
        return Err("recorded task process groups are still running; stop is unresolved".into());
    }
    Ok(held)
}

impl OwnerGate {
    pub fn new(lock_path: impl Into<PathBuf>) -> Self {
        let lock_path = lock_path.into();
        let mut marker = lock_path.clone().into_os_string();
        marker.push(".owner.json");
        Self { lock_path, marker_path: PathBuf::from(marker) }
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn marker_path(&self) -> &Path {
        &self.marker_path
    }

    /// Try to become the owner, retrying for up to `wait` while the lock is held (a status probe holds it for a
    /// moment; a real owner holds it for its whole life). A held lock is never broken, whatever its marker says.
    pub fn acquire(
        &self,
        role: &str,
        identity: ProcessIdentity,
        groups: Option<Vec<String>>,
        now_ms: i64,
        wait: Duration,
    ) -> Acquire {
        let deadline = Instant::now() + wait;
        loop {
            let file = match open_lock_file(&self.lock_path) {
                Ok(file) => file,
                Err(error) => return Acquire::Unknown(format!("owner lock cannot be opened: {}", error.kind())),
            };
            match file.try_lock() {
                Ok(()) => {
                    let marker = OwnerMarker {
                        v: FORMAT_VERSION,
                        role: role.to_string(),
                        identity,
                        acquired_at: now_ms,
                        refreshed_at: now_ms,
                        groups,
                    };
                    // An owner that cannot say who it is would be unverifiable for everybody else: give the lock back.
                    if let Err(error) = write_marker(&self.marker_path, &marker) {
                        drop(file);
                        return Acquire::Unknown(format!("owner marker cannot be written: {error}"));
                    }
                    return Acquire::Acquired(OwnerGuard { file, marker, marker_path: self.marker_path.clone() });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Acquire::Held { marker: read_marker(&self.marker_path) };
                    }
                    drop(file);
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Acquire::Unknown(format!("owner lock cannot be taken: {}", error.kind()))
                }
            }
        }
    }

    /// Who owns the lock. Takes the lock for an instant when it is free, so an owner starting at that very moment
    /// retries within its acquire wait instead of failing.
    pub fn probe(&self) -> OwnerProbe {
        self.probe_with(true)
    }

    /// Probe an existing gate only. Missing directories/files stay unavailable and are never created.
    pub fn probe_existing(&self) -> OwnerProbe {
        self.probe_with(false)
    }

    fn probe_with(&self, create: bool) -> OwnerProbe {
        let file = match open_lock_file_with(&self.lock_path, create) {
            Ok(file) => file,
            Err(error) => return OwnerProbe::Unknown(format!("owner lock cannot be opened: {}", error.kind())),
        };
        match file.try_lock() {
            Ok(()) => {
                drop(file);
                OwnerProbe::Free { last: read_marker(&self.marker_path) }
            }
            Err(std::fs::TryLockError::WouldBlock) => OwnerProbe::Held { marker: read_marker(&self.marker_path) },
            Err(std::fs::TryLockError::Error(error)) => {
                OwnerProbe::Unknown(format!("owner lock cannot be probed: {}", error.kind()))
            }
        }
    }
}

impl OwnerGuard {
    pub fn marker(&self) -> &OwnerMarker {
        &self.marker
    }

    /// Republish the marker with a new `refreshedAt` (diagnostics only: age never decides ownership).
    pub fn refresh(&mut self, now_ms: i64) -> io::Result<()> {
        self.marker.refreshed_at = now_ms;
        write_marker(&self.marker_path, &self.marker)
    }

    /// The lock file handle (kept open for the guard's life).
    pub fn file(&self) -> &File {
        &self.file
    }
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    open_lock_file_with(path, true)
}

fn open_lock_file_with(path: &Path, create: bool) -> io::Result<File> {
    if create {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Never truncate: the file may be the legacy lock that an older binary also reads.
    let gate = if create { crate::guarded_file::Gate::Create } else { crate::guarded_file::Gate::Existing };
    Ok(crate::guarded_file::open_gate(path, gate)?)
}

/// Pin a Linux directory before opening any file underneath it. A later rename
/// or symlink replacement of its pathname cannot redirect the pinned handle.
#[cfg(target_os = "linux")]
pub(crate) fn pin_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)
}

#[cfg(target_os = "linux")]
pub(crate) fn pinned_directory_path(directory: &File) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn read_marker(path: &Path) -> Option<OwnerMarker> {
    let bytes = std::fs::read(path).ok()?;
    let marker: OwnerMarker = serde_json::from_slice(&bytes).ok()?;
    (marker.v == FORMAT_VERSION).then_some(marker)
}

fn write_marker(path: &Path, marker: &OwnerMarker) -> io::Result<()> {
    let bytes = serde_json::to_vec(marker).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    crate::guarded_file::write_atomic(path, &bytes).map_err(io::Error::from)
}

// ---- desired state ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesiredState {
    Running,
    Stopped,
    Maintenance,
}

impl DesiredState {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "running" => Some(Self::Running),
            "stopped" => Some(Self::Stopped),
            "maintenance" => Some(Self::Maintenance),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Maintenance => "maintenance",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredRecord {
    pub v: u8,
    pub state: DesiredState,
    pub by: String,
    pub reason: String,
    pub at: i64,
    /// Maintenance only: when it ends by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredRead {
    Absent,
    Present(DesiredRecord),
    Unreadable(String),
}

/// What automatic starters may keep running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Wants {
    /// A live message-handling owner.
    pub live: bool,
    /// The console (web page, MCP tool routes).
    pub console: bool,
    /// Scheduled background jobs.
    pub jobs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effective {
    Act { state: DesiredState, wants: Wants },
    /// The desired state cannot be read: start nothing, stop nothing.
    Unknown(String),
}

/// The effective desired state at `now`: a missing record is `running`, an expired maintenance window is `running`.
pub fn effective(read: &DesiredRead, now_ms: i64) -> Effective {
    let state = match read {
        DesiredRead::Unreadable(reason) => return Effective::Unknown(reason.clone()),
        DesiredRead::Absent => DesiredState::Running,
        DesiredRead::Present(record) => match (record.state, record.until) {
            (DesiredState::Maintenance, Some(until)) if until <= now_ms => DesiredState::Running,
            (state, _) => state,
        },
    };
    let wants = match state {
        DesiredState::Running => Wants { live: true, console: true, jobs: true },
        DesiredState::Maintenance => Wants { live: false, console: true, jobs: false },
        DesiredState::Stopped => Wants { live: false, console: false, jobs: false },
    };
    Effective::Act { state, wants }
}

#[derive(Debug, Clone)]
pub struct DesiredStore {
    dir: PathBuf,
}

impl DesiredStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(DESIRED_FILE)
    }

    pub fn read(&self) -> DesiredRead {
        let bytes = match std::fs::read(self.path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return DesiredRead::Absent,
            Err(error) => return DesiredRead::Unreadable(format!("desired state is unreadable: {}", error.kind())),
        };
        match serde_json::from_slice::<DesiredRecord>(&bytes) {
            Ok(record) if record.v == FORMAT_VERSION => DesiredRead::Present(record),
            Ok(record) => DesiredRead::Unreadable(format!("desired state has unsupported version {}", record.v)),
            Err(_) => DesiredRead::Unreadable("desired state is not a valid record".to_string()),
        }
    }

    /// Record a new desired state under the lifecycle gate. An unreadable previous record is kept as
    /// `desired-state.json.corrupt-<ms>` and replaced: an explicit administrator decision resolves "unknown".
    pub fn write(
        &self,
        state: DesiredState,
        by: &str,
        reason: &str,
        until: Option<i64>,
        now_ms: i64,
    ) -> Result<DesiredRecord, String> {
        self.write_if_revision(state, by, reason, until, now_ms, None)
    }

    /// Optionally require the current revision under the same gate as the mutation.
    pub fn write_if_revision(
        &self,
        state: DesiredState,
        by: &str,
        reason: &str,
        until: Option<i64>,
        now_ms: i64,
        expected_revision: Option<u64>,
    ) -> Result<DesiredRecord, String> {
        let by = clean_text(by, MAX_BY_CHARS);
        if by.is_empty() {
            return Err("who changes the desired state is required".to_string());
        }
        let reason = clean_text(&crate::run_record::sanitize_reason(reason, &|text| {
            crate::pipeline::redact_sensitive_text(text, &[])
        }), MAX_REASON_CHARS);
        let until = match (state, until) {
            (DesiredState::Maintenance, Some(until)) if until <= now_ms => {
                return Err("maintenance must end in the future".to_string())
            }
            (DesiredState::Maintenance, until) => until,
            (_, Some(_)) => return Err("only maintenance can have an end time".to_string()),
            (_, None) => None,
        };
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("lifecycle directory unavailable: {}", e.kind()))?;
        let _gate = lock_with_wait(&self.dir.join(DESIRED_GATE), DESIRED_GATE_WAIT)?;
        let mut audit = OpenOptions::new().read(true).append(true).create(true).open(self.dir.join("audit.jsonl"))
            .map_err(|e| format!("lifecycle audit unavailable: {}", e.kind()))?;
        let previous_read = self.read();
        if let Some(expected) = expected_revision {
            let current = match &previous_read {
                DesiredRead::Present(record) => Some(record.revision),
                DesiredRead::Absent => Some(0),
                DesiredRead::Unreadable(_) => None,
            };
            if current != Some(expected) { return Err("desired state revision changed".into()); }
        }
        let previous = match previous_read {
            DesiredRead::Present(record) => record.revision,
            DesiredRead::Absent => 0,
            DesiredRead::Unreadable(_) => {
                let backup = self.dir.join(format!("{DESIRED_FILE}.corrupt-{now_ms}"));
                std::fs::rename(self.path(), &backup)
                    .map_err(|e| format!("unreadable desired state could not be set aside: {}", e.kind()))?;
                0
            }
        };
        let record = DesiredRecord { v: FORMAT_VERSION, state, by, reason, at: now_ms, until, revision: previous + 1 };
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
        let append_audit = |file: &mut File, result: &str| -> Result<(), String> {
            let mut line = serde_json::to_vec(&serde_json::json!({
                "v": FORMAT_VERSION, "state": record.state, "by": record.by, "reason": record.reason,
                "at": record.at, "revision": record.revision, "result": result,
            })).map_err(|e| e.to_string())?;
            line.push(b'\n');
            if crate::runtime::ends_mid_line(file).map_err(|e| format!("lifecycle audit unreadable: {}", e.kind()))? {
                line.insert(0, b'\n');
            }
            file.write_all(&line).and_then(|_| file.sync_data())
                .map_err(|e| format!("lifecycle audit cannot be written: {}", e.kind()))
        };
        append_audit(&mut audit, "intent")?;
        crate::guarded_file::write_atomic(&self.path(), &bytes).map_err(|e| format!("desired state cannot be written: {}", io::Error::from(e).kind()))?;
        append_audit(&mut audit, "applied").map_err(|error| format!("desired state changed but {error}"))?;
        Ok(record)
    }
}

/// Control characters become spaces, runs collapse, the result is trimmed and cut.
fn clean_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in text.chars() {
        if c.is_control() || c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    out.chars().take(max_chars).collect()
}

/// Take an exclusive OS lock on `path`, waiting up to `wait`. The returned file holds it.
pub(crate) fn lock_with_wait(path: &Path, wait: Duration) -> Result<File, String> {
    let deadline = Instant::now() + wait;
    loop {
        let file = open_lock_file(path).map_err(|e| format!("gate cannot be opened: {}", e.kind()))?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                drop(file);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => return Err("gate is busy".to_string()),
            Err(std::fs::TryLockError::Error(error)) => return Err(format!("gate cannot be taken: {}", error.kind())),
        }
    }
}

/// Read a whole file into a string, for callers that hold the path.
pub(crate) fn read_text(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!("her-lifecycle-{name}-{}-{}", std::process::id(), crate::parity_clock::now_ms())));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn me() -> ProcessIdentity {
        current_identity().expect("own identity")
    }

    #[cfg(windows)]
    #[test]
    fn source_transition_refuses_redirected_temp_before_allocating() {
        if std::env::var_os("HER_04_SOURCE_CHILD").is_some() {
            let result = source_transition(&"127.0.0.1:43151".parse().unwrap(), true);
            assert!(result.err().is_some_and(|e| e.contains("development runtime root is under the production install root")));
            return;
        }
        let base = temp_dir("source-temp-guard");
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
        let output = cmd.args(["--exact", "lifecycle::tests::source_transition_refuses_redirected_temp_before_allocating", "--nocapture"])
            .env("HER_04_SOURCE_CHILD", "1").env("HER_INSTALL_ROOT", &base)
            .env("TEMP", &base).env("TMP", &base).output().unwrap();
        let wrote = base.join("qqbot-source-transitions-v1").exists();
        std::fs::remove_dir_all(base).unwrap();
        assert!(output.status.success() && !wrote, "{output:?}; wrote={wrote}");
    }

    // ---- contender helper: a real second process that competes for an owner lock ----

    const CONTENDER_LOCK: &str = "HER_LIFECYCLE_CONTENDER_LOCK";
    const CONTENDER_START_AT: &str = "HER_LIFECYCLE_CONTENDER_START_AT";
    const CONTENDER_HOLD_MS: &str = "HER_LIFECYCLE_CONTENDER_HOLD_MS";
    const CONTENDER_ABORT: &str = "HER_LIFECYCLE_CONTENDER_ABORT";

    /// Not a test: the body of a contender process started by the tests below.
    #[test]
    #[ignore]
    fn owner_contender_process() {
        let Ok(lock) = std::env::var(CONTENDER_LOCK) else { return };
        let start_at: i64 = std::env::var(CONTENDER_START_AT).unwrap().parse().unwrap();
        let hold_ms: u64 = std::env::var(CONTENDER_HOLD_MS).unwrap().parse().unwrap();
        while crate::parity_clock::now_ms() < start_at {
            std::thread::sleep(Duration::from_millis(1));
        }
        let gate = OwnerGate::new(lock);
        match gate.acquire("live", me(), Some(vec!["group:1".into()]), crate::parity_clock::now_ms(), Duration::from_millis(300)) {
            Acquire::Acquired(guard) => {
                println!("CONTENDER ACQUIRED {}", std::process::id());
                use std::io::Write as _;
                std::io::stdout().flush().unwrap();
                if std::env::var(CONTENDER_ABORT).is_ok() {
                    std::process::abort(); // a crash: no destructor runs, the OS releases the lock
                }
                std::thread::sleep(Duration::from_millis(hold_ms));
                drop(guard);
            }
            Acquire::Held { .. } => println!("CONTENDER HELD {}", std::process::id()),
            Acquire::Unknown(error) => println!("CONTENDER UNKNOWN {error}"),
        }
    }

    fn contender(lock: &Path, start_at: i64, hold_ms: u64, abort: bool) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
        command
            .args(["--ignored", "--exact", "lifecycle::tests::owner_contender_process", "--nocapture", "--test-threads=1"])
            .env(CONTENDER_LOCK, lock)
            .env(CONTENDER_START_AT, start_at.to_string())
            .env(CONTENDER_HOLD_MS, hold_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if abort {
            command.env(CONTENDER_ABORT, "1");
        }
        command
    }

    fn wait_for_line(child: &mut std::process::Child, prefix: &str) -> String {
        use std::io::BufRead as _;
        let stdout = child.stdout.as_mut().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                panic!("contender ended without {prefix}");
            }
            // The harness prints `test <name> ... ` on the same line before the helper's output.
            if let Some(at) = line.find(prefix) {
                return line[at..].trim().to_string();
            }
        }
    }

    #[test]
    fn simultaneous_starts_admit_exactly_one_owner() {
        let dir = temp_dir("simultaneous");
        let lock = dir.join(LIVE_LOCK);
        let start_at = crate::parity_clock::now_ms() + 1500;
        let children: Vec<_> = (0..8).map(|_| contender(&lock, start_at, 2500, false).spawn().unwrap()).collect();
        let mut acquired = Vec::new();
        let mut held = 0;
        for child in children {
            let output = child.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                let line = line.find("CONTENDER ").map_or(line, |at| &line[at..]);
                if let Some(pid) = line.strip_prefix("CONTENDER ACQUIRED ") {
                    acquired.push(pid.trim().parse::<u32>().unwrap());
                } else if line.starts_with("CONTENDER HELD ") {
                    held += 1;
                } else if line.starts_with("CONTENDER UNKNOWN") {
                    panic!("contender could not decide: {line}");
                }
            }
        }
        assert_eq!(acquired.len(), 1, "exactly one of eight simultaneous starts may own the lock: {acquired:?}");
        assert_eq!(held, 7);
        match OwnerGate::new(&lock).probe() {
            OwnerProbe::Free { last: Some(marker) } => assert_eq!(marker.identity.pid, acquired[0]),
            other => panic!("after the owner exited the lock must be free with its marker: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_crashed_owner_is_recovered_without_deleting_the_lock() {
        let dir = temp_dir("crash");
        let lock = dir.join(LIVE_LOCK);
        let mut child = contender(&lock, 0, 0, true).spawn().unwrap();
        let line = wait_for_line(&mut child, "CONTENDER ACQUIRED");
        let crashed_pid: u32 = line.rsplit(' ').next().unwrap().parse().unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "the contender aborted");
        let inode_before = std::fs::metadata(&lock).unwrap();
        match OwnerGate::new(&lock).acquire("live", me(), None, 5, Duration::ZERO) {
            Acquire::Acquired(guard) => {
                assert_eq!(guard.marker().identity, me());
                assert_eq!(guard.marker().acquired_at, 5);
            }
            other => panic!("a crashed owner's lock must be free: {other:?}"),
        }
        assert!(lock.exists(), "the lock file is never deleted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(std::fs::metadata(&lock).unwrap().ino(), inode_before.ino(), "the lock file is never replaced");
        }
        let _ = (inode_before, crashed_pid);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_living_owner_is_never_displaced_however_old_its_marker() {
        let dir = temp_dir("hung");
        let lock = dir.join(LIVE_LOCK);
        let mut child = contender(&lock, 0, 4000, false).spawn().unwrap();
        wait_for_line(&mut child, "CONTENDER ACQUIRED");
        let gate = OwnerGate::new(&lock);
        // Pretend the owner stopped refreshing a day ago (a hung process).
        let mut marker = read_marker(gate.marker_path()).expect("owner published its marker");
        marker.refreshed_at = 0;
        marker.acquired_at = 0;
        write_marker(gate.marker_path(), &marker).unwrap();
        match gate.acquire("live", me(), None, i64::MAX / 2, Duration::from_millis(200)) {
            Acquire::Held { marker: Some(found) } => assert_eq!(found.identity.pid, child.id()),
            other => panic!("a held lock must never be taken over: {other:?}"),
        }
        assert!(matches!(gate.probe(), OwnerProbe::Held { marker: Some(_) }));
        // An unreadable marker still means "held": identity unknown, ownership intact.
        std::fs::write(gate.marker_path(), b"{torn").unwrap();
        assert_eq!(gate.probe(), OwnerProbe::Held { marker: None });
        assert!(matches!(gate.acquire("live", me(), None, 1, Duration::ZERO), Acquire::Held { marker: None }));
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unopenable_lock_is_unknown_not_free() {
        let dir = temp_dir("unopenable");
        let lock = dir.join("is-a-directory");
        std::fs::create_dir_all(&lock).unwrap();
        let gate = OwnerGate::new(&lock);
        assert!(matches!(gate.probe(), OwnerProbe::Unknown(_)));
        assert!(matches!(gate.acquire("live", me(), None, 1, Duration::ZERO), Acquire::Unknown(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refresh_republishes_the_marker_while_holding() {
        let dir = temp_dir("refresh");
        let gate = OwnerGate::new(dir.join(SUPERVISOR_LOCK));
        let Acquire::Acquired(mut guard) = gate.acquire("supervisor", me(), None, 10, Duration::ZERO) else {
            panic!("free lock")
        };
        guard.refresh(99).unwrap();
        // probe from this process: a second handle conflicts with our own lock, as another process's would
        match gate.probe() {
            OwnerProbe::Held { marker: Some(marker) } => {
                assert_eq!((marker.acquired_at, marker.refreshed_at, marker.role.as_str()), (10, 99, "supervisor"))
            }
            other => panic!("{other:?}"),
        }
        drop(guard);
        assert!(matches!(gate.probe(), OwnerProbe::Free { last: Some(_) }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_and_verified_kill_follow_the_os() {
        let own = me();
        assert_eq!(own.pid, std::process::id());
        assert_eq!(check_identity(&own), Liveness::Matches);

        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let identity = identity_of(child.id()).unwrap().expect("sleep is running");
        assert!(identity.exe.ends_with("sleep"), "{identity:?}");

        // Same pid, other start stamp: a reused pid. Nothing is stopped.
        let reused = ProcessIdentity { start: identity.start + 1, ..identity.clone() };
        assert!(matches!(check_identity(&reused), Liveness::Mismatch(_)));
        assert!(matches!(kill_verified(&reused), KillOutcome::Refused(_)));
        let other_exe = ProcessIdentity { exe: "/usr/bin/not-sleep".into(), ..identity.clone() };
        assert!(matches!(kill_verified(&other_exe), KillOutcome::Refused(_)));
        assert!(child.try_wait().unwrap().is_none(), "a refused kill leaves the process running");

        assert_eq!(kill_verified(&identity), KillOutcome::Killed);
        child.wait().unwrap();
        assert_eq!(check_identity(&identity), Liveness::Gone);
        assert_eq!(kill_verified(&identity), KillOutcome::Gone);
    }

    #[cfg(windows)]
    #[test]
    fn windows_pinned_handle_refuses_mismatches_and_stops_the_verified_child() {
        use std::os::windows::process::CommandExt;
        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let dir = temp_dir("windows-pinned-kill");
        let mut command = contender(&dir.join(LIVE_LOCK), 0, 30_000, false);
        command.creation_flags(0x08000000);
        let mut child = Reap(command.spawn().unwrap());
        wait_for_line(&mut child.0, "CONTENDER ACQUIRED");
        let identity = identity_of(child.0.id()).unwrap().expect("fixture child is alive");
        let wrong_start = ProcessIdentity { start: identity.start + 1, ..identity.clone() };
        assert!(matches!(kill_verified(&wrong_start), KillOutcome::Refused(_)));
        let wrong_image = ProcessIdentity { exe: "C:\\fixture-not-this-process.exe".into(), ..identity.clone() };
        assert!(matches!(kill_verified(&wrong_image), KillOutcome::Refused(_)));
        assert!(child.0.try_wait().unwrap().is_none());
        assert_eq!(kill_verified(&identity), KillOutcome::Killed);
        child.0.wait().unwrap();
        assert_eq!(check_identity(&identity), Liveness::Gone);
        assert_eq!(kill_verified(&identity), KillOutcome::Gone);
        drop(child);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_desired_state_means_running_and_unreadable_means_unknown() {
        let dir = temp_dir("desired-read");
        let store = DesiredStore::new(&dir);
        assert_eq!(store.read(), DesiredRead::Absent);
        assert_eq!(
            effective(&store.read(), 0),
            Effective::Act { state: DesiredState::Running, wants: Wants { live: true, console: true, jobs: true } }
        );
        std::fs::write(store.path(), b"{\"state\":").unwrap();
        assert!(matches!(store.read(), DesiredRead::Unreadable(_)));
        assert!(matches!(effective(&store.read(), 0), Effective::Unknown(_)));
        std::fs::write(store.path(), br#"{"v":9,"state":"running","by":"x","reason":"","at":1,"revision":1}"#).unwrap();
        assert!(matches!(store.read(), DesiredRead::Unreadable(_)), "a future format is not guessed at");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_and_maintenance_are_persistent_and_maintenance_expires() {
        let dir = temp_dir("desired-write");
        let store = DesiredStore::new(&dir);
        let stopped = store.write(DesiredState::Stopped, "console", "rollback to JS\nnow", None, 1_000).unwrap();
        assert_eq!((stopped.revision, stopped.reason.as_str()), (1, "rollback to JS now"));
        // A fresh reader (another process, a restart) sees the same decision.
        let again = DesiredStore::new(&dir);
        assert_eq!(again.read(), DesiredRead::Present(stopped.clone()));
        assert_eq!(
            effective(&again.read(), i64::MAX),
            Effective::Act { state: DesiredState::Stopped, wants: Wants { live: false, console: false, jobs: false } },
            "a stop never expires"
        );

        let maintenance = store.write(DesiredState::Maintenance, "cli", "upgrade", Some(5_000), 2_000).unwrap();
        assert_eq!(maintenance.revision, 2);
        assert_eq!(
            effective(&store.read(), 4_999),
            Effective::Act { state: DesiredState::Maintenance, wants: Wants { live: false, console: true, jobs: false } }
        );
        assert!(matches!(effective(&store.read(), 5_000), Effective::Act { state: DesiredState::Running, .. }));

        assert!(store.write(DesiredState::Maintenance, "cli", "", Some(10), 20).is_err(), "end in the past");
        assert!(store.write(DesiredState::Stopped, "cli", "", Some(10_000), 20).is_err(), "only maintenance ends");
        assert!(store.write(DesiredState::Running, " \n", "", None, 20).is_err(), "author required");
        assert_eq!(store.read(), DesiredRead::Present(maintenance), "refused writes change nothing");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_explicit_write_replaces_an_unreadable_record_and_keeps_it_aside() {
        let dir = temp_dir("desired-corrupt");
        let store = DesiredStore::new(&dir);
        std::fs::write(store.path(), b"garbage").unwrap();
        let record = store.write(DesiredState::Running, "cli", "repair", None, 77).unwrap();
        assert_eq!(record.revision, 1);
        assert_eq!(std::fs::read(dir.join(format!("{DESIRED_FILE}.corrupt-77"))).unwrap(), b"garbage");
        assert_eq!(store.read(), DesiredRead::Present(record));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_desired_state_is_refused_and_left_alone() {
        let dir = temp_dir("desired-linked");
        let outside = temp_dir("desired-linked-outside");
        let elsewhere = DesiredStore::new(&outside);
        let kept = elsewhere.write(DesiredState::Running, "cli", "elsewhere", None, 1).unwrap();
        let before = std::fs::read(elsewhere.path()).unwrap();
        let store = DesiredStore::new(&dir);
        std::os::unix::fs::symlink(elsewhere.path(), store.path()).unwrap();
        let error = store.write(DesiredState::Stopped, "cli", "linked", None, 2).unwrap_err();
        assert!(error.contains("desired state cannot be written"), "{error}");
        assert!(std::fs::symlink_metadata(store.path()).unwrap().file_type().is_symlink(), "the link itself stays");
        assert_eq!(std::fs::read(elsewhere.path()).unwrap(), before, "the link target is untouched");
        assert_eq!(elsewhere.read(), DesiredRead::Present(kept));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn a_busy_lifecycle_gate_refuses_after_its_bound() {
        let dir = temp_dir("desired-gate");
        let store = DesiredStore::new(&dir);
        let _held = lock_with_wait(&dir.join(DESIRED_GATE), Duration::ZERO).unwrap();
        let started = Instant::now();
        let error = store.write(DesiredState::Stopped, "cli", "", None, 1).unwrap_err();
        assert!(error.contains("busy"), "{error}");
        assert!(started.elapsed() >= DESIRED_GATE_WAIT);
        assert_eq!(store.read(), DesiredRead::Absent);
        let _ = std::fs::remove_dir_all(dir);
    }
}
