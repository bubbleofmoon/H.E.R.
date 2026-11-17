//! Live settings: the registry of owner-tunable settings and the settings store `data/settings.db`.
//!
//! The store is its own SQLite file with its own forward-only schema version (`PRAGMA user_version`); a newer version
//! is refused. It opens no other database. The configuration loader applies stored values over `config.json`
//! (`overlay`, called from `RuntimeContext::overlay_config`); `apply`, `reset` and `undo` change one value with one
//! audit row each, under the store gate (`write_store`), and `history` lists the audit rows. A legacy (no-root)
//! context has no store.

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use rusqlite::config::DbConfig;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const DB: &str = "settings.db";
const SIDECARS: [&str; 3] = ["settings.db-wal", "settings.db-shm", "settings.db-journal"];
/// The schema version this release reads and writes.
const SCHEMA_VERSION: i64 = 1;
const STORAGE_ERROR: &str = "settings storage is unavailable";
pub(crate) const NEWER_SCHEMA_ERROR: &str = "settings database is newer than this release";
const UNSUPPORTED_SCHEMA_ERROR: &str = "settings database version is not supported";
const LEGACY_ERROR: &str = "settings require a selected runtime";

#[cfg(test)]
thread_local! {
    /// Makes the next change (`apply`, `reset` or `undo`) on this thread fail after it wrote the value and before the
    /// audit row.
    pub(crate) static FAIL_BEFORE_AUDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Counts attempts to switch the store to WAL that met another connection's lock (test hook for the retry).
#[cfg(test)]
pub(crate) static WAL_SWITCH_BUSY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The value type and bounds of a setting.
pub(crate) enum Kind {
    /// A JSON integer from `min` to `max`, both included.
    Integer { min: i64, max: i64, default: i64 },
}

/// One registry entry.
pub(crate) struct Setting {
    /// The registry key, as the console names it.
    pub key: &'static str,
    /// Where the value lives in the configuration document (a JSON pointer of plain member names).
    pub pointer: &'static str,
    pub kind: Kind,
    /// When a change takes effect: `live`, `next wake` or `restart`.
    pub applies: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

impl Setting {
    /// The value as stored and read, or `None` when this setting does not accept it.
    pub(crate) fn normalize(&self, value: &Value) -> Option<Value> {
        match self.kind {
            Kind::Integer { min, max, .. } => value.as_i64().filter(|n| (min..=max).contains(n)).map(|n| json!(n)),
        }
    }

    pub(crate) fn default_value(&self) -> Value {
        match self.kind {
            Kind::Integer { default, .. } => json!(default),
        }
    }

    /// Why a refused value was refused.
    fn requirement(&self) -> String {
        match self.kind {
            Kind::Integer { min, max, .. } => format!("{} must be an integer from {min} to {max}", self.key),
        }
    }

    fn kind_view(&self) -> Value {
        match self.kind {
            Kind::Integer { min, max, .. } => json!({"type": "integer", "min": min, "max": max}),
        }
    }
}

/// Every setting the store accepts. A key that is not here is refused by `apply` and ignored by `overlay`.
pub(crate) const REGISTRY: &[Setting] = &[Setting {
    key: "reply.wakeQuietMs",
    pointer: "/socialV2/reply/wakeQuietMs",
    kind: Kind::Integer { min: 1, max: 30_000, default: 3_000 },
    applies: "next wake",
    group: "reply rhythm",
    label: "Quiet time before a message wake",
    hint: "Milliseconds without a new message before a message wake starts; each new message restarts the wait.",
}];

pub(crate) fn find(key: &str) -> Option<&'static Setting> {
    REGISTRY.iter().find(|setting| setting.key == key)
}

/// The result of one `apply`.
pub(crate) struct Applied {
    pub value: Value,
    pub revision: i64,
    pub changed: bool,
}

/// Applies the stored settings over `document`, the parsed `config.json`. A stored value goes to its registry pointer;
/// missing or null objects on the way are created, and any other non-object on the way leaves that setting unapplied.
/// A legacy context and a document that is not an object are returned unchanged. A store this release cannot read
/// applies nothing and is reported once per process on stderr (MCP servers use stdout).
pub(crate) fn overlay(context: &RuntimeContext, mut document: Value) -> Value {
    if !context.is_isolated() || !document.is_object() {
        return document;
    }
    match stored_values(context) {
        Ok(values) => {
            for (setting, value) in values {
                set_at(&mut document, setting.pointer, value);
            }
        }
        Err(error) => {
            static REPORTED: AtomicBool = AtomicBool::new(false);
            if !REPORTED.swap(true, Ordering::Relaxed) {
                crate::safe_eprintln!("[settings] stored settings are not applied: {error}");
            }
        }
    }
    document
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

/// Validates the store and its sidecars through the runtime's containment checks.
fn check_paths(context: &RuntimeContext, intent: PathIntent) -> Result<std::path::PathBuf, String> {
    for name in SIDECARS {
        context.resolve_path(RuntimeArea::Data, name, intent).map_err(|_| STORAGE_ERROR.to_string())?;
    }
    context.resolve_path(RuntimeArea::Data, DB, intent).map_err(|_| STORAGE_ERROR.to_string())
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

/// Takes the store gate, waiting until `deadline`. The gate file is opened read-write and locked, so it must be a
/// plain file with one link before it is opened, and still have one once it is open; a second name for another file
/// is refused. `Gate::Create`'s other callers keep their own checks.
fn store_gate(context: &RuntimeContext, deadline: std::time::Instant) -> Result<std::fs::File, String> {
    let path = context
        .resolve_path(RuntimeArea::Data, STORE_GATE, PathIntent::ReadOptionalFile)
        .map_err(|_| STORAGE_ERROR.to_string())?;
    file_len(&path)?;
    refuse_hard_links(&path)?;
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

/// The schema on an open connection: `Ok(false)` for version 0 with no schema object, `Ok(true)` for version 1 with
/// exactly this release's schema (`has_settings_schema`); anything else is refused.
fn check_schema(conn: &Connection) -> Result<bool, String> {
    match schema_version(conn)? {
        0 => {
            let objects: i64 = conn
                .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
                .map_err(|_| STORAGE_ERROR.to_string())?;
            if objects == 0 { Ok(false) } else { Err(STORAGE_ERROR.to_string()) }
        }
        SCHEMA_VERSION if has_settings_schema(conn) => Ok(true),
        SCHEMA_VERSION => Err(STORAGE_ERROR.to_string()),
        version => Err(refuse_version(version)),
    }
}

/// Looks at the store without opening it for writing. Every store file (the database, its `-wal`, `-shm` and `-journal`,
/// and the store gate) must be a plain file with one link or missing, whether or not the database exists. A leftover
/// rollback journal (beside a missing database, or hot: no connection holds the write lock) is refused, and nothing
/// recovers it; a live writer's journal is waited for. A file that is not a SQLite database, a version other than 0 or 1
/// (read from the file header while no WAL holds later commits), a version 0 with any schema object and a version 1
/// whose schema is not exactly this release's are refused too. Otherwise a read-only connection that never checkpoints
/// on close reads the schema; only SQLite's shared-memory index (`-shm`) may be rebuilt meanwhile.
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
        if header != 0 && header != SCHEMA_VERSION {
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
/// yet is an empty list; a store with a journal is unreadable. Rows with a key the
/// registry does not have, or a value its setting does not accept, are skipped.
fn stored_values(context: &RuntimeContext) -> Result<Vec<(&'static Setting, Value)>, String> {
    let conn = match inspect(context)? {
        Inspection::Missing | Inspection::Fresh => return Ok(Vec::new()),
        Inspection::Ready(conn) => conn,
    };
    let mut statement = conn.prepare("SELECT key, value FROM settings").map_err(|_| STORAGE_ERROR.to_string())?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|_| STORAGE_ERROR.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| STORAGE_ERROR.to_string())?;
    Ok(rows
        .into_iter()
        .filter_map(|(key, text)| {
            let setting = find(&key)?;
            let value = serde_json::from_str::<Value>(&text).ok()?;
            Some((setting, setting.normalize(&value)?))
        })
        .collect())
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

/// Whether `sqlite_master` holds exactly schema version 1: the same objects (type, name, table) with the same CREATE
/// text after `normalized_sql`, and nothing else. A store that claims the version without it is refused before it
/// is touched, so the WAL switch never rewrites its header.
fn has_settings_schema(conn: &Connection) -> bool {
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
    let mut expected: Vec<Object> = SCHEMA_V1
        .iter()
        .map(|(kind, name, table, sql)| (kind.to_string(), name.to_string(), table.to_string(), Some(sql.to_string())))
        .chain(
            SCHEMA_V1_IMPLICIT
                .iter()
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
/// schema is newer than this release's (checked before anything is written). The caller holds the store gate
/// (`write_store`).
fn open_for_write(context: &RuntimeContext) -> Result<Connection, String> {
    // Refuse what this release cannot use before any connection that can write opens the store.
    drop(inspect(context)?);
    let path = check_paths(context, PathIntent::WriteFile)?;
    let mut conn = Connection::open_with_flags(
        path,
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
    match schema_version(&tx)? {
        0 => {
            for (_, _, _, sql) in SCHEMA_V1 {
                tx.execute_batch(sql).map_err(|_| STORAGE_ERROR.to_string())?;
            }
            tx.execute_batch("PRAGMA user_version=1;").map_err(|_| STORAGE_ERROR.to_string())?;
        }
        SCHEMA_VERSION => {}
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

/// Runs one write under the store gate. The gate is taken (waiting up to `WAL_SWITCH_WAIT`, else 503) before the
/// inspection and held until the connection closes, so no other writer of this store can stop between this writer's
/// inspection and its read-write open and leave a journal that open would play back. With `existing_only`, a store
/// that is missing or fresh is 404 under the gate and none is created. `write` runs in one immediate transaction,
/// committed when it returns `Ok` and rolled back otherwise.
fn write_store<T>(
    context: &RuntimeContext,
    existing_only: bool,
    write: impl FnOnce(&Connection) -> Result<T, (u16, String)>,
) -> Result<T, (u16, String)> {
    let unavailable = |error: String| -> (u16, String) { (503, error) };
    let _gate = store_gate(context, std::time::Instant::now() + WAL_SWITCH_WAIT).map_err(unavailable)?;
    if existing_only && matches!(inspect(context).map_err(unavailable)?, Inspection::Missing | Inspection::Fresh) {
        return Err((404, UNKNOWN_ENTRY_ERROR.to_string()));
    }
    let mut conn = open_for_write(context).map_err(unavailable)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| unavailable(STORAGE_ERROR.to_string()))?;
    let result = write(&tx)?;
    tx.commit().map_err(|_| unavailable(STORAGE_ERROR.to_string()))?;
    Ok(result)
}

/// The stored text and revision of a global setting, read in the caller's transaction.
fn stored_row(conn: &Connection, key: &str) -> Result<Option<(String, i64)>, (u16, String)> {
    conn.query_row("SELECT value, revision FROM settings WHERE key=?1", [key], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
        .map_err(|_| (503, STORAGE_ERROR.to_string()))
}

/// Writes one change of a global setting and its audit row in the caller's transaction: `new` is the text to store,
/// `None` removes the stored value. Returns the stored revision (0 when removed). A revision continues from the stored
/// row, or, without one (never set, or removed by an undo), after the setting's audit rows, so it is never reused.
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
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let request = format!("{at}-{}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed));
    conn.execute(
        "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(?1, ?2, ?3, 'global', ?4, ?5, ?6, ?7)",
        params![at, actor, source, setting.key, old.map(|(old_text, _)| old_text), new, request],
    )
    .map_err(failed)?;
    Ok(revision)
}

/// Sets one global setting: validated against the registry, then written with one audit row in one transaction.
/// The same value again changes nothing and writes no audit row. A refused value leaves the stored value as it was.
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
    let value = setting.normalize(value).ok_or_else(|| (400, setting.requirement()))?;
    let text = value.to_string();
    write_store(context, false, |conn| {
        let old = stored_row(conn, setting.key)?;
        if let Some((old_text, revision)) = &old {
            if *old_text == text {
                return Ok(Applied { value, revision: *revision, changed: false });
            }
        }
        let revision = record_change(conn, setting, old, Some(&text), actor, source)?;
        Ok(Applied { value, revision, changed: true })
    })
}

/// Returns a global setting to its registry default: the default is stored, so it wins over `config.json`, with one
/// audit row (source `reset`). A stored default already changes nothing.
pub(crate) fn reset(context: &RuntimeContext, key: &str, actor: &str) -> Result<Applied, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let setting = find(key).ok_or((400, "unknown setting".to_string()))?;
    apply(context, setting.key, &setting.default_value(), actor, "reset")
}

const UNKNOWN_ENTRY_ERROR: &str = "unknown history entry";
const CHANGED_SINCE_ERROR: &str = "the setting changed after this entry; undo the later change first";
const NOT_UNDOABLE_ERROR: &str = "this entry cannot be undone";
const OLD_VALUE_REFUSED_ERROR: &str = "the old value of this entry is no longer accepted";

/// What `undo` restored.
pub(crate) struct Undone {
    pub key: &'static str,
    /// The stored value now; `None` when the undo removed it.
    pub stored: Option<Value>,
}

/// Undoes one history entry (`settings_audit.id`): restores its old value (none removes the stored value) with one
/// audit row (source `undo`), so undoing that row redoes the change. In the same transaction it refuses with 409 and
/// writes nothing unless the entry names a global setting the registry still has, no later audit row names that
/// setting, the stored value is still the entry's new value and the registry still accepts the entry's old value. An
/// entry that does not exist, and any entry while there is no store yet, is 404 (and creates no store).
pub(crate) fn undo(context: &RuntimeContext, entry: i64, actor: &str) -> Result<Undone, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    write_store(context, true, |conn| {
        let failed = |_: rusqlite::Error| (503, STORAGE_ERROR.to_string());
        let row: Option<(String, String, Option<String>, Option<String>)> = conn
            .query_row("SELECT scope, key, old_value, new_value FROM settings_audit WHERE id=?1", [entry], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()
            .map_err(failed)?;
        let (scope, key, old_text, new_text) = row.ok_or((404, UNKNOWN_ENTRY_ERROR.to_string()))?;
        let setting = find(&key).filter(|_| scope == "global").ok_or((409, NOT_UNDOABLE_ERROR.to_string()))?;
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
                    .and_then(|value| setting.normalize(&value))
                    .ok_or((409, OLD_VALUE_REFUSED_ERROR.to_string()))
            })
            .transpose()?;
        let restored_text = restored.as_ref().map(Value::to_string);
        record_change(conn, setting, current, restored_text.as_deref(), actor, "undo")?;
        Ok(Undone { key: setting.key, stored: restored })
    })
}

/// The most entries one history page lists.
pub(crate) const HISTORY_PAGE: usize = 100;

/// The change history (`settings_audit`), newest first: at most `HISTORY_PAGE` entries with an id below `before` (from
/// the newest when `None`), only `setting`'s when given (an unknown setting is 400). Each entry has its id (`entry`,
/// what undo takes), `atMs`, `actor`, `source`, `scope`, `setting` and `old` and `new` (`null`: no stored value; text
/// that is not JSON is shown as a string). `next` is the `before` of the next page, `null` on the last. Read through
/// the inspection's read-only connection: no store yet is an empty history, a store this release cannot read is 503.
pub(crate) fn history(
    context: &RuntimeContext,
    setting: Option<&str>,
    before: Option<i64>,
) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let key = setting
        .map(|key| find(key).map(|found| found.key).ok_or((400, "unknown setting".to_string())))
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
             WHERE id < ?1 AND (?2 IS NULL OR key = ?2) ORDER BY id DESC LIMIT ?3",
        )
        .map_err(failed)?;
    let entries = statement
        .query_map(params![before.unwrap_or(i64::MAX), key, HISTORY_PAGE as i64], |row| {
            let id: i64 = row.get(0)?;
            let at: i64 = row.get(1)?;
            let actor: String = row.get(2)?;
            let source: String = row.get(3)?;
            let scope: String = row.get(4)?;
            let key: String = row.get(5)?;
            let old: Option<String> = row.get(6)?;
            let new: Option<String> = row.get(7)?;
            Ok(json!({
                "entry": id, "atMs": at, "actor": actor, "source": source, "scope": scope, "setting": key,
                "old": shown(old), "new": shown(new),
            }))
        })
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)?;
    let next = if entries.len() == HISTORY_PAGE { entries.last().map(|entry| entry["entry"].clone()) } else { None };
    Ok(json!({"ok": true, "entries": entries, "next": next}))
}

/// For a whole `config.json` an audit undo restores: keeps `current`'s value at every registry pointer in `restored`,
/// and removes it where `current` has none, because registry settings change and undo through the store only. A
/// value in `restored` differed from `current`'s. Err with the key when a non-object on the way in `restored` cannot
/// hold `current`'s value: the undo must refuse, because the restored file would lose that value.
pub(crate) fn keep_registry_values(current: &Value, restored: &mut Value) -> Result<Vec<&'static str>, &'static str> {
    let mut kept = Vec::new();
    for setting in REGISTRY {
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

/// Removes the member a pointer of plain member names ends at, if its parent is an object.
fn remove_at(document: &mut Value, pointer: &str) {
    let Some((parent, last)) = pointer.rsplit_once('/') else { return };
    if let Some(object) = document.pointer_mut(parent).and_then(Value::as_object_mut) {
        object.remove(last);
    }
}

/// Every registry setting with its label, hint, group, when it applies, type and bounds, default, stored value
/// (`null` when none) and `value`, what the next use reads. `storeError` says why stored values are not applied.
pub(crate) fn view(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let document = context.load_config_document().map_err(|_| (503, "runtime config is unavailable".to_string()))?;
    let (stored, store_error) = match stored_values(context) {
        Ok(stored) => (stored, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let settings: Vec<Value> = REGISTRY
        .iter()
        .map(|setting| {
            let stored_value = stored.iter().find(|(entry, _)| entry.key == setting.key).map(|(_, value)| value.clone());
            let value = document
                .pointer(setting.pointer)
                .and_then(|value| setting.normalize(value))
                .unwrap_or_else(|| setting.default_value());
            json!({
                "key": setting.key,
                "group": setting.group,
                "label": setting.label,
                "hint": setting.hint,
                "applies": setting.applies,
                "kind": setting.kind_view(),
                "default": setting.default_value(),
                "stored": stored_value,
                "value": value,
            })
        })
        .collect();
    let mut reply = json!({"ok": true, "settings": settings});
    if let Some(error) = store_error {
        reply["storeError"] = json!(error);
    }
    Ok(reply)
}
