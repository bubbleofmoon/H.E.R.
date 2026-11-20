//! Durable browser authentication state and the settings archive boundary.

use super::{
    administrator_password_in, check_schema, migrate, open_for_write, record_change,
    schema_version, store_gate, stored_row, Inspection, ADMINISTRATOR_PASSWORD, CLI_MODEL_API_KEY,
    SIDECARS, STORAGE_ERROR, WAL_SWITCH_WAIT, WEB_SEARCH_API_KEY,
};
use crate::console_auth::{new_generation, AdministratorPasswordHash};
use crate::runtime::RuntimeContext;
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use serde_json::json;
use std::path::Path;
use std::time::Instant;

pub(super) const SCHEMA_V3_ADDED: [(&str, &str, &str, &str); 2] = [
    (
        "table",
        "console_browser_sessions",
        "console_browser_sessions",
        "CREATE TABLE console_browser_sessions (
            id_digest BLOB PRIMARY KEY NOT NULL CHECK (typeof(id_digest)='blob' AND length(id_digest)=32),
            origin TEXT NOT NULL CHECK (typeof(origin)='text' AND length(origin)>0),
            token_digest BLOB NOT NULL CHECK (typeof(token_digest)='blob' AND length(token_digest)=32),
            auth_generation BLOB NOT NULL CHECK (typeof(auth_generation)='blob' AND length(auth_generation)=32),
            restore_generation BLOB NOT NULL CHECK (typeof(restore_generation)='blob' AND length(restore_generation)=32),
            created_ms INTEGER NOT NULL CHECK (typeof(created_ms)='integer' AND created_ms>=0),
            last_used_ms INTEGER NOT NULL CHECK (typeof(last_used_ms)='integer' AND last_used_ms>=created_ms),
            expires_ms INTEGER NOT NULL CHECK (typeof(expires_ms)='integer' AND expires_ms>last_used_ms),
            issued_seq INTEGER NOT NULL UNIQUE CHECK (typeof(issued_seq)='integer' AND issued_seq>0)
        )",
    ),
    (
        "table",
        "console_auth_state",
        "console_auth_state",
        "CREATE TABLE console_auth_state (
            id INTEGER PRIMARY KEY NOT NULL CHECK (id=1),
            auth_generation BLOB NOT NULL CHECK (typeof(auth_generation)='blob' AND length(auth_generation)=32),
            consecutive_failures INTEGER NOT NULL CHECK (typeof(consecutive_failures)='integer' AND consecutive_failures>=0),
            deadline_ms INTEGER NOT NULL CHECK (typeof(deadline_ms)='integer' AND deadline_ms>=0),
            clock_high_water_ms INTEGER NOT NULL CHECK (typeof(clock_high_water_ms)='integer' AND clock_high_water_ms>=0)
        )",
    ),
];

pub(super) const SCHEMA_V3_IMPLICIT: [(&str, &str, &str, Option<&str>); 2] = [
    (
        "index",
        "sqlite_autoindex_console_browser_sessions_1",
        "console_browser_sessions",
        None,
    ),
    (
        "index",
        "sqlite_autoindex_console_browser_sessions_2",
        "console_browser_sessions",
        None,
    ),
];

#[cfg(test)]
thread_local! {
    static FAIL_AUTH_MIGRATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn storage_error(_: rusqlite::Error) -> String {
    STORAGE_ERROR.to_string()
}

/// Run within the credential writer's immediate transaction, including any required v2 migration.
fn migrate_auth(conn: &Connection) -> Result<(), String> {
    if schema_version(conn)? == 3 {
        return Ok(());
    }
    migrate(conn)?;
    for (_, _, _, sql) in SCHEMA_V3_ADDED {
        conn.execute_batch(sql).map_err(storage_error)?;
    }
    conn.execute_batch("PRAGMA user_version=3;")
        .map_err(storage_error)
}

/// A password mutation owns its SQLite transaction. The caller holds the configuration gate.
fn write_administrator_password(
    context: &RuntimeContext,
    existing_only: bool,
    write: impl FnOnce(&Connection) -> Result<(), (u16, String)>,
) -> Result<(), (u16, String)> {
    if !context.is_isolated() {
        return Err((503, super::LEGACY_ERROR.to_string()));
    }
    let unavailable = |error| (503, error);
    let _gate = store_gate(context, Instant::now() + WAL_SWITCH_WAIT).map_err(unavailable)?;
    if existing_only
        && matches!(
            super::inspect(context).map_err(unavailable)?,
            Inspection::Missing | Inspection::Fresh
        )
    {
        return Ok(());
    }
    let mut conn = open_for_write(context).map_err(unavailable)?;
    conn.execute_batch("PRAGMA secure_delete=ON;")
        .map_err(storage_error)
        .map_err(unavailable)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)
        .map_err(unavailable)?;
    #[cfg(test)]
    let previous = schema_version(&tx).map_err(unavailable)?;
    write(&tx)?;
    #[cfg(test)]
    if previous < 3 && FAIL_AUTH_MIGRATION.with(|fail| { fail.set(false); false }) { // RED-R2-B
        return Err(unavailable(STORAGE_ERROR.to_string()));
    }
    tx.commit().map_err(storage_error).map_err(unavailable)
}

fn initialize_auth_state(conn: &Connection) -> Result<(), String> {
    if !auth_rows_exist(conn, "console_auth_state")? {
        let generation = new_generation().map_err(|_| STORAGE_ERROR.to_string())?;
        conn.execute(
            "INSERT INTO console_auth_state VALUES (1, ?1, 0, 0, 0)",
            [generation.as_slice()],
        )
        .map_err(storage_error)?;
    }
    Ok(())
}

pub(crate) fn set_initial_administrator_password(
    context: &RuntimeContext,
    hash: &AdministratorPasswordHash,
    actor: &str,
) -> Result<(), (u16, String)> {
    write_administrator_password(context, false, |conn| {
        let old = stored_row(conn, ADMINISTRATOR_PASSWORD.key)?;
        if old.is_some() {
            return Err((409, "administrator password is already set".to_string()));
        }
        migrate_auth(conn).map_err(|error| (503, error))?;
        initialize_auth_state(conn).map_err(|error| (503, error))?;
        let text = json!(hash.as_str()).to_string();
        record_change(
            conn,
            &ADMINISTRATOR_PASSWORD,
            old,
            Some(&text),
            actor,
            "first-run",
        )?;
        Ok(())
    })
}

fn reset_auth_state(conn: &Connection) -> Result<(), String> {
    let generation = new_generation().map_err(|_| STORAGE_ERROR.to_string())?;
    conn.execute("DELETE FROM console_browser_sessions", [])
        .map_err(storage_error)?;
    conn.execute(
        "INSERT INTO console_auth_state VALUES (1, ?1, 0, 0, 0)
         ON CONFLICT(id) DO UPDATE SET auth_generation=excluded.auth_generation,
             consecutive_failures=0, deadline_ms=0, clock_high_water_ms=0",
        [generation.as_slice()],
    )
    .map_err(storage_error)?;
    Ok(())
}

pub(crate) fn reset_administrator_password(context: &RuntimeContext) -> Result<(), (u16, String)> {
    write_administrator_password(context, true, |conn| {
        let unavailable = |error| (503, error);
        // Reset also removes rows whose value cannot be decoded as text or a supported PHC.
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1)",
                [ADMINISTRATOR_PASSWORD.key],
                |r| r.get(0),
            )
            .map_err(storage_error)
            .map_err(unavailable)?;
        if exists || schema_version(conn).map_err(unavailable)? == 3 {
            migrate_auth(conn).map_err(unavailable)?;
            reset_auth_state(conn).map_err(unavailable)?;
        }
        if exists {
            record_change(
                conn,
                &ADMINISTRATOR_PASSWORD,
                None,
                None,
                "her-cli",
                "reset-password",
            )?;
        }
        Ok(())
    })
}

#[derive(Default)]
pub(crate) struct ArchivedCredentials {
    pub(crate) has_endpoints: bool,
    pub(crate) has_password: bool,
}

fn archive_schema(conn: &Connection) -> Result<i64, String> {
    if !check_schema(conn)? {
        return Err(STORAGE_ERROR.to_string());
    }
    schema_version(conn)
}

fn auth_rows_exist(conn: &Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table})"),
        [],
        |row| row.get(0),
    )
    .map_err(storage_error)
}

pub(crate) fn archive_credentials(conn: &Connection) -> Result<ArchivedCredentials, String> {
    if archive_schema(conn)? == 3
        && (auth_rows_exist(conn, "console_browser_sessions")?
            || auth_rows_exist(conn, "console_auth_state")?)
    {
        return Err("backup settings contain operational console authentication state".to_string());
    }
    let has_endpoints = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key IN (?1, ?2) AND value <> 'null')",
            params![WEB_SEARCH_API_KEY.key, CLI_MODEL_API_KEY.key],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    let has_password = administrator_password_in(conn)?.is_some();
    Ok(ArchivedCredentials {
        has_endpoints,
        has_password,
    })
}

/// Sanitize only the private snapshot. Close it in rollback journal mode so the ZIP needs one file.
pub(crate) fn scrub_auth_snapshot(
    path: &Path,
    check: &dyn Fn() -> Result<(), String>,
) -> Result<(), String> {
    check()?;
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(storage_error)?;
    let version = archive_schema(&conn)?;
    let journal: String = conn
        .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
        .map_err(storage_error)?;
    if journal != "delete" {
        return Err(STORAGE_ERROR.to_string());
    }
    if version == 3 {
        conn.execute_batch("PRAGMA secure_delete=ON;")
            .map_err(storage_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        tx.execute("DELETE FROM console_browser_sessions", [])
            .map_err(storage_error)?;
        tx.execute("DELETE FROM console_auth_state", [])
            .map_err(storage_error)?;
        check()?;
        tx.commit().map_err(storage_error)?;
    }
    conn.close().map_err(|(_, error)| storage_error(error))?;
    for name in SIDECARS {
        if path
            .with_file_name(name)
            .try_exists()
            .map_err(|_| STORAGE_ERROR.to_string())?
        {
            return Err(STORAGE_ERROR.to_string());
        }
    }
    check()
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::settings::StoreProbe;

    fn old_store(version: i64) -> (crate::runtime::tests::FixtureRoot, RuntimeContext) {
        let (root, context) = crate::console_auth::tests::runtime(false);
        super::super::apply(
            &context,
            "reply.wakeQuietMs",
            &json!(4000),
            "fixture",
            "console",
        )
        .unwrap();
        if version == 2 {
            let mut conn = StoreProbe::at(&root.0).open();
            let tx = conn.transaction().unwrap();
            migrate(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(StoreProbe::at(&root.0).user_version(), version);
        (root, context)
    }

    #[test]
    fn console_auth_migrates_v1_and_v2_atomically() {
        for version in [1, 2] {
            let (root, context) = old_store(version);
            let store = StoreProbe::at(&root.0);
            let hash =
                AdministratorPasswordHash::parse(&crate::console_auth::tests::supported_phc())
                    .unwrap();
            set_initial_administrator_password(&context, &hash, "her-cli").unwrap();
            assert!(
                matches!(inspect(&context), Ok(Inspection::Ready(conn)) if schema_version(&conn) == Ok(3)),
                "reopen must recognize the committed v3 schema"
            );
            assert_eq!(store.value("reply.wakeQuietMs").as_deref(), Some("4000"));
            assert_eq!(
                administrator_password(&context).unwrap().unwrap().as_str(),
                hash.as_str()
            );
            assert_eq!(store.objects_named("conversation_settings"), 1);
        }
    }

    #[test]
    fn console_auth_migration_failure_leaves_old_schema() {
        for version in [1, 2] {
            let (root, context) = old_store(version);
            let store = StoreProbe::at(&root.0);
            let phc = json!(crate::console_auth::tests::supported_phc()).to_string();
            store.insert_value(ADMINISTRATOR_PASSWORD.key, &phc);
            let audit = store.audit_count();
            super::FAIL_AUTH_MIGRATION.with(|fail| fail.set(true));
            let result = reset_administrator_password(&context);
            assert_eq!(
                (
                    store.user_version(),
                    store.value(ADMINISTRATOR_PASSWORD.key)
                ),
                (version, Some(phc)),
                "failed auth migration must leave the old schema and credential"
            );
            assert!(result.is_err());
            assert_eq!(store.objects_named("console_auth_state"), 0);
            assert_eq!(store.audit_count(), audit);
        }
    }

    #[test]
    fn console_auth_v3_checks_row_ranges_and_schema_shape() {
        let (root, context) = crate::console_auth::tests::runtime(false);
        let store = StoreProbe::at(&root.0);
        let hash =
            AdministratorPasswordHash::parse(&crate::console_auth::tests::supported_phc()).unwrap();
        set_initial_administrator_password(&context, &hash, "her-cli").unwrap();
        let conn = store.open();
        conn.execute(
            "INSERT INTO console_browser_sessions VALUES
             (randomblob(32), 'http://her.localhost:4567', randomblob(32), randomblob(32),
              randomblob(32), 0, 1, 2, 1)",
            [],
        )
        .unwrap();
        for wrong_session in [
            "id_digest=randomblob(31)",
            "origin=''",
            "token_digest=hex(randomblob(16))",
            "auth_generation=randomblob(33)",
            "restore_generation=randomblob(31)",
            "created_ms=-1",
            "last_used_ms=-1",
            "expires_ms=last_used_ms",
            "issued_seq=0",
        ] {
            assert!(
                conn.execute(
                    &format!("UPDATE console_browser_sessions SET {wrong_session}"),
                    []
                )
                .is_err(),
                "{wrong_session}"
            );
        }
        assert!(
            conn.execute(
                "INSERT INTO console_browser_sessions
             SELECT randomblob(32), origin, token_digest, auth_generation, restore_generation,
                    created_ms, last_used_ms, expires_ms, issued_seq FROM console_browser_sessions",
                [],
            )
            .is_err(),
            "issuance sequence must be unique"
        );
        for wrong_state in [
            "id=2",
            "auth_generation=randomblob(31)",
            "consecutive_failures=-1",
            "deadline_ms=-1",
            "clock_high_water_ms=-1",
        ] {
            assert!(
                conn.execute(&format!("UPDATE console_auth_state SET {wrong_state}"), [])
                    .is_err(),
                "{wrong_state}"
            );
        }
        conn.execute_batch("CREATE INDEX unexpected ON console_auth_state(deadline_ms);")
            .unwrap();
        drop(conn);
        assert!(
            inspect(&context).is_err(),
            "v3 must have exactly the supported table and index shape"
        );
    }

    #[test]
    fn console_auth_reset_clears_auth_state() {
        let (root, context) = crate::console_auth::tests::runtime(false);
        let store = StoreProbe::at(&root.0);
        reset_administrator_password(&context).unwrap();
        assert!(!store.path().exists());
        let hash =
            AdministratorPasswordHash::parse(&crate::console_auth::tests::supported_phc()).unwrap();
        set_initial_administrator_password(&context, &hash, "her-cli").unwrap();
        let session_count = |conn: &Connection| {
            conn.query_row("SELECT count(*) FROM console_browser_sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        let state = |conn: &Connection| {
            conn.query_row("SELECT auth_generation, consecutive_failures, deadline_ms, clock_high_water_ms FROM console_auth_state",
                [], |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?)))
                .unwrap()
        };
        for password_was_set in [true, false] {
            store.edit(
                "UPDATE console_auth_state SET consecutive_failures=5, deadline_ms=60001, clock_high_water_ms=1;
                 INSERT INTO console_browser_sessions
                 SELECT randomblob(32), 'http://her.localhost:4567', randomblob(32),
                        auth_generation, randomblob(32), 0, 0, 1, 1 FROM console_auth_state;",
            );
            let before = state(&store.open());
            let audit = store.audit_count();
            if password_was_set {
                super::super::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
                assert!(reset_administrator_password(&context).is_err());
                assert_eq!(state(&store.open()), before);
                assert_eq!(session_count(&store.open()), 1);
                assert!(store.value(ADMINISTRATOR_PASSWORD.key).is_some());
                assert_eq!(store.audit_count(), audit);
            }
            reset_administrator_password(&context).unwrap();
            let conn = store.open();
            assert_eq!(
                session_count(&conn),
                0,
                "reset must remove every browser session row"
            );
            let (generation, failures, deadline, high_water) = state(&conn);
            assert_eq!(
                (failures, deadline),
                (0, 0),
                "reset must clear the durable password wait"
            );
            assert_eq!(high_water, 0);
            assert_ne!(generation, before.0);
            assert!(store.value(ADMINISTRATOR_PASSWORD.key).is_none());
            assert_eq!(store.audit_count(), audit + i64::from(password_was_set));
        }
    }
}
