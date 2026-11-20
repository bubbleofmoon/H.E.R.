//! The settings store `data/settings.db`: its gate, inspection, schema, seed, write-back, apply, reset, undo, history and
//! view, the overlay the configuration loader applies, and the registry helpers of the console's `config.json` writers
//! (`console_changes`, `apply_changes`, `keep_file_members`, `keep_registry_values`, `registry_free`). The values kept per
//! conversation (#218, schema version 2) are in `store/conversation.rs`. The module root (`settings.rs`) re-exports what
//! callers use; the contract is described there.

mod conversation;

pub(crate) use conversation::{
    apply_conversation, apply_switch_choice, canonical_conversation, conversation_admitted, conversation_value,
    conversation_view, engine_choice, has_legacy_engine_choices, import_engine_choices, reset_conversation,
    restore_switch_choice, CONVERSATION_ERROR,
};

use super::{find, find_conversation, registry, Setting, CLI_MODEL_API_KEY, HARNESS_ROUTES, OWNER, WEB_SEARCH_API_KEY};
use super::registry::{ALLOW_GROUPS, ALLOW_PRIVATE, ROLLOUT_GROUPS};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use rusqlite::config::DbConfig;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const DB: &str = "settings.db";
const SIDECARS: [&str; 3] = ["settings.db-wal", "settings.db-shm", "settings.db-journal"];
/// The newest schema version this release reads and writes: 1 (`settings`, `settings_audit`) or 2 (version 1 and
/// `conversation_settings`, #218). A store stays at version 1 until its first conversation write (`migrate`).
const SCHEMA_VERSION: i64 = 2;
const SECRET_MARKER: &str = "(hidden)";
const STORAGE_ERROR: &str = "settings storage is unavailable";
pub(crate) const NEWER_SCHEMA_ERROR: &str = "settings database is newer than this release";
const UNSUPPORTED_SCHEMA_ERROR: &str = "settings database version is not supported";
const LEGACY_ERROR: &str = "settings require a selected runtime";
/// The configuration file the store seeds from and writes back into (`RuntimeArea::Config`).
const CONFIG: &str = "config.json";
/// The copy of `config.json` from before settings first wrote anything, beside it.
const PRE_SETTINGS_COPY: &str = "config.json.pre-settings";
const CONFIG_ERROR: &str = "runtime config is unavailable";
const COPY_ERROR: &str = "config.json.pre-settings cannot be written";
/// Who a seeded value is recorded as (`updated_by` and the audit actor); the audit source is `migration`.
const SEED_ACTOR: &str = "system";

#[cfg(test)]
thread_local! {
    /// Makes the next change (`apply`, `reset` or `undo`) on this thread fail after it wrote the value and before the
    /// audit row.
    pub(crate) static FAIL_BEFORE_AUDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the next write of `config.json` by the settings on this thread (a `write_back` that would change it, or an
    /// undo's `remove_from_config`) fail before it writes, as a full disk or a crash before that write would.
    pub(crate) static FAIL_WRITE_BACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the next store write on this thread panic just before its commit, as a crash there would (#154).
    pub(crate) static PANIC_BEFORE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the next look at an undo's entry (`preview`) on this thread fail as a store that cannot be read right now
    /// would (503); the look after it succeeds (#154).
    pub(crate) static FAIL_UNDO_LOOK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the next migration to schema version 2 on this thread fail after it created `conversation_settings` and
    /// before its transaction commits, as a crash midway would (#218).
    pub(crate) static FAIL_MIGRATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the next clear of unadmitted conversations' values on this thread fail after it removed them and before
    /// the commit, as a crash there would (#218).
    pub(crate) static FAIL_AFTER_CLEAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Counts attempts to switch the store to WAL that met another connection's lock (test hook for the retry).
#[cfg(test)]
pub(crate) static WAL_SWITCH_BUSY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The result of one `apply` or `reset`.
pub(crate) struct Applied {
    pub value: Value,
    pub revision: i64,
    pub changed: bool,
    /// Why `config.json` does not hold every stored value after this write; the stored change stands either way.
    pub mirror_error: Option<String>,
}

/// Applies the stored settings over `document`, the parsed `config.json`. A stored value goes to its registry pointer;
/// missing or null objects on the way are created, and any other non-object on the way leaves that setting unapplied.
/// A legacy context and a document that is not an object are returned unchanged. A store this release cannot read
/// applies nothing and is reported once per process on stderr (MCP servers use stdout).
pub(crate) fn overlay(context: &RuntimeContext, mut document: Value) -> Value {
    if let Err(error) = overlay_checked(context, &mut document) {
        static REPORTED: AtomicBool = AtomicBool::new(false);
        if !REPORTED.swap(true, Ordering::Relaxed) {
            crate::safe_eprintln!("[settings] stored settings are not applied: {error}");
        }
    }
    document
}

/// Applies the overlay only after a successful read, so durable repairs can refuse to use file-only values.
pub(crate) fn overlay_checked(context: &RuntimeContext, document: &mut Value) -> Result<(), String> {
    if context.is_isolated() && document.is_object() {
        for (setting, value) in stored_values(context)? {
            set_at(document, setting.pointer, value);
        }
    }
    Ok(())
}

/// Fill the timing settings in the effective runtime snapshot. Raw documents and mirror writes keep their spelling.
pub(crate) fn complete_timing(document: &mut Value) {
    use super::send::{first_gap_setting, FIRST_BASE, FIRST_PER_CHAR, GAP_BASE, GAP_PER_CHAR};
    for (first, fallback) in [(FIRST_BASE, GAP_BASE), (FIRST_PER_CHAR, GAP_PER_CHAR)] {
        let value = first_gap_setting(document, &first, &fallback);
        set_at(document, first.pointer, json!(value));
    }
    for setting in super::reply::SETTINGS.iter().chain(super::wake_timing::SETTINGS).chain(super::send::SETTINGS) {
        let value = setting.read(document);
        if !value.is_null() {
            set_at(document, setting.pointer, value);
        }
    }
}

/// Materialize the settings managed by the live controls in a read-only configuration view.
pub(crate) fn resolve_live_batch(document: &mut Value) {
    for setting in super::registry::live_batch() {
        let value = setting.read(document);
        set_at(document, setting.pointer, value);
    }
}

fn validate_timing(
    conn: &Connection,
    document: &Value,
    written: &[&str],
    removed: Option<&Setting>,
) -> Result<(), (u16, String)> {
    let turn = super::wake_timing::TURN_FIELDS.iter().any(|setting| written.contains(&setting.key));
    let mut ordered = super::send::ORDERED
        .iter()
        .chain(&super::wake_timing::ORDERED)
        .filter(|pair| pair.iter().any(|setting| written.contains(&setting.key)))
        .peekable();
    if !turn && ordered.peek().is_none() && !super::reply::WINDOWS.iter().flatten().any(|setting| written.contains(&setting.key)) {
        return Ok(());
    }
    let mut candidate = document.clone();
    if let Some(setting) = removed {
        remove_at(&mut candidate, setting.pointer);
    }
    for (setting, value) in read_stored_values(conn).map_err(|error| (503, error))? {
        if !set_at(&mut candidate, setting.pointer, value) && written.contains(&setting.key) {
            return Err((400, format!("config.json cannot hold {}", setting.key)));
        }
    }
    if turn {
        crate::wake::validate_turn_settings(&candidate).map_err(|error| (400, error.to_string()))?;
    }
    for [min, max] in ordered {
        if min.integer(&candidate) > max.integer(&candidate) {
            return Err((400, format!("{} <= {} is required", min.key, max.key)));
        }
    }
    for [min, median, max] in &super::reply::WINDOWS {
        if [min, median, max].iter().any(|setting| written.contains(&setting.key))
            && !(min.integer(&candidate)..=max.integer(&candidate)).contains(&median.integer(&candidate))
        {
            return Err((400, format!("{} <= {} <= {} is required", min.key, median.key, max.key)));
        }
    }
    Ok(())
}

/// Resolves settings owned by the batches that use the normalized configuration snapshot.
pub(crate) fn resolve_config(document: &mut Value) {
    let material_items = super::registry::material_item_limit(document);
    for setting in super::registry::RESOLVED_GROUPS.iter().flat_map(|group| group.iter()) {
        let value = if setting.key == super::registry::MATERIAL_ITEMS.key {
            Value::from(material_items)
        } else {
            setting.read(document)
        };
        set_at(document, setting.pointer, value);
    }
}

/// Sets the member a pointer names, creating missing or null objects on the way. Returns false, and changes nothing
/// below the first non-object, when a member on the way is not an object.
fn set_at(document: &mut Value, pointer: &str, value: Value) -> bool {
    let names: Vec<&str> = pointer.split('/').skip(1).collect();
    let Some((last, parents)) = names.split_last() else { return false };
    let mut slot = document;
    for name in parents {
        let Some(object) = slot.as_object_mut() else { return false };
        let next = object.entry(name.to_string()).or_insert(Value::Null);
        // An explicit null on the way reads as absent, so it is replaced by an object.
        if next.is_null() {
            *next = json!({});
        }
        slot = next;
    }
    match slot.as_object_mut() {
        Some(object) => {
            object.insert(last.to_string(), value);
            true
        }
        None => false,
    }
}

/// Puts each stored value at its registry pointer in `document`, unless the member the setting reads there already
/// holds it (`SettingKind::file_holds`: an id list's digit strings or alias spelling stay as they are). Returns whether
/// `document` changed and the keys whose pointer passes through a non-object (left in place).
fn place(document: &mut Value, stored: Vec<(&'static Setting, Value)>) -> (bool, Vec<&'static str>) {
    let mut changed = false;
    let mut blocked = Vec::new();
    for (setting, value) in stored {
        if setting.key == super::wake_timing::HARD_CAP.key && value.is_null() {
            if document.pointer(setting.pointer).is_some() {
                remove_at(document, setting.pointer);
                changed = true;
            }
            continue;
        }
        if setting.found_in(document).is_some_and(|found| setting.kind.file_holds(found, &value)) {
            continue;
        }
        if set_at(document, setting.pointer, value) {
            changed = true;
        } else {
            blocked.push(setting.key);
        }
    }
    (changed, blocked)
}

/// Validates the store and its sidecars through the runtime's containment checks.
fn check_paths(context: &RuntimeContext, intent: PathIntent) -> Result<std::path::PathBuf, String> {
    for name in SIDECARS {
        context.resolve_path(RuntimeArea::Data, name, intent).map_err(|_| STORAGE_ERROR.to_string())?;
    }
    context.resolve_path(RuntimeArea::Data, DB, intent).map_err(|_| STORAGE_ERROR.to_string())
}

/// The store's path for a restore's file set: the store and its sidecars pass the runtime's containment checks (no
/// link; a plain file or missing), and so does the gate file, whether or not the store exists. A missing store is a
/// path that does not exist.
pub(crate) fn store_path(context: &RuntimeContext) -> Result<std::path::PathBuf, String> {
    let path = check_paths(context, PathIntent::ReadOptionalFile)?;
    check_gate_file(context)?;
    Ok(path)
}

/// The store gate for a restore, taken with the same wait as a settings write. A writer that holds it makes the restore
/// wait and then refuse; the gate is held until the returned file is dropped.
pub(crate) fn restore_gate(context: &RuntimeContext) -> Result<std::fs::File, String> {
    store_gate(context, std::time::Instant::now() + WAL_SWITCH_WAIT)
}

/// A read-only connection to the store for the backup's online snapshot, `None` when there is no store (or no
/// selected runtime). The store and its sidecars pass the same checks as every other open (contained, and a plain
/// file with one link or missing), and the connection never checkpoints on close, so a backup never changes the
/// store. The schema is not checked: a store from a newer release is copied as it is. A store SQLite cannot read (a
/// leftover journal, a file that is not a database) fails the snapshot.
///
/// A writer's last connection removes the WAL and its index when it closes, and the next writer creates them again,
/// so a sidecar check can meet one halfway (on Windows a removed file refuses every open until its last handle
/// closes). The sidecar checks are tried again until `WAL_SWITCH_WAIT`; the database's own checks are not, so a
/// redirected or hard-linked store fails at once. Once this connection is open no writer is the last connection, so
/// the sidecars stay for the snapshot.
pub(crate) fn backup_source(context: &RuntimeContext) -> Result<Option<Connection>, String> {
    if !context.is_isolated() {
        return Ok(None);
    }
    check_gate_file(context)?;
    let path = context
        .resolve_path(RuntimeArea::Data, DB, PathIntent::ReadOptionalFile)
        .map_err(|_| STORAGE_ERROR.to_string())?;
    if file_len(&path)?.is_none() {
        return Ok(None);
    }
    refuse_hard_links(&path)?;
    let deadline = std::time::Instant::now() + WAL_SWITCH_WAIT;
    while let Err(error) = check_sidecars(context) {
        if std::time::Instant::now() >= deadline {
            return Err(error);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW)
        .map_err(|_| STORAGE_ERROR.to_string())?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).map_err(|_| STORAGE_ERROR.to_string())?;
    Ok(Some(conn))
}

/// The store's `-wal`, `-shm` and `-journal`: contained, and each a plain file with one link or missing.
fn check_sidecars(context: &RuntimeContext) -> Result<(), String> {
    for name in SIDECARS {
        let sidecar = context
            .resolve_path(RuntimeArea::Data, name, PathIntent::ReadOptionalFile)
            .map_err(|_| STORAGE_ERROR.to_string())?;
        file_len(&sidecar)?;
        refuse_hard_links(&sidecar)?;
    }
    Ok(())
}

fn schema_version(conn: &Connection) -> Result<i64, String> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(|_| STORAGE_ERROR.to_string())
}

fn refuse_version(version: i64) -> String {
    let message = if version > SCHEMA_VERSION { NEWER_SCHEMA_ERROR } else { UNSUPPORTED_SCHEMA_ERROR };
    message.to_string()
}

/// What a read-only look at the store found. Nothing here opens the store for writing, so a store this release
/// refuses is never checkpointed, rolled back or switched to WAL.
enum Inspection {
    /// No `settings.db` yet.
    Missing,
    /// No schema yet: an empty file, or version 0 without any schema object.
    Fresh,
    /// Version 1 with exactly this release's schema, open read-only and with no checkpoint on close.
    Ready(Connection),
}

/// The store gate, a file under `data/`: every write holds it from its inspection until its connection closes
/// (`write_store`).
const STORE_GATE: &str = "locks/settings-store.lock";

/// The refusal for a rollback journal beside the store. Nothing recovers it: our store has a journal only while its
/// schema is being created, so the owner removes both files.
const LEFTOVER_JOURNAL_ERROR: &str =
    "settings storage has a leftover journal; remove settings.db and settings.db-journal";

/// How long a store whose WAL is present waits for its wal-index (`-shm`): SQLite removes the index just before the
/// WAL when its last connection closes.
const SIDECAR_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// The length of a store file, `None` when it is missing; anything but a plain file is refused.
fn file_len(path: &std::path::Path) -> Result<Option<u64>, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        _ => Err(STORAGE_ERROR.to_string()),
    }
}

/// Refuses a store file with another hard link: the containment checks see paths, not files, so a second name for
/// another database would pass them. A missing file passes.
fn refuse_hard_links(path: &std::path::Path) -> Result<(), String> {
    match link_count(path) {
        Ok(1) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(STORAGE_ERROR.to_string()),
    }
}

#[cfg(unix)]
fn link_count(path: &std::path::Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).map(|metadata| metadata.nlink())
}

#[cfg(windows)]
fn link_count(path: &std::path::Path) -> std::io::Result<u64> {
    file_link_count(&std::fs::File::open(path)?)
}

/// The hard links of an open file.
#[cfg(unix)]
fn file_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().map(|metadata| metadata.nlink())
}

/// The hard links of an open file.
#[cfg(windows)]
fn file_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value of this plain C struct.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle stays open for the call and `information` is a valid, writable out pointer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(u64::from(information.nNumberOfLinks))
}

/// The gate file (`locks/settings-store.lock`) must be a plain file with one link or missing, whether or not the store
/// exists: a second name for another file would otherwise pass the store's checks and keep an invalid gate.
fn check_gate_file(context: &RuntimeContext) -> Result<(), String> {
    let path = context
        .resolve_path(RuntimeArea::Data, STORE_GATE, PathIntent::ReadOptionalFile)
        .map_err(|_| STORAGE_ERROR.to_string())?;
    file_len(&path)?;
    refuse_hard_links(&path)
}

/// Takes the store gate, waiting until `deadline`. The gate file is opened read-write and locked, so it must be a
/// plain file with one link before it is opened, and still have one once it is open; a second name for another file
/// is refused. `Gate::Create`'s other callers keep their own checks.
fn store_gate(context: &RuntimeContext, deadline: std::time::Instant) -> Result<std::fs::File, String> {
    check_gate_file(context)?;
    loop {
        match context.open_locked_file(RuntimeArea::Data, STORE_GATE) {
            Ok(gate) if file_link_count(&gate).is_ok_and(|links| links == 1) => return Ok(gate),
            Ok(_) => return Err(STORAGE_ERROR.to_string()),
            Err(error) if error == "runtime file is already locked" && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return Err(STORAGE_ERROR.to_string()),
        }
    }
}

/// Whitespace runs made one space, and no space next to `(`, `)`, `,` or `;`, so stored CREATE text compares equal
/// whatever its layout.
fn normalized_sql(sql: &str) -> String {
    let mut normalized = String::with_capacity(sql.len());
    let mut space = false;
    for c in sql.trim().chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !normalized.ends_with(['(', ')', ',', ';']) && !matches!(c, '(' | ')' | ',' | ';') {
            normalized.push(' ');
        }
        space = false;
        normalized.push(c);
    }
    normalized
}

/// The schema on an open connection: `Ok(false)` for version 0 with no schema object, `Ok(true)` for version 1 or 2
/// with exactly this release's schema of that version (`has_settings_schema`); anything else is refused.
fn check_schema(conn: &Connection) -> Result<bool, String> {
    match schema_version(conn)? {
        0 => {
            let objects: i64 = conn
                .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
                .map_err(|_| STORAGE_ERROR.to_string())?;
            if objects == 0 { Ok(false) } else { Err(STORAGE_ERROR.to_string()) }
        }
        version @ 1..=SCHEMA_VERSION if has_settings_schema(conn, version) => Ok(true),
        1..=SCHEMA_VERSION => Err(STORAGE_ERROR.to_string()),
        version => Err(refuse_version(version)),
    }
}

/// Looks at the store without opening it for writing. Every store file (the database, its `-wal`, `-shm` and `-journal`,
/// and the store gate) must be a plain file with one link or missing, whether or not the database exists. A leftover
/// rollback journal (beside a missing database, or hot: no connection holds the write lock) is refused, and nothing
/// recovers it; a live writer's journal is waited for. A file that is not a SQLite database, a version other than 0, 1
/// or 2 (read from the file header while no WAL holds later commits), a version 0 with any schema object and a version
/// 1 or 2 whose schema is not exactly this release's of that version are refused too. Otherwise a read-only connection
/// that never checkpoints on close reads the schema; only SQLite's shared-memory index (`-shm`) may be rebuilt
/// meanwhile.
fn inspect(context: &RuntimeContext) -> Result<Inspection, String> {
    inspect_store(context, true)
}

fn inspect_store(context: &RuntimeContext, again: bool) -> Result<Inspection, String> {
    let resolve = |name: &str| {
        context.resolve_path(RuntimeArea::Data, name, PathIntent::ReadOptionalFile).map_err(|_| STORAGE_ERROR.to_string())
    };
    let path = resolve(DB)?;
    let (wal, shm, journal, gate) =
        (resolve(SIDECARS[0])?, resolve(SIDECARS[1])?, resolve(SIDECARS[2])?, resolve(STORE_GATE)?);
    // SQLite opens, truncates or removes the sidecars even when it creates the database.
    for file in [&path, &wal, &shm, &journal, &gate] {
        file_len(file)?;
        refuse_hard_links(file)?;
    }
    let journal_len = file_len(&journal)?.unwrap_or(0);
    let wal_len = file_len(&wal)?.unwrap_or(0);
    let Some(len) = file_len(&path)? else {
        return if journal_len > 0 { Err(LEFTOVER_JOURNAL_ERROR.to_string()) } else { Ok(Inspection::Missing) };
    };
    if len == 0 {
        // An empty database has no page a journal could restore: a journal beside it is a live writer's (`BEGIN
        // IMMEDIATE` on a new store writes one) or one SQLite discards, and the writer's open waits for a live lock.
        return if wal_len == 0 { Ok(Inspection::Fresh) } else { Err(STORAGE_ERROR.to_string()) };
    }
    let header = header_version(&path)?;
    if wal_len == 0 {
        // The header is current while no WAL holds later commits.
        if !(0..=SCHEMA_VERSION).contains(&header) {
            return Err(refuse_version(header));
        }
    } else {
        // A read-only connection would create a missing index; wait for a closing connection to finish instead.
        let settled = std::time::Instant::now() + SIDECAR_SETTLE;
        while file_len(&shm)?.is_none() {
            if file_len(&wal)?.is_none() {
                return if again { inspect_store(context, false) } else { Err(STORAGE_ERROR.to_string()) };
            }
            if std::time::Instant::now() >= settled {
                return Err(STORAGE_ERROR.to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW)
        .map_err(|_| STORAGE_ERROR.to_string())?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).map_err(|_| STORAGE_ERROR.to_string())?;
    conn.execute_batch("PRAGMA busy_timeout=5000;").map_err(|_| STORAGE_ERROR.to_string())?;
    // A journal is hot when no connection holds the store's write lock: a leftover, which this read-only connection
    // refuses to play back. A live writer's journal is not hot, and its lock is waited for under `busy_timeout`.
    if let Err(rusqlite::Error::SqliteFailure(error, _)) =
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
    {
        if error.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK {
            return Err(LEFTOVER_JOURNAL_ERROR.to_string());
        }
    }
    if check_schema(&conn)? { Ok(Inspection::Ready(conn)) } else { Ok(Inspection::Fresh) }
}

/// The schema version in a SQLite file's header (bytes 60-63); a file without the SQLite magic is refused.
fn header_version(path: &std::path::Path) -> Result<i64, String> {
    let mut header = [0u8; 100];
    std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut header))
        .map_err(|_| STORAGE_ERROR.to_string())?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(STORAGE_ERROR.to_string());
    }
    Ok(i64::from(i32::from_be_bytes([header[60], header[61], header[62], header[63]])))
}

/// The stored values of registry settings, read through the inspection's read-only connection. No store or no schema
/// yet is an empty list; a store with a journal is unreadable. Rows with a key the registry does not have are skipped,
/// and so are rows with a value their setting does not keep (`storable`), except for a text setting: its last good
/// version (`last_good`) stands in, and the refusal is reported once per key and process on stderr.
fn stored_values(context: &RuntimeContext) -> Result<Vec<(&'static Setting, Value)>, String> {
    let conn = match inspect(context)? {
        Inspection::Missing | Inspection::Fresh => return Ok(Vec::new()),
        Inspection::Ready(conn) => conn,
    };
    read_stored_values(&conn)
}

/// The stored values of registry settings on an open connection of the store; rows with a key the registry does not
/// have, or a value its setting does not accept, are skipped.
fn read_stored_values(conn: &Connection) -> Result<Vec<(&'static Setting, Value)>, String> {
    let mut statement = conn.prepare("SELECT key, value FROM settings").map_err(|_| STORAGE_ERROR.to_string())?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|_| STORAGE_ERROR.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| STORAGE_ERROR.to_string())?;
    let mut values = Vec::new();
    for (key, text) in rows {
        let Some(setting) = find(&key).filter(|setting| !setting.secret) else { continue };
        match serde_json::from_str::<Value>(&text).ok().and_then(|value| setting.storable(&value)) {
            Some(value) => values.push((setting, value)),
            None if setting.kind.text_default().is_some() => {
                report_refused_text(setting.key);
                if let Some(value) = last_good(conn, setting, |_| true) {
                    values.push((setting, value));
                }
            }
            None => {}
        }
    }
    Ok(values)
}

/// Presence check for the restore preview. The verified archive may predate the settings table.
pub(crate) fn archive_has_endpoint_credentials(conn: &Connection) -> Result<bool, String> {
    let failed = |_: rusqlite::Error| "backup settings credential status is unavailable".to_string();
    let has_settings: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='settings')",
        [], |row| row.get(0),
    ).map_err(failed)?;
    if !has_settings {
        return Ok(false);
    }
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings WHERE key IN (?1, ?2) AND value <> 'null')",
        params![WEB_SEARCH_API_KEY.key, CLI_MODEL_API_KEY.key],
        |row| row.get(0),
    ).map_err(failed)
}

/// Only the credential readers and redaction use these rows. Ordinary settings never decode them.
fn read_secrets(context: &RuntimeContext) -> Result<Vec<(&'static str, String, String)>, String> {
    if !context.is_isolated() {
        return Ok(Vec::new());
    }
    match inspect(context)? {
        Inspection::Ready(conn) => secrets_in(&conn),
        Inspection::Missing | Inspection::Fresh => Ok(Vec::new()),
    }
}

fn secret_row(conn: &Connection, setting: &Setting) -> Result<Option<(String, String)>, String> {
    let Some((text, _)) = stored_row(conn, setting.key).map_err(|(_, error)| error)? else { return Ok(None) };
    if text == "null" {
        return Ok(None);
    }
    let (endpoint, credential): (String, String) = serde_json::from_str(&text).map_err(|_| STORAGE_ERROR.to_string())?;
    if endpoint.is_empty() || setting.normalize(&json!(credential)).is_none() {
        return Err(STORAGE_ERROR.to_string());
    }
    Ok(Some((endpoint, credential)))
}

fn secrets_in(conn: &Connection) -> Result<Vec<(&'static str, String, String)>, String> {
    let mut secrets = Vec::new();
    for setting in [&WEB_SEARCH_API_KEY, &CLI_MODEL_API_KEY] {
        if let Some((endpoint, credential)) = secret_row(conn, setting)? {
            secrets.push((setting.key, endpoint, credential));
        }
    }
    Ok(secrets)
}

pub(crate) fn secret_value(context: &RuntimeContext, setting: &Setting, endpoint: &str) -> Result<Option<String>, String> {
    if !setting.secret || !context.is_isolated() {
        return Ok(None);
    }
    let Inspection::Ready(conn) = inspect(context)? else { return Ok(None) };
    Ok(secret_row(&conn, setting)?.filter(|(bound, _)| bound == endpoint).map(|(_, credential)| credential))
}

/// The settings row revision partitions the per-process search cache after a credential change.
pub(crate) fn secret_revision(context: &RuntimeContext, setting: &Setting) -> Option<i64> {
    if !context.is_isolated() {
        return None;
    }
    let Inspection::Ready(conn) = inspect(context).ok()? else { return None };
    stored_row(&conn, setting.key).ok().flatten().map(|(_, revision)| revision)
}

pub(crate) fn stored_secrets(context: &RuntimeContext) -> Result<Vec<String>, String> {
    let mut secrets: Vec<_> = read_secrets(context)?.into_iter().map(|(_, _, credential)| credential).collect();
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    Ok(secrets)
}

pub(crate) fn harness_credential(context: &RuntimeContext) -> Option<String> {
    if !context.is_isolated() {
        return None;
    }
    let text = context.read_text(RuntimeArea::Config, "harness-api-key").ok()?;
    let key = text.trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn reject_credential_text(
    conn: &Connection, setting: &Setting, value: &Value, harness_key: Option<&str>,
) -> Result<(), (u16, String)> {
    let prompt_text = value.as_str().filter(|_| setting.kind.text_default().is_some());
    let routes = value.as_array().filter(|_| setting.key == HARNESS_ROUTES.key);
    if prompt_text.is_none() && routes.is_none() {
        return Ok(());
    }
    let secrets = secrets_in(conn).map_err(|error| (503, error))?;
    let has_endpoint_key = |text: &str| secrets.iter().any(|(_, _, key)| text.contains(key));
    if prompt_text.is_some_and(|text| harness_key.is_some_and(|key| text.contains(key)) || has_endpoint_key(text))
        || routes.is_some_and(|routes| routes.iter().any(|route| {
            ["id", "model"].iter().filter_map(|field| route[*field].as_str()).any(has_endpoint_key)
        }))
    {
        return Err((400, "setting text contains a credential".to_string()));
    }
    Ok(())
}

/// The last good version of a text setting whose stored value is refused (a row changed outside `apply`, or a denied
/// term a later release added): the newest value in the global audit that the setting accepts, such as the default a
/// reset stored. A global audit row without a new value (an undo that removed the stored value) ends the search.
/// When there is no usable row, the file's value or the default applies.
fn last_good(conn: &Connection, setting: &Setting, accepted: impl Fn(&Value) -> bool) -> Option<Value> {
    let mut statement =
        conn.prepare("SELECT new_value FROM settings_audit WHERE scope='global' AND key=?1 ORDER BY id DESC").ok()?;
    let mut rows = statement.query([setting.key]).ok()?;
    while let Ok(Some(row)) = rows.next() {
        let text: Option<String> = row.get(0).ok()?;
        let value = serde_json::from_str::<Value>(&text?)
            .ok()
            .and_then(|value| setting.normalize(&value))
            .filter(|value| accepted(value));
        if value.is_some() {
            return value;
        }
    }
    None
}

/// The newest audited text of text setting `key` that the registry accepts and that `accepted` takes, read through the
/// store's read-only connection; `None` when there is no store, no such row, or the search ends (`last_good`). Used when
/// a text that passed the load check fails the delivery credential check.
pub(crate) fn last_good_text(context: &RuntimeContext, key: &str, accepted: impl Fn(&str) -> bool) -> Option<String> {
    let setting = find(key)?;
    let Ok(Inspection::Ready(conn)) = inspect(context) else { return None };
    last_good(&conn, setting, |value| value.as_str().is_some_and(&accepted))?.as_str().map(str::to_string)
}

/// One stderr line per text setting and process when its stored value is refused at load.
fn report_refused_text(key: &'static str) {
    static REPORTED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut reported = REPORTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !reported.contains(&key) {
        reported.push(key);
        crate::safe_eprintln!("[settings] {key}: the stored text is refused; its last good version or the default applies");
    }
}

/// How long a writer waits for the store gate and for other connections to let go while it switches a new store to
/// WAL (the same as `busy_timeout`).
const WAL_SWITCH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Puts the store in WAL mode. The switch asks for the write lock while it holds a read lock, and SQLite then answers
/// SQLITE_BUSY at once, without `busy_timeout`, when another connection in any process holds the write lock. The
/// caller holds `locks/settings-store.lock` (`write_store`), so a store not yet in WAL is switched one writer at a
/// time, and the switch is retried while another connection holds a lock, until `WAL_SWITCH_WAIT`. A store already in
/// WAL is left as it is.
fn enable_wal(conn: &Connection) -> Result<(), String> {
    let mode = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, String>(0));
    if mode("PRAGMA journal_mode").is_ok_and(|mode| mode.eq_ignore_ascii_case("wal")) {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + WAL_SWITCH_WAIT;
    loop {
        let busy = match mode("PRAGMA journal_mode=WAL") {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(_) => true,
            Err(rusqlite::Error::SqliteFailure(error, _)) => {
                matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            }
            Err(_) => false,
        };
        #[cfg(test)]
        if busy {
            WAL_SWITCH_BUSY.fetch_add(1, Ordering::SeqCst);
        }
        if !busy || std::time::Instant::now() >= deadline {
            return Err(STORAGE_ERROR.to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Schema version 1 as (type, name, table, CREATE text): the statements this release runs, in order.
const SCHEMA_V1: [(&str, &str, &str, &str); 4] = [
    (
        "table",
        "settings",
        "settings",
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, revision INTEGER NOT NULL, \
         updated_ms INTEGER NOT NULL, updated_by TEXT NOT NULL)",
    ),
    (
        "table",
        "settings_audit",
        "settings_audit",
        "CREATE TABLE settings_audit (id INTEGER PRIMARY KEY AUTOINCREMENT, at_ms INTEGER NOT NULL, \
         actor TEXT NOT NULL, source TEXT NOT NULL, scope TEXT NOT NULL, key TEXT NOT NULL, old_value TEXT, \
         new_value TEXT, request_id TEXT NOT NULL)",
    ),
    (
        "trigger",
        "settings_audit_no_update",
        "settings_audit",
        "CREATE TRIGGER settings_audit_no_update BEFORE UPDATE ON settings_audit \
         BEGIN SELECT RAISE(ABORT,'settings audit is append only'); END",
    ),
    (
        "trigger",
        "settings_audit_no_delete",
        "settings_audit",
        "CREATE TRIGGER settings_audit_no_delete BEFORE DELETE ON settings_audit \
         BEGIN SELECT RAISE(ABORT,'settings audit is append only'); END",
    ),
];

/// The objects SQLite adds for schema version 1: the primary-key index of `settings` (no text) and `sqlite_sequence`
/// for the AUTOINCREMENT key of `settings_audit`.
const SCHEMA_V1_IMPLICIT: [(&str, &str, &str, Option<&str>); 2] = [
    ("index", "sqlite_autoindex_settings_1", "settings", None),
    ("table", "sqlite_sequence", "sqlite_sequence", Some("CREATE TABLE sqlite_sequence(name,seq)")),
];

/// What schema version 2 adds to version 1 (#218), as (type, name, table, CREATE text): the values kept per
/// conversation, one row per conversation and setting (`store/conversation.rs`).
const SCHEMA_V2_ADDED: [(&str, &str, &str, &str); 1] = [(
    "table",
    "conversation_settings",
    "conversation_settings",
    "CREATE TABLE conversation_settings (conversation TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, \
     revision INTEGER NOT NULL, updated_ms INTEGER NOT NULL, updated_by TEXT NOT NULL, PRIMARY KEY (conversation, key))",
)];

/// The object SQLite adds for schema version 2: the primary-key index of `conversation_settings` (no text).
const SCHEMA_V2_IMPLICIT: [(&str, &str, &str, Option<&str>); 1] =
    [("index", "sqlite_autoindex_conversation_settings_1", "conversation_settings", None)];

/// Whether `sqlite_master` holds exactly schema version `version` (1, or 2 with `SCHEMA_V2_ADDED`): the same objects
/// (type, name, table) with the same CREATE text after `normalized_sql`, and nothing else. A store that claims the
/// version without it is refused before it is touched, so the WAL switch never rewrites its header.
fn has_settings_schema(conn: &Connection, version: i64) -> bool {
    type Object = (String, String, String, Option<String>);
    let Ok(mut statement) = conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master") else {
        return false;
    };
    let found = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?))
        })
        .and_then(|rows| rows.collect::<Result<Vec<Object>, _>>());
    let Ok(mut found) = found else { return false };
    let added: &[(&str, &str, &str, &str)] = if version >= 2 { &SCHEMA_V2_ADDED } else { &[] };
    let added_implicit: &[(&str, &str, &str, Option<&str>)] = if version >= 2 { &SCHEMA_V2_IMPLICIT } else { &[] };
    let mut expected: Vec<Object> = SCHEMA_V1
        .iter()
        .chain(added)
        .map(|(kind, name, table, sql)| (kind.to_string(), name.to_string(), table.to_string(), Some(sql.to_string())))
        .chain(
            SCHEMA_V1_IMPLICIT
                .iter()
                .chain(added_implicit)
                .map(|(kind, name, table, sql)| (kind.to_string(), name.to_string(), table.to_string(), sql.map(str::to_string))),
        )
        .collect();
    for objects in [&mut found, &mut expected] {
        for object in objects.iter_mut() {
            object.3 = object.3.as_deref().map(normalized_sql);
        }
        objects.sort();
    }
    found == expected
}

/// Opens the store for a write: created when missing, given its schema in one transaction when new, refused when its
/// schema is newer than this release's (checked before anything is written). A new store gets schema version 1; a
/// write that keeps a conversation's value migrates it to version 2 inside its own transaction (`migrate`). The caller
/// holds the store gate (`write_store`).
fn open_for_write(context: &RuntimeContext) -> Result<Connection, String> {
    // Refuse what this release cannot use before any connection that can write opens the store.
    drop(inspect(context)?);
    let path = check_paths(context, PathIntent::WriteFile)?;
    // Protect existing files before opening a write connection. SQLite's Unix sidecars use the database mode.
    match crate::guarded_credentials::create_settings_file(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(STORAGE_ERROR.to_string()),
    }
    for file in std::iter::once(path.clone()).chain(SIDECARS.map(|name| path.with_file_name(name))) {
        match crate::guarded_credentials::restrict_settings_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && file != path => {}
            Err(_) => return Err(STORAGE_ERROR.to_string()),
        }
    }
    let mut conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| STORAGE_ERROR.to_string())?;
    // No checkpoint on close until the store is known to be usable, so a refusal below leaves it as it was.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).map_err(|_| STORAGE_ERROR.to_string())?;
    conn.execute_batch("PRAGMA busy_timeout=5000;").map_err(|_| STORAGE_ERROR.to_string())?;
    // The same checks again, for a store changed since the inspection.
    check_schema(&conn)?;
    enable_wal(&conn)?;
    conn.execute_batch("PRAGMA synchronous=FULL;").map_err(|_| STORAGE_ERROR.to_string())?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| STORAGE_ERROR.to_string())?;
    // BEGIN IMMEDIATE opens and pins the Windows WAL and its index before any row is written.
    // The journal used for switching to WAL carries SQLite's database header page only.
    #[cfg(windows)]
    for name in &SIDECARS[..2] {
        crate::guarded_credentials::restrict_settings_file(&path.with_file_name(name))
            .map_err(|_| STORAGE_ERROR.to_string())?;
    }
    match schema_version(&tx)? {
        0 => {
            for (_, _, _, sql) in SCHEMA_V1 {
                tx.execute_batch(sql).map_err(|_| STORAGE_ERROR.to_string())?;
            }
            tx.execute_batch("PRAGMA user_version=1;").map_err(|_| STORAGE_ERROR.to_string())?;
        }
        1..=SCHEMA_VERSION => {}
        version => return Err(refuse_version(version)),
    }
    tx.commit().map_err(|_| STORAGE_ERROR.to_string())?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, false).map_err(|_| STORAGE_ERROR.to_string())?;
    Ok(conn)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// A request id for audit rows: the time, the process and a per-process sequence.
fn request_id(at: i64) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    format!("{at}-{}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed))
}

/// The stored `config.json` as a JSON object (a leading UTF-8 BOM accepted), never the overlaid view.
fn raw_config(context: &RuntimeContext) -> Result<Value, String> {
    let text = context.read_text(RuntimeArea::Config, CONFIG).map_err(|_| CONFIG_ERROR.to_string())?;
    serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| CONFIG_ERROR.to_string())
}

/// Keeps `config/config.json.pre-settings`, a byte-for-byte copy of `config.json` from before settings first wrote
/// anything: made when missing, never replaced. Anything there but a plain file is refused and left alone. The caller
/// holds the configuration gate, so no other writer changes `config.json` between the check and the copy.
fn keep_pre_settings_copy(context: &RuntimeContext) -> Result<(), String> {
    let path = context
        .resolve_path(RuntimeArea::Config, PRE_SETTINGS_COPY, PathIntent::ReadOptionalFile)
        .map_err(|_| COPY_ERROR.to_string())?;
    if file_len(&path).map_err(|_| COPY_ERROR.to_string())?.is_some() {
        return Ok(());
    }
    let bytes = context.read_file(RuntimeArea::Config, CONFIG).map_err(|_| CONFIG_ERROR.to_string())?;
    context.write_atomic(RuntimeArea::Config, PRE_SETTINGS_COPY, &bytes).map_err(|_| COPY_ERROR.to_string())
}

/// Copies into the store, inside the writer's transaction (`write_store`), every registry setting that has never been
/// in it (no row and no audit row) and whose kind can seed a value from `document`, the stored `config.json`:
/// revision 1 and one audit row each (actor `system`, source `migration`, one request id for the batch). Later
/// writes retry values that could not be seeded. A setting a later release adds is seeded by the first write after
/// the upgrade, and nothing is seeded twice. A seeded value's
/// one audit row keeps `record_change`'s revision rule: no revision is ever reused. The value is the kind's
/// (`SettingKind::seed`) of the member the setting reads (`Setting::found_in`, an alias spelling included).
///
/// Other than rollout, a security setting is seeded only by a write of itself (`written` names the keys the
/// write changes), so a write of another setting never takes the access lists over from `config.json`, and its first
/// change's undo restores the file's value. When `config.json` holds a value for it that the store cannot keep (an id
/// list over its limit, an owner that is not an id), that write is 409 and nothing is written: its undo could not
/// restore the file's value. Rollout seeds only when its file value can launch and is within the effective allow lists.
fn seed(
    conn: &Connection, document: &Value, at: i64, written: &[&'static str], harness_key: Option<&str>,
) -> Result<(), (u16, String)> {
    let failed = |_: rusqlite::Error| (503, STORAGE_ERROR.to_string());
    let request = request_id(at);
    for setting in registry() {
        if setting.secret {
            continue;
        }
        let own_write_only = setting.confirmed() && setting.key != ROLLOUT_GROUPS.key;
        if own_write_only && !written.contains(&setting.key) {
            continue;
        }
        let Some(found) = setting.found_in(document) else { continue };
        let known: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1) OR EXISTS(SELECT 1 FROM settings_audit WHERE key=?1)",
                [setting.key],
                |row| row.get(0),
            )
            .map_err(failed)?;
        if known {
            continue;
        }
        if setting.key == ROLLOUT_GROUPS.key && !rollout_can_seed(conn, document, found)? {
            continue;
        }
        let Some(value) = setting.kind.seed(found) else {
            if own_write_only {
                return Err((
                    409,
                    format!(
                        "config.json holds a value of {} the settings store cannot keep ({}); correct it there first",
                        setting.key,
                        setting.refusal(found)
                    ),
                ));
            }
            continue;
        };
        if reject_credential_text(conn, setting, &value, harness_key).is_err() {
            continue;
        }
        let text = value.to_string();
        conn.execute(
            "INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES(?1, ?2, 1, ?3, ?4)",
            params![setting.key, text, at, SEED_ACTOR],
        )
        .map_err(failed)?;
        conn.execute(
            "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
             VALUES(?1, ?2, 'migration', 'global', ?3, NULL, ?4, ?5)",
            params![at, SEED_ACTOR, setting.key, text, request],
        )
        .map_err(failed)?;
    }
    Ok(())
}

/// Writes every stored value into `config.json` at its registry pointer, through one atomic write and only when the
/// file differs, so a release without the store reads the current values. A stored `null` (a model id reset to its
/// missing default) is written as `null`, which every release reads as not configured; a setting the store does not
/// hold keeps the file's value. The caller holds the configuration gate. Before the first change to the file,
/// `config.json.pre-settings` is kept. Returns the keys whose pointer passes through a non-object in the file (left in
/// place); a file `canonical_config` refuses is not written. No store, no schema yet and no stored value write
/// nothing; a store that cannot be read is an error.
pub(crate) fn write_back(context: &RuntimeContext) -> Result<Vec<&'static str>, String> {
    if !context.is_isolated() {
        return Ok(Vec::new());
    }
    write_back_values(context, stored_values(context)?)
}

/// `write_back` of stored values already read: by a writer, on its own read-write connection before it closed, so that
/// close stays the store's last and removes the WAL and its index (a read-only connection opened after it would leave
/// `settings.db-wal` and `settings.db-shm` behind).
fn write_back_values(
    context: &RuntimeContext,
    stored: Vec<(&'static Setting, Value)>,
) -> Result<Vec<&'static str>, String> {
    if stored.is_empty() {
        return Ok(Vec::new());
    }
    let mut document = raw_config(context)?;
    if crate::config::canonical_config(&document).is_err() {
        return Err(CONFIG_ERROR.to_string());
    }
    let (changed, blocked) = place(&mut document, stored);
    if changed {
        #[cfg(test)]
        if FAIL_WRITE_BACK.with(|fail| fail.replace(false)) {
            return Err(CONFIG_ERROR.to_string());
        }
        keep_pre_settings_copy(context)?;
        let text = serde_json::to_string_pretty(&document).map_err(|_| CONFIG_ERROR.to_string())?;
        context.write_atomic(RuntimeArea::Config, CONFIG, text.as_bytes()).map_err(|_| CONFIG_ERROR.to_string())?;
    }
    Ok(blocked)
}

/// Writes `stored`, read by a writer after its commit, back into `config.json` (`write_back_values`): `None` when
/// `config.json` holds every stored value, else why not. The stored change stands either way, and the next write or
/// the console's start finishes the write-back. A failure is reported on stderr.
fn mirror_after_commit(
    context: &RuntimeContext,
    stored: Result<Vec<(&'static Setting, Value)>, String>,
) -> Option<String> {
    let mirror_error = match stored.and_then(|stored| write_back_values(context, stored)) {
        Ok(blocked) if blocked.is_empty() => None,
        Ok(blocked) => Some(format!("config.json cannot hold {}", blocked.join(", "))),
        Err(error) => Some(error),
    };
    if let Some(error) = &mirror_error {
        crate::safe_eprintln!("[settings] config.json does not hold the stored settings: {error}");
    }
    mirror_error
}

/// Puts every stored value into `document`, a `config.json` a console writer is about to write, so the writer never
/// puts back an older value of a stored setting. Returns whether `document` changed; a change keeps
/// `config.json.pre-settings` first, and a copy that cannot be kept is an error. A legacy context, a document that is
/// not an object and a store that cannot be read (the overlay reports it) change nothing.
pub(crate) fn mirror_into(context: &RuntimeContext, document: &mut Value) -> Result<bool, String> {
    if !context.is_isolated() || !document.is_object() {
        return Ok(false);
    }
    let Ok(stored) = stored_values(context) else { return Ok(false) };
    let (changed, _) = place(document, stored);
    if changed {
        keep_pre_settings_copy(context)?;
    }
    Ok(changed)
}

/// Whether the store holds a value `write_back` would write; false for a legacy context, no store and a store that
/// cannot be read. Reports a read error on stderr. Reads only: nothing is created.
pub(crate) fn has_stored_values(context: &RuntimeContext) -> bool {
    if !context.is_isolated() {
        return false;
    }
    match stored_values(context) {
        Ok(stored) => !stored.is_empty(),
        Err(error) => {
            crate::safe_eprintln!("[settings] could not read stored settings for write-back: {error}");
            false
        }
    }
}

/// Creates the store with its schema and no value, as a crash after the schema commit leaves it (test seam). It takes
/// the store gate like every writer.
#[cfg(test)]
pub(crate) fn create_store(context: &RuntimeContext) -> Result<(), String> {
    let _gate = store_gate(context, std::time::Instant::now() + WAL_SWITCH_WAIT)?;
    open_for_write(context).map(drop)
}

/// Removes `setting`'s member from `config.json`, for an undo that removes the stored value: the file holds the
/// written-back copy of that value, which would otherwise go on applying once the store no longer holds it. It runs
/// inside the undo's transaction, before the commit: a failure refuses the undo and rolls it back, and a commit that
/// fails after it leaves the stored value, which the overlay still applies and the next write-back puts back into the
/// file. A missing member writes nothing. The caller holds the configuration gate.
fn remove_from_config(context: &RuntimeContext, setting: &Setting) -> Result<(), String> {
    let mut document = raw_config(context)?;
    if crate::config::canonical_config(&document).is_err() {
        return Err(CONFIG_ERROR.to_string());
    }
    if document.pointer(setting.pointer).is_none() {
        return Ok(());
    }
    remove_at(&mut document, setting.pointer);
    #[cfg(test)]
    if FAIL_WRITE_BACK.with(|fail| fail.replace(false)) {
        return Err(CONFIG_ERROR.to_string());
    }
    keep_pre_settings_copy(context)?;
    let text = serde_json::to_string_pretty(&document).map_err(|_| CONFIG_ERROR.to_string())?;
    context.write_atomic(RuntimeArea::Config, CONFIG, text.as_bytes()).map_err(|_| CONFIG_ERROR.to_string())
}

/// Runs one write under the store gate. The caller holds the configuration gate (`console::config_gate`: the console's
/// settings routes take it through their authority, the chat settings tools through `memory_actions::handle`), so the
/// order is always the configuration gate, then the store gate, never the reverse. The store gate is taken (waiting up
/// to `WAL_SWITCH_WAIT`, else 503) before the inspection and held until the connection closes, so no other writer of
/// this store can stop between this writer's inspection and its read-write open and leave a journal that open would
/// play back. With `existing_only`, a store that is missing or fresh is 404 under the gate and none is created. Then
/// `config.json` is read (not a JSON object: 503 before any store is created), the store is opened,
/// `config.json.pre-settings` is kept (503 when it cannot be) and, in one immediate transaction, the store is seeded
/// from `config.json` (`seed`) before `write` runs; the transaction commits when `write` returns `Ok` and rolls back,
/// seed included, otherwise. After the commit the stored values are read on the same connection, so its read-write
/// close is the store's last and leaves no `-wal` or `-shm`; with the connection closed and the store gate released
/// they are written back into `config.json` (`write_back_values`). The second value says why `config.json` does not
/// hold every stored value (`None` when it does); the stored change stands either way. `written` names the keys the
/// write changes, for the seed of the security settings (`seed`).
fn write_store<T>(
    context: &RuntimeContext,
    existing_only: bool,
    written: &[&'static str],
    write: impl FnOnce(&Connection, &Value) -> Result<T, (u16, String)>,
) -> Result<(T, Option<String>), (u16, String)> {
    write_store_with(context, existing_only, written, false, write)
}

/// `write_store`, and #218: a write that keeps a conversation's value (`conversations`) first migrates a version-1
/// store to version 2 inside the same transaction (`migrate`), and every write that changes the stored value of an
/// access setting (`Setting::access`: an allow or deny list, the owner) then clears, still inside its transaction, the
/// values of every conversation that is no longer admitted (`conversation::clear_unadmitted`), so a commit holds both
/// or neither.
fn write_store_with<T>(
    context: &RuntimeContext,
    existing_only: bool,
    written: &[&'static str],
    conversations: bool,
    write: impl FnOnce(&Connection, &Value) -> Result<T, (u16, String)>,
) -> Result<(T, Option<String>), (u16, String)> {
    let unavailable = |error: String| -> (u16, String) { (503, error) };
    let harness_key = harness_credential(context);
    let (result, stored) = {
        let _gate = store_gate(context, std::time::Instant::now() + WAL_SWITCH_WAIT).map_err(unavailable)?;
        if existing_only && matches!(inspect(context).map_err(unavailable)?, Inspection::Missing | Inspection::Fresh) {
            return Err((404, UNKNOWN_ENTRY_ERROR.to_string()));
        }
        let document = raw_config(context).map_err(unavailable)?;
        let mut conn = open_for_write(context).map_err(unavailable)?;
        keep_pre_settings_copy(context).map_err(unavailable)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| unavailable(STORAGE_ERROR.to_string()))?;
        if conversations {
            migrate(&tx).map_err(unavailable)?;
        }
        seed(&tx, &document, now_ms(), written, harness_key.as_deref())?;
        let access_before = access_rows(&tx)?;
        let result = write(&tx, &document)?;
        if access_rows(&tx)? != access_before {
            conversation::clear_unadmitted(&tx, context)?;
        }
        #[cfg(test)]
        if PANIC_BEFORE_COMMIT.with(|panic| panic.replace(false)) {
            panic!("settings: simulated crash before the store commit");
        }
        tx.commit().map_err(|_| unavailable(STORAGE_ERROR.to_string()))?;
        // Read on this connection, so its read-write close below stays the store's last close and removes the WAL and
        // its index. A failed read is a write-back error, reported in the reply; the committed write stands.
        let stored = read_stored_values(&conn);
        (result, stored)
    };
    Ok((result, mirror_after_commit(context, stored)))
}

/// Migrates a version-1 store to schema version 2 inside the caller's immediate transaction: `conversation_settings`
/// is created and `user_version` set to 2 together, so a crash or a refusal before the commit leaves version 1 as it
/// was (`PRAGMA user_version` is part of the transaction). A version-2 store is left as it is. Only a write that keeps a
/// conversation's value migrates, so a store that never held one stays at version 1, which the previous release opens.
fn migrate(conn: &Connection) -> Result<(), String> {
    if schema_version(conn)? >= 2 {
        return Ok(());
    }
    for (_, _, _, sql) in SCHEMA_V2_ADDED {
        conn.execute_batch(sql).map_err(|_| STORAGE_ERROR.to_string())?;
    }
    #[cfg(test)]
    if FAIL_MIGRATION.with(|fail| fail.replace(false)) {
        return Err(STORAGE_ERROR.to_string());
    }
    conn.execute_batch("PRAGMA user_version=2;").map_err(|_| STORAGE_ERROR.to_string())
}

/// The stored rows of the access settings (`Setting::access`), as (key, stored text), ordered by key, read in the
/// caller's transaction: a write whose rows differ after it changed access.
fn access_rows(conn: &Connection) -> Result<Vec<(String, String)>, (u16, String)> {
    let mut statement =
        conn.prepare("SELECT key, value FROM settings ORDER BY key").map_err(|_| (503, STORAGE_ERROR.to_string()))?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|_| (503, STORAGE_ERROR.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| (503, STORAGE_ERROR.to_string()))?;
    Ok(rows.into_iter().filter(|(key, _)| find(key).is_some_and(Setting::access)).collect())
}

/// The stored text and revision of a global setting, read in the caller's transaction.
fn stored_row(conn: &Connection, key: &str) -> Result<Option<(String, i64)>, (u16, String)> {
    conn.query_row("SELECT value, revision FROM settings WHERE key=?1", [key], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
        .map_err(|_| (503, STORAGE_ERROR.to_string()))
}

const ROLLOUT_ALLOW_LISTS: [(&str, &Setting); 2] = [("group", &ALLOW_GROUPS), ("private", &ALLOW_PRIVATE)];

/// The file with the three scope rows applied using this write transaction's view of the store.
fn scope_document(conn: &Connection, document: &Value) -> Result<Value, (u16, String)> {
    let mut effective = document.clone();
    for setting in [&ALLOW_GROUPS, &ALLOW_PRIVATE, &ROLLOUT_GROUPS] {
        if let Some(value) = stored_row(conn, setting.key)?
            .and_then(|(text, _)| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| setting.storable(&value))
        {
            set_at(&mut effective, setting.pointer, value);
        }
    }
    Ok(effective)
}

fn allow_ids(setting: &Setting, document: &Value) -> HashSet<u64> {
    setting.read(document).as_array().into_iter().flatten().filter_map(Value::as_u64).collect()
}

fn unallowed_rollout_key<'a>(keys: &'a [String], kind: &str, allowed: &HashSet<u64>) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        let (entry_kind, id) = key.split_once(':')?;
        let id = id.parse::<u64>().ok()?;
        (entry_kind == kind && !allowed.contains(&id)).then_some(key.as_str())
    })
}

fn rollout_can_seed(conn: &Connection, document: &Value, found: &Value) -> Result<bool, (u16, String)> {
    let Ok(keys) = crate::watchdog::rollout_groups(found) else { return Ok(false) };
    let effective = scope_document(conn, document)?;
    Ok(ROLLOUT_ALLOW_LISTS.iter().all(|(kind, setting)| {
        unallowed_rollout_key(&keys, kind, &allow_ids(setting, &effective)).is_none()
    }))
}

/// Validate the proposal after the in-transaction seed, before writing or mirroring any change.
/// A file without a usable rollout list has no launch target. A change of rollout checks both kinds.
fn validate_rollout_change<'a>(
    conn: &Connection,
    document: &Value,
    changes: impl IntoIterator<Item = (&'a Setting, Option<&'a Value>)>,
) -> Result<(), (u16, String)> {
    let mut scope = changes.into_iter().filter(|(setting, _)| {
        matches!(setting.key, "allow.groups" | "allow.private" | "rollout.rustGroups")
    }).peekable();
    if scope.peek().is_none() {
        return Ok(());
    }
    let before = scope_document(conn, document)?;
    let mut after = before.clone();
    let mut rollout_written = false;
    for (setting, value) in scope {
        rollout_written |= setting.key == ROLLOUT_GROUPS.key;
        if let Some(value) = value {
            if !set_at(&mut after, setting.pointer, value.clone()) {
                return Err((400, format!("config.json cannot hold {}", setting.key)));
            }
        } else {
            remove_at(&mut after, setting.pointer);
        }
    }
    let Ok(keys) = crate::watchdog::rollout_groups(&after["rollout"]["rustGroups"]) else { return Ok(()) };
    for (kind, setting) in ROLLOUT_ALLOW_LISTS {
        let allowed = allow_ids(setting, &after);
        if rollout_written || allowed != allow_ids(setting, &before) {
            if let Some(key) = unallowed_rollout_key(&keys, kind, &allowed) {
                return Err((409, format!(
                    "{} requires {key} to be in {}. To revoke access immediately, use conversation scope `remove` or add a deny entry.",
                    ROLLOUT_GROUPS.key, setting.key
                )));
            }
        }
    }
    Ok(())
}

/// Writes one change of a global setting and its audit row in the caller's transaction: `new` is the text to store,
/// `None` removes the stored value. Returns the stored revision (0 when removed). A revision continues from the stored
/// row, or, without one (never set, or removed by an undo), after the setting's audit rows, so it is never reused. A
/// seeded value is revision 1 with its one `migration` audit row (`seed`), so the rule holds for seeded settings too.
fn record_change(
    conn: &Connection,
    setting: &Setting,
    old: Option<(String, i64)>,
    new: Option<&str>,
    actor: &str,
    source: &str,
) -> Result<i64, (u16, String)> {
    let failed = |_: rusqlite::Error| (503, STORAGE_ERROR.to_string());
    let at = now_ms();
    let revision = match new {
        Some(text) => {
            let revision = match &old {
                Some((_, revision)) => revision + 1,
                None => {
                    let changes: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM settings_audit WHERE scope='global' AND key=?1",
                            [setting.key],
                            |row| row.get(0),
                        )
                        .map_err(failed)?;
                    changes + 1
                }
            };
            conn.execute(
                "INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, revision=excluded.revision,
                 updated_ms=excluded.updated_ms, updated_by=excluded.updated_by",
                params![setting.key, text, revision, at, actor],
            )
            .map_err(failed)?;
            revision
        }
        None => {
            conn.execute("DELETE FROM settings WHERE key=?1", [setting.key]).map_err(failed)?;
            0
        }
    };
    #[cfg(test)]
    if FAIL_BEFORE_AUDIT.with(|fail| fail.replace(false)) {
        return Err((503, STORAGE_ERROR.to_string()));
    }
    let mut old_value = old.map(|(old_text, _)| old_text);
    let mut new_value = new.map(str::to_string);
    if setting.secret {
        let marker = json!(SECRET_MARKER).to_string();
        (old_value, new_value) = (Some(marker.clone()), Some(marker));
    }
    conn.execute(
        "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(?1, ?2, ?3, 'global', ?4, ?5, ?6, ?7)",
        params![at, actor, source, setting.key, old_value, new_value, request_id(at)],
    )
    .map_err(failed)?;
    Ok(revision)
}

/// Sets one global setting: validated against the registry, then written with one audit row (`store`). The same value
/// again writes no value and no audit row. A refused value leaves the stored value as it was.
pub(crate) fn apply(
    context: &RuntimeContext,
    key: &str,
    value: &Value,
    actor: &str,
    source: &str,
) -> Result<Applied, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let setting = find(key).ok_or((400, "unknown setting".to_string()))?;
    let value = setting.normalize(value).ok_or_else(|| (400, setting.refusal(value)))?;
    store(context, setting, value, actor, source)
}

/// Tests the effective schedule after applying all writes in this request. Callers hold the configuration gate.
fn check_schedule_change(
    mut document: Value,
    stored: Vec<(&'static Setting, Value)>,
    changes: impl IntoIterator<Item = (&'static Setting, Option<Value>)>,
) -> Result<(), (u16, String)> {
    place(&mut document, stored);
    for (setting, value) in changes {
        if let Some(value) = value {
            if !set_at(&mut document, setting.pointer, value) {
                return Err((409, format!("config.json cannot hold {}", setting.key)));
            }
        } else {
            remove_at(&mut document, setting.pointer);
        }
    }
    super::registry::check_schedule(&document)
}

/// Writes one valid stored value and its audit row, then mirrors the committed value to config.json.
fn store(
    context: &RuntimeContext,
    setting: &'static Setting,
    value: Value,
    actor: &str,
    source: &str,
) -> Result<Applied, (u16, String)> {
    if super::registry::checks_schedule(setting.key) {
        let unavailable = |error| (503, error);
        check_schedule_change(
            raw_config(context).map_err(unavailable)?,
            stored_values(context).map_err(unavailable)?,
            [(setting, Some(value.clone()))],
        )?;
    }
    let harness_key = setting.kind.text_default().and_then(|_| harness_credential(context));
    let (applied, mirror_error) = write_store(context, false, &[setting.key], |conn, document| {
        // The configuration gate keeps the endpoint stable until the store commits.
        let text = if setting.secret && !value.is_null() {
            let endpoint = if setting.key == WEB_SEARCH_API_KEY.key {
                crate::aux_search::settings_endpoint(document)
            } else {
                context.dependency_url("model").map(str::to_string)
            }
            .ok_or((400, "the credential endpoint is unavailable".to_string()))?;
            json!([endpoint, value]).to_string()
        } else {
            value.to_string()
        };
        reject_credential_text(conn, setting, &value, harness_key.as_deref())?;
        let value = if setting.secret { json!(!value.is_null()) } else { value };
        let old = stored_row(conn, setting.key)?;
        if let Some((old_text, revision)) = &old {
            if *old_text == text {
                return Ok(Applied { value, revision: *revision, changed: false, mirror_error: None });
            }
        }
        validate_rollout_change(conn, document, [(setting, Some(&value))])?;
        let revision = record_change(conn, setting, old, Some(&text), actor, source)?;
        validate_timing(conn, document, &[setting.key], None)?;
        Ok(Applied { value, revision, changed: true, mirror_error: None })
    })?;
    Ok(Applied { mirror_error, ..applied })
}

/// Returns a global setting to its registry default: the default is stored, so it wins over `config.json`, with one
/// audit row (source `reset`), and is written back into `config.json` like any change. A model id without a default
/// stores `null`, read as not configured, and `config.json` gets `null`; a text setting stores its compiled text, an
/// empty one included. A stored default already changes nothing.
pub(crate) fn reset(context: &RuntimeContext, key: &str, actor: &str) -> Result<Applied, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let setting = find(key).ok_or((400, "unknown setting".to_string()))?;
    store(context, setting, setting.default_value(), actor, "reset")
}

const UNKNOWN_ENTRY_ERROR: &str = "unknown history entry";
const CHANGED_SINCE_ERROR: &str = "the setting changed after this entry; undo the later change first";
const NOT_UNDOABLE_ERROR: &str = "this entry cannot be undone";
const OLD_VALUE_REFUSED_ERROR: &str = "the old value of this entry is no longer accepted";

/// What `undo` restored.
pub(crate) struct Undone {
    pub key: &'static str,
    /// The conversation whose value it restored (#218); `None` for a global setting.
    pub conversation: Option<String>,
    /// The stored value now; `None` when the undo removed it.
    pub stored: Option<Value>,
    /// Why `config.json` does not hold every stored value after this undo; the undo stands either way.
    pub mirror_error: Option<String>,
}

/// Undoes one history entry (`settings_audit.id`): restores its old value (none removes the stored value) with one
/// audit row (source `undo`), so undoing that row redoes the change. In the same transaction it refuses with 409 and
/// writes nothing unless the entry names a global setting the registry still has and is not a seeded copy of
/// `config.json` (source `migration`: undoing it would only drop the store's copy of the file's own value), no later
/// audit row names that setting, the stored value is still the entry's new value and the registry still accepts the
/// entry's old value. An undo that removes the stored value also removes the setting's member from `config.json`
/// before the commit (`remove_from_config`); rollout refuses that undo with 409 to keep an explicit launch list.
/// When the file write fails the undo is 503 and writes nothing. An entry that
/// does not exist, and any entry while there is no store yet, is 404 (and creates no store). `permit` is asked, inside the
/// transaction, about the setting the entry names; its error refuses the undo with nothing written (#154: the console's
/// plain path, which looked at the entry before it existed or before it was known, refuses a security setting here).
/// An entry whose scope is not `global` is a conversation's (#218, `conversation::undo_entry`, the same rules for that
/// conversation; `permit` is not asked, no conversation setting being a security setting). An undo never migrates.
pub(crate) fn undo(
    context: &RuntimeContext,
    entry: i64,
    actor: &str,
    permit: &dyn Fn(&Setting) -> Result<(), (u16, String)>,
) -> Result<Undone, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    // An undone setting has audit rows, which the seed skips, so an undo names no key to seed.
    let harness_key = harness_credential(context);
    let (undone, mirror_error) = write_store(context, true, &[], |conn, document| {
        let failed = |_: rusqlite::Error| (503, STORAGE_ERROR.to_string());
        let row: Option<(String, String, String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT scope, key, source, old_value, new_value FROM settings_audit WHERE id=?1",
                [entry],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()
            .map_err(failed)?;
        let (scope, key, source, old_text, new_text) = row.ok_or((404, UNKNOWN_ENTRY_ERROR.to_string()))?;
        if scope != "global" {
            return conversation::undo_entry(conn, context, entry, (scope, key, source, old_text, new_text), actor);
        }
        let setting = find(&key)
            .filter(|setting| !setting.secret && scope == "global" && source != "migration")
            .ok_or((409, NOT_UNDOABLE_ERROR.to_string()))?;
        permit(setting)?;
        let later: i64 = conn
            .query_row(
                "SELECT count(*) FROM settings_audit WHERE scope='global' AND key=?1 AND id>?2",
                params![setting.key, entry],
                |row| row.get(0),
            )
            .map_err(failed)?;
        let current = stored_row(conn, setting.key)?;
        if later > 0 || current.as_ref().map(|(text, _)| text.as_str()) != new_text.as_deref() {
            return Err((409, CHANGED_SINCE_ERROR.to_string()));
        }
        let restored = old_text
            .map(|text| {
                serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|value| setting.storable(&value))
                    .ok_or((409, OLD_VALUE_REFUSED_ERROR.to_string()))
            })
            .transpose()?;
        if setting.key == ROLLOUT_GROUPS.key && restored.is_none() {
            return Err((
                409,
                "This entry has no previous rollout list to restore. Set or reset rollout.rustGroups explicitly.".to_string(),
            ));
        }
        if let Some(restored) = &restored {
            reject_credential_text(conn, setting, restored, harness_key.as_deref())?;
        }
        if super::registry::checks_schedule(setting.key) {
            let unavailable = |error| (503, error);
            check_schedule_change(
                document.clone(),
                read_stored_values(conn).map_err(unavailable)?,
                [(setting, restored.clone())],
            )?;
        }
        let restored_text = restored.as_ref().map(Value::to_string);
        validate_rollout_change(conn, document, [(setting, restored.as_ref())])?;
        record_change(conn, setting, current, restored_text.as_deref(), actor, "undo")?;
        validate_timing(conn, document, &[setting.key], restored.is_none().then_some(setting))?;
        if restored.is_none() {
            remove_from_config(context, setting).map_err(|error| (503, error))?;
        }
        Ok(Undone { key: setting.key, conversation: None, stored: restored, mirror_error: None })
    })?;
    Ok(Undone { mirror_error, ..undone })
}

/// The most entries one history page lists.
pub(crate) const HISTORY_PAGE: usize = 100;

/// The change history (`settings_audit`), newest first: at most `HISTORY_PAGE` entries with an id below `before` (from
/// the newest when `None`), only `setting`'s when given (a key of either scope; an unknown setting is 400), and only
/// `conversation`'s when given (#218: a canonical conversation key, the audit `scope` of its values; anything else is
/// 400). Each entry has its id (`entry`, what undo takes), `atMs`, `actor`, `source`, `scope` (`global`, or the
/// conversation), `setting` and `old` and `new` (`null`: no stored value; text that is not JSON is shown as a string).
/// `next` is the `before` of the next page, `null` on the last. Read through the inspection's read-only connection: no
/// store yet is an empty history, a store this release cannot read is 503.
pub(crate) fn history(
    context: &RuntimeContext,
    setting: Option<&str>,
    conversation: Option<&str>,
    before: Option<i64>,
) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let key = setting
        .map(|key| {
            find(key)
                .map(|found| found.key)
                .or_else(|| find_conversation(key).map(|found| found.key))
                .ok_or((400, "unknown setting".to_string()))
        })
        .transpose()?;
    let scope = conversation
        .map(|conversation| {
            conversation::is_canonical(conversation).then_some(conversation).ok_or((400, CONVERSATION_ERROR.to_string()))
        })
        .transpose()?;
    let conn = match inspect(context).map_err(|error| (503, error))? {
        Inspection::Missing | Inspection::Fresh => return Ok(json!({"ok": true, "entries": [], "next": null})),
        Inspection::Ready(conn) => conn,
    };
    let failed = |_: rusqlite::Error| (503, STORAGE_ERROR.to_string());
    let shown = |text: Option<String>| {
        text.map_or(Value::Null, |text| serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
    };
    let mut statement = conn
        .prepare(
            "SELECT id, at_ms, actor, source, scope, key, old_value, new_value FROM settings_audit
             WHERE id < ?1 AND (?2 IS NULL OR key = ?2) AND (?4 IS NULL OR scope = ?4) ORDER BY id DESC LIMIT ?3",
        )
        .map_err(failed)?;
    let entries = statement
        .query_map(params![before.unwrap_or(i64::MAX), key, HISTORY_PAGE as i64, scope], |row| {
            let id: i64 = row.get(0)?;
            let at: i64 = row.get(1)?;
            let actor: String = row.get(2)?;
            let source: String = row.get(3)?;
            let scope: String = row.get(4)?;
            let key: String = row.get(5)?;
            let old: Option<String> = row.get(6)?;
            let new: Option<String> = row.get(7)?;
            let (old, new) = if find(&key).is_some_and(|setting| setting.secret) {
                (json!(SECRET_MARKER), json!(SECRET_MARKER))
            } else {
                (shown(old), shown(new))
            };
            Ok(json!({
                "entry": id, "atMs": at, "actor": actor, "source": source, "scope": scope, "setting": key,
                "old": old, "new": new,
            }))
        })
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)?;
    let next = if entries.len() == HISTORY_PAGE { entries.last().map(|entry| entry["entry"].clone()) } else { None };
    Ok(json!({"ok": true, "entries": entries, "next": next}))
}

/// For a whole `config.json` an audit undo restores: keeps `current`'s value at every ordinary registry pointer in `restored`,
/// and removes it where `current` has none, because registry settings change and undo through the store only. A
/// value in `restored` differed from `current`'s. Err with the key when a non-object on the way in `restored` cannot
/// hold `current`'s value: the undo must refuse, because the restored file would lose that value. The console then
/// writes the restored file through its one write path, which puts every stored value in as well (`mirror_into`).
pub(crate) fn keep_registry_values(current: &Value, restored: &mut Value) -> Result<Vec<&'static str>, &'static str> {
    let mut kept = Vec::new();
    for setting in registry().filter(|setting| !setting.secret) {
        let now = current.pointer(setting.pointer).cloned();
        if restored.pointer(setting.pointer) == now.as_ref() {
            continue;
        }
        kept.push(setting.key);
        let placed = match now {
            Some(value) => set_at(restored, setting.pointer, value),
            None => {
                remove_at(restored, setting.pointer);
                true
            }
        };
        if !placed {
            return Err(setting.key);
        }
    }
    Ok(kept)
}

/// The registry settings a console configuration page's save changes (#177): each setting whose member `carried`, the
/// document of the members the request sends, holds (not `null`) and whose value differs from the value in effect
/// (`base`, the stored `config.json` the page read under the configuration gate, with the stored values applied), paired
/// with the value the store keeps. A value the registry refuses is 400 with its refusal, before anything is written,
/// unless `base` holds that same value already (an old or hand-edited file's value passes through unchanged). A member
/// the request omits or sends as `null` is no change: a console page never removes a registry setting (the settings
/// page's reset and undo do), and a member it does not send keeps the file's value. A legacy context has none. The page
/// stores the changes (`apply_changes`), then keeps the file's registry members (`keep_registry_values`) and writes it.
pub(crate) fn console_changes(
    context: &RuntimeContext,
    base: &Value,
    carried: &Value,
) -> Result<Vec<(&'static Setting, Value)>, (u16, String)> {
    if !context.is_isolated() {
        return Ok(Vec::new());
    }
    let mut now = overlay(context, base.clone());
    complete_timing(&mut now);
    let mut changes = Vec::new();
    for setting in registry().filter(|setting| !setting.secret) {
        // The member the setting reads (`Setting::found_in`), so an id list sent under its alias spelling counts.
        let Some(posted) = setting.found_in(carried).filter(|value| !value.is_null()) else { continue };
        let Some(value) = setting.normalize(posted) else {
            if setting.found_in(base) == Some(posted) {
                continue;
            }
            return Err((400, setting.refusal(posted)));
        };
        let unchanged = if setting.key == ROLLOUT_GROUPS.key {
            setting.found_in(&now) == Some(&value)
        } else {
            value == setting.read(&now)
        };
        if !unchanged {
            changes.push((setting, value));
        }
    }
    Ok(changes)
}

/// Stores `changes` (`console_changes`) for a console page in one transaction through `write_store`: one audit row per
/// setting whose stored value changes (`actor`, `source`), seeded first and written back after the commit like
/// `apply`. The caller holds the configuration gate and writes its own `config.json` change afterwards, keeping the
/// file's registry members. No change writes nothing. Returns why `config.json` does not hold every stored value
/// (`None` when it does); the stored changes stand either way.
pub(crate) fn apply_changes(
    context: &RuntimeContext,
    changes: Vec<(&'static Setting, Value)>,
    actor: &str,
    source: &str,
) -> Result<Option<String>, (u16, String)> {
    if changes.iter().any(|(setting, _)| setting.secret) {
        return Err((400, "credentials require the settings route".to_string()));
    }
    if changes.is_empty() {
        return Ok(None);
    }
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    if changes.iter().any(|(setting, _)| super::registry::checks_schedule(setting.key)) {
        let unavailable = |error| (503, error);
        check_schedule_change(
            raw_config(context).map_err(unavailable)?,
            stored_values(context).map_err(unavailable)?,
            changes.iter().map(|(setting, value)| (*setting, Some(value.clone()))),
        )?;
    }
    let written: Vec<&'static str> = changes.iter().map(|(setting, _)| setting.key).collect();
    let harness_key = harness_credential(context);
    let ((), mirror_error) = write_store(context, false, &written, |conn, document| {
        validate_rollout_change(conn, document, changes.iter().map(|(setting, value)| (*setting, Some(value))))?;
        for (setting, value) in &changes {
            reject_credential_text(conn, setting, value, harness_key.as_deref())?;
            let text = value.to_string();
            let old = stored_row(conn, setting.key)?;
            if old.as_ref().is_some_and(|(old_text, _)| *old_text == text) {
                continue;
            }
            record_change(conn, setting, old, Some(&text), actor, source)?;
        }
        validate_timing(conn, document, &written, None)?;
        Ok(())
    })?;
    Ok(mirror_error)
}

/// The refusal of a console write that would change a registry setting's member in `config.json` itself
/// (`keep_file_members`).
pub(crate) fn direct_change_error(key: &str) -> String {
    format!("{key} changes only through the settings store")
}

/// For a console write of `config.json` (`console::write_selected_config_text`, #177): every ordinary registry member of `next`
/// must be the one `current`, the stored file, holds. A member `next` lacks or holds as `null` is put back from
/// `current` (removed when `current` has none), because a console writer never removes a registry setting; any other
/// value that differs is refused with the setting's key, because a console writer changes a registry setting through
/// the store (`console_changes`, `apply_changes`). Err with the key too when a non-object on the way in `next` cannot
/// hold `current`'s value. Returns whether `next` changed.
pub(crate) fn keep_file_members(current: &Value, next: &mut Value) -> Result<bool, &'static str> {
    let mut changed = false;
    for setting in registry().filter(|setting| !setting.secret) {
        let now = current.pointer(setting.pointer);
        let written = next.pointer(setting.pointer);
        if written == now {
            continue;
        }
        if written.is_some_and(|value| !value.is_null()) {
            return Err(setting.key);
        }
        match now.cloned() {
            Some(value) => {
                if !set_at(next, setting.pointer, value) {
                    return Err(setting.key);
                }
            }
            None => remove_at(next, setting.pointer),
        }
        changed = true;
    }
    Ok(changed)
}

/// Removes the member a pointer of plain member names ends at, if its parent is an object.
fn remove_at(document: &mut Value, pointer: &str) {
    let Some((parent, last)) = pointer.rsplit_once('/') else { return };
    if let Some(object) = document.pointer_mut(parent).and_then(Value::as_object_mut) {
        object.remove(last);
    }
}

/// `document` as the console audit compares a configuration: ordinary registry pointers are removed, along with every
/// member on their paths that is missing, null or an empty object (null and empty read as
/// absent, `set_at` fills them), from the deepest to the shallowest, up to the first member that holds something; the
/// members of every object in sorted order, so its serialization is canonical. An ordinary settings write-back does not
/// change the audited file. Legacy credential literals remain in the digest. The security settings
/// (#154) stay: an audit undo restores them through the store (`security_restore`), so any later change of one, on the
/// settings page included, makes an older record stale.
pub(crate) fn registry_free(document: &Value) -> Value {
    let mut stripped = document.clone();
    for setting in registry().filter(|setting| !setting.confirmed() && !setting.secret) {
        remove_at(&mut stripped, setting.pointer);
        let mut below = setting.pointer;
        while let Some((above, _)) = below.rsplit_once('/') {
            if above.is_empty() {
                break;
            }
            match stripped.pointer(above) {
                None => {}
                Some(member) if member.is_null() || member.as_object().is_some_and(|object| object.is_empty()) => {
                    remove_at(&mut stripped, above);
                }
                Some(_) => break,
            }
            below = above;
        }
    }
    sorted(stripped)
}

fn sorted(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut members: Vec<(String, Value)> = object.into_iter().collect();
            members.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(members.into_iter().map(|(name, member)| (name, sorted(member))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Every registry setting with its label, hint, group, when it applies, type and bounds (`SettingKind::view`), default,
/// stored value (`null` when none), `value`, what the next use reads (`Setting::read`; `null` for a model id that is
/// not configured or for no owner), and `confirm: true` for a security setting (#154). `storeError` says why stored
/// values are not applied. While the admin agent mode puts another owner in force (`RuntimeContext::load_config`), the
/// owner's entry has `overriddenBy` with that owner.
pub(crate) fn view(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let mut document = context.load_config_document().map_err(|_| (503, "runtime config is unavailable".to_string()))?;
    complete_timing(&mut document);
    let (stored, store_error) = match stored_values(context) {
        Ok(stored) => (stored, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let (secrets, store_error) = match read_secrets(context) {
        Ok(secrets) => (secrets, store_error),
        Err(error) => (Vec::new(), store_error.or(Some(error))),
    };
    let effective_owner = context.load_config().ok().map(|effective| OWNER.read(&effective));
    let settings: Vec<Value> = registry()
        .map(|setting| {
            let mut entry = json!({
                "key": setting.key,
                "scope": "global",
                "group": setting.group,
                "label": setting.label,
                "hint": setting.hint,
                "applies": setting.applies,
                "kind": setting.kind.view(),
            });
            if setting.secret {
                entry["isSet"] = json!(secrets.iter().any(|(key, _, _)| *key == setting.key));
                return entry;
            }
            let stored_value = stored.iter().find(|(entry, _)| entry.key == setting.key).map(|(_, value)| value.clone());
            let value = if setting.key == ROLLOUT_GROUPS.key {
                setting.found_in(&document).cloned().unwrap_or(Value::Null)
            } else {
                setting.read(&document)
            };
            entry["default"] = setting.default_value();
            entry["stored"] = json!(stored_value);
            entry["value"] = value.clone();
            if setting.confirmed() {
                entry["confirm"] = json!(true);
            }
            if let Some(owner) = effective_owner.as_ref().filter(|owner| setting.key == OWNER.key && **owner != value) {
                entry["overriddenBy"] = json!({"source": "admin agent mode", "value": owner});
            }
            entry
        })
        .collect();
    let mut reply = json!({"ok": true, "settings": settings});
    if let Some(error) = store_error {
        reply["storeError"] = json!(error);
    }
    Ok(reply)
}

/// #154: the security settings a console audit undo restores from `restored`, the whole `config.json` of a backup:
/// each security setting whose value there, as the next use would read it (`Setting::read`: a missing member is the
/// default), differs from the value in effect (`current`, the stored `config.json` the undo read under the
/// configuration gate, with the stored values applied), paired with the value the store keeps. A value the store
/// cannot keep (an id list over its limit) is 409, before anything is written. The undo stores them
/// (`apply_changes`) after it recorded its revocation; every other registry setting keeps its current value. A legacy
/// context has none.
pub(crate) fn security_restore(
    context: &RuntimeContext,
    current: &Value,
    restored: &Value,
) -> Result<Vec<(&'static Setting, Value)>, (u16, String)> {
    if !context.is_isolated() {
        return Ok(Vec::new());
    }
    let now = overlay(context, current.clone());
    let mut changes = Vec::new();
    for setting in registry().filter(|setting| setting.confirmed()) {
        if setting.key == ROLLOUT_GROUPS.key {
            let Some(value) = setting.found_in(restored) else { continue };
            if setting.found_in(&now) == Some(value) {
                continue;
            }
            let value = setting.normalize(value).ok_or_else(|| {
                (409, format!("the backup's {} cannot be stored ({}); nothing was undone", setting.key, setting.refusal(value)))
            })?;
            changes.push((setting, value));
            continue;
        }
        let value = setting.read(restored);
        if value == setting.read(&now) {
            continue;
        }
        let value = setting.storable(&value).ok_or_else(|| {
            (409, format!("the backup's {} cannot be stored ({}); nothing was undone", setting.key, setting.refusal(&value)))
        })?;
        changes.push((setting, value));
    }
    Ok(changes)
}

/// #154: one change on the settings routes, previewed (`preview`) before an access change records its revocation.
pub(crate) enum Change<'a> {
    Set { key: &'a str, value: &'a Value },
    Reset { key: &'a str },
    Undo { entry: i64 },
}

/// #154: what `change` would store, without writing: for a set the accepted value (400 with the refusal otherwise),
/// for a reset the default, for an undo the entry's old value as the store keeps it, or `None` when the undo would
/// remove the stored value. An unknown setting is 400; an unknown entry, and any entry while there is no store yet, is
/// 404; an entry whose setting the registry no longer has is 409. Read through the inspection's read-only connection.
/// The caller holds the configuration gate, so no other writer changes the store before its own write, which checks
/// everything again.
pub(crate) fn preview(
    context: &RuntimeContext,
    change: &Change<'_>,
) -> Result<Vec<(&'static Setting, Option<Value>)>, (u16, String)> {
    match change {
        Change::Set { key, value } => {
            let setting = find(key).ok_or((400, "unknown setting".to_string()))?;
            let value = setting.normalize(value).ok_or_else(|| (400, setting.refusal(value)))?;
            Ok(vec![(setting, Some(value))])
        }
        Change::Reset { key } => {
            let setting = find(key).ok_or((400, "unknown setting".to_string()))?;
            Ok(vec![(setting, Some(setting.default_value()))])
        }
        Change::Undo { entry } => {
            #[cfg(test)]
            if FAIL_UNDO_LOOK.with(|fail| fail.replace(false)) {
                return Err((503, STORAGE_ERROR.to_string()));
            }
            let conn = match inspect(context).map_err(|error| (503, error))? {
                Inspection::Missing | Inspection::Fresh => return Err((404, UNKNOWN_ENTRY_ERROR.to_string())),
                Inspection::Ready(conn) => conn,
            };
            let row: Option<(String, Option<String>)> = conn
                .query_row("SELECT key, old_value FROM settings_audit WHERE id=?1", [entry], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()
                .map_err(|_| (503, STORAGE_ERROR.to_string()))?;
            let (key, old) = row.ok_or((404, UNKNOWN_ENTRY_ERROR.to_string()))?;
            let setting = find(&key).ok_or((409, NOT_UNDOABLE_ERROR.to_string()))?;
            if setting.secret {
                return Ok(vec![(setting, None)]);
            }
            let restored = old
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| setting.storable(&value));
            Ok(vec![(setting, restored)])
        }
    }
}

/// #154: puts each previewed change at its registry pointer in `document`: a value is set (missing or null objects on
/// the way are created), `None` removes the member, so the file's own value or the default applies again.
pub(crate) fn place_changes(document: &mut Value, changes: &[(&'static Setting, Option<Value>)]) {
    for (setting, value) in changes.iter().filter(|(setting, _)| !setting.secret) {
        match value {
            Some(value) => {
                set_at(document, setting.pointer, value.clone());
            }
            None => remove_at(document, setting.pointer),
        }
    }
}

/// #154: whether the store holds any of `changes` (`None`: no stored value) as its stored value now, after a failed
/// write: a commit the caller did not see succeed. A store that cannot be read is an error.
pub(crate) fn holds_any(context: &RuntimeContext, changes: &[(&'static Setting, Option<Value>)]) -> Result<bool, String> {
    let stored = stored_values(context)?;
    Ok(changes.iter().any(|(setting, value)| {
        let now = stored.iter().find(|(entry, _)| entry.key == setting.key).map(|(_, value)| value);
        now == value.as_ref()
    }))
}

/// #154: the newest audit row id, a mark that changes with every stored change, for what the console keys on the
/// configuration (`console_access.rs`). A legacy context and no store yet are 0; a store that cannot be read is `None`.
pub(crate) fn store_mark(context: &RuntimeContext) -> Option<i64> {
    if !context.is_isolated() {
        return Some(0);
    }
    match inspect(context).ok()? {
        Inspection::Missing | Inspection::Fresh => Some(0),
        Inspection::Ready(conn) => {
            conn.query_row("SELECT coalesce(max(id), 0) FROM settings_audit", [], |row| row.get(0)).ok()
        }
    }
}
