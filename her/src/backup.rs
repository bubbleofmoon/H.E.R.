//! Selected-runtime backup creation and verification.

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use crate::task_runs::{Finish, LogStream, RunHandle};
use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OpenFlags,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zip::{write::FileOptions, ZipArchive, ZipWriter};

type Check<'a> = &'a dyn Fn() -> Result<(), String>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDigest {
    pub size: u64,
    pub sha256: String,
}

/// Version 1 maps archive-relative payload paths to size and lowercase SHA-256.
/// The manifest itself is excluded from its file map.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub files: BTreeMap<String, FileDigest>,
}

/// Backup policy read afresh for each run.
pub struct Settings {
    pub destination: PathBuf,
    pub keep_days: u64,
    pub include_state: bool,
    pub dir: Option<String>,
    pub time: String,
}

impl Settings {
    pub fn from_config(runtime: &Path, config: &serde_json::Value) -> Result<Self, String> {
        let backup = crate::config::backup_config(config)?;
        let configured = match &backup["dir"] {
            serde_json::Value::Null => PathBuf::from("backups"),
            serde_json::Value::String(value) => PathBuf::from(value),
            _ => return Err("backup.dir must be a path string".into()),
        };
        let destination = crate::install_root::InstallRoot::configured()?
            .backup_destination(runtime, &configured)?;
        Ok(Self {
            destination,
            keep_days: backup["keepDays"]
                .as_u64()
                .expect("validated retention days"),
            include_state: backup["includeState"]
                .as_bool()
                .expect("validated state setting"),
            dir: backup["dir"].as_str().map(str::to_owned),
            time: backup["time"]
                .as_str()
                .expect("validated daily time")
                .to_owned(),
        })
    }

    /// Settings for a console save or a backup run: the lexical checks of `from_config`
    /// plus resolution of existing local aliases of the destination.
    pub fn from_config_resolved(runtime: &Path, config: &serde_json::Value) -> Result<Self, String> {
        let settings = Self::from_config(runtime, config)?;
        crate::install_root::InstallRoot::configured()?
            .check_resolved_backup_destination(&settings.destination)?;
        Ok(settings)
    }

    pub fn view(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "dir": self.dir.as_deref(), "keepDays": self.keep_days,
            "time": self.time, "includeState": self.include_state,
            "resolvedDir": crate::subprocess::external_path(&self.destination)?,
        }))
    }
}

pub fn daily_schedule(config: &serde_json::Value) -> Result<crate::tasks::Schedule, String> {
    let config = crate::config::backup_config(config)?;
    let time = config["time"].as_str().expect("validated backup time");
    Ok(crate::tasks::Schedule::Daily {
        hour: time[..2].parse().expect("validated hour"),
        minute: time[3..].parse().expect("validated minute"),
    })
}

pub fn refresh_schedule(
    config: &serde_json::Value,
    scheduler: &mut crate::tasks::Scheduler,
    now: i64,
) -> Result<(), String> {
    scheduler.set_schedule(crate::tasks::TaskId::Backup, daily_schedule(config)?, now);
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct NoticeState {
    version: u32,
    schedule: Option<String>,
    active_since: i64,
    checked_through: Option<i64>,
    seen: BTreeMap<String, i64>,
}

impl Default for NoticeState {
    fn default() -> Self {
        Self {
            version: 1,
            schedule: None,
            active_since: 0,
            checked_through: None,
            seen: BTreeMap::new(),
        }
    }
}

const NOTICE_STATE: &str = "backup-notices.json";

fn claim_notice(
    state: &mut NoticeState,
    now: i64,
    key: String,
    text: String,
    out: &mut Vec<(String, String)>,
) {
    if !state.seen.contains_key(&key) {
        state.seen.insert(key.clone(), now);
        out.push((key, text));
    }
}

/// Claims are synced before sending; unknown send outcomes are never retried.
fn update_notices(
    context: &RuntimeContext,
    update: impl FnOnce(&mut NoticeState) -> Result<Vec<(String, String)>, String>,
) -> Result<Vec<(String, String)>, String> {
    let started = Instant::now();
    let _gate = loop {
        match context.open_locked_file(RuntimeArea::Data, "locks/backup-notices.lock") {
            Ok(file) => break file,
            Err(error)
                if error == "runtime file is already locked"
                    && started.elapsed() < Duration::from_secs(1) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    };
    let mut state: NoticeState = match context
        .read_optional_text(RuntimeArea::Data, NOTICE_STATE)
        .map_err(str::to_owned)?
    {
        None => NoticeState::default(),
        Some(text) => {
            serde_json::from_str(&text).map_err(|e| format!("backup notices invalid: {e}"))?
        }
    };
    if state.version != 1 {
        return Err("backup notices version unsupported".into());
    }
    let notices = update(&mut state)?;
    context.write_atomic(
        RuntimeArea::Data,
        NOTICE_STATE,
        &serde_json::to_vec(&state).map_err(|e| e.to_string())?,
    )?;
    Ok(notices)
}

fn send_notices(
    context: &RuntimeContext,
    config: &serde_json::Value,
    notices: Result<Vec<(String, String)>, String>,
) {
    match notices {
        Ok(notices) => {
            for (key, text) in notices {
                crate::admin_alert::notify_notice(context, config, &text, &key);
            }
        }
        Err(error) => crate::safe_eprintln!("[backup] administrator notice unavailable: {error}"),
    }
}

pub fn notify_failure(
    context: &RuntimeContext,
    config: &serde_json::Value,
    run_id: &str,
    reason: &str,
) {
    let now = crate::parity_clock::now_ms();
    let notices = update_notices(context, |state| {
        let mut notices = Vec::new();
        claim_notice(
            state,
            now,
            format!("backup-failed-{run_id}"),
            format!("Backup failed ({run_id}): {reason}"),
            &mut notices,
        );
        Ok(notices)
    });
    send_notices(context, config, notices);
}

/// Observe retained failed runs and elapsed daily slots without catch-up execution.
/// A minute's grace allows the scheduler to claim a due slot before it is called missed.
pub fn monitor_notices(
    context: &RuntimeContext,
    config: &serde_json::Value,
    store: &crate::task_runs::TaskStore,
    now: i64,
) -> Result<(), String> {
    use crate::task_runs::{RunQuery, RunSource, RunState};
    use crate::tasks::{self, TaskId};
    let schedule = daily_schedule(config)?;
    let on = crate::supervisor::enabled_in(context, config, tasks::task_def(TaskId::Backup));
    let signature = format!("{}:{on}", crate::config::backup_config(config)?["time"]);
    let notices = update_notices(context, |state| {
        // The first observation and every time/enabled change start now: neither older
        // history nor slots before this moment (e.g. a time already past) are alerted.
        if state.schedule.as_deref() != Some(signature.as_str()) {
            state.schedule = Some(signature);
            state.active_since = now;
            state.checked_through = Some(now);
        }
        // Read history inside the notice gate: pruning a stale snapshot must
        // never erase a failure claim concurrently saved by another process.
        let mut runs = Vec::new();
        let mut before = None;
        loop {
            let page = store.list(&RunQuery {
                task: Some("backup".into()),
                before,
                limit: Some(200),
            })?;
            before = page.next_before;
            runs.extend(page.runs);
            if before.is_none() {
                break;
            }
        }
        let retained: BTreeSet<_> = runs
            .iter()
            .map(|run| format!("backup-failed-{}", run.run_id))
            .collect();
        // Failed-run keys need live only as long as their retained task history.
        state
            .seen
            .retain(|key, _| !key.starts_with("backup-failed-") || retained.contains(key));
        let mut notices = Vec::new();
        for run in &runs {
            if run.ended_at.unwrap_or(run.queued_at) < state.active_since {
                continue;
            }
            if matches!(run.state, RunState::Failed | RunState::Unknown)
                || (run.source == RunSource::Scheduled && run.state == RunState::Cancelled)
            {
                claim_notice(
                    state,
                    now,
                    format!("backup-failed-{}", run.run_id),
                    format!(
                        "Backup failed ({}): {}",
                        run.run_id,
                        run.reason.as_deref().unwrap_or("result unknown")
                    ),
                    &mut notices,
                );
            }
        }
        if on {
            let mut after = state
                .checked_through
                .unwrap_or(state.active_since.saturating_sub(1));
            while let Some(slot) = tasks::next_due(schedule, after, tasks::local_offset_ms())
                .filter(|slot| *slot <= now.saturating_sub(60_000))
            {
                let attempted = runs.iter().any(|run| {
                    run.source == RunSource::Scheduled
                        && run.queued_at >= slot
                        && run.queued_at < slot.saturating_add(86_400_000)
                });
                if !attempted {
                    claim_notice(
                        state,
                        now,
                        format!("backup-missed-{slot}"),
                        format!(
                            "Backup missed its daily slot at {} UTC (local {}).",
                            archive_stem(slot).trim_start_matches("qqbot-"),
                            config["backup"]["time"].as_str().unwrap_or("04:00")
                        ),
                        &mut notices,
                    );
                }
                state.checked_through = Some(slot);
                after = slot;
            }
        }
        Ok(notices)
    });
    send_notices(context, config, notices);
    Ok(())
}

pub(crate) fn redirected(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn ensure_destination(path: &Path) -> Result<(), String> {
    inspect_destination(path, true).map(|_| ())
}

fn inspect_destination(path: &Path, create: bool) -> Result<bool, String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        // A verbatim drive prefix (\\?\C:) is not an openable directory until its root separator.
        if matches!(component, std::path::Component::Prefix(_)) || !current.has_root() {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !redirected(&metadata) && metadata.is_dir() => {}
            Ok(_) => return Err("backup destination is redirected or is not a directory".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(false);
                }
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(format!("backup destination unavailable: {}", error.kind()))
                    }
                }
                let metadata = fs::symlink_metadata(&current).map_err(|e| e.to_string())?;
                if redirected(&metadata) || !metadata.is_dir() {
                    return Err("backup destination is redirected".into());
                }
            }
            Err(error) => return Err(format!("backup destination unavailable: {}", error.kind())),
        }
    }
    Ok(true)
}

const DESTINATION_LOCK: &str = ".her-backup.lock";

/// Keep this inode in place: unlinking a stale lock can split simultaneous waiters
/// across different files. The OS lock is authoritative, including after a crash.
/// The identity marker detects dead/reused owners without using file age.
struct DestinationLock(File);
impl DestinationLock {
    fn acquire(destination: &Path, check: Check<'_>) -> Result<Self, String> {
        use crate::lifecycle::{check_identity, current_identity, Liveness, ProcessIdentity};
        let path = destination.join(DESTINATION_LOCK);
        let identity = current_identity()?;
        let started = Instant::now();
        loop {
            check()?;
            if started.elapsed() >= Duration::from_secs(30) {
                return Err("backup destination lock timed out".into());
            }
            ensure_destination(destination)?;
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
            }
            let mut file = match options.clone().create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => options
                    .open(&path)
                    .map_err(|e| format!("backup lock unavailable: {}", e.kind()))?,
                Err(error) => return Err(format!("backup lock unavailable: {}", error.kind())),
            };
            let metadata = file.metadata().map_err(|e| e.to_string())?;
            if !metadata.is_file() || redirected(&metadata) {
                return Err("backup destination lock is redirected or invalid".into());
            }
            match file.try_lock() {
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(error) => return Err(format!("backup destination lock failed: {error}")),
                Ok(()) => {}
            }
            let mut marker = Vec::new();
            (&mut file)
                .take(4097)
                .read_to_end(&mut marker)
                .map_err(|e| e.to_string())?;
            if let Ok(owner) = serde_json::from_slice::<ProcessIdentity>(&marker) {
                match check_identity(&owner) {
                    Liveness::Matches => {
                        // A still-live marker is never displaced merely because it is old.
                        drop(file);
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Liveness::Unknown(_) => {
                        return Err("backup lock owner identity unavailable".into())
                    }
                    Liveness::Gone | Liveness::Mismatch(_) => {}
                }
            }
            // Empty/partial markers can remain after a crash during publication. It is
            // safe to repair them only while holding this same persistent OS lock.
            let mut guard = Self(file);
            guard.0.set_len(0).map_err(|e| e.to_string())?;
            guard
                .0
                .seek(SeekFrom::Start(0))
                .map_err(|e| e.to_string())?;
            serde_json::to_writer(&mut guard.0, &identity).map_err(|e| e.to_string())?;
            guard.0.sync_all().map_err(|e| e.to_string())?;
            return Ok(guard);
        }
    }
}
impl Drop for DestinationLock {
    fn drop(&mut self) {
        // Clear the marker while still locked; close releases the OS lock.
        let _ = self.0.set_len(0);
        let _ = self.0.sync_all();
    }
}

fn publish(candidate: &Path, destination: &Path, check: Check<'_>) -> Result<PathBuf, String> {
    let stem = archive_stem(crate::parity_clock::now_ms());
    for collision in 0u64.. {
        check()?;
        let name = if collision == 0 {
            format!("{stem}.zip")
        } else {
            format!("{stem}-{collision}.zip")
        };
        let target = destination.join(name);
        let mut output = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(file) => file,
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    || fs::symlink_metadata(&target).is_ok() =>
            {
                continue
            }
            Err(error) => return Err(format!("backup publication failed: {}", error.kind())),
        };
        // create_new is supported on SMB too; a check-then-rename can overwrite on Unix.
        let result = (|| {
            let mut input = File::open(candidate).map_err(|e| e.to_string())?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                check()?;
                let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
                if count == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..count])
                    .map_err(|e| e.to_string())?;
            }
            output.sync_all().map_err(|e| e.to_string())
        })();
        drop(output);
        if let Err(error) = result {
            let _ = fs::remove_file(&target);
            return Err(error);
        }
        return Ok(target);
    }
    Err("backup archive names exhausted".into())
}

fn unix_ms(at: std::time::SystemTime) -> i64 {
    match at.duration_since(std::time::UNIX_EPOCH) {
        Ok(age) => age.as_millis().min(i64::MAX as u128) as i64,
        Err(age) => -(age.duration().as_millis().min(i64::MAX as u128) as i64),
    }
}

fn archive_files(
    destination: &Path,
    check: Check<'_>,
) -> Result<Vec<(std::time::SystemTime, PathBuf, u64)>, String> {
    let pattern = regex::Regex::new(
        r"^qqbot-[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}-[0-9]{2}-[0-9]{2}(?:-[0-9]+)?\.zip$",
    )
    .expect("fixed backup name pattern");
    let mut archives = Vec::new();
    for entry in fs::read_dir(destination).map_err(|e| e.to_string())? {
        check()?;
        let entry = entry.map_err(|e| e.to_string())?;
        if !pattern.is_match(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if !metadata.is_file() || redirected(&metadata) {
            continue;
        }
        archives.push((
            metadata.modified().map_err(|e| e.to_string())?,
            entry.path(),
            metadata.len(),
        ));
    }
    archives.sort_by(|a, b| b.cmp(a));
    Ok(archives)
}

#[derive(Serialize)]
pub struct ArchiveView {
    name: String,
    time: i64,
    size: u64,
    result: &'static str,
}

/// Published regular HER archives with their verification result. Results are cached for
/// the process per unchanged archive, and a list stays within `LIST_BUDGET`.
pub fn list(destination: &Path, check: Check<'_>) -> Result<Vec<ArchiveView>, String> {
    list_with(destination, check, LIST_BUDGET, &verify_file)
}

/// A list stops starting verifications after this; the rest are reported "pending".
const LIST_BUDGET: Duration = Duration::from_secs(3);
const LIST_DEADLINE: &str = "backup list deadline reached";
type Verifier<'a> = &'a dyn Fn(File, Check<'_>) -> Result<Manifest, String>;
/// Destination, file name, size and modification time of an archive whose result is known.
type ListKey = (PathBuf, String, u64, i64);

fn list_cache() -> &'static std::sync::Mutex<std::collections::HashMap<ListKey, &'static str>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<ListKey, &'static str>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn list_with(
    destination: &Path,
    check: Check<'_>,
    budget: Duration,
    verify: Verifier<'_>,
) -> Result<Vec<ArchiveView>, String> {
    if !inspect_destination(destination, false)? {
        return Ok(Vec::new());
    }
    let started = Instant::now();
    let bounded = || {
        check()?;
        if started.elapsed() >= budget {
            return Err(LIST_DEADLINE.to_string());
        }
        Ok(())
    };
    // A destination still busy at the deadline is listed without its lock; archives that
    // were not verified before are then reported pending.
    let candidates = match DestinationLock::acquire(destination, &bounded) {
        Ok(lock) => {
            let files = archive_files(destination, check);
            drop(lock);
            files?
        }
        Err(error) if error == LIST_DEADLINE => archive_files(destination, check)?,
        Err(error) => return Err(error),
    };
    let mut listed = BTreeSet::new();
    let mut result = Vec::new();
    for (modified, path, size) in candidates {
        check()?;
        let name = path
            .file_name()
            .expect("archive filename")
            .to_string_lossy()
            .into_owned();
        let key: ListKey = (destination.to_path_buf(), name, size, unix_ms(modified));
        let cached = list_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .copied();
        let (key, status) = match cached {
            Some(status) => (key, status),
            None if started.elapsed() >= budget => (key, "pending"),
            None => match verify_listed(destination, &path, key.1.clone(), &bounded, check, verify)? {
                None => continue,
                Some((key, status)) if status == "pending" => (key, status),
                Some((key, status)) => {
                    list_cache()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(key.clone(), status);
                    (key, status)
                }
            },
        };
        result.push(ArchiveView {
            name: key.1.clone(),
            time: key.3,
            size: key.2,
            result: status,
        });
        listed.insert(key);
    }
    list_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|key, _| key.0 != destination || listed.contains(key));
    Ok(result)
}

/// Open one published archive under the publication lock and verify it from the open handle,
/// so hashing does not block the next backup. None: it vanished or is no longer a regular file.
fn verify_listed(
    destination: &Path,
    path: &Path,
    name: String,
    bounded: Check<'_>,
    check: Check<'_>,
    verify: Verifier<'_>,
) -> Result<Option<(ListKey, &'static str)>, String> {
    let (file, metadata) = {
        let _lock = match DestinationLock::acquire(destination, bounded) {
            Ok(lock) => lock,
            Err(error) if error == LIST_DEADLINE => {
                let metadata = match fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.is_file() && !redirected(&metadata) => metadata,
                    Ok(_) => return Ok(None),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error.to_string()),
                };
                let modified = unix_ms(metadata.modified().map_err(|e| e.to_string())?);
                let key = (destination.to_path_buf(), name, metadata.len(), modified);
                return Ok(Some((key, "pending")));
            }
            Err(error) => return Err(error),
        };
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if !metadata.is_file() || redirected(&metadata) {
            return Ok(None);
        }
        (File::open(path).map_err(|e| e.to_string())?, metadata)
    };
    let modified = unix_ms(metadata.modified().map_err(|e| e.to_string())?);
    let key = (destination.to_path_buf(), name, metadata.len(), modified);
    let status = match verify(file, bounded) {
        Ok(_) => "verified",
        Err(error) if error == LIST_DEADLINE => "pending",
        Err(error) if error == "backup manifest missing" => "unverified",
        Err(_) => {
            check()?;
            "failed"
        }
    };
    Ok(Some((key, status)))
}

fn prune(
    destination: &Path,
    latest: &Path,
    keep_days: u64,
    check: Check<'_>,
) -> Result<(), String> {
    let archives = archive_files(destination, check)?;
    let age = keep_days.saturating_mul(86_400_000).min(i64::MAX as u64) as i64;
    let cutoff = crate::parity_clock::now_ms().saturating_sub(age);
    for (_, path, _) in archives
        .into_iter()
        .filter(|(modified, path, _)| path != latest && unix_ms(*modified) < cutoff)
    {
        check()?;
        ensure_destination(destination)?;
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !metadata.is_file() || redirected(&metadata) {
            return Err("backup archive changed before pruning".into());
        }
        let pin_path = path.with_extension("zip.restore-until");
        let pinned = match fs::symlink_metadata(&pin_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("restore retention pin unavailable: {e}")),
            Ok(meta) if meta.is_file() && !redirected(&meta) => {
                let until: i64 = fs::read_to_string(&pin_path).map_err(|e| e.to_string())?.trim().parse().map_err(|_| "restore retention pin invalid")?;
                until > crate::parity_clock::now_ms()
            }
            _ => return Err("restore retention pin redirected or invalid".into()),
        };
        if pinned { continue; }
        fs::remove_file(path).map_err(|e| format!("backup retention failed: {}", e.kind()))?;
        if pin_path.exists() { fs::remove_file(pin_path).map_err(|e| e.to_string())?; }
    }
    Ok(())
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(parent: &Path) -> Result<Self, String> {
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).map_err(|_| "backup randomness unavailable")?;
        let name = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let dir = parent.join(format!(".her-backup-{name}"));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)] { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
        builder.create(&dir).map_err(|e| format!("backup staging unavailable: {}", e.kind()))?;
        Ok(Self(dir))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn run(
    context: &RuntimeContext,
    run: &mut RunHandle,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Finish {
    let started = Instant::now();
    let check = || {
        if cancel.load(Ordering::SeqCst) {
            return Err("backup cancelled".into());
        }
        if started.elapsed() >= timeout {
            return Err("backup timed out".into());
        }
        if !matches!(crate::supervisor::desired(context), crate::lifecycle::Effective::Act { wants, .. } if wants.jobs)
        {
            return Err("background jobs are held".into());
        }
        Ok(())
    };
    match create(context, &check) {
        Ok(path) => {
            run.log(
                LogStream::Meta,
                &format!(
                    "backup verified: {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
            );
            Finish::Succeeded
        }
        Err(reason) if cancel.load(Ordering::SeqCst) || reason == "background jobs are held" => {
            Finish::Cancelled { reason }
        }
        Err(reason) => Finish::Failed {
            exit_code: Some(1),
            reason,
        },
    }
}

/// One selected-runtime snapshot, verified before publication and retention.
pub fn create(context: &RuntimeContext, check: Check<'_>) -> Result<PathBuf, String> {
    create_snapshot(context, check, false)
}

/// A complete rollback snapshot, protected from ordinary retention for seven days.
pub fn create_pre_restore(context: &RuntimeContext, check: Check<'_>) -> Result<PathBuf, String> {
    create_snapshot(context, check, true)
}

/// Renew seven full days after restored health (or rollback restart), under the pruning gate.
pub fn protect_pre_restore(path: &Path, check: Check<'_>) -> Result<(), String> {
    let destination = path.parent().ok_or("pre-restore backup parent unavailable")?;
    ensure_destination(destination)?;
    let _lock = DestinationLock::acquire(destination, check)?;
    if !archive_files(destination, check)?.iter().any(|(_, candidate, _)| candidate == path) {
        return Err("pre-restore backup is no longer a regular HER archive".into());
    }
    let pin_path = path.with_extension("zip.restore-until");
    let metadata = fs::symlink_metadata(&pin_path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || redirected(&metadata) { return Err("restore retention pin redirected or invalid".into()); }
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW); }
    let mut pin = options.open(pin_path).map_err(|e| e.to_string())?;
    let until = crate::parity_clock::now_ms().saturating_add(7 * 86_400_000);
    pin.write_all(until.to_string().as_bytes()).and_then(|_| pin.sync_all()).map_err(|e| e.to_string())
}

/// Validate the optional conversation database and every sidecar SQLite may open.
/// ReadOptionalFile preserves runtimes that have never opened the harness.
pub(crate) fn conversation_db_path(context: &RuntimeContext) -> Result<PathBuf, String> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        context
            .resolve_path(
                RuntimeArea::Data,
                &format!("conversations.db{suffix}"),
                PathIntent::ReadOptionalFile,
            )
            .map_err(|_| "selected conversation database is unavailable")?;
    }
    context
        .resolve_path(
            RuntimeArea::Data,
            "conversations.db",
            PathIntent::ReadOptionalFile,
        )
        .map_err(str::to_owned)
}

fn online_snapshot(
    db: Connection,
    snapshot: &Path,
    label: &str,
    check: Check<'_>,
) -> Result<(), String> {
    db.busy_timeout(Duration::from_millis(100))
        .map_err(|e| e.to_string())?;
    let mut copy = Connection::open(snapshot).map_err(|e| e.to_string())?;
    {
        let started = Instant::now();
        let backup =
            Backup::new(&db, &mut copy).map_err(|e| format!("{label} online backup: {e}"))?;
        loop {
            check()?;
            // A live writer may keep restarting a stepped backup, even without a
            // BUSY result. Bound the whole snapshot, including successful partial steps.
            if started.elapsed() >= Duration::from_secs(30) {
                return Err(format!("{label} online backup timed out"));
            }
            let step = match backup.step(256) {
                Ok(step) => step,
                Err(error)
                    if matches!(
                        error.sqlite_error_code(),
                        Some(
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        )
                    ) =>
                {
                    StepResult::Busy
                }
                Err(error) => return Err(format!("{label} online backup: {error}")),
            };
            match step {
                StepResult::Done => break,
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {}
            }
        }
    }
    copy.close().map_err(|(_, e)| e.to_string())?;
    Ok(())
}

fn create_snapshot(
    context: &RuntimeContext,
    check: Check<'_>,
    pre_restore: bool,
) -> Result<PathBuf, String> {
    check()?;
    let root = context
        .runtime_root()
        .ok_or("backup requires a selected runtime root")?;
    let config = context.read_file(RuntimeArea::Config, "config.json")?;
    let text = std::str::from_utf8(&config).map_err(|_| "backup config is not UTF-8")?;
    let raw = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| "backup config is invalid")?;
    let settings = Settings::from_config_resolved(root, &raw)?;
    let destination = settings.destination;
    ensure_destination(&destination)?;
    let scratch = Scratch::new(&destination)?;
    online_snapshot(
        crate::memory::open_read_only_for_runtime(context)?,
        &scratch.0.join("memory.db"),
        "memory",
        check,
    )?;
    let mut databases = vec!["memory.db"];
    let conversations = conversation_db_path(context)?;
    if conversations.try_exists().map_err(|e| e.to_string())? {
        let db = Connection::open_with_flags(
            conversations,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "cannot open conversation backup source")?;
        online_snapshot(
            db,
            &scratch.0.join("conversations.db"),
            "conversation",
            check,
        )?;
        databases.push("conversations.db");
    }
    let candidate = scratch.0.join("archive.zip");
    let mut archive = ZipWriter::new(File::create(&candidate).map_err(|e| e.to_string())?);
    let mut manifest = Manifest {
        version: 1,
        files: BTreeMap::new(),
    };
    for name in databases {
        add(
            &mut archive,
            &mut manifest,
            name,
            File::open(scratch.0.join(name)).map_err(|e| e.to_string())?,
            check,
        )?;
    }
    add(
        &mut archive,
        &mut manifest,
        "config.json",
        config.as_slice(),
        check,
    )?;
    if settings.include_state || pre_restore {
        let data = context.resolve_path(RuntimeArea::Data, "", PathIntent::ReadDirectory)?;
        for entry in fs::read_dir(data).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "state filename is not UTF-8")?;
            if !name.ends_with(".json") {
                continue;
            }
            let path = context.resolve_path(RuntimeArea::Data, &name, PathIntent::ReadFile)?;
            add(
                &mut archive,
                &mut manifest,
                &format!("state/{name}"),
                File::open(path).map_err(|e| e.to_string())?,
                check,
            )?;
        }
    }
    for (directory, recursive) in [("roles", false), ("patches", true), ("docs", true)] {
        resource_tree(
            context,
            directory,
            recursive,
            &mut archive,
            &mut manifest,
            check,
        )?;
    }
    let readme = context.resolve_path(
        RuntimeArea::Resources,
        "说明.md",
        PathIntent::ReadOptionalFile,
    )?;
    match File::open(readme) {
        Ok(file) => add(&mut archive, &mut manifest, "说明.md", file, check)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("backup readme unavailable: {}", error.kind())),
    }
    archive
        .start_file("manifest.json", FileOptions::default())
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut archive, &manifest).map_err(|e| e.to_string())?;
    archive
        .finish()
        .map_err(|e| e.to_string())?
        .sync_all()
        .map_err(|e| e.to_string())?;
    verify_archive(&candidate, check)?;
    check()?;
    let _lock = DestinationLock::acquire(&destination, check)?;
    let target = publish(&candidate, &destination, check)?;
    if pre_restore {
        let until = crate::parity_clock::now_ms().saturating_add(7 * 86_400_000);
        let mut pin = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target.with_extension("zip.restore-until"))
            .map_err(|e| format!("pre-restore retention pin failed: {e}"))?;
        pin.write_all(until.to_string().as_bytes())
            .and_then(|_| pin.sync_all())
            .map_err(|e| e.to_string())?;
        // A restore must never prune its selected input before it has completed.
        return Ok(target);
    }
    prune(&destination, &target, settings.keep_days, check)?;
    Ok(target)
}

fn add(
    archive: &mut ZipWriter<File>,
    manifest: &mut Manifest,
    name: &str,
    mut input: impl Read,
    check: Check<'_>,
) -> Result<(), String> {
    archive
        .start_file(name, FileOptions::default().large_file(true))
        .map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut size = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check()?;
        let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        archive
            .write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
        size += count as u64;
    }
    manifest.files.insert(
        name.to_string(),
        FileDigest {
            size,
            sha256: format!("{:x}", digest.finalize()),
        },
    );
    Ok(())
}

fn resource_tree(
    context: &RuntimeContext,
    relative: &str,
    recursive: bool,
    archive: &mut ZipWriter<File>,
    manifest: &mut Manifest,
    check: Check<'_>,
) -> Result<(), String> {
    check()?;
    let base = context.resolve_path(RuntimeArea::Resources, "", PathIntent::ReadDirectory)?;
    match fs::symlink_metadata(base.join(relative)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
        Ok(_) => {}
    }
    let directory =
        context.resolve_path(RuntimeArea::Resources, relative, PathIntent::ReadDirectory)?;
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        check()?;
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "resource filename is not UTF-8")?;
        let relative = format!("{relative}/{name}");
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            // Resolve even skipped role subdirectories so redirected descendants never get followed.
            context.resolve_path(RuntimeArea::Resources, &relative, PathIntent::ReadDirectory)?;
            if recursive {
                resource_tree(context, &relative, true, archive, manifest, check)?;
            }
        } else {
            let path =
                context.resolve_path(RuntimeArea::Resources, &relative, PathIntent::ReadFile)?;
            add(
                archive,
                manifest,
                &relative,
                File::open(path).map_err(|e| e.to_string())?,
                check,
            )?;
        }
    }
    Ok(())
}

/// Reopen, hash every payload and integrity-check the extracted database. Never extracts archive paths.
pub fn verify_archive(path: &Path, check: Check<'_>) -> Result<Manifest, String> {
    check()?;
    verify_file(File::open(path).map_err(|e| e.to_string())?, check)
}

fn verify_file(file: File, check: Check<'_>) -> Result<Manifest, String> {
    let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;
    let manifest: Manifest = {
        let file = archive
            .by_name("manifest.json")
            .map_err(|_| "backup manifest missing")?;
        serde_json::from_reader(file).map_err(|e| format!("backup manifest invalid: {e}"))?
    };
    if manifest.version != 1
        || !manifest.files.contains_key("memory.db")
        || !manifest.files.contains_key("config.json")
    {
        return Err("backup manifest incomplete or unsupported".into());
    }
    let scratch = Scratch::new(&std::env::temp_dir())?;
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        check()?;
        let mut file = archive.by_index(index).map_err(|e| e.to_string())?;
        let name = file.name().to_string();
        if !safe_name(&name) || !seen.insert(name.clone()) {
            return Err("backup contains unsafe or duplicate paths".into());
        }
        if name == "manifest.json" {
            continue;
        }
        let expected = manifest
            .files
            .get(&name)
            .ok_or("backup file is not in manifest")?;
        if file.size() != expected.size {
            return Err(format!("backup size mismatch: {name}"));
        }
        let mut db = if matches!(name.as_str(), "memory.db" | "conversations.db") {
            Some(File::create(scratch.0.join(&name)).map_err(|e| e.to_string())?)
        } else {
            None
        };
        let mut hash = Sha256::new();
        let mut size = 0;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            check()?;
            let count = file
                .read(&mut buffer)
                .map_err(|e| format!("backup ZIP read failed: {e}"))?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            size += count as u64;
            if let Some(db) = &mut db {
                db.write_all(&buffer[..count]).map_err(|e| e.to_string())?;
            }
        }
        if size != expected.size || format!("{:x}", hash.finalize()) != expected.sha256 {
            return Err(format!("backup hash mismatch: {name}"));
        }
    }
    if seen.len() != manifest.files.len() + 1 {
        return Err("backup files missing from ZIP".into());
    }
    for name in ["memory.db", "conversations.db"] {
        if !manifest.files.contains_key(name) {
            continue;
        }
        let db = Connection::open_with_flags(
            scratch.0.join(name),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("backup integrity check ({name}): {e}"))?;
        let mut statement = db
            .prepare("PRAGMA integrity_check")
            .map_err(|e| format!("backup integrity check ({name}): {e}"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("backup integrity check ({name}): {e}"))?;
        let mut ok = false;
        for result in rows {
            check()?;
            if result.map_err(|e| format!("backup integrity check ({name}): {e}"))? != "ok" {
                return Err("backup database integrity check failed".into());
            }
            ok = true;
        }
        if !ok {
            return Err("backup database integrity check returned no result".into());
        }
    }
    check()?;
    Ok(manifest)
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['\\', ':'])
        && name
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

fn archive_stem(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    // Gregorian civil date from Unix days (Howard Hinnant's civil_from_days).
    let z = seconds.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year + i64::from(month <= 2);
    let time = seconds.rem_euclid(86400);
    format!(
        "qqbot-{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use crate::runtime::RuntimeContext;
    use crate::task_runs::RunState;
    use crate::tasks::{TaskId, Trigger};
    use rusqlite::Connection;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, Instant},
    };

    struct Fixture(PathBuf);
    #[test]
    fn backup_daily_schedule_refresh_preserves_due_slots_and_applies_changed_time() {
        use crate::tasks::{Schedule, Scheduler};
        let mut scheduler = Scheduler::new(|def| def.id == TaskId::Backup, 0, 0);
        let config = json!({"backup":{"time":"06:30"}});
        super::refresh_schedule(&config, &mut scheduler, 0).unwrap();
        assert_eq!(
            scheduler.schedule(TaskId::Backup),
            Some(Schedule::Daily {
                hour: 6,
                minute: 30
            })
        );
        assert_eq!(scheduler.next_run(TaskId::Backup), Some(23_400_000));
        super::refresh_schedule(&config, &mut scheduler, 23_400_000).unwrap();
        let due = scheduler.tick(23_400_000);
        assert_eq!(
            due.len(),
            1,
            "an unchanged refresh must not consume a due slot"
        );
        assert_eq!(due[0].task, TaskId::Backup);
        assert_eq!(due[0].slot, 23_400_000);
        assert!(scheduler.tick(23_400_000).is_empty());
        super::refresh_schedule(
            &json!({"backup":{"time":"07:00"}}),
            &mut scheduler,
            23_400_001,
        )
        .unwrap();
        assert_eq!(scheduler.next_run(TaskId::Backup), Some(25_200_000));
        scheduler.set_enabled(TaskId::Backup, false, 23_400_002);
        super::refresh_schedule(
            &json!({"backup":{"time":"08:00"}}),
            &mut scheduler,
            23_400_002,
        )
        .unwrap();
        assert_eq!(scheduler.next_run(TaskId::Backup), None);
        scheduler.set_enabled(TaskId::Backup, true, 23_400_003);
        assert_eq!(scheduler.next_run(TaskId::Backup), Some(28_800_000));
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-backup-{name}-{}-{}",
                std::process::id(),
                crate::parity_clock::now_ms()
            )));
            for path in [
                "config",
                "data",
                "resources/roles",
                "resources/patches/nested",
                "resources/docs",
            ] {
                fs::create_dir_all(root.join(path)).unwrap();
            }
            fs::write(root.join("config/config.json"), serde_json::to_vec(&json!({"consoleToken":"backup-fixture", "backup":{}, "tasks":{"nodePath":root.join("missing-node"),"installRoot":root}})).unwrap()).unwrap();
            fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
            Self(root)
        }
        fn context(&self) -> RuntimeContext {
            RuntimeContext::from_root(&self.0).unwrap()
        }
        fn run(&self, request: &str) -> crate::task_runs::RunView {
            self.run_task(crate::tasks::task_def(TaskId::Backup), request)
        }
        fn run_task(
            &self,
            def: &'static crate::tasks::TaskDef,
            request: &str,
        ) -> crate::task_runs::RunView {
            let context = self.context();
            let api =
                crate::supervisor::task_api(&context, &context.load_config().unwrap()).unwrap();
            let Trigger::Started(run) = api.dispatcher.start(
                def,
                crate::task_runs::RunSource::Manual,
                Some(request),
                None,
                "fixture",
            ) else {
                panic!("backup refused")
            };
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let view = api.dispatcher.store().get(&run.run_id).unwrap().unwrap();
                if view.state.is_terminal() {
                    return view;
                }
                assert!(Instant::now() < deadline, "backup did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn backup_retention_keeps_all_recent_archives_and_preserves_an_aged_verified_backup_on_failure()
    {
        let fixture = Fixture::new("age-retention");
        Connection::open(fixture.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        let destination = fixture.0.join("backups");
        fs::create_dir(&destination).unwrap();
        let now = std::time::UNIX_EPOCH
            + Duration::from_millis(crate::parity_clock::now_ms().max(0) as u64);
        let mut recent = Vec::new();
        for day in 1..=11 {
            let path = destination.join(format!("qqbot-2026-01-{day:02}T04-00-00.zip"));
            let file = fs::File::create(&path).unwrap();
            file.set_modified(now - Duration::from_secs(6 * 86_400))
                .unwrap();
            recent.push(path);
        }
        let expired = destination.join("qqbot-2020-01-01T04-00-00.zip");
        fs::File::create(&expired)
            .unwrap()
            .set_modified(now - Duration::from_secs(8 * 86_400))
            .unwrap();
        let unrelated = destination.join("personal.zip");
        let mut personal = fs::File::create(&unrelated).unwrap();
        use std::io::Write;
        personal.write_all(b"unrelated archive").unwrap();
        personal
            .set_modified(now - Duration::from_secs(8 * 86_400))
            .unwrap();
        drop(personal);
        let context = fixture.context();
        let newest = super::create(&context, &|| Ok(())).unwrap();
        assert!(
            recent.iter().all(|path| path.is_file()),
            "retention is by age, not an archive count"
        );
        assert!(!expired.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated archive");
        fs::OpenOptions::new()
            .write(true)
            .open(&newest)
            .unwrap()
            .set_modified(now - Duration::from_secs(8 * 86_400))
            .unwrap();
        fs::write(fixture.0.join("data/memory.db"), b"corrupt database").unwrap();
        assert!(super::create(&context, &|| Ok(())).is_err());
        super::verify_archive(&newest, &|| Ok(()))
            .expect("failed backup must preserve even an aged verified archive");
    }

    #[test]
    fn concurrent_backups_share_destination_and_keep_recent_verified_archives() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        let fixture = Fixture::new("concurrent");
        Connection::open(fixture.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        let config_path = fixture.0.join("config/config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["backup"] = json!({"keepDays":1});
        fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let destination = fixture.0.join("backups");
        fs::create_dir(&destination).unwrap();
        let other = Fixture::new("concurrent-other-runtime");
        Connection::open(other.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        let other_config = other.0.join("config/config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(&other_config).unwrap()).unwrap();
        config["backup"] = json!({"dir":destination, "keepDays":1});
        fs::write(other_config, serde_json::to_vec(&config).unwrap()).unwrap();
        fs::write(destination.join("qqbot-2020-01-01T00-00-00.zip"), "old").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(destination.join("qqbot-2020-01-01T00-00-00.zip"))
            .unwrap()
            .set_modified(
                std::time::UNIX_EPOCH
                    + Duration::from_millis(crate::parity_clock::now_ms().max(0) as u64)
                    - Duration::from_secs(8 * 86_400),
            )
            .unwrap();
        fs::write(destination.join("personal.zip"), "unrelated").unwrap();
        let (published_tx, published_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let first_context = fixture.context();
            let first_destination = &destination;
            let first = scope.spawn(move || {
                let paused = AtomicBool::new(false);
                super::create(&first_context, &|| {
                    let archives = fs::read_dir(first_destination)
                        .unwrap()
                        .filter_map(Result::ok)
                        .filter(|e| e.file_name().to_string_lossy().starts_with("qqbot-"))
                        .count();
                    if archives >= 2 && !paused.swap(true, Ordering::SeqCst) {
                        published_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                    Ok(())
                })
            });
            published_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let second = scope.spawn(|| {
                second_tx
                    .send(super::create(&other.context(), &|| Ok(())))
                    .unwrap();
            });
            let early = second_rx.recv_timeout(Duration::from_millis(500)).ok();
            release_tx.send(()).unwrap();
            let first = first.join().unwrap();
            second.join().unwrap();
            let second = early.unwrap_or_else(|| second_rx.recv().unwrap());
            assert!(first.is_ok(), "first backup: {first:?}");
            assert!(second.is_ok(), "second backup: {second:?}");
        });
        let archives: Vec<_> = fs::read_dir(&destination)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("qqbot-")
            })
            .collect();
        assert_eq!(
            archives.len(),
            2,
            "concurrent retention must keep both recent archives"
        );
        for archive in &archives {
            super::verify_archive(archive, &|| Ok(())).unwrap();
        }
        assert_eq!(
            fs::read_to_string(destination.join("personal.zip")).unwrap(),
            "unrelated"
        );
    }

    #[test]
    fn backup_refuses_redirected_sqlite_sidecars() {
        let fixture = Fixture::new("redirected-sidecars");
        let outside = Fixture::new("sidecar-outside");
        let sentinel = outside.0.join("sentinel");
        fs::write(&sentinel, b"outside must remain unchanged").unwrap();
        Connection::open(fixture.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        Connection::open(fixture.0.join("data/conversations.db"))
            .unwrap()
            .execute_batch("CREATE TABLE turns(content TEXT);")
            .unwrap();
        for database in ["memory.db", "conversations.db"] {
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = fixture.0.join(format!("data/{database}{suffix}"));
                #[cfg(windows)]
                {
                    let output = crate::subprocess::command_without_console("cmd.exe")
                        .args(["/d", "/c", "mklink", "/J"])
                        .arg(sidecar.to_str().unwrap().replace('/', "\\"))
                        .arg(outside.0.to_str().unwrap().replace('/', "\\"))
                        .output()
                        .expect("junction helper must start and be reaped");
                    if !output.status.success() {
                        eprintln!(
                            "SKIP: cannot create unelevated sidecar junction {suffix}: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        return;
                    }
                }
                #[cfg(unix)]
                if let Err(error) = std::os::unix::fs::symlink(&sentinel, &sidecar) {
                    eprintln!("SKIP: cannot create sidecar symlink {suffix}: {error}");
                    return;
                }
                let result = super::create(&fixture.context(), &|| Ok(()));
                #[cfg(windows)]
                fs::remove_dir(&sidecar).unwrap();
                #[cfg(unix)]
                fs::remove_file(&sidecar).unwrap();
                let error = result.expect_err("backup must refuse redirected SQLite sidecars");
                assert!(
                    error.contains(if database == "memory.db" {
                        "selected memory database is unavailable"
                    } else {
                        "selected conversation database is unavailable"
                    }),
                    "{suffix}: {error}"
                );
                assert_eq!(
                    fs::read(&sentinel).unwrap(),
                    b"outside must remain unchanged"
                );
                eprintln!("redirected sidecar refused: {suffix}");
            }
        }
    }

    #[test]
    fn backup_retries_a_concurrent_writer_and_bounds_snapshot_time() {
        use std::{cell::Cell, sync::mpsc};
        let fixture = Fixture::new("concurrent-writer");
        let writer = Connection::open(fixture.0.join("data/memory.db")).unwrap();
        writer.busy_timeout(Duration::from_secs(5)).unwrap();
        writer.execute_batch("CREATE TABLE memory(content TEXT); INSERT INTO memory VALUES('before'); BEGIN EXCLUSIVE; INSERT INTO memory VALUES('writer commit');").unwrap();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            attempt_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            writer.execute_batch("COMMIT;").unwrap();
            writer
        });
        let checks = Cell::new(0);
        let path = super::create(&fixture.context(), &|| {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                attempt_tx.send(()).unwrap();
            }
            Ok(())
        });
        let writer = writer.join().unwrap();
        let path = path.expect("backup must recover after the concurrent writer commits");
        super::verify_archive(&path, &|| Ok(())).unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        let snapshot = fixture.0.join("writer-snapshot.db");
        std::io::copy(
            &mut archive.by_name("memory.db").unwrap(),
            &mut fs::File::create(&snapshot).unwrap(),
        )
        .unwrap();
        let copy = Connection::open(snapshot).unwrap();
        assert_eq!(
            copy.query_row(
                "SELECT count(*) FROM memory WHERE content='writer commit'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        drop((copy, archive));

        // Direct callers may have no cancellation deadline. A writer that never lets
        // readers in must still produce a bounded failure, preserving the good archive.
        writer
            .execute_batch("BEGIN EXCLUSIVE; INSERT INTO memory VALUES('never committed');")
            .unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let blocked_writer = std::thread::spawn(move || {
            let _ = release_rx.recv_timeout(Duration::from_secs(32));
            writer.execute_batch("ROLLBACK;").unwrap();
        });
        let started = Instant::now();
        let result = super::create(&fixture.context(), &|| Ok(()));
        let elapsed = started.elapsed();
        let _ = release_tx.send(());
        blocked_writer.join().unwrap();
        let error = result.expect_err("an indefinitely busy source must time out");
        assert!(error.contains("memory online backup timed out"), "{error}");
        assert!(
            elapsed < Duration::from_secs(32),
            "snapshot deadline was not enforced: {elapsed:?}"
        );
        assert_eq!(
            fs::read_dir(fixture.0.join("backups"))
                .unwrap()
                .filter(|e| e.as_ref().unwrap().file_name() != super::DESTINATION_LOCK)
                .count(),
            1
        );
    }

    #[test]
    fn backup_job_archives_selected_runtime_and_committed_wal_without_node() {
        let fixture = Fixture::new("selected's-runtime");
        let db = Connection::open(fixture.0.join("data/memory.db")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE memory(content TEXT); INSERT INTO memory VALUES('committed in WAL');").unwrap();
        let conversations = Connection::open(fixture.0.join("data/conversations.db")).unwrap();
        conversations.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=0; CREATE TABLE turns(content TEXT); INSERT INTO turns VALUES('conversation committed in WAL');").unwrap();
        assert!(
            fs::metadata(fixture.0.join("data/conversations.db-wal"))
                .unwrap()
                .len()
                > 0
        );
        fs::write(fixture.0.join("resources/roles/main.md"), "selected role").unwrap();
        fs::write(fixture.0.join("resources/patches/nested/fix.txt"), "patch").unwrap();
        fs::write(fixture.0.join("resources/docs/guide.md"), "guide").unwrap();
        fs::write(fixture.0.join("resources/说明.md"), "readme").unwrap();
        fs::write(fixture.0.join("data/ignored.jsonl"), "not state").unwrap();
        let done = fixture.run("backup-selected-01");
        assert_eq!(done.state, RunState::Succeeded, "{done:?}");
        assert!(done.pid.is_none(), "backup is an in-process Rust action");
        let archives: Vec<_> = fs::read_dir(fixture.0.join("backups"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap() != super::DESTINATION_LOCK)
            .collect();
        assert_eq!(archives.len(), 1);
        let mut archive = zip::ZipArchive::new(fs::File::open(&archives[0]).unwrap()).unwrap();
        let mut names: Vec<_> = archive.file_names().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "config.json",
                "conversations.db",
                "docs/guide.md",
                "manifest.json",
                "memory.db",
                "patches/nested/fix.txt",
                "roles/main.md",
                "state/social-v2.json",
                "说明.md"
            ]
        );
        let snapshot = fixture.0.join("snapshot.db");
        std::io::copy(
            &mut archive.by_name("memory.db").unwrap(),
            &mut fs::File::create(&snapshot).unwrap(),
        )
        .unwrap();
        let copy = Connection::open(snapshot).unwrap();
        assert_eq!(
            copy.query_row("SELECT content FROM memory", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "committed in WAL"
        );
        assert_eq!(
            copy.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        let snapshot = fixture.0.join("conversation-snapshot.db");
        std::io::copy(
            &mut archive.by_name("conversations.db").unwrap(),
            &mut fs::File::create(&snapshot).unwrap(),
        )
        .unwrap();
        let copy = Connection::open(snapshot).unwrap();
        assert_eq!(
            copy.query_row("SELECT content FROM turns", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "conversation committed in WAL"
        );
        assert!(super::verify_archive(&archives[0], &|| Ok(()))
            .unwrap()
            .files
            .contains_key("conversations.db"));
    }

    #[test]
    fn backup_settings_select_destination_state_and_prune_only_legacy_archives() {
        let fixture = Fixture::new("settings");
        Connection::open(fixture.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        let destination = fixture.0.join("backup's destination");
        fs::create_dir(&destination).unwrap();
        let old = [
            "qqbot-2026-01-01T04-00-00.zip",
            "qqbot-2026-01-02T04-00-00.zip",
            "qqbot-2026-01-03T04-00-00.zip",
        ];
        let now = std::time::UNIX_EPOCH
            + Duration::from_millis(crate::parity_clock::now_ms().max(0) as u64);
        for (i, name) in old.iter().enumerate() {
            let file = fs::File::create(destination.join(name)).unwrap();
            file.set_modified(now - Duration::from_secs([8, 3, 1][i] * 86_400))
                .unwrap();
        }
        fs::write(destination.join("qqbot-personal.zip"), "unrelated").unwrap();
        fs::create_dir(destination.join("qqbot-2026-01-04T04-00-00.zip")).unwrap();
        let config_path = fixture.0.join("config/config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["backup"] =
            json!({"dir":"backup's destination", "keepDays":2, "includeState":false});
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let done = fixture.run("backup-settings-01");
        assert_eq!(done.state, RunState::Succeeded, "{done:?}");
        assert!(
            !destination.join(old[0]).exists(),
            "configured destination must be pruned"
        );
        assert!(!destination.join(old[1]).exists());
        assert!(
            destination.join(old[2]).is_file(),
            "the retained legacy archive keeps its name"
        );
        assert!(destination.join("qqbot-personal.zip").exists());
        assert!(destination.join("qqbot-2026-01-04T04-00-00.zip").is_dir());
        let new = fs::read_dir(&destination)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.is_file()
                    && p.file_name().unwrap() != old[2]
                    && p.file_name().unwrap() != "qqbot-personal.zip"
                    && p.file_name().unwrap() != super::DESTINATION_LOCK
            })
            .unwrap();
        let archive = zip::ZipArchive::new(fs::File::open(new).unwrap()).unwrap();
        assert!(!archive.file_names().any(|name| name.starts_with("state/")));
        assert!(
            !fixture.0.join("backups").exists(),
            "no write to the default destination"
        );
        drop(archive);

        let absolute = fixture.0.join("absolute destination");
        config["backup"] = json!({"dir":absolute, "keepDays":1});
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(fixture.run("backup-settings-02").state, RunState::Succeeded);
        assert_eq!(
            fs::read_dir(absolute)
                .unwrap()
                .filter(|e| e.as_ref().unwrap().file_name() != super::DESTINATION_LOCK)
                .count(),
            1
        );
    }

    #[test]
    fn verification_rejects_hash_mismatch_missing_extra_duplicate_and_corrupt_database() {
        use super::verify_archive;
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        let fixture = Fixture::new("verification");
        Connection::open(fixture.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        Connection::open(fixture.0.join("data/conversations.db"))
            .unwrap()
            .execute_batch("CREATE TABLE turns(content TEXT);")
            .unwrap();
        let path = super::create(&fixture.context(), &|| Ok(())).unwrap();
        let manifest = verify_archive(&path, &|| Ok(())).unwrap();
        assert_eq!(manifest.version, 1);
        assert_eq!(
            manifest.files["state/social-v2.json"].sha256,
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        let mut archive = zip::ZipArchive::new(fs::File::open(&path).unwrap()).unwrap();
        let original: Vec<_> = (0..archive.len())
            .map(|i| {
                let mut file = archive.by_index(i).unwrap();
                let name = file.name().to_string();
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                (name, bytes)
            })
            .collect();
        drop(archive);
        for case in [
            "hash",
            "missing",
            "extra",
            "duplicate",
            "database",
            "conversation-database",
            "manifest",
            "traversal",
        ] {
            let mut files = original.clone();
            match case {
                "hash" => {
                    files
                        .iter_mut()
                        .find(|(n, _)| n == "config.json")
                        .unwrap()
                        .1[0] ^= 1
                }
                "missing" => files.retain(|(n, _)| n != "config.json"),
                "extra" => files.push(("extra.txt".into(), b"unexpected".to_vec())),
                "duplicate" => files.push(
                    files
                        .iter()
                        .find(|(n, _)| n == "config.json")
                        .unwrap()
                        .clone(),
                ),
                "database" | "conversation-database" => {
                    let name = if case == "database" {
                        "memory.db"
                    } else {
                        "conversations.db"
                    };
                    let bytes = b"not a SQLite database".to_vec();
                    files.iter_mut().find(|(n, _)| n == name).unwrap().1 = bytes.clone();
                    let (_, raw) = files
                        .iter_mut()
                        .find(|(n, _)| n == "manifest.json")
                        .unwrap();
                    let mut value: serde_json::Value = serde_json::from_slice(raw).unwrap();
                    value["files"][name] =
                        json!({"size":bytes.len(), "sha256":format!("{:x}", Sha256::digest(&bytes))});
                    *raw = serde_json::to_vec(&value).unwrap();
                }
                "manifest" => files.retain(|(n, _)| n != "manifest.json"),
                "traversal" => files.push(("../escaped.txt".into(), b"unsafe".to_vec())),
                _ => unreachable!(),
            }
            let damaged = fixture.0.join(format!("damaged-{case}.zip"));
            let mut writer = zip::ZipWriter::new(fs::File::create(&damaged).unwrap());
            for (name, bytes) in files {
                writer
                    .start_file(name, zip::write::FileOptions::default())
                    .unwrap();
                writer.write_all(&bytes).unwrap();
            }
            writer.finish().unwrap();
            let error = verify_archive(&damaged, &|| Ok(())).expect_err(case);
            if case == "hash" {
                assert!(error.contains("hash mismatch"), "{error}");
            }
            if case == "database" || case == "conversation-database" {
                assert!(error.contains("integrity check"), "{error}");
            }
        }
        assert!(!fixture.0.join("escaped.txt").exists());
    }

    #[test]
    fn failed_or_cancelled_backup_keeps_existing_archives_and_cleans_staging() {
        let fixture = Fixture::new("failure");
        fs::create_dir(fixture.0.join("backups")).unwrap();
        let previous = fixture.0.join("backups/qqbot-2026-01-01T04-00-00.zip");
        fs::write(&previous, "previous successful archive sentinel").unwrap();
        fs::write(fixture.0.join("data/memory.db"), "corrupt database").unwrap();
        let done = fixture.run("backup-corrupt-01");
        assert_eq!(
            (done.state, done.exit_code),
            (RunState::Failed, Some(1)),
            "{done:?}"
        );
        assert!(
            done.reason
                .as_deref()
                .unwrap()
                .contains("memory online backup"),
            "{done:?}"
        );
        assert_eq!(
            fs::read_to_string(previous).unwrap(),
            "previous successful archive sentinel"
        );
        assert_eq!(fs::read_dir(fixture.0.join("backups")).unwrap().count(), 1);
        assert!(
            super::create(&fixture.context(), &|| Err("cancelled by caller".into()))
                .unwrap_err()
                .contains("cancelled")
        );
        assert_eq!(fs::read_dir(fixture.0.join("backups")).unwrap().count(), 1);
    }

    #[test]
    fn backup_preserves_utf16_readme_bytes() {
        use std::io::Read;
        let fixture = Fixture::new("utf16-readme");
        Connection::open(fixture.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        let bytes = [0xff, 0xfe, b'R', 0, b'\r', 0, b'\n', 0];
        fs::write(fixture.0.join("resources/说明.md"), bytes).unwrap();
        let path = super::create(&fixture.context(), &|| Ok(())).unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        let mut restored = Vec::new();
        archive
            .by_name("说明.md")
            .unwrap()
            .read_to_end(&mut restored)
            .unwrap();
        assert_eq!(restored, bytes);
    }

    #[test]
    fn backup_job_timeout_is_recorded_without_publishing_an_archive() {
        static EXPIRED: crate::tasks::TaskDef = crate::tasks::TaskDef {
            timeout: Duration::ZERO,
            ..crate::tasks::REGISTRY[3]
        };
        let fixture = Fixture::new("timeout");
        assert_eq!(EXPIRED.id, TaskId::Backup);
        let done = fixture.run_task(&EXPIRED, "backup-timeout-01");
        assert_eq!((done.state, done.exit_code), (RunState::Failed, Some(1)));
        assert!(done.reason.unwrap().contains("timed out"));
        assert!(!fixture.0.join("backups").exists());
    }

    fn claimed_notices(fixture: &Fixture) -> std::collections::BTreeSet<String> {
        match fs::read(fixture.0.join("data/backup-notices.json")) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["seen"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(error) => panic!("notice state unreadable: {error}"),
        }
    }

    fn record_run(
        store: &crate::task_runs::TaskStore,
        source: crate::task_runs::RunSource,
        request: &str,
        at: i64,
        finish: crate::task_runs::Finish,
    ) -> String {
        let crate::task_runs::Claim::Started(run) =
            store.claim("backup", source, Some(request), None, "fixture", at)
        else {
            panic!("fixture run refused")
        };
        let id = run.run_id().to_owned();
        run.finish(finish, at + 1).unwrap();
        id
    }

    #[test]
    fn backup_notice_first_observation_starts_now_and_later_occurrences_alert_once() {
        use crate::task_runs::{Finish, RunSource};
        let fixture = Fixture::new("first-observation");
        let config_path = fixture.0.join("config/config.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["backup"] = json!({"time":"04:00"});
        config["tasks"]["backup"] = json!({"enabled":true});
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let now = crate::parity_clock::now_ms();
        let days = |n: u64| Duration::from_secs(n * 86_400);
        let wall = std::time::UNIX_EPOCH + Duration::from_millis(now as u64);
        fs::File::options()
            .write(true)
            .open(&config_path)
            .unwrap()
            .set_modified(wall - days(9))
            .unwrap();
        let context = fixture.context();
        let config = context.load_config().unwrap();
        let api = crate::supervisor::task_api(&context, &config).unwrap();
        let store = api.dispatcher.store();
        let failed = |reason: &str| Finish::Failed {
            exit_code: Some(1),
            reason: reason.into(),
        };
        // History from before this supervisor ever observed notices (older build, lost state).
        record_run(store, RunSource::Manual, "old-failed-01", now - 3 * 86_400_000, failed("old"));
        record_run(store, RunSource::Manual, "old-unknown-02", now - 2 * 86_400_000,
            Finish::Unknown { reason: "old".into() });
        record_run(store, RunSource::Scheduled, "old-cancel-03", now - 86_400_000,
            Finish::Cancelled { reason: "old".into() });

        super::monitor_notices(&context, &config, store, now).unwrap();
        assert!(
            claimed_notices(&fixture).is_empty(),
            "first observation must not alert old failures or slots since config mtime: {:?}",
            claimed_notices(&fixture)
        );
        super::monitor_notices(&context, &config, store, now + 30_000).unwrap();
        assert!(claimed_notices(&fixture).is_empty());

        let later = record_run(store, RunSource::Recovery, "later-unknown-04", now + 60_000,
            Finish::Unknown { reason: "lost process".into() });
        super::monitor_notices(&context, &config, store, now + 120_000).unwrap();
        let expected: std::collections::BTreeSet<_> = [format!("backup-failed-{later}")].into();
        assert_eq!(claimed_notices(&fixture), expected, "a later failure alerts once");
        super::monitor_notices(&context, &config, store, now + 180_000).unwrap();
        assert_eq!(claimed_notices(&fixture), expected, "and only once");

        let schedule = super::daily_schedule(&config).unwrap();
        let slot = crate::tasks::next_due(schedule, now + 180_000, crate::tasks::local_offset_ms())
            .unwrap();
        super::monitor_notices(&context, &config, store, slot + 61_000).unwrap();
        let mut expected = expected;
        expected.insert(format!("backup-missed-{slot}"));
        assert_eq!(claimed_notices(&fixture), expected, "a later missed slot alerts once");
        super::monitor_notices(&context, &config, store, slot + 3_600_000).unwrap();
        assert_eq!(claimed_notices(&fixture), expected, "and only once");
    }

    #[test]
    fn backup_list_caches_verification_and_reports_unverified_archives_after_its_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fixture = Fixture::new("list-cache");
        Connection::open(fixture.0.join("data/memory.db"))
            .unwrap()
            .execute_batch("CREATE TABLE memory(content TEXT);")
            .unwrap();
        let context = fixture.context();
        let first = super::create(&context, &|| Ok(())).unwrap();
        let second = super::create(&context, &|| Ok(())).unwrap();
        let destination = fixture.0.join("backups");
        let verified = AtomicUsize::new(0);
        let counting = |file: fs::File, check: super::Check<'_>| {
            verified.fetch_add(1, Ordering::SeqCst);
            super::verify_file(file, check)
        };
        let results = |views: &[super::ArchiveView]| {
            views
                .iter()
                .map(|view| (view.name.clone(), view.result))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let name = |path: &std::path::Path| path.file_name().unwrap().to_str().unwrap().to_owned();
        let budget = Duration::from_secs(20);
        let listed = super::list_with(&destination, &|| Ok(()), budget, &counting).unwrap();
        let all_verified: std::collections::BTreeMap<_, _> =
            [(name(&first), "verified"), (name(&second), "verified")].into();
        assert_eq!(results(&listed), all_verified);
        assert_eq!(verified.load(Ordering::SeqCst), 2);
        let listed = super::list_with(&destination, &|| Ok(()), budget, &counting).unwrap();
        assert_eq!(results(&listed), all_verified);
        assert_eq!(
            verified.load(Ordering::SeqCst),
            2,
            "unchanged archives are not verified again"
        );
        let modified = fs::metadata(&second).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(&second)
            .unwrap()
            .set_modified(modified - Duration::from_secs(60))
            .unwrap();
        super::list_with(&destination, &|| Ok(()), budget, &counting).unwrap();
        assert_eq!(verified.load(Ordering::SeqCst), 3, "a changed archive is verified again");

        let third = super::create(&context, &|| Ok(())).unwrap();
        let fourth = super::create(&context, &|| Ok(())).unwrap();
        let slow = |_: fs::File, check: super::Check<'_>| -> Result<super::Manifest, String> {
            verified.fetch_add(1, Ordering::SeqCst);
            loop {
                check()?;
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let started = Instant::now();
        let listed =
            super::list_with(&destination, &|| Ok(()), Duration::from_millis(200), &slow).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "listing must return at its deadline: {:?}",
            started.elapsed()
        );
        let mut expected = all_verified.clone();
        expected.insert(name(&third), "pending");
        expected.insert(name(&fourth), "pending");
        assert_eq!(results(&listed), expected, "cached results stay, the rest are not verified yet");
        assert_eq!(verified.load(Ordering::SeqCst), 4, "no verification starts after the deadline");
        let listed = super::list_with(&destination, &|| Ok(()), budget, &counting).unwrap();
        expected.insert(name(&third), "verified");
        expected.insert(name(&fourth), "verified");
        assert_eq!(results(&listed), expected, "an interrupted verification is not cached");
    }

    #[cfg(windows)]
    #[test]
    fn backup_settings_preserve_unc_destination_without_contacting_a_share() {
        let fixture = Fixture::new("unc-settings");
        let unc = PathBuf::from(r"\\backup-fixture.invalid\share\HER backups");
        let settings =
            super::Settings::from_config(&fixture.0, &json!({"backup":{"dir":unc}})).unwrap();
        assert_eq!(settings.destination, unc);
        assert_eq!(settings.keep_days, 7);
        assert!(settings.include_state);
    }
}
