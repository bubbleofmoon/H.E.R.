//! Test-only: `StoreProbe`, the one way tests read and write `data/settings.db` behind the store's back, as another
//! release, another program, a crash or a hand edit would. It knows the store's path and the SQL of its rows, its
//! audit rows and its schema version, so a schema change edits this file and not every test.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The settings store of one runtime root, reached without the store's own code.
pub(crate) struct StoreProbe(PathBuf);

/// One `settings_audit` row written outside `apply`: an older release's, another writer's or old data.
pub(crate) struct AuditRow {
    pub at_ms: i64,
    pub actor: &'static str,
    pub source: &'static str,
    pub scope: &'static str,
    pub key: &'static str,
    pub old: Option<String>,
    pub new: Option<String>,
    pub request: String,
}

impl AuditRow {
    /// A global row of `key` at time 0 with actor and source `test`.
    pub(crate) fn test(key: &'static str, old: Option<String>, new: Option<String>, request: &str) -> Self {
        AuditRow { at_ms: 0, actor: "test", source: "test", scope: "global", key, old, new, request: request.to_string() }
    }
}

impl StoreProbe {
    /// The store of the runtime whose root is `root` (`data/settings.db`).
    pub(crate) fn at(root: &Path) -> Self {
        StoreProbe(root.join("data/settings.db"))
    }

    /// The store's path, for file checks and file fixtures (links, bytes, an empty file).
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// A read-write connection to the existing store.
    pub(crate) fn open(&self) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(&self.0, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap()
    }

    /// Builds a database at the store's path (created when missing) with `sql`, in rollback-journal mode; the
    /// connection is closed when this returns.
    pub(crate) fn build(&self, sql: &str) {
        let conn = rusqlite::Connection::open(&self.0).unwrap();
        conn.execute_batch(sql).unwrap();
    }

    /// `build` in WAL mode. With `keep_frames` the close does not checkpoint, so the committed frames stay in `-wal`
    /// beside the index, as a process that ended without its last checkpoint leaves them.
    pub(crate) fn build_wal(&self, sql: &str, keep_frames: bool) {
        let conn = rusqlite::Connection::open(&self.0).unwrap();
        if keep_frames {
            conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).unwrap();
        }
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)).unwrap();
        assert_eq!(mode, "wal");
        conn.execute_batch(sql).unwrap();
    }

    /// Runs `sql` on the existing store.
    pub(crate) fn edit(&self, sql: &str) {
        self.open().execute_batch(sql).unwrap();
    }

    /// `PRAGMA user_version`.
    pub(crate) fn user_version(&self) -> i64 {
        self.open().query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap()
    }

    /// `PRAGMA journal_mode`.
    pub(crate) fn journal_mode(&self) -> String {
        self.open().query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap()
    }

    /// How many schema objects are named `name`.
    pub(crate) fn objects_named(&self, name: &str) -> i64 {
        self.open().query_row("SELECT count(*) FROM sqlite_master WHERE name=?1", [name], |row| row.get(0)).unwrap()
    }

    /// The audit rows, oldest first: `actor`, `source`, `scope`, `key`, `old`, `new` (the stored text, `null` for none)
    /// and `request`.
    pub(crate) fn audit_rows(&self) -> Vec<Value> {
        let conn = self.open();
        let mut statement = conn
            .prepare("SELECT actor, source, scope, key, old_value, new_value, request_id FROM settings_audit ORDER BY id")
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let actor: String = row.get(0)?;
                let source: String = row.get(1)?;
                let scope: String = row.get(2)?;
                let key: String = row.get(3)?;
                let old: Option<String> = row.get(4)?;
                let new: Option<String> = row.get(5)?;
                let request: String = row.get(6)?;
                Ok(json!({
                    "actor": actor, "source": source, "scope": scope, "key": key,
                    "old": old, "new": new, "request": request,
                }))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    /// How many audit rows the store holds.
    pub(crate) fn audit_count(&self) -> i64 {
        self.open().query_row("SELECT count(*) FROM settings_audit", [], |row| row.get(0)).unwrap()
    }

    /// Replaces the stored text of `key` without an audit row.
    pub(crate) fn set_value(&self, key: &str, text: &str) {
        self.open().execute("UPDATE settings SET value=?1 WHERE key=?2", [text, key]).unwrap();
    }

    /// Adds a stored row for `key` (revision 1, by `test`) without an audit row.
    pub(crate) fn insert_value(&self, key: &str, text: &str) {
        self.open()
            .execute(
                "INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES(?1, ?2, 1, 0, 'test')",
                [key, text],
            )
            .unwrap();
    }

    /// Appends `rows` to the audit in one transaction; returns the id of the last one.
    pub(crate) fn insert_audit(&self, rows: &[AuditRow]) -> i64 {
        let mut conn = self.open();
        let tx = conn.transaction().unwrap();
        for row in rows {
            tx.execute(
                "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![row.at_ms, row.actor, row.source, row.scope, row.key, row.old, row.new, row.request],
            )
            .unwrap();
        }
        let id = tx.last_insert_rowid();
        tx.commit().unwrap();
        id
    }
}
