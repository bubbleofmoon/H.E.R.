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
    if previous < 3 && FAIL_AUTH_MIGRATION.with(|fail| fail.replace(false)) {
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

use crate::console_auth::{
    digests_equal, restore_generation, BrowserProof, Generation, BROWSER_IDLE_MS, MAX_BROWSER_SESSIONS,
};
use rusqlite::OptionalExtension;

pub(crate) struct BrowserAdmission {
    pub(crate) proof: BrowserProof,
    pub(crate) renewed: bool,
}

fn unavailable(error: String) -> (u16, String) {
    (503, error)
}

fn session_write<T>(
    context: &RuntimeContext,
    operation: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<Option<T>, (u16, String)> {
    if matches!(super::inspect(context).map_err(unavailable)?, Inspection::Missing | Inspection::Fresh) {
        return Ok(None);
    }
    let _gate = store_gate(context, Instant::now() + WAL_SWITCH_WAIT).map_err(unavailable)?;
    let Inspection::Ready(conn) = super::inspect(context).map_err(unavailable)? else {
        return Ok(None);
    };
    if schema_version(&conn).map_err(unavailable)? != 3 {
        return Ok(None);
    }
    if !auth_rows_exist(&conn, "console_auth_state").map_err(unavailable)? {
        if auth_rows_exist(&conn, "console_browser_sessions").map_err(unavailable)? {
            return Err(unavailable(STORAGE_ERROR.to_string()));
        }
        return Ok(None);
    }
    drop(conn);
    let mut conn = open_for_write(context).map_err(unavailable)?;
    conn.execute_batch("PRAGMA secure_delete=ON;").map_err(storage_error).map_err(unavailable)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error).map_err(unavailable)?;
    let result = operation(&tx).map_err(unavailable)?;
    tx.commit().map_err(storage_error).map_err(unavailable)?;
    Ok(Some(result))
}

fn invalidate_sessions(conn: &Connection, clock: Option<i64>) -> Result<(), String> {
    let generation = new_generation().map_err(|_| STORAGE_ERROR.to_string())?;
    conn.execute("DELETE FROM console_browser_sessions", []).map_err(storage_error)?;
    conn.execute(
        "UPDATE console_auth_state SET auth_generation=?1,
         clock_high_water_ms=coalesce(?2,clock_high_water_ms) WHERE id=1",
        params![generation.as_slice(), clock],
    ).map_err(storage_error)?;
    Ok(())
}

pub(crate) fn revoke_browser_sessions(context: &RuntimeContext) -> Result<(), (u16, String)> {
    session_write(context, |conn| invalidate_sessions(conn, None))?;
    context.browser_anchors.0.lock().map_err(|_| unavailable(STORAGE_ERROR.to_string()))?.clear();
    Ok(())
}

fn browser_clock_state(conn: &Connection) -> Result<(Generation, i64), String> {
    let (generation, failures, deadline, mark): (Vec<u8>, i64, i64, i64) = conn.query_row(
        "SELECT auth_generation,consecutive_failures,deadline_ms,clock_high_water_ms
         FROM console_auth_state WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).map_err(storage_error)?;
    let generation: Generation = generation.try_into().map_err(|_| STORAGE_ERROR.to_string())?;
    if failures < 0 || deadline < 0 || mark < 0 {
        return Err(STORAGE_ERROR.to_string());
    }
    Ok((generation, mark))
}

/// The clock transition commits even if there is no valid authority in the request.
fn observe_browser_clock(conn: &Connection, now: i64) -> Result<Option<(Generation, i64)>, String> {
    let (generation, mark) = browser_clock_state(conn)?;
    if now < 0 { return Err(STORAGE_ERROR.to_string()); }
    if now < mark.saturating_sub(60_000) {
        invalidate_sessions(conn, Some(now))?;
        return Ok(None);
    }
    if now > mark {
        conn.execute("UPDATE console_auth_state SET clock_high_water_ms=?1 WHERE id=1", [now])
            .map_err(storage_error)?;
    }
    Ok(Some((generation, now.max(mark))))
}

fn session_timestamps_valid(created: i64, last: i64, expires: i64, now: i64) -> bool {
    0 <= created && created <= last && last <= now && last.checked_add(BROWSER_IDLE_MS) == Some(expires)
}

struct SessionRow {
    origin: String,
    token: Generation,
    auth: Generation,
    restore: Generation,
    created: i64,
    last: i64,
    expires: i64,
    sequence: i64,
}

fn load_session(conn: &Connection, id: &Generation) -> Result<Option<SessionRow>, String> {
    let result = conn.query_row(
        "SELECT origin,token_digest,auth_generation,restore_generation,created_ms,last_used_ms,expires_ms,issued_seq
         FROM console_browser_sessions WHERE id_digest=?1",
        [id.as_slice()],
        |row| Ok(SessionRow {
            origin: row.get(0)?, token: row.get(1)?, auth: row.get(2)?, restore: row.get(3)?,
            created: row.get(4)?, last: row.get(5)?, expires: row.get(6)?, sequence: row.get(7)?,
        }),
    ).optional();
    match result {
        Ok(row) => Ok(row),
        Err(rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)) => {
            delete_session(conn, id)?;
            Ok(None)
        }
        Err(error) => Err(storage_error(error)),
    }
}

fn delete_session(conn: &Connection, id: &Generation) -> Result<(), String> {
    conn.execute("DELETE FROM console_browser_sessions WHERE id_digest=?1", [id.as_slice()])
        .map_err(storage_error)?;
    Ok(())
}

fn live_session(
    conn: &Connection, id: &Generation, now: i64, expiry_clock: i64,
    current: &BrowserProof, token: &Generation,
) -> Result<Option<SessionRow>, String> {
    let Some(row) = load_session(conn, id)? else { return Ok(None) };
    if row.sequence <= 0 || !session_timestamps_valid(row.created, row.last, row.expires, now)
        || expiry_clock >= row.expires
        || !digests_equal(&row.auth, &current.auth_generation)
        || !digests_equal(&row.restore, &current.restore_generation)
        || !digests_equal(&row.token, token)
    {
        delete_session(conn, id)?;
        return Ok(None);
    }
    Ok((row.origin == current.origin).then_some(row))
}

fn session_ids(conn: &Connection) -> Result<Vec<(Generation, i64)>, String> {
    let mut query = conn.prepare(
        "SELECT id_digest,last_used_ms FROM console_browser_sessions LIMIT ?1",
    ).map_err(storage_error)?;
    let rows = query.query_map([MAX_BROWSER_SESSIONS as i64 + 1], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(storage_error)?.collect::<Result<Vec<_>, _>>().map_err(storage_error)?;
    if rows.len() > MAX_BROWSER_SESSIONS {
        return Err(STORAGE_ERROR.to_string());
    }
    Ok(rows)
}

pub(crate) fn start_browser_clock(context: &RuntimeContext) -> Result<(), (u16, String)> {
    let now = crate::parity_clock::now_ms();
    let monotonic = Instant::now();
    let rows = session_write(context, |conn| {
        observe_browser_clock(conn, now)?;
        session_ids(conn)
    })?.unwrap_or_default();
    let mut anchors = context.browser_anchors.0.lock().map_err(|_| unavailable(STORAGE_ERROR.to_string()))?;
    *anchors = rows.into_iter().map(|(id, last)| (id, (last, monotonic))).collect();
    Ok(())
}

pub(crate) fn admit_browser(
    context: &RuntimeContext, origin: &str, token: Option<Generation>, ids: &[Generation], renew: bool,
) -> Result<Option<BrowserAdmission>, (u16, String)> {
    admit_browser_at(context, origin, token, ids, renew, crate::parity_clock::now_ms(), Instant::now())
}

fn admit_browser_at(
    context: &RuntimeContext, origin: &str, token: Option<Generation>, ids: &[Generation],
    renew: bool, now: i64, monotonic: Instant,
) -> Result<Option<BrowserAdmission>, (u16, String)> {
    let mut anchors = context.browser_anchors.0.lock().map_err(|_| unavailable(STORAGE_ERROR.to_string()))?;
    let restore = restore_generation(context);
    let (admission, rows) = session_write(context, |conn| {
        let result = (|| -> Result<Option<BrowserAdmission>, String> {
            let Some((auth, expiry_clock)) = observe_browser_clock(conn, now)? else { return Ok(None) };
            let (Some(token), Some(restore)) = (token, restore.as_ref().ok().copied().flatten()) else {
                return Ok(None);
            };
            let mut proof = BrowserProof { id_digest: [0; 32], origin: origin.to_owned(), auth_generation: auth, restore_generation: restore };
            let mut selected: Option<SessionRow> = None;
            for id in ids {
                if let Some(row) = live_session(conn, id, now, expiry_clock, &proof, &token)? {
                    if selected.as_ref().is_none_or(|current| row.sequence > current.sequence) {
                        proof.id_digest = *id;
                        selected = Some(row);
                    }
                }
            }
            let Some(row) = selected else { return Ok(None) };
            let renewed = if let Some((wall, instant)) = anchors.get(&proof.id_digest).filter(|_| renew) {
                let elapsed = i64::try_from(monotonic.saturating_duration_since(*instant).as_millis()).unwrap_or(i64::MAX);
                let safe = now; // RED-R3-R
                if let Some(expires) = safe.checked_add(BROWSER_IDLE_MS).filter(|_| safe > row.last) {
                    conn.execute(
                        "UPDATE console_browser_sessions SET last_used_ms=?1,expires_ms=?2
                         WHERE id_digest=?3 AND last_used_ms=?4",
                        params![safe, expires, proof.id_digest.as_slice(), row.last],
                    ).map_err(storage_error)? == 1
                } else { false }
            } else { false };
            Ok(Some(BrowserAdmission { proof, renewed }))
        })()?;
        Ok((result, session_ids(conn)?))
    })?.unwrap_or_default();
    anchors.retain(|id, _| rows.iter().any(|(current, _)| current == id));
    for (id, last) in rows {
        anchors.entry(id).or_insert((last, monotonic));
    }
    restore.map_err(unavailable)?;
    Ok(admission)
}

pub(crate) fn recheck_browser(
    context: &RuntimeContext, proof: &BrowserProof, token: Option<Generation>,
) -> Result<bool, (u16, String)> {
    let now = crate::parity_clock::now_ms();
    let restore = restore_generation(context);
    let valid = session_write(context, |conn| {
        let Some((auth, expiry_clock)) = observe_browser_clock(conn, now)? else { return Ok(false) };
        let (Some(token), Some(restore)) = (token, restore.as_ref().ok().copied().flatten()) else { return Ok(false) };
        if !digests_equal(&auth, &proof.auth_generation) || !digests_equal(&restore, &proof.restore_generation) {
            return Ok(false);
        }
        Ok(live_session(conn, &proof.id_digest, now, expiry_clock, proof, &token)?.is_some())
    })?.unwrap_or(false);
    restore.map_err(unavailable)?;
    Ok(valid)
}

#[cfg(test)]
pub(crate) fn seed_browser_session(
    context: &RuntimeContext, port: u16, token: &str, id: Generation, last: i64,
) -> Result<(), (u16, String)> {
    let restore = match restore_generation(context).map_err(unavailable)? {
        Some(value) => value,
        None => crate::console_auth::rotate_restore_generation(context).map_err(|_| unavailable(STORAGE_ERROR.to_string()))?,
    };
    write_administrator_password(context, false, |conn| {
        migrate_auth(conn).map_err(unavailable)?;
        initialize_auth_state(conn).map_err(unavailable)?;
        conn.execute(
            "INSERT INTO console_browser_sessions
             SELECT ?1,?2,?3,auth_generation,?4,?5,?5,?6,
             (SELECT coalesce(max(issued_seq),0)+1 FROM console_browser_sessions) FROM console_auth_state WHERE id=1",
            params![id.as_slice(), crate::console_auth::canonical_origin(port),
                crate::console_auth::credential_digest(token).as_slice(), restore.as_slice(), last, last + BROWSER_IDLE_MS],
        ).map_err(storage_error).map_err(unavailable)?;
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
    use super::{admit_browser_at, BrowserAdmission, Generation, BROWSER_IDLE_MS};
    use std::time::Instant;
    use crate::settings::StoreProbe;

    fn browser_clock_fixture(last: i64) -> (crate::runtime::tests::FixtureRoot, RuntimeContext, Generation) {
        let (root, context) = crate::console_auth::tests::runtime(true);
        let id = [72; 32];
        let token = context.effective_console_token(&context.load_config().unwrap());
        seed_browser_session(&context, 3999, &token, id, last).unwrap();
        (root, context, id)
    }

    fn check_at(context: &RuntimeContext, id: Generation, now: i64, monotonic: Instant) -> Option<BrowserAdmission> {
        let token = context.effective_console_token(&context.load_config().unwrap());
        admit_browser_at(context, "http://her.localhost:3999", Some(crate::console_auth::credential_digest(&token)), &[id], true, now, monotonic).unwrap()
    }

    #[test]
    fn browser_session_renews_until_the_exact_expiry_boundary() {
        let (_root, context, id) = browser_clock_fixture(100_000);
        let anchor = Instant::now();
        context.browser_anchors.0.lock().unwrap().insert(id, (100_000, anchor));
        assert!(check_at(&context, id, 101_000, anchor + std::time::Duration::from_secs(1)).unwrap().renewed);
        assert!(check_at(&context, id, 101_000 + BROWSER_IDLE_MS - 1, anchor + std::time::Duration::from_secs(1)).is_some());
        assert!(check_at(&context, id, 101_000 + BROWSER_IDLE_MS, anchor + std::time::Duration::from_secs(1)).is_none(),
            "a session must expire at its exact seven-day boundary");
    }

    #[test]
    fn future_browser_timestamp_fails_closed_after_restart() {
        let (root, context, id) = browser_clock_fixture(120_000);
        let restarted = RuntimeContext::from_root(&root.0).unwrap();
        assert!(check_at(&restarted, id, 119_999, Instant::now()).is_none(),
            "a future persisted last-use must refuse after a tolerated backward step");
        assert!(check_at(&context, id, 120_000, Instant::now()).is_none(),
            "the invalid row must be removed when checked");
    }

    #[test]
    fn wall_clock_jump_cannot_extend_absolute_browser_expiry() {
        let (root, context, id) = browser_clock_fixture(120_000);
        let anchor = Instant::now();
        context.browser_anchors.0.lock().unwrap().insert(id, (120_000, anchor));
        let result = check_at(&context, id, 120_000 + BROWSER_IDLE_MS / 2, anchor).unwrap();
        assert!(!result.renewed, "a forward wall-only step must never renew a browser cookie");
        let conn = StoreProbe::at(&root.0).open();
        let expiry: i64 = conn.query_row(
            "SELECT expires_ms FROM console_browser_sessions WHERE id_digest=?",
            [id.as_slice()], |row| row.get(0),
        ).unwrap();
        assert_eq!(expiry, 120_000 + BROWSER_IDLE_MS);
        drop(conn);
        assert!(check_at(&context, id, 120_000 + BROWSER_IDLE_MS / 2, anchor + std::time::Duration::from_secs(1)).unwrap().renewed);
        let restarted = RuntimeContext::from_root(&root.0).unwrap();
        assert!(!check_at(&restarted, id, 120_000 + BROWSER_IDLE_MS / 2, Instant::now()).unwrap().renewed);
        assert!(check_at(&restarted, id, 121_000 + BROWSER_IDLE_MS, Instant::now()).is_none());
    }

    #[test]
    fn browser_clock_tolerates_one_minute_and_revokes_everything_one_tick_earlier() {
        let root_time = 250_000;
        let (root, context, id) = browser_clock_fixture(root_time - 61_000);
        let token = context.effective_console_token(&context.load_config().unwrap());
        seed_browser_session(&context, 3999, &token, [87; 32], root_time - 61_000).unwrap();
        let anchor = Instant::now();
        assert!(check_at(&context, id, root_time, anchor).is_some());
        assert!(check_at(&context, id, root_time - 60_000, anchor).is_some());
        assert!(check_at(&context, [11; 32], root_time - 60_001, anchor).is_none());
        let db = StoreProbe::at(&root.0).open();
        let remaining: i64 = db.query_row("SELECT count(*) FROM console_browser_sessions", [], |row| row.get(0)).unwrap();
        assert_eq!(remaining, 0);
        let mark: i64 = db.query_row("SELECT clock_high_water_ms FROM console_auth_state", [], |row| row.get(0)).unwrap();
        assert_eq!(mark, root_time - 60_001);
    }

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
