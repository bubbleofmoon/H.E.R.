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
pub const CONSOLE_LOCK: &str = "rbridge-console.lock";
pub const SUPERVISOR_LOCK: &str = "rbridge-supervisor.lock";
pub const DESIRED_FILE: &str = "desired-state.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnerRole {
    Live,
    Console,
    Supervisor,
}

impl OwnerRole {
    pub const ALL: [Self; 3] = [Self::Live, Self::Console, Self::Supervisor];

    pub const fn lock_file(self) -> &'static str {
        match self {
            Self::Live => LIVE_LOCK,
            Self::Console => CONSOLE_LOCK,
            Self::Supervisor => SUPERVISOR_LOCK,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Console => "console",
            Self::Supervisor => "supervisor",
        }
    }

    pub fn parse(role: &str) -> Option<Self> {
        match role {
            "live" => Some(Self::Live),
            "console" => Some(Self::Console),
            "supervisor" => Some(Self::Supervisor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePolicy {
    /// Exact caller executable or same-directory `her`/`her-cli` sibling.
    SameReleaseTwin,
    /// Installed member image under `<root>/releases/<tag>`.
    ReleaseTag {
        root: crate::install_root::InstallRoot,
        tag: String,
    },
    /// `SameReleaseTwin` plus the `HER_TRIAL_CURRENT` read-only current-release exception.
    /// Strictly refused for stop, retire, adoption, or process control.
    HealthReadOnly,
}

impl ImagePolicy {
    pub fn is_control_safe(&self) -> bool {
        !matches!(self, Self::HealthReadOnly)
    }

    pub fn matches(&self, caller_exe: &str, owner_exe: &str) -> bool {
        match self {
            Self::SameReleaseTwin => crate::update::same_release_twin(caller_exe, owner_exe),
            Self::ReleaseTag { root, tag } => {
                crate::update::release_owner_image(root, tag, owner_exe)
            }
            Self::HealthReadOnly => crate::update::health_image(caller_exe, owner_exe),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    /// Read-only probe: validates existing gate paths and probes existing lock files without
    /// creating directories or lock files.
    Existing,
    /// Control/mutation probe: prepares missing gate directories and allows gate file creation.
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOwner {
    pub role: OwnerRole,
    pub marker: OwnerMarker,
    pub gate_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerObservation {
    Free {
        last: Option<OwnerMarker>,
    },
    Verified(VerifiedOwner),
    Mismatch {
        reason: String,
        marker: Option<OwnerMarker>,
        liveness: Option<Liveness>,
    },
    Unknown(String),
}
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
    let base = std::fs::canonicalize(data).map_err(|_| "runtime data directory is unavailable")?;
    for name in ["lifecycle", "logs"] {
        let dir = data.join(name);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() && !crate::guarded_file::is_link(&meta) => {
                if std::fs::canonicalize(&dir).ok().as_deref().and_then(Path::parent) != Some(base.as_path()) {
                    return Err("gate directory is redirected".into());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                std::fs::create_dir(&dir).map_err(|e| format!("gate directory is unavailable: {}", e.kind()))?;
                if std::fs::canonicalize(&dir).ok().as_deref().and_then(Path::parent) != Some(base.as_path()) {
                    return Err("gate directory is redirected".into());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create && name == "lifecycle" => {}
            _ => return Err("gate directory is redirected or unavailable".into()),
        }
    }
    for path in [
        data.join("lifecycle").join(DESIRED_GATE),
        OwnerGate::role_lock_path(data, OwnerRole::Live),
        OwnerGate::role_lock_path(data, OwnerRole::Console),
        OwnerGate::role_lock_path(data, OwnerRole::Supervisor),
    ] {
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
    for role in OwnerRole::ALL {
        match OwnerGate::for_role(data, role).acquire(
            "maintenance-guard", identity.clone(), None, crate::parity_clock::now_ms(), Duration::ZERO,
        ) {
            Acquire::Acquired(owner) => owners.push(owner),
            _ => return Err(format!("owner is held or cannot be established: {}", role.lock_file())),
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
    pub fn role_lock_path(data: &Path, role: OwnerRole) -> PathBuf {
        data.join("logs").join(role.lock_file())
    }

    pub fn for_role(data: &Path, role: OwnerRole) -> Self {
        Self::new(Self::role_lock_path(data, role))
    }

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
    let bytes = crate::guarded_file::read_bounded_no_follow(path, 64 * 1024, false).ok()??;
    let marker: OwnerMarker = serde_json::from_slice(&bytes).ok()?;
    (marker.v == FORMAT_VERSION).then_some(marker)
}

// ---- unified lifecycle seam & adapters --------------------------------------------------------

pub struct GateHold(Box<dyn std::any::Any + Send>);

impl GateHold {
    pub fn new<T: Send + 'static>(inner: T) -> Self {
        Self(Box::new(inner))
    }
}

pub struct QuiescentHold {
    _tasks: GateHold,
    _desired: GateHold,
    _owners: Vec<GateHold>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuiesceError {
    Superseded(String),
    Gate(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetireFailure {
    pub reason: String,
    pub stopped: bool,
}

pub trait LifecycleBackend {
    fn caller_identity(&mut self) -> Result<ProcessIdentity, String>;
    fn validate_gate_paths(&mut self, data: &Path, mode: ProbeMode) -> Result<(), String>;
    fn probe_owner(&mut self, data: &Path, role: OwnerRole, mode: ProbeMode) -> OwnerProbe;
    fn acquire_owner(
        &mut self,
        data: &Path,
        role: OwnerRole,
        claimant: &str,
        identity: ProcessIdentity,
        now_ms: i64,
    ) -> Result<GateHold, String>;
    fn check_identity(&mut self, expected: &ProcessIdentity) -> Liveness;
    fn check_liveness(&mut self, pid: u32, start: u64) -> Liveness;
    fn image_matches(&mut self, policy: &ImagePolicy, caller_exe: &str, owner_exe: &str) -> bool;
    fn read_desired(&mut self, data: &Path) -> DesiredRead;
    fn lock_desired_gate(&mut self, data: &Path, wait: Duration) -> Result<GateHold, String>;
    fn hold_stopped_task_gates(&mut self, data: &Path) -> Result<GateHold, String>;
    fn kill_verified(&mut self, expected: &ProcessIdentity) -> KillOutcome;
    fn read_heartbeat(&mut self, data: &Path) -> io::Result<Vec<u8>>;
    fn now_ms(&mut self) -> i64;
    fn monotonic_ms(&mut self) -> u64;
    fn sleep_step(&mut self, duration: Duration);
}

pub struct LiveLifecycle;

impl LifecycleBackend for LiveLifecycle {
    fn caller_identity(&mut self) -> Result<ProcessIdentity, String> {
        current_identity()
    }

    fn validate_gate_paths(&mut self, data: &Path, mode: ProbeMode) -> Result<(), String> {
        match mode {
            ProbeMode::Existing => validate_existing_owner_gate_paths(data),
            ProbeMode::Create => validate_owner_gate_paths(data),
        }
    }

    fn probe_owner(&mut self, data: &Path, role: OwnerRole, mode: ProbeMode) -> OwnerProbe {
        let gate = OwnerGate::for_role(data, role);
        match mode {
            ProbeMode::Existing => gate.probe_existing(),
            ProbeMode::Create => gate.probe(),
        }
    }

    fn acquire_owner(
        &mut self,
        data: &Path,
        role: OwnerRole,
        claimant: &str,
        identity: ProcessIdentity,
        now_ms: i64,
    ) -> Result<GateHold, String> {
        match OwnerGate::for_role(data, role).acquire(
            claimant,
            identity,
            None,
            now_ms,
            Duration::ZERO,
        ) {
            Acquire::Acquired(guard) => Ok(GateHold::new(guard)),
            _ => Err(format!("{} owner is held or cannot be established", role.as_str())),
        }
    }

    fn check_identity(&mut self, expected: &ProcessIdentity) -> Liveness {
        check_identity(expected)
    }

    fn check_liveness(&mut self, pid: u32, start: u64) -> Liveness {
        crate::process_identity::check_liveness(pid, start)
    }

    fn image_matches(&mut self, policy: &ImagePolicy, caller_exe: &str, owner_exe: &str) -> bool {
        policy.matches(caller_exe, owner_exe)
    }

    fn read_desired(&mut self, data: &Path) -> DesiredRead {
        DesiredStore::new(data.join("lifecycle")).read()
    }

    fn lock_desired_gate(&mut self, data: &Path, wait: Duration) -> Result<GateHold, String> {
        lock_with_wait(&data.join("lifecycle").join(DESIRED_GATE), wait).map(GateHold::new)
    }

    fn hold_stopped_task_gates(&mut self, data: &Path) -> Result<GateHold, String> {
        hold_stopped_task_gates(data).map(GateHold::new)
    }

    fn kill_verified(&mut self, expected: &ProcessIdentity) -> KillOutcome {
        kill_verified(expected)
    }

    fn read_heartbeat(&mut self, data: &Path) -> io::Result<Vec<u8>> {
        match crate::guarded_file::read_bounded_no_follow(
            &data.join("rust-heartbeat-live.json"),
            1024 * 1024,
            false,
        )? {
            Some(bytes) => Ok(bytes),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn now_ms(&mut self) -> i64 {
        crate::parity_clock::now_ms()
    }

    fn monotonic_ms(&mut self) -> u64 {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_millis() as u64
    }

    fn sleep_step(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub fn verify_owner(
    backend: &mut dyn LifecycleBackend,
    data: &Path,
    role: OwnerRole,
    policy: &ImagePolicy,
    mode: ProbeMode,
) -> OwnerObservation {
    if let Err(error) = backend.validate_gate_paths(data, mode) {
        return OwnerObservation::Unknown(error);
    }
    let gate_path = OwnerGate::role_lock_path(data, role);
    match backend.probe_owner(data, role, mode) {
        OwnerProbe::Free { last } => OwnerObservation::Free { last },
        OwnerProbe::Unknown(reason) => OwnerObservation::Unknown(reason),
        OwnerProbe::Held { marker: None } => OwnerObservation::Mismatch {
            reason: format!("{} owner cannot be identified", role.as_str()),
            marker: None,
            liveness: None,
        },
        OwnerProbe::Held {
            marker: Some(marker),
        } => {
            if marker.role != role.as_str() {
                return OwnerObservation::Mismatch {
                    reason: format!("{} owner cannot be identified", role.as_str()),
                    marker: Some(marker),
                    liveness: None,
                };
            }
            let caller_exe = match policy {
                ImagePolicy::SameReleaseTwin | ImagePolicy::HealthReadOnly => {
                    match backend.caller_identity() {
                        Ok(caller) => caller.exe,
                        Err(error) => return OwnerObservation::Unknown(error),
                    }
                }
                ImagePolicy::ReleaseTag { .. } => String::new(),
            };
            if !backend.image_matches(policy, &caller_exe, &marker.identity.exe) {
                return OwnerObservation::Mismatch {
                    reason: format!("{} owner belongs to another executable", role.as_str()),
                    marker: Some(marker),
                    liveness: None,
                };
            }
            match backend.check_identity(&marker.identity) {
                Liveness::Matches => OwnerObservation::Verified(VerifiedOwner {
                    role,
                    marker,
                    gate_path,
                }),
                Liveness::Unknown(reason) => OwnerObservation::Unknown(reason),
                liveness @ (Liveness::Gone | Liveness::Mismatch(_)) => OwnerObservation::Mismatch {
                    reason: format!(
                        "{} owner process identity cannot be verified",
                        role.as_str()
                    ),
                    marker: Some(marker),
                    liveness: Some(liveness),
                },
            }
        }
    }
}

pub fn probe_existing_role_identity(data: &Path, role: OwnerRole) -> Option<ProcessIdentity> {
    let mut backend = LiveLifecycle;
    if backend.validate_gate_paths(data, ProbeMode::Existing).is_err() {
        return None;
    }
    match backend.probe_owner(data, role, ProbeMode::Existing) {
        OwnerProbe::Held {
            marker: Some(marker),
        } if marker.role == role.as_str() => Some(marker.identity),
        _ => None,
    }
}

pub fn live_observation(
    backend: &mut dyn LifecycleBackend,
    data: &Path,
    policy: &ImagePolicy,
    mode: ProbeMode,
) -> crate::watchdog::LiveObservation {
    match verify_owner(backend, data, OwnerRole::Live, policy, mode) {
        OwnerObservation::Free { .. } => crate::watchdog::LiveObservation::Free,
        OwnerObservation::Verified(owner) => crate::watchdog::LiveObservation::Held {
            marker: owner.marker,
            liveness: Liveness::Matches,
        },
        OwnerObservation::Mismatch { marker: None, .. } => {
            crate::watchdog::LiveObservation::HeldUnidentified
        }
        OwnerObservation::Mismatch {
            marker: Some(marker),
            liveness: Some(liveness),
            ..
        } => crate::watchdog::LiveObservation::Held { marker, liveness },
        OwnerObservation::Mismatch {
            marker: Some(marker),
            liveness: None,
            reason,
        } => crate::watchdog::LiveObservation::Held {
            marker,
            liveness: Liveness::Mismatch(reason),
        },
        OwnerObservation::Unknown(reason) => crate::watchdog::LiveObservation::Unknown(reason),
    }
}

pub fn same_groups(left: &[String], right: &[String]) -> bool {
    let mut a = left.to_vec();
    let mut b = right.to_vec();
    a.sort();
    b.sort();
    a == b
}

pub fn verify_live_heartbeat(
    backend: &mut dyn LifecycleBackend,
    data: &Path,
    marker: &OwnerMarker,
    expected_groups: &[String],
) -> Result<(), String> {
    if !marker
        .groups
        .as_deref()
        .is_some_and(|actual| same_groups(actual, expected_groups))
    {
        return Err("live owner does not serve the configured groups".into());
    }
    let bytes = match backend.read_heartbeat(data) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err("live heartbeat is missing".into());
        }
        Err(error) => {
            return Err(format!("live heartbeat is unreadable: {}", error.kind()));
        }
    };
    let heartbeat: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "live heartbeat is invalid")?;
    let pid_matches =
        heartbeat.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(marker.identity.pid));
    let ts_matches = heartbeat.get("ts").is_some_and(|ts| {
        ts.as_i64().map_or_else(
            || {
                ts.as_f64()
                    .is_some_and(|value| value.is_finite() && value >= marker.acquired_at as f64)
            },
            |value| value >= marker.acquired_at,
        )
    });
    if !pid_matches || !ts_matches {
        return Err("live heartbeat does not belong to the current owner".into());
    }
    let now_ms = backend.now_ms();
    match crate::watchdog::read_heartbeat(Ok(bytes), now_ms) {
        crate::watchdog::Heartbeat::Fresh { live: true, groups }
            if same_groups(&groups, expected_groups) =>
        {
            Ok(())
        }
        _ => Err("live heartbeat is not ready for the configured groups".into()),
    }
}

pub fn collect_verified_owners(
    backend: &mut dyn LifecycleBackend,
    data: &Path,
    roles: &[OwnerRole],
    policy: &ImagePolicy,
    mode: ProbeMode,
    expected_supervisor: Option<&ProcessIdentity>,
) -> Result<Vec<VerifiedOwner>, String> {
    if !policy.is_control_safe() {
        return Err("read-only health image policy cannot authorize process control".into());
    }
    backend.validate_gate_paths(data, mode)?;
    let mut owners = Vec::new();
    for &role in roles {
        match verify_owner(backend, data, role, policy, mode) {
            OwnerObservation::Free { .. } => {
                if let Some(old) = expected_supervisor.filter(|_| role == OwnerRole::Supervisor) {
                    let gone = old.pid == 0
                        || matches!(
                            backend.check_liveness(old.pid, old.start),
                            Liveness::Gone | Liveness::Mismatch(_)
                        );
                    if !gone {
                        return Err("supervisor owner is not identified".into());
                    }
                }
            }
            OwnerObservation::Verified(owner) => {
                if let Some(old) = expected_supervisor.filter(|_| role == OwnerRole::Supervisor) {
                    if !same_identity(old, &owner.marker.identity) {
                        return Err("supervisor owner is not the old release".into());
                    }
                }
                owners.push(owner);
            }
            OwnerObservation::Mismatch { reason, .. } => return Err(reason),
            OwnerObservation::Unknown(reason) => {
                return Err(format!("{} owner cannot be identified: {reason}", role.as_str()));
            }
        }
    }
    Ok(owners)
}

pub fn retire_verified_owners(
    backend: &mut dyn LifecycleBackend,
    data: &Path,
    owners: &[VerifiedOwner],
    mode: ProbeMode,
    gate_release_wait: Duration,
    gate_release_poll: Duration,
    fault_after: Option<u64>,
) -> Result<(), RetireFailure> {
    let mut stopped = false;
    let mut retired = 0u64;
    let budget_ms = gate_release_wait.as_millis() as u64;
    for owner in owners {
        match backend.kill_verified(&owner.marker.identity) {
            KillOutcome::Killed | KillOutcome::Gone => stopped = true,
            other => {
                return Err(RetireFailure {
                    reason: format!("{} owner could not be stopped: {other:?}", owner.role.as_str()),
                    stopped,
                });
            }
        }
        retired += 1;
        if fault_after == Some(retired) {
            return Err(RetireFailure {
                reason: "injected retirement failure".into(),
                stopped,
            });
        }
        let start = backend.monotonic_ms();
        loop {
            match backend.probe_owner(data, owner.role, mode) {
                OwnerProbe::Free { .. } => break,
                _ if backend.monotonic_ms().saturating_sub(start) < budget_ms => {
                    backend.sleep_step(gate_release_poll);
                }
                _ => {
                    return Err(RetireFailure {
                        reason: format!("{} owner did not release its gate", owner.role.as_str()),
                        stopped,
                    });
                }
            }
        }
    }
    Ok(())
}

// Lock ordering: ordinary task gates (`tasks/locks/*.lock`) are never acquired while holding
// `lifecycle.lock`. `stop_verified_owners_transaction` checks desired state under
// `lifecycle.lock`, drops `lifecycle.lock`, waits for `hold_stopped_task_gates`, then
// re-acquires `lifecycle.lock` and re-verifies desired state and revision before kills.
pub fn stop_verified_owners_transaction(
    backend: &mut dyn LifecycleBackend,
    data: &Path,
    expected_revision: u64,
    roles: &[OwnerRole],
    policy: &ImagePolicy,
    mode: ProbeMode,
) -> Result<(), String> {
    if !policy.is_control_safe() {
        return Err("read-only health image policy cannot authorize process control".into());
    }
    backend.validate_gate_paths(data, mode)?;
    let desired_gate = backend.lock_desired_gate(data, DESIRED_GATE_WAIT)?;
    if !matches!(
        backend.read_desired(data),
        DesiredRead::Present(record) if record.state == DesiredState::Stopped && record.revision == expected_revision
    ) {
        return Err("the stop was superseded by another lifecycle transition".into());
    }
    // A running task may itself need the lifecycle gate. Never hold it while
    // waiting for that task to observe the stopped state and release its gate.
    drop(desired_gate);
    let _tasks = backend.hold_stopped_task_gates(data)?;
    let _desired_gate = backend.lock_desired_gate(data, DESIRED_GATE_WAIT)?;
    if !matches!(
        backend.read_desired(data),
        DesiredRead::Present(record) if record.state == DesiredState::Stopped && record.revision == expected_revision
    ) {
        return Err("the stop was superseded by another lifecycle transition".into());
    }
    let owners = collect_verified_owners(backend, data, roles, policy, mode, None)?;
    retire_verified_owners(
        backend,
        data,
        &owners,
        mode,
        Duration::from_secs(10),
        Duration::from_millis(50),
        None,
    )
    .map_err(|failure| failure.reason)
}

// Lock ordering: holds stopped task gates before taking `lifecycle.lock`, then acquires owner gates.
pub fn quiesce_stopped_gates(
    backend: &mut dyn LifecycleBackend,
    data: &Path,
    expected_revision: u64,
    roles: &[OwnerRole],
    claimant: &str,
) -> Result<QuiescentHold, QuiesceError> {
    let tasks = backend
        .hold_stopped_task_gates(data)
        .map_err(QuiesceError::Gate)?;
    let desired = backend
        .lock_desired_gate(data, DESIRED_GATE_WAIT)
        .map_err(QuiesceError::Gate)?;
    if !matches!(
        backend.read_desired(data),
        DesiredRead::Present(record) if record.state == DesiredState::Stopped && record.revision == expected_revision
    ) {
        return Err(QuiesceError::Superseded(
            "restore stop was superseded".into(),
        ));
    }
    let identity = backend.caller_identity().map_err(QuiesceError::Gate)?;
    let now = backend.now_ms();
    let mut owners = Vec::with_capacity(roles.len());
    for &role in roles {
        let hold = backend
            .acquire_owner(data, role, claimant, identity.clone(), now)
            .map_err(|_| QuiesceError::Gate("bridge owner is not confirmed stopped".into()))?;
        owners.push(hold);
    }
    Ok(QuiescentHold {
        _tasks: tasks,
        _desired: desired,
        _owners: owners,
    })
}

#[cfg(test)]
pub(crate) struct FakeLifecycle {
    pub caller: Result<ProcessIdentity, String>,
    pub gate_paths_error: Option<String>,
    pub probes: std::collections::HashMap<OwnerRole, std::collections::VecDeque<OwnerProbe>>,
    pub default_probes: std::collections::HashMap<OwnerRole, OwnerProbe>,
    pub liveness: std::collections::HashMap<u32, Liveness>,
    pub default_liveness: Liveness,
    pub kill_results: std::collections::HashMap<u32, KillOutcome>,
    pub default_kill: KillOutcome,
    pub desired_reads: std::collections::VecDeque<DesiredRead>,
    pub default_desired: DesiredRead,
    pub desired_gate_error: Option<String>,
    pub task_gate_errors: std::collections::VecDeque<Result<(), String>>,
    pub acquire_errors: std::collections::HashMap<OwnerRole, String>,
    pub heartbeat: Option<io::Result<Vec<u8>>>,
    pub image_override: Option<bool>,
    pub now_ms: i64,
    pub monotonic_ms: u64,
    pub events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub on_hold_tasks: Option<Box<dyn FnMut(&mut FakeLifecycle)>>,
    pub on_kill: Option<Box<dyn FnMut(&mut FakeLifecycle, &ProcessIdentity)>>,
}

#[cfg(test)]
impl Default for FakeLifecycle {
    fn default() -> Self {
        Self {
            caller: Ok(ProcessIdentity {
                pid: 1000,
                start: 1000,
                exe: "/opt/her/her".into(),
            }),
            gate_paths_error: None,
            probes: std::collections::HashMap::new(),
            default_probes: std::collections::HashMap::new(),
            liveness: std::collections::HashMap::new(),
            default_liveness: Liveness::Matches,
            kill_results: std::collections::HashMap::new(),
            default_kill: KillOutcome::Killed,
            desired_reads: std::collections::VecDeque::new(),
            default_desired: DesiredRead::Absent,
            desired_gate_error: None,
            task_gate_errors: std::collections::VecDeque::new(),
            acquire_errors: std::collections::HashMap::new(),
            heartbeat: None,
            image_override: None,
            now_ms: 1_000_000,
            monotonic_ms: 0,
            events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            on_hold_tasks: None,
            on_kill: None,
        }
    }
}

#[cfg(test)]
impl FakeLifecycle {
    fn record(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }
}

#[cfg(test)]
struct FakeDropLog {
    label: &'static str,
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(test)]
impl Drop for FakeDropLog {
    fn drop(&mut self) {
        self.events.lock().unwrap().push(self.label.into());
    }
}

#[cfg(test)]
impl LifecycleBackend for FakeLifecycle {
    fn caller_identity(&mut self) -> Result<ProcessIdentity, String> {
        self.caller.clone()
    }

    fn validate_gate_paths(&mut self, _data: &Path, mode: ProbeMode) -> Result<(), String> {
        self.record(format!("validate:{mode:?}"));
        self.gate_paths_error.clone().map_or(Ok(()), Err)
    }

    fn probe_owner(&mut self, _data: &Path, role: OwnerRole, mode: ProbeMode) -> OwnerProbe {
        self.record(format!("probe:{}:{mode:?}", role.as_str()));
        if let Some(probe) = self
            .probes
            .get_mut(&role)
            .and_then(std::collections::VecDeque::pop_front)
        {
            return probe;
        }
        self.default_probes
            .get(&role)
            .cloned()
            .unwrap_or(OwnerProbe::Free { last: None })
    }

    fn acquire_owner(
        &mut self,
        _data: &Path,
        role: OwnerRole,
        claimant: &str,
        identity: ProcessIdentity,
        now_ms: i64,
    ) -> Result<GateHold, String> {
        self.record(format!("acquire:{}:{claimant}", role.as_str()));
        if let Some(err) = self.acquire_errors.get(&role) {
            return Err(err.clone());
        }
        self.default_probes.insert(
            role,
            OwnerProbe::Held {
                marker: Some(OwnerMarker {
                    v: FORMAT_VERSION,
                    role: role.as_str().into(),
                    identity,
                    acquired_at: now_ms,
                    refreshed_at: now_ms,
                    groups: None,
                }),
            },
        );
        Ok(GateHold::new(()))
    }

    fn check_identity(&mut self, expected: &ProcessIdentity) -> Liveness {
        self.liveness
            .get(&expected.pid)
            .cloned()
            .unwrap_or_else(|| self.default_liveness.clone())
    }

    fn check_liveness(&mut self, pid: u32, _start: u64) -> Liveness {
        self.liveness
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| self.default_liveness.clone())
    }

    fn image_matches(&mut self, policy: &ImagePolicy, caller_exe: &str, owner_exe: &str) -> bool {
        self.image_override
            .unwrap_or_else(|| policy.matches(caller_exe, owner_exe))
    }

    fn read_desired(&mut self, _data: &Path) -> DesiredRead {
        self.record("read-desired");
        self.desired_reads
            .pop_front()
            .unwrap_or_else(|| self.default_desired.clone())
    }

    fn lock_desired_gate(&mut self, _data: &Path, _wait: Duration) -> Result<GateHold, String> {
        self.record("lock-desired");
        if let Some(err) = &self.desired_gate_error {
            return Err(err.clone());
        }
        Ok(GateHold::new(FakeDropLog {
            label: "unlock-desired",
            events: self.events.clone(),
        }))
    }

    fn hold_stopped_task_gates(&mut self, _data: &Path) -> Result<GateHold, String> {
        self.record("hold-tasks");
        if let Some(mut cb) = self.on_hold_tasks.take() {
            cb(self);
            self.on_hold_tasks = Some(cb);
        }
        self.task_gate_errors.pop_front().unwrap_or(Ok(()))?;
        Ok(GateHold::new(()))
    }

    fn kill_verified(&mut self, expected: &ProcessIdentity) -> KillOutcome {
        self.record(format!("kill:{}", expected.pid));
        if let Some(mut cb) = self.on_kill.take() {
            cb(self, expected);
            self.on_kill = Some(cb);
        }
        self.kill_results
            .get(&expected.pid)
            .cloned()
            .unwrap_or_else(|| self.default_kill.clone())
    }

    fn read_heartbeat(&mut self, _data: &Path) -> io::Result<Vec<u8>> {
        match &self.heartbeat {
            Some(Ok(bytes)) => Ok(bytes.clone()),
            Some(Err(err)) => Err(io::Error::new(err.kind(), err.to_string())),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn now_ms(&mut self) -> i64 {
        self.now_ms
    }

    fn monotonic_ms(&mut self) -> u64 {
        self.monotonic_ms
    }

    fn sleep_step(&mut self, duration: Duration) {
        self.monotonic_ms = self.monotonic_ms.saturating_add(duration.as_millis() as u64);
        self.record(format!("sleep:{}ms", duration.as_millis()));
    }
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

    fn fake_owner_marker(role: OwnerRole, pid: u32, exe: &str, acquired_at: i64, groups: Option<Vec<&str>>) -> OwnerMarker {
        OwnerMarker {
            v: FORMAT_VERSION,
            role: role.as_str().into(),
            identity: ProcessIdentity {
                pid,
                start: 10_000 + u64::from(pid),
                exe: exe.into(),
            },
            acquired_at,
            refreshed_at: acquired_at,
            groups: groups.map(|list| list.into_iter().map(str::to_string).collect()),
        }
    }

    fn desired_stopped(revision: u64) -> DesiredRead {
        DesiredRead::Present(DesiredRecord {
            v: FORMAT_VERSION,
            state: DesiredState::Stopped,
            by: "test".into(),
            reason: "quiesce".into(),
            at: 1_000,
            until: None,
            revision,
        })
    }

    #[test]
    fn verify_live_heartbeat_rejects_stale_pid_groups_and_expired_heartbeats() {
        let mut fake = FakeLifecycle::default();
        fake.now_ms = 550_000;
        let expected = vec!["group:1".to_string(), "private:2".to_string()];
        let marker = fake_owner_marker(OwnerRole::Live, 42, "/opt/her/her", 500_000, Some(vec!["group:1", "private:2"]));
        let unused = Path::new("/unused");

        // Missing heartbeat
        fake.heartbeat = None;
        let err = verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap_err();
        assert!(err.contains("missing"), "{err}");

        // Invalid JSON
        fake.heartbeat = Some(Ok(b"{not-json".to_vec()));
        let err = verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap_err();
        assert!(err.contains("invalid"), "{err}");

        // PID mismatch
        fake.heartbeat = Some(Ok(
            serde_json::json!({"ts": 500_000, "pid": 99, "live": true, "groups": ["group:1", "private:2"]})
                .to_string()
                .into_bytes(),
        ));
        let err = verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap_err();
        assert!(err.contains("does not belong to the current owner"), "{err}");

        // Stale heartbeat written before this owner acquired its gate (ts < acquired_at)
        fake.heartbeat = Some(Ok(
            serde_json::json!({"ts": 499_999, "pid": 42, "live": true, "groups": ["group:1", "private:2"]})
                .to_string()
                .into_bytes(),
        ));
        let err = verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap_err();
        assert!(err.contains("does not belong to the current owner"), "{err}");

        // Expired heartbeat (age >= 90_000 ms)
        fake.now_ms = 590_000;
        fake.heartbeat = Some(Ok(
            serde_json::json!({"ts": 500_000, "pid": 42, "live": true, "groups": ["group:1", "private:2"]})
                .to_string()
                .into_bytes(),
        ));
        let err = verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap_err();
        assert!(err.contains("not ready"), "{err}");

        // live: false
        fake.now_ms = 505_000;
        fake.heartbeat = Some(Ok(
            serde_json::json!({"ts": 500_000, "pid": 42, "live": false, "groups": ["group:1", "private:2"]})
                .to_string()
                .into_bytes(),
        ));
        let err = verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap_err();
        assert!(err.contains("not ready"), "{err}");

        // Heartbeat groups mismatch
        fake.heartbeat = Some(Ok(
            serde_json::json!({"ts": 500_000, "pid": 42, "live": true, "groups": ["group:1"]})
                .to_string()
                .into_bytes(),
        ));
        let err = verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap_err();
        assert!(err.contains("not ready"), "{err}");

        // Marker groups mismatch
        let wrong_marker = fake_owner_marker(OwnerRole::Live, 42, "/opt/her/her", 500_000, Some(vec!["group:1"]));
        let err = verify_live_heartbeat(&mut fake, unused, &wrong_marker, &expected).unwrap_err();
        assert!(err.contains("does not serve"), "{err}");

        // Fresh heartbeat matching pid, ts >= acquired_at, and groups (regardless of order)
        fake.now_ms = 589_999;
        fake.heartbeat = Some(Ok(
            serde_json::json!({"ts": 500_000, "pid": 42, "live": true, "groups": ["private:2", "group:1"]})
                .to_string()
                .into_bytes(),
        ));
        verify_live_heartbeat(&mut fake, unused, &marker, &expected).unwrap();
    }

    #[test]
    fn foreign_image_and_health_only_policy_refused_for_process_control() {
        let mut fake = FakeLifecycle::default();
        fake.default_desired = desired_stopped(1);
        fake.default_probes.insert(
            OwnerRole::Live,
            OwnerProbe::Held {
                marker: Some(fake_owner_marker(OwnerRole::Live, 55, "/foreign/her", 100, None)),
            },
        );
        let unused = Path::new("/unused");

        // Foreign executable refused by verify_owner and stop transaction without killing.
        assert!(matches!(
            verify_owner(&mut fake, unused, OwnerRole::Live, &ImagePolicy::SameReleaseTwin, ProbeMode::Existing),
            OwnerObservation::Mismatch { marker: Some(_), liveness: None, .. }
        ));
        let err = stop_verified_owners_transaction(
            &mut fake,
            unused,
            1,
            &[OwnerRole::Live],
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        )
        .unwrap_err();
        assert!(err.contains("another executable"), "{err}");
        assert!(
            !fake.events.lock().unwrap().iter().any(|e| e.starts_with("kill:")),
            "foreign executable must never be killed"
        );

        // Role mismatch on lock marker is refused.
        fake.default_probes.insert(
            OwnerRole::Live,
            OwnerProbe::Held {
                marker: Some(fake_owner_marker(OwnerRole::Console, 55, "/opt/her/her", 100, None)),
            },
        );
        assert!(matches!(
            verify_owner(&mut fake, unused, OwnerRole::Live, &ImagePolicy::SameReleaseTwin, ProbeMode::Existing),
            OwnerObservation::Mismatch { marker: Some(_), liveness: None, .. }
        ));

        // ImagePolicy::HealthReadOnly is strictly refused for collection, stop, and retirement.
        fake.default_probes.insert(
            OwnerRole::Live,
            OwnerProbe::Held {
                marker: Some(fake_owner_marker(OwnerRole::Live, 55, "/opt/her/her", 100, None)),
            },
        );
        let err = collect_verified_owners(
            &mut fake,
            unused,
            &[OwnerRole::Live],
            &ImagePolicy::HealthReadOnly,
            ProbeMode::Existing,
            None,
        )
        .unwrap_err();
        assert!(err.contains("read-only health image policy"), "{err}");
        let err = stop_verified_owners_transaction(
            &mut fake,
            unused,
            1,
            &[OwnerRole::Live],
            &ImagePolicy::HealthReadOnly,
            ProbeMode::Create,
        )
        .unwrap_err();
        assert!(err.contains("read-only health image policy"), "{err}");
    }

    #[test]
    fn trial_authority_is_accepted_only_for_health_read_only() {
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        if std::env::var_os("HER_348_TRIAL_CHILD").is_some() {
            let root = crate::install_root::InstallRoot::configured().unwrap();
            let v1 = root.root().join("releases").join("v1.0.0");
            let v2 = root.root().join("releases").join("v2.0.0");
            let caller = v2.join("her-cli.exe").to_string_lossy().into_owned();
            let owner = v1.join("her.exe").to_string_lossy().into_owned();

            assert!(ImagePolicy::HealthReadOnly.matches(&caller, &owner));
            assert!(!ImagePolicy::SameReleaseTwin.matches(&caller, &owner));
            assert!(ImagePolicy::ReleaseTag { root: root.clone(), tag: "v1.0.0".into() }.matches(&caller, &owner));
            assert!(!ImagePolicy::ReleaseTag { root: root.clone(), tag: "v2.0.0".into() }.matches(&caller, &owner));
            assert!(!ImagePolicy::HealthReadOnly.matches(
                &root.root().join("herupdate.exe").to_string_lossy(),
                &owner,
            ));

            let mut fake = FakeLifecycle::default();
            fake.caller = Ok(ProcessIdentity { pid: 900, start: 900, exe: caller.clone() });
            fake.default_probes.insert(
                OwnerRole::Supervisor,
                OwnerProbe::Held {
                    marker: Some(fake_owner_marker(OwnerRole::Supervisor, 50, &owner, 100, None)),
                },
            );
            assert!(matches!(
                verify_owner(&mut fake, &root.root().join("runtime/data"), OwnerRole::Supervisor, &ImagePolicy::HealthReadOnly, ProbeMode::Existing),
                OwnerObservation::Verified(_)
            ));
            assert!(matches!(
                verify_owner(&mut fake, &root.root().join("runtime/data"), OwnerRole::Supervisor, &ImagePolicy::SameReleaseTwin, ProbeMode::Existing),
                OwnerObservation::Mismatch { .. }
            ));
            assert!(collect_verified_owners(
                &mut fake,
                &root.root().join("runtime/data"),
                &[OwnerRole::Supervisor],
                &ImagePolicy::HealthReadOnly,
                ProbeMode::Existing,
                None,
            ).is_err());

            // ProbeMode::Existing on real LiveLifecycle never creates missing gate directories/locks.
            let runtime_data = root.root().join("runtime/data");
            assert!(matches!(
                verify_owner(&mut LiveLifecycle, &runtime_data, OwnerRole::Supervisor, &ImagePolicy::HealthReadOnly, ProbeMode::Existing),
                OwnerObservation::Unknown(_)
            ));
            assert!(!runtime_data.join("logs").exists());
            assert!(!runtime_data.join("lifecycle").exists());
            return;
        }

        let base = temp_dir("trial-policy");
        let _cleanup = Cleanup(base.clone());
        for rel in [
            base.join("releases").join("v1.0.0"),
            base.join("releases").join("v2.0.0"),
            base.join("runtime").join("data"),
        ] {
            std::fs::create_dir_all(rel).unwrap();
        }
        for rel in [
            base.join("releases").join("v1.0.0").join("her.exe"),
            base.join("releases").join("v1.0.0").join("her-cli.exe"),
            base.join("releases").join("v2.0.0").join("her-cli.exe"),
            base.join("herupdate.exe"),
        ] {
            std::fs::write(rel, b"").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("releases").join("v1.0.0"), base.join("current")).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let out = Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(base.join("current"))
                .arg(base.join("releases").join("v1.0.0"))
                .creation_flags(0x0800_0000)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }

        let mut cmd = Command::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let output = cmd
            .args(["--exact", "lifecycle::tests::trial_authority_is_accepted_only_for_health_read_only", "--nocapture"])
            .env("HER_348_TRIAL_CHILD", "1")
            .env("HER_INSTALL_ROOT", &base)
            .env("HER_TRIAL_CURRENT", base.join("current"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn ordered_quiescence_drains_tasks_checks_revision_and_waits_for_gate_release() {
        let unused = Path::new("/unused");

        // 1. Happy path: lock-desired -> read -> unlock-desired -> hold-tasks -> lock-desired -> read -> kill -> poll Free
        let mut fake = FakeLifecycle::default();
        fake.default_desired = desired_stopped(5);
        let live_marker = fake_owner_marker(OwnerRole::Live, 101, "/opt/her/her", 100, None);
        let console_marker = fake_owner_marker(OwnerRole::Console, 102, "/opt/her/her", 100, None);
        fake.probes.insert(
            OwnerRole::Live,
            std::collections::VecDeque::from([
                OwnerProbe::Held { marker: Some(live_marker.clone()) }, // verify_owner
                OwnerProbe::Held { marker: Some(live_marker) },         // 1st poll after kill -> Held
                OwnerProbe::Free { last: None },                        // 2nd poll -> Free
            ]),
        );
        fake.probes.insert(
            OwnerRole::Console,
            std::collections::VecDeque::from([
                OwnerProbe::Held { marker: Some(console_marker) }, // verify_owner
                OwnerProbe::Free { last: None },                   // 1st poll after kill -> Free
            ]),
        );
        stop_verified_owners_transaction(
            &mut fake,
            unused,
            5,
            &[OwnerRole::Live, OwnerRole::Console],
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        )
        .unwrap();
        let events = fake.events.lock().unwrap().clone();
        let unlock_pos = events.iter().position(|e| e == "unlock-desired").unwrap();
        let tasks_pos = events.iter().position(|e| e == "hold-tasks").unwrap();
        let kill_live_pos = events.iter().position(|e| e == "kill:101").unwrap();
        let sleep_pos = events.iter().position(|e| e == "sleep:50ms").unwrap();
        let kill_console_pos = events.iter().position(|e| e == "kill:102").unwrap();
        assert!(unlock_pos < tasks_pos, "lifecycle.lock must drop before waiting on tasks: {events:?}");
        assert!(tasks_pos < kill_live_pos, "tasks must drain before killing owners: {events:?}");
        assert!(kill_live_pos < sleep_pos && sleep_pos < kill_console_pos, "wait for live gate Free before console kill: {events:?}");

        // 2. Revision supersession while holding task gates aborts before any kill.
        let mut fake = FakeLifecycle::default();
        fake.default_desired = desired_stopped(5);
        fake.default_probes.insert(
            OwnerRole::Live,
            OwnerProbe::Held { marker: Some(fake_owner_marker(OwnerRole::Live, 101, "/opt/her/her", 100, None)) },
        );
        fake.on_hold_tasks = Some(Box::new(|state| {
            state.default_desired = desired_stopped(6);
        }));
        let err = stop_verified_owners_transaction(
            &mut fake,
            unused,
            5,
            &[OwnerRole::Live],
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        )
        .unwrap_err();
        assert!(err.contains("superseded"), "{err}");
        assert!(!fake.events.lock().unwrap().iter().any(|e| e.starts_with("kill:")));

        // 3. Task-gate failure aborts before any kill.
        let mut fake = FakeLifecycle::default();
        fake.default_desired = desired_stopped(5);
        fake.task_gate_errors.push_back(Err("running tasks did not stop".into()));
        let err = stop_verified_owners_transaction(
            &mut fake,
            unused,
            5,
            &[OwnerRole::Live],
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        )
        .unwrap_err();
        assert!(err.contains("running tasks did not stop"), "{err}");
        assert!(!fake.events.lock().unwrap().iter().any(|e| e.starts_with("kill:")));

        // 4. Gate-release timeout fails once monotonic deadline passes.
        let mut fake = FakeLifecycle::default();
        fake.default_desired = desired_stopped(5);
        fake.default_probes.insert(
            OwnerRole::Live,
            OwnerProbe::Held { marker: Some(fake_owner_marker(OwnerRole::Live, 101, "/opt/her/her", 100, None)) },
        );
        let err = stop_verified_owners_transaction(
            &mut fake,
            unused,
            5,
            &[OwnerRole::Live],
            &ImagePolicy::SameReleaseTwin,
            ProbeMode::Create,
        )
        .unwrap_err();
        assert!(err.contains("did not release its gate"), "{err}");
        assert!(fake.monotonic_ms >= 10_000);

        // 5. Partial-stop tracking in retire_verified_owners: fail before stop vs after stop.
        let owners = vec![
            VerifiedOwner {
                role: OwnerRole::Supervisor,
                marker: fake_owner_marker(OwnerRole::Supervisor, 201, "/opt/her/her", 100, None),
                gate_path: PathBuf::from("/unused/supervisor.lock"),
            },
            VerifiedOwner {
                role: OwnerRole::Live,
                marker: fake_owner_marker(OwnerRole::Live, 202, "/opt/her/her", 100, None),
                gate_path: PathBuf::from("/unused/live.lock"),
            },
        ];
        let mut fake = FakeLifecycle::default();
        fake.kill_results.insert(201, KillOutcome::Refused("pid reused".into()));
        let failure = retire_verified_owners(
            &mut fake,
            unused,
            &owners,
            ProbeMode::Existing,
            Duration::from_secs(5),
            Duration::from_millis(20),
            None,
        )
        .unwrap_err();
        assert!(!failure.stopped, "refused first kill leaves stopped=false: {failure:?}");

        let mut fake = FakeLifecycle::default();
        fake.default_probes.insert(OwnerRole::Supervisor, OwnerProbe::Free { last: None });
        let failure = retire_verified_owners(
            &mut fake,
            unused,
            &owners,
            ProbeMode::Existing,
            Duration::from_secs(5),
            Duration::from_millis(20),
            Some(1),
        )
        .unwrap_err();
        assert!(failure.stopped, "failure after first kill sets stopped=true: {failure:?}");
        assert!(!fake.events.lock().unwrap().iter().any(|e| e == "kill:202"));

        // 6. quiesce_stopped_gates verifies revision and acquires owner gates.
        let mut fake = FakeLifecycle::default();
        fake.default_desired = desired_stopped(9);
        assert!(matches!(
            quiesce_stopped_gates(&mut fake, unused, 8, &[OwnerRole::Live, OwnerRole::Console], "restore"),
            Err(QuiesceError::Superseded(_))
        ));
        let hold = quiesce_stopped_gates(&mut fake, unused, 9, &[OwnerRole::Live, OwnerRole::Console], "restore").unwrap();
        drop(hold);
        assert!(matches!(
            fake.default_probes.get(&OwnerRole::Live),
            Some(OwnerProbe::Held { marker: Some(m) }) if m.role == "live"
        ));
    }

    #[test]
    fn handover_owner_probe_ignores_unlocked_stale_and_redirected_markers() {
        let dir = temp_dir("handover-marker");
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let lock = logs.join(LIVE_LOCK);
        let gate = OwnerGate::new(&lock);

        // 1. Stale marker left after owner released the lock must be ignored.
        let guard = match gate.acquire("live", me(), Some(vec!["group:1".into()]), 100, Duration::ZERO) {
            Acquire::Acquired(guard) => guard,
            other => panic!("acquire failed: {other:?}"),
        };
        drop(guard);
        assert!(
            crate::update::probe_held_owner_identity(&lock, "live").is_none(),
            "unlocked gate with stale .owner.json must be ignored"
        );

        // 2. Actively held gate with valid marker returns identity.
        let guard = match gate.acquire("live", me(), Some(vec!["group:1".into()]), 200, Duration::ZERO) {
            Acquire::Acquired(guard) => guard,
            other => panic!("acquire failed: {other:?}"),
        };
        assert_eq!(crate::update::probe_held_owner_identity(&lock, "live"), Some(me()));
        assert_eq!(crate::update::probe_held_owner_identity(&lock, "console"), None);

        // 3. Oversized marker (> 64 KiB) is refused.
        let saved = std::fs::read(gate.marker_path()).unwrap();
        std::fs::write(gate.marker_path(), vec![b' '; 65 * 1024]).unwrap();
        assert_eq!(crate::update::probe_held_owner_identity(&lock, "live"), None);

        // 4. Symlinked marker is refused on Unix.
        #[cfg(unix)]
        {
            let target = dir.join("real-marker.json");
            std::fs::write(&target, &saved).unwrap();
            std::fs::remove_file(gate.marker_path()).unwrap();
            std::os::unix::fs::symlink(&target, gate.marker_path()).unwrap();
            assert_eq!(crate::update::probe_held_owner_identity(&lock, "live"), None);
        }
        let _ = saved;
        drop(guard);
        let _ = std::fs::remove_dir_all(dir);
    }
}
