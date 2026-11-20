//! Selected-runtime backup creation and verification.

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use crate::task_runs::{Finish, LogStream, RunHandle};
use crate::tasks::{TaskDef, TaskId};
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

#[derive(Clone, Copy)]
enum Verification {
    FullBackup,
    MemoryBackup,
    Restore { memory_only: bool },
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_fingerprint: Option<MemoryFingerprint>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryFingerprint {
    schema_sha256: String,
    table_rows: BTreeMap<String, i64>,
}

fn memory_fingerprint(db: &Connection, check: Check<'_>) -> Result<MemoryFingerprint, String> {
    let version: i64 = db
        .query_row("PRAGMA main.user_version", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let mut statement = db
        .prepare("SELECT type, name, tbl_name, sql FROM main.sqlite_schema ORDER BY type, name")
        .map_err(|e| e.to_string())?;
    let entries = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut schema = Sha256::new();
    schema.update(version.to_le_bytes());
    let mut table_rows = BTreeMap::new();
    for entry in entries {
        check()?;
        let entry = entry.map_err(|e| e.to_string())?;
        schema.update(serde_json::to_vec(&entry).map_err(|e| e.to_string())?);
        schema.update(b"\n");
        let (kind, name, _, _) = entry;
        if kind == "table" {
            let quoted = name.replace('"', "\"\"");
            let count = db
                .query_row(&format!("SELECT COUNT(*) FROM main.\"{quoted}\""), [], |row| {
                    row.get(0)
                })
                .map_err(|e| e.to_string())?;
            table_rows.insert(name, count);
        }
    }
    check()?;
    Ok(MemoryFingerprint {
        schema_sha256: format!("{:x}", schema.finalize()),
        table_rows,
    })
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

type Notice = (String, String);

#[derive(Default)]
struct TaskNotices {
    failures: Vec<Notice>,
    missed: Vec<Notice>,
}

fn claim_notice(
    state: &mut NoticeState,
    now: i64,
    key: String,
    text: String,
    out: &mut Vec<Notice>,
) {
    if !state.seen.contains_key(&key) {
        state.seen.insert(key.clone(), now);
        out.push((key, text));
    }
}

/// Claims are synced before sending; unknown send outcomes are never retried.
fn update_notices<T>(
    context: &RuntimeContext,
    state_file: &str,
    update: impl FnOnce(&mut NoticeState) -> Result<T, String>,
) -> Result<T, String> {
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
        .read_optional_text(RuntimeArea::Data, state_file)
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
        state_file,
        &serde_json::to_vec(&state).map_err(|e| e.to_string())?,
    )?;
    Ok(notices)
}

fn send_notices(
    context: &RuntimeContext,
    config: &serde_json::Value,
    notices: Result<Vec<Notice>, String>,
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
    let notices = update_notices(context, NOTICE_STATE, |state| {
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

/// Observe retained full backup failures and elapsed backup slots without catch-up execution.
/// A minute's grace allows the scheduler to claim a due slot before it is called missed.
pub fn monitor_notices(
    context: &RuntimeContext,
    config: &serde_json::Value,
    store: &crate::task_runs::TaskStore,
    now: i64,
) -> Result<(), String> {
    monitor_notices_with(context, config, store, now, |key, text| {
        crate::admin_alert::notify_notice(context, config, text, key);
    })
}

fn monitor_notices_with(
    context: &RuntimeContext,
    config: &serde_json::Value,
    store: &crate::task_runs::TaskStore,
    now: i64,
    mut send: impl FnMut(&str, &str),
) -> Result<(), String> {
    // An unreadable pause store is not an observed schedule change.
    let paused = crate::schedule::ScheduleStore::new(context).paused_jobs();
    let paused = paused.unwrap_or_else(|_| crate::schedule::PAUSABLE_JOBS.iter().map(|job| (*job).to_owned()).collect()); // RED-M3
    let mut result = Ok(());
    let mut missed = Vec::new();
    let mut failures = Vec::new();
    for task in [
        TaskId::Backup,
        TaskId::MemoryBackup,
        TaskId::MemoryRestoreTest,
    ] {
        match monitor_task_notices(context, config, store, now, task, &paused) {
            Ok(notices) => {
                if !notices.missed.is_empty() {
                    missed.push(notices.missed);
                }
                failures.extend(notices.failures);
            }
            Err(error) => result = Err(error),
        }
    }
    send_missed_notices(&missed, &mut send);
    for (key, text) in failures {
        send(&key, &text);
    }
    result
}

fn send_missed_notices(missed: &[Vec<Notice>], send: &mut impl FnMut(&str, &str)) {
    let [first_group, rest @ ..] = missed else {
        return;
    };
    if let ([(key, text)], []) = (first_group.as_slice(), rest) {
        send(key, text);
        return;
    }

    // Summarize each job within one alert so a long outage cannot spend the
    // notice budget on one job before any of the other jobs are mentioned.
    let mut key = String::from("backup-missed-slots");
    let mut body = Vec::new();
    for group in missed {
        let (_, first) = &group[0];
        let (last_key, last) = &group[group.len() - 1];
        key.push('-');
        key.push_str(last_key);
        if group.len() == 1 {
            body.push(first.clone());
        } else {
            body.push(format!(
                "{} missed slots. First: {first} Last: {last}",
                group.len()
            ));
        }
    }
    send(&key, &body.join("\n"));
}

fn monitor_task_notices(
    context: &RuntimeContext,
    config: &serde_json::Value,
    store: &crate::task_runs::TaskStore,
    now: i64,
    task: TaskId,
    paused: &[String],
) -> Result<TaskNotices, String> {
    use crate::task_runs::{RunQuery, RunSource, RunState, SlotObservation};
    use crate::tasks::{self, Due, Schedule};

    let name = task.as_str();
    let def = tasks::task_def(task);
    let full = task == TaskId::Backup;
    let schedule = tasks::configured_schedule(def, config);
    let on = crate::supervisor::enabled(config, def) && !paused.iter().any(|job| job == name);
    let (time, cadence) = match schedule {
        Schedule::Daily { hour, minute } => (format!("{hour:02}:{minute:02}"), "daily"),
        Schedule::Monthly { hour, minute } => (format!("{hour:02}:{minute:02}"), "monthly"),
        _ => return Err(format!("unexpected backup schedule for {name}")),
    };
    let zone = crate::local_time::configured_zone(config);
    let signature = format!("\"{time}\":{on}:{zone:?}");
    let state_file = format!("{name}-notices.json");
    update_notices(context, &state_file, |state| {
        // The first observation and every time/enabled change start now: neither older
        // history nor slots before this moment (e.g. a time already past) are alerted.
        if state.schedule.as_deref() != Some(signature.as_str()) {
            state.schedule = Some(signature);
            state.active_since = now;
            state.checked_through = Some(now);
        }
        let mut notices = TaskNotices::default();
        let mut runs = Vec::new();
        // The other jobs report ordinary execution failures through the task executor.
        if full {
            // Read history inside the notice gate: pruning a stale snapshot must
            // never erase a failure claim concurrently saved by another process.
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
                        &mut notices.failures,
                    );
                }
            }
        }
        let title = match task {
            TaskId::MemoryBackup => "Memory backup",
            TaskId::MemoryRestoreTest => "Memory restore test",
            _ => "Backup",
        };
        if on {
            let mut after = state
                .checked_through
                .unwrap_or(state.active_since.saturating_sub(1));
            while let Some(slot) = tasks::next_due(schedule, after, &zone)
                .filter(|slot| *slot <= now.saturating_sub(60_000))
            {
                let missed = if full {
                    let next_slot = tasks::next_due(schedule, slot, &zone);
                    !runs.iter().any(|run| {
                        run.source == RunSource::Scheduled
                            && run.queued_at >= slot
                            && next_slot.is_none_or(|next| run.queued_at < next)
                    })
                } else {
                    match store.record_missed(Due { task, slot })? {
                        SlotObservation::AwaitingStart => break,
                        SlotObservation::Covered => false,
                        SlotObservation::Missed => true,
                    }
                };
                if missed {
                    claim_notice(
                        state,
                        now,
                        format!("{name}-missed-{slot}"),
                        format!(
                            "{title} missed its {cadence} slot at {} UTC (local {} {}).",
                            archive_stem(slot).trim_start_matches("qqbot-"),
                            time,
                            crate::local_time::label(&crate::local_time::at(slot, &zone)),
                        ),
                        &mut notices.missed,
                    );
                }
                state.checked_through = Some(slot);
                after = slot;
            }
        }
        Ok(notices)
    })
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
            Ok(metadata) if !crate::guarded_file::is_link(&metadata) && metadata.is_dir() => {}
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
                if crate::guarded_file::is_link(&metadata) || !metadata.is_dir() {
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
            let mut file = crate::guarded_file::open_gate(&path, crate::guarded_file::Gate::Create).map_err(|error| match error {
                crate::guarded_file::GateError::Open(error) => format!("backup lock unavailable: {}", error.kind()),
                _ => "backup destination lock is redirected or invalid".to_string(),
            })?;
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
        let mut output = match crate::guarded_credentials::create_credential(&target) {
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
        if !metadata.is_file() || crate::guarded_file::is_link(&metadata) {
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
                    Ok(metadata) if metadata.is_file() && !crate::guarded_file::is_link(&metadata) => metadata,
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
        if !metadata.is_file() || crate::guarded_file::is_link(&metadata) {
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
        if !metadata.is_file() || crate::guarded_file::is_link(&metadata) {
            return Err("backup archive changed before pruning".into());
        }
        let pin_path = path.with_extension("zip.restore-until");
        let pinned = match fs::symlink_metadata(&pin_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("restore retention pin unavailable: {e}")),
            Ok(meta) if meta.is_file() && !crate::guarded_file::is_link(&meta) => {
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
        let scratch = Self(dir);
        crate::guarded_credentials::restrict_credential(&scratch.0)
            .map_err(|_| "backup staging permissions unavailable".to_string())?;
        Ok(scratch)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn run(
    context: &RuntimeContext,
    task: &TaskDef,
    run: &mut RunHandle,
    cancel: &AtomicBool,
) -> Finish {
    let started = Instant::now();
    let check = || {
        if cancel.load(Ordering::SeqCst) {
            return Err("backup cancelled".into());
        }
        if started.elapsed() >= task.timeout {
            return Err("backup timed out".into());
        }
        if !matches!(crate::supervisor::desired(context), crate::lifecycle::Effective::Act { wants, .. } if wants.jobs)
        {
            return Err("background jobs are held".into());
        }
        Ok(())
    };
    let (result, message) = match task.id {
        TaskId::MemoryBackup => (
            create_memory_snapshot(context, run.view().queued_at, &check, run),
            "memory backup verified",
        ),
        TaskId::MemoryRestoreTest => (restore_test(context, &check), "memory restore verified"),
        _ => (create(context, &check), "backup verified"),
    };
    match result {
        Ok(path) => {
            run.log(
                LogStream::Meta,
                &format!(
                    "{message}: {}",
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

const MEMORY_COPIES: usize = 14;

fn memory_destination(context: &RuntimeContext) -> Result<PathBuf, String> {
    let runtime = context
        .runtime_root()
        .ok_or("memory backup requires a selected runtime root")?;
    let destination = crate::install_root::InstallRoot::configured()?
        .backup_destination(runtime, Path::new("backups/memory"))?;
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

        const DRIVE_FIXED: u32 = 3;
        let Some(Component::Prefix(prefix)) = destination.components().next() else {
            return Err("memory backups require a local fixed drive".into());
        };
        let (Prefix::Disk(drive) | Prefix::VerbatimDisk(drive)) = prefix.kind() else {
            return Err("memory backups require a local fixed drive".into());
        };
        let root = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
        // SAFETY: root is a NUL-terminated drive root.
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            return Err("memory backups require a local fixed drive".into());
        }
    }
    Ok(destination)
}

fn create_memory_snapshot(
    context: &RuntimeContext,
    at: i64,
    check: Check<'_>,
    run: &mut RunHandle,
) -> Result<PathBuf, String> {
    check()?;
    let destination = memory_destination(context)?;
    ensure_destination(&destination)?;
    let scratch = Scratch::new(&destination)?;
    let database = scratch.0.join("memory.db");
    let source = crate::memory::open_read_only_for_runtime(context)?;
    source.busy_timeout(Duration::from_millis(100)).map_err(|e| e.to_string())?;
    // Pin the live view through both operations, even if another connection writes to the WAL.
    source.execute_batch("BEGIN DEFERRED").map_err(|e| e.to_string())?;
    let fingerprint = memory_fingerprint(&source, check)?;
    online_snapshot(source, &database, "memory", check)?;
    let candidate = scratch.0.join("archive.zip");
    let file = crate::guarded_credentials::create_credential(&candidate).map_err(|e| e.to_string())?;
    let mut archive = ZipWriter::new(file);
    let mut manifest = Manifest {
        version: 1,
        files: BTreeMap::new(),
        memory_fingerprint: Some(fingerprint),
    };
    add(
        &mut archive,
        &mut manifest,
        "memory.db",
        File::open(database).map_err(|e| e.to_string())?,
        check,
    )?;
    finish_archive(archive, &manifest)?;
    let verification = Scratch::new(&scratch.0)?;
    verify_payload(
        File::open(&candidate).map_err(|e| e.to_string())?,
        check,
        Verification::MemoryBackup,
        &verification.0,
    )?;

    let _lock = DestinationLock::acquire(&destination, check)?;
    let target = publish_memory(&candidate, &destination, at, check)?;
    if let Err(error) = prune_memory(&destination, &target, check) {
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        run.log(
            LogStream::Meta,
            &format!("warning: memory backup {name} succeeded; retention could not finish: {error}"),
        );
    }
    Ok(target)
}

fn prune_memory(destination: &Path, target: &Path, check: Check<'_>) -> Result<(), String> {
    for (_, path, _) in archive_files(destination, check)?
        .into_iter()
        .filter(|(_, path, _)| path != target)
        .skip(MEMORY_COPIES - 1)
    {
        check()?;
        fs::remove_file(path).map_err(|e| e.kind().to_string())?;
    }
    Ok(())
}

fn publish_memory(
    candidate: &Path,
    destination: &Path,
    at: i64,
    check: Check<'_>,
) -> Result<PathBuf, String> {
    let stem = archive_stem(at);
    for collision in 0u64.. {
        check()?;
        let name = if collision == 0 {
            format!("{stem}.zip")
        } else {
            format!("{stem}-{collision}.zip")
        };
        let target = destination.join(name);
        // Publish the synced file without exposing a partially copied archive or replacing a collision.
        match fs::hard_link(candidate, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("memory backup publication failed: {}", error.kind())),
        }
    }
    Err("memory backup archive names exhausted".into())
}

fn restore_test(context: &RuntimeContext, check: Check<'_>) -> Result<PathBuf, String> {
    check()?;
    let destination = memory_destination(context)?;
    if !inspect_destination(&destination, false)? {
        return Err("no local memory backup to restore".into());
    }
    let _lock = DestinationLock::acquire(&destination, check)?;
    let archive = archive_files(&destination, check)?
        .into_iter()
        .filter_map(|(_, path, _)| {
            let name = path.file_stem()?.to_str()?;
            // archive_files already checked the fixed-width ASCII timestamp and suffix shape.
            let (stamp, suffix) = name.split_at("qqbot-YYYY-MM-DDTHH-mm-ss".len());
            let collision = match suffix.strip_prefix('-') {
                Some(number) => number.parse::<u64>().ok()?,
                None => 0,
            };
            Some(((stamp.to_owned(), collision), path))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, path)| path)
        .ok_or("no local memory backup to restore")?;
    let scratch = Scratch::new(&destination)?;
    for directory in ["config", "data", "resources"] {
        fs::create_dir(scratch.0.join(directory)).map_err(|e| e.to_string())?;
    }
    for (path, contents) in [
        ("config/config.json", r#"{"consoleToken":"memory-restore-test"}"#),
        ("data/social-v2.json", "{}"),
        ("dependencies.json", r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#),
    ] {
        fs::write(scratch.0.join(path), contents).map_err(|e| e.to_string())?;
    }
    let runtime = RuntimeContext::from_root(&scratch.0)?;
    let file = File::open(&archive).map_err(|e| e.to_string())?;
    let (manifest, _) = verify_payload(file, check, Verification::MemoryBackup, runtime.data_dir())?;
    let expected = manifest.memory_fingerprint.ok_or("memory backup fingerprint missing")?;
    // Compare the restored bytes before the normal application open can migrate them.
    let restored = crate::memory::open_read_only_for_runtime(&runtime)?;
    if memory_fingerprint(&restored, check)? != expected {
        return Err("restored memory fingerprint mismatch".into());
    }
    drop(restored);
    let db = crate::memory::open_ready_for_runtime(&runtime)?;
    db.query_row("SELECT EXISTS(SELECT 1 FROM memory)", [], |row| {
        row.get::<_, bool>(0)
    })
    .map_err(|e| format!("restored memory is unreadable: {e}"))?;
    check()?;
    Ok(archive)
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
    if !metadata.is_file() || crate::guarded_file::is_link(&metadata) { return Err("restore retention pin redirected or invalid".into()); }
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

/// The stored `config.json` bytes the archive keeps, and the configuration the snapshot's settings come from: the
/// same bytes parsed, with the settings overlay applied as for every configuration reader.
pub(crate) fn snapshot_config(context: &RuntimeContext) -> Result<(Vec<u8>, serde_json::Value), String> {
    let config = context.read_file(RuntimeArea::Config, "config.json")?;
    let text = std::str::from_utf8(&config).map_err(|_| "backup config is not UTF-8")?;
    let raw: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| "backup config is invalid")?;
    Ok((config, context.overlay_config(raw)))
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
    let (config, raw) = snapshot_config(context)?;
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
    // The settings store is optional too. One that cannot be read safely fails the backup, as a damaged conversation
    // database does, so no archive silently lacks it.
    if let Some(db) =
        crate::settings::backup_source(context).map_err(|error| format!("settings backup: {error}"))?
    {
        online_snapshot(db, &scratch.0.join("settings.db"), "settings", check)?;
        databases.push("settings.db");
    }
    let candidate = scratch.0.join("archive.zip");
    let mut archive = ZipWriter::new(File::create(&candidate).map_err(|e| e.to_string())?);
    let mut manifest = Manifest {
        version: 1,
        files: BTreeMap::new(),
        memory_fingerprint: None,
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
    finish_archive(archive, &manifest)?;
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

fn finish_archive(mut archive: ZipWriter<File>, manifest: &Manifest) -> Result<(), String> {
    archive
        .start_file("manifest.json", FileOptions::default())
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut archive, manifest).map_err(|e| e.to_string())?;
    archive
        .finish()
        .map_err(|e| e.to_string())?
        .sync_all()
        .map_err(|e| e.to_string())
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

pub(crate) fn verify_archive_for_restore(
    path: &Path,
    memory_only: bool,
) -> Result<(Manifest, bool), String> {
    verify_file_inner(
        File::open(path).map_err(|e| e.to_string())?,
        &|| Ok(()),
        Verification::Restore { memory_only },
    )
}

fn verify_file(file: File, check: Check<'_>) -> Result<Manifest, String> {
    verify_file_inner(file, check, Verification::FullBackup).map(|(manifest, _)| manifest)
}

fn verify_file_inner(
    file: File,
    check: Check<'_>,
    verification: Verification,
) -> Result<(Manifest, bool), String> {
    let scratch = Scratch::new(&std::env::temp_dir())?;
    verify_payload(file, check, verification, &scratch.0)
}

fn verify_payload(
    file: File,
    check: Check<'_>,
    verification: Verification,
    restored: &Path,
) -> Result<(Manifest, bool), String> {
    let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;
    let manifest: Manifest = {
        let file = archive
            .by_name("manifest.json")
            .map_err(|_| "backup manifest missing")?;
        serde_json::from_reader(file).map_err(|e| format!("backup manifest invalid: {e}"))?
    };
    let has_config = manifest.files.contains_key("config.json");
    let valid_scope = match verification {
        Verification::FullBackup | Verification::Restore { memory_only: false } => has_config,
        Verification::MemoryBackup => manifest.files.len() == 1,
        Verification::Restore { memory_only: true } => has_config || manifest.files.len() == 1,
    };
    if manifest.version != 1 || !manifest.files.contains_key("memory.db") || !valid_scope {
        return Err("backup manifest incomplete or unsupported".into());
    }
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
        let mut db = if matches!(name.as_str(), "memory.db" | "conversations.db" | "settings.db") {
            Some(File::create(restored.join(&name)).map_err(|e| e.to_string())?)
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
    let mut has_endpoint_credentials = false;
    for name in ["memory.db", "conversations.db", "settings.db"] {
        if !manifest.files.contains_key(name) {
            continue;
        }
        let db = Connection::open_with_flags(
            restored.join(name),
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
        if matches!(verification, Verification::Restore { memory_only: false }) && name == "settings.db" {
            // A future settings schema can still be restored when its credential presence is unknown.
            has_endpoint_credentials = crate::settings::archive_has_endpoint_credentials(&db).unwrap_or(true);
        }
    }
    check()?;
    Ok((manifest, has_endpoint_credentials))
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['\\', ':'])
        && name
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

fn archive_stem(ms: i64) -> String {
    let utc = crate::local_time::at(ms, &jiff::tz::TimeZone::UTC);
    format!("qqbot-{}", utc.strftime("%Y-%m-%dT%H-%M-%S"))
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
    fn nightly_memory_backup_keeps_fourteen_copies() {
        let fixture = Fixture::new("nightly-memory");
        let context = fixture.context();
        let db = Connection::open(fixture.0.join("data/memory.db")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE fixture (value TEXT);
             INSERT INTO fixture VALUES ('recent memory');",
        ).unwrap();

        let root = fixture.0.join("backups/memory");
        fs::create_dir_all(&root).unwrap();
        let unrelated = root.join("keep.txt");
        fs::write(&unrelated, "unrelated").unwrap();
        let config_path = fixture.0.join("config/config.json");
        let mut config = context.load_config().unwrap();
        config["backup"]["dir"] = json!("elsewhere");
        fs::write(config_path, config.to_string()).unwrap();

        let run_nightly = |day: u8| {
            let now = timestamp(&format!("2026-01-{day:02}T04:00:00Z"));
            let done = run_scheduled(&fixture, TaskId::MemoryBackup, now);
            assert_eq!(done.state, RunState::Succeeded, "{done:?}");
        };
        run_nightly(1);
        let first = root.join("qqbot-2026-01-01T04-00-00.zip");
        assert!(first.is_file(), "nightly copies stay in the local backup set");
        for day in 1..14 {
            let path = root.join(format!("qqbot-2026-01-{day:02}T04-00-00.zip"));
            if day > 1 {
                fs::copy(&first, &path).unwrap();
            }
            let modified = std::time::UNIX_EPOCH + Duration::from_secs(86_400 * day);
            fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(modified).unwrap();
        }
        run_nightly(14);
        assert!(first.exists(), "the fourteenth slot keeps the oldest copy");

        run_nightly(15);
        let copies = super::archive_files(&root, &|| Ok(())).unwrap();
        assert_eq!(copies.len(), 14, "the fifteenth copy prunes exactly one backup");
        assert!(!first.exists());
        assert!(unrelated.is_file());
        assert!(!fixture.0.join("elsewhere").exists());

        let latest = root.join("qqbot-2026-01-15T04-00-00.zip");
        let preview = crate::restore::preview(&context, &latest, true).unwrap();
        assert_eq!(preview["replaces"], json!(["data/memory.db"]));
        assert!(crate::restore::preview(&context, &latest, false).is_err());
        let mut zip = zip::ZipArchive::new(fs::File::open(latest).unwrap()).unwrap();
        assert_eq!(zip.len(), 2, "the archive holds only memory and its manifest");
        let restored = fixture.0.join("restored.db");
        std::io::copy(
            &mut zip.by_name("memory.db").unwrap(),
            &mut fs::File::create(&restored).unwrap(),
        ).unwrap();
        let restored = Connection::open(restored).unwrap();
        let value: String = restored.query_row("SELECT value FROM fixture", [], |row| row.get(0)).unwrap();
        assert_eq!(value, "recent memory", "committed WAL content was recovered");
    }

    #[test]
    fn monthly_memory_restore_drill_runs_from_the_fake_clock() {
        let fixture = Fixture::new("monthly-memory");
        let root = fixture.0.join("backups/memory");
        let context = fixture.context();
        let db = crate::memory::open_ready_for_runtime(&context).unwrap();
        let january = timestamp("2026-01-31T04:00:00Z");
        let february = timestamp("2026-02-01T05:30:00Z");
        let march = timestamp("2026-03-01T05:30:00Z");
        let task = TaskId::MemoryRestoreTest;
        let nightly = run_scheduled(&fixture, TaskId::MemoryBackup, january);
        assert_eq!(nightly.state, RunState::Succeeded, "{nightly:?}");
        let archive = root.join("qqbot-2026-01-31T04-00-00.zip");

        let before = fs::read(&archive).unwrap();
        let older = root.join("qqbot-2026-01-30T04-00-00.zip");
        fs::copy(&archive, &older).unwrap();
        fs::OpenOptions::new().write(true).open(older).unwrap().set_modified(
            std::time::UNIX_EPOCH + Duration::from_secs(86_400),
        ).unwrap();
        db.execute_batch("CREATE TABLE live_only (value TEXT);").unwrap();
        let completed = run_scheduled(&fixture, task, february);
        assert_eq!(completed.state, RunState::Succeeded, "{completed:?}");
        assert_eq!(fs::read(&archive).unwrap(), before, "the drill preserves its input");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3, "the throwaway runtime was removed");
        let live = Connection::open(fixture.0.join("data/memory.db")).unwrap();
        assert!(live.prepare("SELECT value FROM live_only").is_ok(), "the live memory stays in place");

        fs::write(&archive, "broken ZIP").unwrap();
        let failed = run_scheduled(&fixture, task, march);
        assert_eq!(failed.state, RunState::Failed, "the drill must fail when restoration fails");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 3, "the failed drill also removes its runtime");
    }

    #[test]
    fn monthly_drill_checks_the_fingerprint_of_the_newest_named_archive() {
        let fixture = Fixture::new("monthly-fingerprint");
        let context = fixture.context();
        let db = crate::memory::open_ready_for_runtime(&context).unwrap();
        db.execute_batch(r#"CREATE TABLE "quoted""table" (value TEXT);
                            INSERT INTO "quoted""table" VALUES ('fixture');"#).unwrap();
        let nightly = run_scheduled(
            &fixture, TaskId::MemoryBackup, timestamp("2026-01-31T04:00:00Z"),
        );
        assert_eq!(nightly.state, RunState::Succeeded, "{nightly:?}");

        let root = fixture.0.join("backups/memory");
        let latest = root.join("qqbot-2026-01-31T04-00-00.zip");
        fs::write(root.join("qqbot-2026-01-30T04-00-00.zip"), "broken older ZIP").unwrap();
        fs::File::options().write(true).open(&latest).unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(86_400)).unwrap();
        db.execute_batch(r#"INSERT INTO "quoted""table" VALUES ('after backup');"#).unwrap();
        let completed = run_scheduled(
            &fixture, TaskId::MemoryRestoreTest, timestamp("2026-02-01T05:30:00Z"),
        );
        assert_eq!(completed.state, RunState::Succeeded, "the filename timestamp selects the archive: {completed:?}");

        // Keep the ZIP and the database file hash valid while altering the expected fingerprint.
        use std::io::{Read, Write};
        let mut archive = zip::ZipArchive::new(fs::File::open(&latest).unwrap()).unwrap();
        let mut database = Vec::new();
        archive.by_name("memory.db").unwrap().read_to_end(&mut database).unwrap();
        let manifest: serde_json::Value = serde_json::from_reader(
            archive.by_name("manifest.json").unwrap(),
        ).unwrap();
        drop(archive);
        for (date, field) in [
            ("2026-03-01T05:30:00Z", "table_rows"),
            ("2026-04-01T05:30:00Z", "schema_sha256"),
        ] {
            let mut manifest = manifest.clone();
            let fingerprint = &mut manifest["memory_fingerprint"];
            assert_eq!(fingerprint["table_rows"]["quoted\"table"], json!(1));
            match field {
                "table_rows" => fingerprint[field]["quoted\"table"] = json!(2),
                _ => fingerprint[field] = json!("different schema digest"),
            }
            let mut writer = zip::ZipWriter::new(fs::File::create(&latest).unwrap());
            writer.start_file("memory.db", zip::write::FileOptions::default()).unwrap();
            writer.write_all(&database).unwrap();
            writer.start_file("manifest.json", zip::write::FileOptions::default()).unwrap();
            serde_json::to_writer(&mut writer, &manifest).unwrap();
            writer.finish().unwrap();
            drop(writer);
            let failed = run_scheduled(&fixture, TaskId::MemoryRestoreTest, timestamp(date));
            assert_eq!(failed.reason.as_deref(), Some("restored memory fingerprint mismatch"),
                "the drill must reject a mismatched {field}: {failed:?}");
            assert_eq!(failed.state, RunState::Failed);
        }
    }

    fn timestamp(utc: &str) -> i64 {
        utc.parse::<jiff::Timestamp>().unwrap().as_millisecond()
    }

    fn run_scheduled(fixture: &Fixture, task: TaskId, at: i64) -> crate::task_runs::RunView {
        use crate::tasks::Clock;
        use std::sync::Arc;

        let lead_hours = if task == TaskId::MemoryRestoreTest { 96 } else { 18 };
        let start = at - lead_hours * 60 * 60 * 1_000;
        let clock = Arc::new(crate::dependency::ManualClock::new(start));
        let context = fixture.context();
        let cfg = context.load_config().unwrap();
        let api = crate::supervisor::task_api_with_clock(&context, &cfg, clock.clone()).unwrap();
        let due = {
            let mut scheduler = api.scheduler.lock().unwrap();
            // Test against explicit UTC slots regardless of the host's configured zone.
            scheduler.set_zone(jiff::tz::TimeZone::UTC, start);
            assert_eq!(scheduler.next_run(task), Some(at), "the next calendar slot must match");
            clock.set(at - 1);
            assert!(!scheduler.tick(clock.now_ms()).iter().any(|slot| slot.task == task));
            clock.set(at);
            let due = scheduler.tick(clock.now_ms()).into_iter().find(|slot| slot.task == task).unwrap();
            assert!(!scheduler.tick(clock.now_ms()).iter().any(|slot| slot.task == task));
            due
        };
        let Trigger::Started(run) = api.dispatcher.on_due(due, &crate::supervisor::desired(&context)) else {
            panic!("scheduled job did not start");
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = api.dispatcher.store().get(&run.run_id).unwrap().unwrap();
            if view.state.is_terminal() {
                return view;
            }
            assert!(Instant::now() < deadline, "scheduled job did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn backup_daily_schedule_refresh_preserves_due_slots_and_applies_changed_time() {
        use crate::tasks::{Schedule, Scheduler};
        let mut scheduler = Scheduler::new(|def| def.id == TaskId::Backup, jiff::tz::TimeZone::UTC, 0);
        let config = json!({"backup":{"time":"06:30"}});
        scheduler.refresh_schedules(&config, 0);
        assert_eq!(
            scheduler.schedule(TaskId::Backup),
            Some(Schedule::Daily {
                hour: 6,
                minute: 30
            })
        );
        assert_eq!(scheduler.next_run(TaskId::Backup), Some(23_400_000));
        scheduler.refresh_schedules(&config, 23_400_000);
        let due = scheduler.tick(23_400_000);
        assert_eq!(
            due.len(),
            1,
            "an unchanged refresh must not consume a due slot"
        );
        assert_eq!(due[0].task, TaskId::Backup);
        assert_eq!(due[0].slot, 23_400_000);
        assert!(scheduler.tick(23_400_000).is_empty());
        scheduler.refresh_schedules(&json!({"backup":{"time":"07:00"}}), 23_400_001);
        assert_eq!(scheduler.next_run(TaskId::Backup), Some(25_200_000));
        scheduler.set_enabled(TaskId::Backup, false, 23_400_002);
        scheduler.refresh_schedules(&json!({"backup":{"time":"08:00"}}), 23_400_002);
        assert_eq!(scheduler.next_run(TaskId::Backup), None);
        scheduler.set_enabled(TaskId::Backup, true, 23_400_003);
        assert_eq!(scheduler.next_run(TaskId::Backup), Some(28_800_000));
    }

    #[test]
    fn scheduled_backup_is_refused_at_admission_while_paused_or_the_schedule_store_is_unreadable() {
        let fixture = Fixture::new("schedule-admission");
        let context = fixture.context();
        let api = crate::supervisor::task_api(&context, &context.load_config().unwrap()).unwrap();
        let def = crate::tasks::task_def(TaskId::Backup);
        let scheduled = |slot: &str| {
            let Trigger::Started(run) = api.dispatcher.start(def, crate::task_runs::RunSource::Scheduled, None, Some(slot), "scheduler") else {
                panic!("scheduled backup was not started")
            };
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let view = api.dispatcher.store().get(&run.run_id).unwrap().unwrap();
                if view.state.is_terminal() {
                    return view;
                }
                assert!(Instant::now() < deadline, "scheduled backup did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        // Paused after the scheduler last refreshed: admission reads the pause afresh.
        crate::schedule::ScheduleStore::new(&context).set_system_paused("backup", true).unwrap();
        let paused = scheduled("scheduled:backup:1");
        assert_eq!(paused.state, crate::task_runs::RunState::Cancelled, "{paused:?}");
        assert!(paused.reason.as_deref().unwrap_or("").contains("paused"), "{paused:?}");

        fs::write(fixture.0.join("data/schedule.json"), "not json").unwrap();
        let unreadable = scheduled("scheduled:backup:2");
        assert_eq!(unreadable.state, crate::task_runs::RunState::Cancelled, "{unreadable:?}");
        assert!(unreadable.reason.as_deref().unwrap_or("").contains("schedule store unavailable"), "{unreadable:?}");
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
    fn missed_memory_slots_are_recorded_and_alerted_once_after_restart() {
        use crate::task_runs::{RunQuery, RunSource};
        use crate::tasks::Clock;

        let fixture = Fixture::new("missed-memory-slots");
        let context = fixture.context();
        let mut config = context.load_config().unwrap();
        config["time"] = json!({"zone":"UTC"});
        let start = timestamp("2026-02-01T03:59:00Z");
        let clock = std::sync::Arc::new(crate::dependency::ManualClock::new(start));
        let task_api = || crate::supervisor::task_api_with_clock(&context, &config, clock.clone()).unwrap();
        let observe = |api: &crate::console_tasks::TasksApi| {
            super::monitor_notices(&context, &config, api.dispatcher.store(), clock.now_ms())
                .unwrap();
        };
        observe(&task_api());
        // No scheduler ran while either calendar slot elapsed.
        clock.set(timestamp("2026-02-01T05:30:59Z"));
        let api = task_api();
        observe(&api);
        assert!(api.dispatcher.store().list(&RunQuery {
            task: Some(TaskId::MemoryRestoreTest.as_str().into()),
            before: None,
            limit: None,
        }).unwrap().runs.is_empty(), "the drill still has its minute of grace");
        drop(api);

        clock.set(timestamp("2026-02-01T05:31:00Z"));
        let api = task_api();
        observe(&api);
        observe(&api);
        for (task, slot) in [
            (TaskId::MemoryBackup, timestamp("2026-02-01T04:00:00Z")),
            (TaskId::MemoryRestoreTest, timestamp("2026-02-01T05:30:00Z")),
        ] {
            let name = task.as_str();
            let runs = api.dispatcher.store().list(&RunQuery {
                task: Some(name.into()),
                before: None,
                limit: None,
            }).unwrap().runs;
            assert_eq!(runs.len(), 1, "one history entry for each missed memory slot: {name}");
            let run = &runs[0];
            assert_eq!((run.source, run.state, run.queued_at), (RunSource::Scheduled, RunState::Cancelled, slot));
            assert!(run.started_at.is_none());
            assert!(run.reason.as_deref().unwrap().contains("missed"));
            let notices = context.read_optional_text(
                crate::runtime::RuntimeArea::Data,
                &format!("{name}-notices.json"),
            ).unwrap().unwrap();
            let notices: serde_json::Value = serde_json::from_str(&notices).unwrap();
            assert_eq!(notices["seen"].as_object().unwrap().keys().collect::<Vec<_>>(),
                [&format!("{name}-missed-{slot}")]);
        }
    }

    #[test]
    fn ten_missed_nights_deliver_memory_recovery_notices() {
        let fixture = Fixture::new("ten-missed-nights");
        let context = fixture.context();
        let mut config = context.load_config().unwrap();
        config["time"] = json!({"zone":"UTC"});
        let api = crate::supervisor::task_api(&context, &config).unwrap();
        let store = api.dispatcher.store();
        let start = timestamp("2026-01-31T06:00:00Z");
        let restart = timestamp("2026-02-10T06:00:00Z");
        let mut limiter = crate::admin_alert::AlertLimiter::new();
        let mut delivered = Vec::new();
        let record_delivery = |key: &str, body: &str| {
            if limiter.check("backup-fixture", key, restart).is_send() {
                delivered.push(crate::admin_alert::prepare_body(body, str::to_owned));
            }
        };

        super::monitor_notices_with(&context, &config, store, start, |_, _| {
            panic!("first observation must stay quiet");
        })
        .unwrap();
        super::monitor_notices_with(&context, &config, store, restart, record_delivery).unwrap();
        assert!(
            delivered.iter().any(|body| body.contains("Memory backup missed")),
            "ten full-backup misses must leave room for the memory backup notice"
        );
        assert!(
            delivered.iter().any(|body| body.contains("Memory restore test missed")),
            "ten full-backup misses must leave room for the restore test notice"
        );
        assert_eq!(delivered.len(), 1, "one missed-slots notice per pass");
        assert_eq!(delivered[0].matches("10 missed slots.").count(), 2);
        super::monitor_notices_with(&context, &config, store, restart, |_, _| {
            panic!("already reported slots must stay quiet");
        })
        .unwrap();
    }

    #[test]
    fn queued_memory_run_recovered_as_unknown_alerts_once() {
        use crate::task_runs::{Claim, RunSource};
        use crate::tasks::Due;

        let fixture = Fixture::new("queued-memory-run");
        let context = fixture.context();
        let mut config = context.load_config().unwrap();
        config["time"] = json!({"zone":"UTC"});
        config["tasks"]["backup"] = json!({"enabled":false});
        let start = timestamp("2026-02-01T03:59:00Z");
        let clock = std::sync::Arc::new(crate::dependency::ManualClock::new(start));
        let task_api = || {
            crate::supervisor::task_api_with_clock(&context, &config, clock.clone()).unwrap()
        };
        let api = task_api();
        let store = api.dispatcher.store();
        let no_notice = |_: &str, _: &str| {
            panic!("expected no backup notice");
        };
        super::monitor_notices_with(&context, &config, store, start, no_notice).unwrap();

        let claim = |due: Due| {
            let Claim::Started(run) = store.claim(
                due.task.as_str(),
                RunSource::Scheduled,
                None,
                Some(&due.dedup_key()),
                "scheduler",
                due.slot,
            ) else {
                panic!("fixture slot claim failed");
            };
            run
        };
        let queued_slot = timestamp("2026-02-01T04:00:00Z");
        let queued = claim(Due {
            task: TaskId::MemoryBackup,
            slot: queued_slot,
        });
        let queued_id = queued.run_id().to_owned();
        super::monitor_notices_with(&context, &config, store, queued_slot + 60_000, no_notice)
            .unwrap();

        let drill_slot = timestamp("2026-02-01T05:30:00Z");
        let mut drill = claim(Due {
            task: TaskId::MemoryRestoreTest,
            slot: drill_slot,
        });
        drill.mark_running(None, drill_slot).unwrap();
        let drill_id = drill.run_id().to_owned();
        queued.simulate_crash();
        drill.simulate_crash();
        drop(api);

        let restart = drill_slot + 60_000;
        clock.set(restart);
        let api = task_api();
        let store = api.dispatcher.store();
        for (id, started_at) in [(&queued_id, None), (&drill_id, Some(drill_slot))] {
            let recovered = store.get(id).unwrap().unwrap();
            assert_eq!(
                (recovered.state, recovered.started_at),
                (RunState::Unknown, started_at)
            );
        }

        let mut notices = Vec::new();
        super::monitor_notices_with(&context, &config, store, restart, |key, _| {
            notices.push(key.to_owned());
        })
        .unwrap();
        assert_eq!(
            notices,
            [format!("memory-backup-missed-{queued_slot}")],
            "only the recovered run that never started must alert"
        );
        assert_eq!(store.list(&Default::default()).unwrap().runs.len(), 2);
        super::monitor_notices_with(&context, &config, store, restart + 60_000, no_notice).unwrap();
    }

    #[test]
    fn unreadable_schedule_preserves_missed_memory_slots_across_restart() {
        use crate::runtime::RuntimeArea;

        let fixture = Fixture::new("unreadable-schedule");
        let context = fixture.context();
        let mut config = context.load_config().unwrap();
        config["time"] = json!({"zone":"UTC"});
        crate::schedule::ScheduleStore::new(&context)
            .set_system_paused("backup", true)
            .unwrap();
        let schedule_path = fixture.0.join("data/schedule.json");
        let readable_schedule = fs::read(&schedule_path).unwrap();
        let start = timestamp("2026-02-01T03:59:00Z");
        let restart = timestamp("2026-02-01T05:31:00Z");
        let no_notice = |_: &str, _: &str| {
            panic!("expected no backup notice");
        };
        let api = crate::supervisor::task_api(&context, &config).unwrap();
        super::monitor_notices_with(&context, &config, api.dispatcher.store(), start, no_notice)
            .unwrap();
        drop(api);
        let saved_state = || {
            [
                "backup-notices.json",
                "memory-backup-notices.json",
                "memory-restore-test-notices.json",
            ]
            .map(|file| {
                context
                    .read_optional_text(RuntimeArea::Data, file)
                    .unwrap()
                    .unwrap()
            })
        };
        let before = saved_state();
        fs::write(&schedule_path, b"not json").unwrap();
        let api = crate::supervisor::task_api(&context, &config).unwrap();
        let store = api.dispatcher.store();
        let unreadable = super::monitor_notices_with(&context, &config, store, restart, no_notice);
        assert_eq!(
            saved_state(),
            before,
            "an unreadable schedule must preserve the notice cursors and signatures"
        );
        assert!(unreadable.is_err());

        fs::write(&schedule_path, readable_schedule).unwrap();
        let mut notices = Vec::new();
        super::monitor_notices_with(&context, &config, store, restart + 60_000, |_, body| {
            notices.push(body.to_owned());
        })
        .unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("Memory backup missed"));
        assert!(notices[0].contains("Memory restore test missed"));
        assert_eq!(store.list(&Default::default()).unwrap().runs.len(), 2);
        super::monitor_notices_with(&context, &config, store, restart + 120_000, no_notice)
            .unwrap();
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

        let schedule = crate::tasks::configured_schedule(crate::tasks::task_def(TaskId::Backup), &config);
        let slot = crate::tasks::next_due(schedule, now + 180_000, &crate::local_time::configured_zone(&config))
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
