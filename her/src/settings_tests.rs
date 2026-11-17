// The settings store (`settings.rs`) and its console routes (`console_settings.rs`), through the console API on a
// fixture runtime and through the configuration loader, as the next use reads a setting.
use super::*;

const QUIET_KEY: &str = "reply.wakeQuietMs";
const LEFTOVER_JOURNAL_ERROR: &str = "settings storage has a leftover journal; remove settings.db and settings.db-journal";

fn settings_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    fixture_runtime(label, "fixture-settings-token-abcdefghij")
}

fn settings_db(root: &FixtureRoot) -> std::path::PathBuf {
    root.0.join("data/settings.db")
}

fn set_quiet(context: &RuntimeContext, value: Value) -> (u16, Value) {
    admin_request(context, "POST", "/api/settings/set", Some(&json!({"setting": QUIET_KEY, "value": value})))
}

/// The `reply.wakeQuietMs` entry of `GET /api/settings`.
fn quiet_view(context: &RuntimeContext) -> Value {
    let (status, body) = admin_request(context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    body["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == QUIET_KEY).unwrap().clone()
}

/// What the next wake reads, through the configuration loader.
fn next_use_quiet(context: &RuntimeContext) -> Option<i64> {
    crate::wake_cfg_of(&context.load_config().unwrap()).reply_wake_quiet_ms
}

/// The audit rows, oldest first.
fn audit_rows(root: &FixtureRoot) -> Vec<Value> {
    let conn = rusqlite::Connection::open_with_flags(settings_db(root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
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

fn write_config(root: &FixtureRoot, edit: impl FnOnce(&mut Value)) {
    let path = root.0.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut config);
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
}

#[test]
fn settings_set_is_live_on_the_next_use_with_one_audit_row_per_change() {
    let (root, context) = settings_fixture("settings live");
    let config_path = root.0.join("config/config.json");
    let stored_file = std::fs::read(&config_path).unwrap();
    let view = quiet_view(&context);
    assert_eq!(view["label"], "Quiet time before a message wake");
    assert_eq!(view["applies"], "next wake");
    assert_eq!(view["group"], "reply rhythm");
    assert_eq!(view["kind"], json!({"type": "integer", "min": 1, "max": 30000}));
    assert_eq!(view["default"], 3000);
    assert_eq!(view["stored"], Value::Null);
    assert_eq!(view["value"], 3000);
    assert_eq!(next_use_quiet(&context), Some(3000));
    assert!(!settings_db(&root).exists(), "reading settings never creates the store");

    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": QUIET_KEY, "value": 4500, "revision": 1, "changed": true}));
    assert_eq!(next_use_quiet(&context), Some(4500), "the next use reads the new value");
    assert_eq!(context.load_config_document().unwrap()["socialV2"]["reply"]["wakeQuietMs"], 4500);
    assert_eq!(quiet_view(&context)["stored"], 4500);
    assert_eq!(quiet_view(&context)["value"], 4500);
    assert_eq!(std::fs::read(&config_path).unwrap(), stored_file, "the store never writes config.json");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["actor"], "administrator");
    assert_eq!(rows[0]["source"], "console");
    assert_eq!(rows[0]["scope"], "global");
    assert_eq!(rows[0]["key"], QUIET_KEY);
    assert_eq!(rows[0]["old"], Value::Null, "no stored value before");
    assert_eq!(rows[0]["new"], "4500");
    assert!(!rows[0]["request"].as_str().unwrap().is_empty());

    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2);
    assert_eq!(next_use_quiet(&context), Some(5000));
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[1]["old"].clone(), rows[1]["new"].clone()), (json!("4500"), json!("5000")));
    assert_ne!(rows[0]["request"], rows[1]["request"]);

    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": QUIET_KEY, "value": 5000, "revision": 2, "changed": false}));
    assert_eq!(audit_rows(&root).len(), 2, "the same value again is not a change");
}

#[test]
fn settings_set_refuses_invalid_values_and_keeps_the_old_value() {
    let (root, context) = settings_fixture("settings refused");
    for (value, revision) in [(json!(4500), 1), (json!(1), 2), (json!(30000), 3)] {
        let (status, body) = set_quiet(&context, value.clone());
        assert_eq!(status, 200, "{value}: {body}");
        assert_eq!(body["revision"], revision, "{value}");
    }
    for value in [
        json!(0),
        json!(30001),
        json!(-1),
        json!(4500.5),
        json!(3000.0),
        json!("4500"),
        json!(true),
        Value::Null,
        json!([4500]),
        json!({"ms": 4500}),
        json!(1e30),
        json!(u64::MAX),
    ] {
        let (status, body) = set_quiet(&context, value.clone());
        assert_eq!(status, 400, "{value}: {body}");
        assert_eq!(body["error"], "reply.wakeQuietMs must be an integer from 1 to 30000", "{value}");
    }
    for (request, error) in [
        (json!({"setting": "reply.other", "value": 4500}), "unknown setting"),
        (json!({"setting": "/socialV2/reply/wakeQuietMs", "value": 4500}), "unknown setting"),
        (json!({"setting": 7, "value": 4500}), "setting is required"),
        (json!({"value": 4500}), "setting is required"),
        (json!({"setting": QUIET_KEY}), "value is required"),
        (json!({"setting": QUIET_KEY, "value": 4500, "confirm": true}), "unknown settings field"),
        (json!([QUIET_KEY, 4500]), "body must be a JSON object"),
    ] {
        let (status, body) = admin_request(&context, "POST", "/api/settings/set", Some(&request));
        assert_eq!(status, 400, "{request}: {body}");
        assert_eq!(body["error"], error, "{request}");
    }
    assert_eq!(quiet_view(&context)["stored"], 30000, "every refusal keeps the old value");
    assert_eq!(next_use_quiet(&context), Some(30000));
    assert_eq!(audit_rows(&root).len(), 3, "a refusal writes no audit row");
}

#[test]
fn settings_store_never_opens_memory_db() {
    let (root, context) = settings_fixture("settings isolation");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(quiet_view(&context)["stored"], 4500);
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert!(settings_db(&root).is_file(), "the store is data/settings.db");
    for name in ["memory.db", "memory.db-wal", "memory.db-shm", "memory.db-journal"] {
        assert!(!root.0.join("data").join(name).exists(), "{name} was created");
    }
    // The store's source names its own database only, and no memory module, area or attached database.
    let source = include_str!("settings.rs");
    assert!(source.contains("\"settings.db\""), "the store names its own database file");
    for needle in ["\"memory.db\"", "crate::memory", "RuntimeArea::Memory", "ATTACH", "attach"] {
        assert!(!source.contains(needle), "settings.rs contains {needle}");
    }
}

#[test]
fn settings_store_refuses_a_newer_schema_and_reads_the_file() {
    // A newer release's WAL store, closed cleanly: everything is in the file and no sidecar is left.
    let (root, context) = settings_fixture("settings newer schema");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)).unwrap();
    assert_eq!(mode, "wal");
    conn.execute_batch("CREATE TABLE later (x INTEGER); PRAGMA user_version=2;").unwrap();
    drop(conn);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "the fixture left settings.db{suffix}");
    }
    let before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], crate::settings::NEWER_SCHEMA_ERROR);
    assert!(context.load_config_document().unwrap().get("socialV2").is_none(), "the loader reads the file");
    assert_eq!(next_use_quiet(&context), Some(3000));
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], crate::settings::NEWER_SCHEMA_ERROR);
    assert_eq!(body["settings"][0]["value"], 3000);
    // Checked before any verification connection opens the file.
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "a newer store keeps every byte");
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "the refusal created settings.db{suffix}");
    }
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, 2, "a newer store is never migrated down");
    let tables: i64 =
        conn.query_row("SELECT count(*) FROM sqlite_master WHERE name='settings'", [], |row| row.get(0)).unwrap();
    assert_eq!(tables, 0, "a newer store is never written");
}

#[test]
fn settings_store_rolls_back_a_change_cut_off_before_its_audit_row() {
    let (root, context) = settings_fixture("settings rollback");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 503, "{body}");
    assert_eq!(quiet_view(&context)["stored"], 4500, "the cut-off change left no value");
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(audit_rows(&root).len(), 1, "and no audit row");
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2, "the revision continues from the last committed change");
}

#[test]
fn settings_store_serializes_concurrent_writers() {
    let (root, context) = settings_fixture("settings concurrent");
    let (first, second) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            for _ in 0..20 {
                let seen = next_use_quiet(&context);
                assert!(matches!(seen, Some(3000 | 4000 | 5000)), "{seen:?}");
            }
        });
        let first = scope.spawn(|| crate::settings::apply(&context, QUIET_KEY, &json!(4000), "administrator", "console"));
        let second = scope.spawn(|| crate::settings::apply(&context, QUIET_KEY, &json!(5000), "administrator", "console"));
        reader.join().unwrap();
        (first.join().unwrap().unwrap(), second.join().unwrap().unwrap())
    });
    assert!(first.changed && second.changed);
    let mut revisions = [first.revision, second.revision];
    revisions.sort();
    assert_eq!(revisions, [1, 2]);
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["old"], Value::Null);
    assert_eq!(rows[1]["old"], rows[0]["new"], "the later writer saw the earlier commit");
    let last: i64 = rows[1]["new"].as_str().unwrap().parse().unwrap();
    assert_eq!(quiet_view(&context)["stored"], last);
    assert_eq!(next_use_quiet(&context), Some(last));
}

#[test]
fn settings_overlay_ignores_rows_it_cannot_use() {
    let (root, context) = settings_fixture("settings old rows");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    conn.execute_batch(
        "UPDATE settings SET value='50000' WHERE key='reply.wakeQuietMs';
         INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES('reply.retired', '1', 1, 0, 'test');",
    )
    .unwrap();
    drop(conn);
    assert_eq!(next_use_quiet(&context), Some(3000), "an out-of-range stored value is not applied");
    assert!(context.load_config_document().unwrap().get("reply").is_none(), "an unknown key is not applied");
    assert_eq!(quiet_view(&context)["stored"], Value::Null);
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2);
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(audit_rows(&root)[1]["old"], "50000", "the audit keeps what was stored");
}

#[test]
fn settings_overlay_creates_missing_objects_and_keeps_other_members() {
    let (root, context) = settings_fixture("settings overlay shape");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let quiet = |context: &RuntimeContext| context.load_config_document().unwrap().pointer("/socialV2/reply/wakeQuietMs").cloned();
    assert_eq!(quiet(&context), Some(json!(4500)), "missing objects are created");
    write_config(&root, |config| config["socialV2"] = Value::Null);
    assert_eq!(quiet(&context), Some(json!(4500)), "a null object is replaced");
    write_config(&root, |config| config["socialV2"] = json!({"enabled": true, "reply": {"quietMinMs": 4000, "wakeQuietMs": 9000}}));
    let document = context.load_config_document().unwrap();
    assert_eq!(document["socialV2"], json!({"enabled": true, "reply": {"quietMinMs": 4000, "wakeQuietMs": 4500}}));
    assert_eq!(quiet_view(&context)["value"], 4500, "the stored value wins over the file's");
    write_config(&root, |config| config["socialV2"] = json!(5));
    assert_eq!(context.load_config_document().unwrap()["socialV2"], 5, "a non-object is left as it is");
    write_config(&root, |config| config["socialV2"] = json!({"reply": "x"}));
    assert_eq!(context.load_config_document().unwrap()["socialV2"], json!({"reply": "x"}));
    assert_eq!(quiet_view(&context)["value"], 3000, "the value the next use reads");
}

#[test]
fn settings_trouble_never_breaks_the_configuration() {
    let (root, context) = settings_fixture("settings corrupt store");
    let garbage = vec![b'x'; 4096];
    std::fs::write(settings_db(&root), &garbage).unwrap();
    assert_eq!(next_use_quiet(&context), Some(3000), "the loader still reads the file");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), garbage, "a store it cannot read is never written");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], "settings storage is unavailable");
    assert_eq!(body["settings"][0]["value"], 3000);
}

#[test]
fn settings_store_finishes_a_store_cut_off_before_its_schema() {
    // A crash after SQLite created the file and before the schema transaction committed leaves an empty file.
    let (root, context) = settings_fixture("settings empty store");
    std::fs::write(settings_db(&root), b"").unwrap();
    assert_eq!(next_use_quiet(&context), Some(3000));
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.get("storeError").is_none(), "a store cut off before its schema is not an error: {body}");
    assert_eq!(quiet_view(&context)["stored"], Value::Null);
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(audit_rows(&root).len(), 1);
}

#[cfg(unix)]
#[test]
fn settings_store_refuses_a_linked_database() {
    let (root, context) = settings_fixture("settings linked store");
    let target = root.0.join("elsewhere.db");
    std::fs::write(&target, b"").unwrap();
    std::os::unix::fs::symlink(&target, settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert_eq!(std::fs::read(&target).unwrap(), b"", "the link target is never written");
    assert_eq!(next_use_quiet(&context), Some(3000), "the loader still reads the file");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], "settings storage is unavailable");
}

#[test]
fn settings_set_never_writes_when_the_credential_was_revoked() {
    // The set route's authority re-checks the credential under the configuration gate (`console.rs`).
    let (root, context) = settings_fixture("settings revoked");
    let body = json!({"setting": QUIET_KEY, "value": 4500}).to_string();
    let revoked = || -> Result<Option<std::fs::File>, (u16, String)> { Err((401, "console credential was revoked".to_string())) };
    let (status, _, reply) = crate::console_settings::route(&context, "POST", "/api/settings/set", &body, false, &revoked);
    assert_eq!(status, 401, "{reply}");
    assert!(!settings_db(&root).exists(), "a revoked credential never writes");
    assert_eq!(next_use_quiet(&context), Some(3000));
    let granted = || -> Result<Option<std::fs::File>, (u16, String)> { Ok(None) };
    let (status, _, reply) = crate::console_settings::route(&context, "POST", "/api/settings/set", &body, false, &granted);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(next_use_quiet(&context), Some(4500));
}

#[test]
fn settings_routes_refuse_agents_legacy_contexts_and_oversized_bodies() {
    let legacy = RuntimeContext::legacy();
    let applied = crate::settings::apply(&legacy, QUIET_KEY, &json!(4500), "administrator", "console");
    assert_eq!(applied.err(), Some((503, "settings require a selected runtime".to_string())));
    let document = json!({"socialV2": {"reply": {"wakeQuietMs": 9000}}});
    assert_eq!(crate::settings::overlay(&legacy, document.clone()), document, "a legacy context has no store");

    let (root, context) = settings_fixture("settings refusals");
    let request = json!({"setting": QUIET_KEY, "value": 4500});
    let (status, body) = agent_request(&context, "POST", "/api/settings/set", Some(&request), "fixture-agent-token");
    assert_eq!(status, 403, "{body}");
    let (status, body) = agent_request(&context, "GET", "/api/settings", None, "fixture-agent-token");
    assert_eq!(status, 403, "{body}");
    let oversized = json!({"setting": QUIET_KEY, "value": "x".repeat(1_000_001)});
    let (status, body) = admin_request(&context, "POST", "/api/settings/set", Some(&oversized));
    assert_eq!(status, 413, "{body}");
    for (method, path) in [("POST", "/api/settings"), ("GET", "/api/settings/set"), ("POST", "/api/settings/reset")] {
        let (status, body) = admin_request(&context, method, path, Some(&request));
        assert_eq!(status, 404, "{method} {path}: {body}");
    }
    assert!(!settings_db(&root).exists(), "no refused request created the store");
}

#[test]
fn settings_store_waits_for_another_writer_while_it_switches_a_new_store_to_wal() {
    // Another connection (another writer, in any process) holds the new, still rollback-journal store's write lock.
    // The switch to WAL then gets SQLITE_BUSY at once, without `busy_timeout`; it must wait, not fail.
    let (root, context) = settings_fixture("settings wal switch");
    std::fs::write(settings_db(&root), b"").unwrap();
    let holder = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    holder.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let busy = || crate::settings::WAL_SWITCH_BUSY.load(std::sync::atomic::Ordering::SeqCst);
    let busy_before = busy();
    let applied = std::thread::scope(|scope| {
        let writer = scope.spawn(|| crate::settings::apply(&context, QUIET_KEY, &json!(4500), "administrator", "console"));
        // Release the lock only once the writer is known to have met it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while busy() == busy_before {
            assert!(std::time::Instant::now() < deadline, "the writer never met the held lock");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        holder.execute_batch("COMMIT;").unwrap();
        writer.join().unwrap()
    });
    let applied = applied.unwrap();
    assert_eq!((applied.revision, applied.changed), (1, true));
    drop(holder);
    assert_eq!(next_use_quiet(&context), Some(4500));
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn settings_store_refuses_a_version_one_store_without_its_tables_and_leaves_it_unchanged() {
    // A store that claims schema version 1 but has no tables is refused before anything is written: the WAL switch
    // rewrites the file header, so the file must not be switched either.
    let (root, context) = settings_fixture("settings tableless store");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    conn.execute_batch("PRAGMA user_version=1;").unwrap();
    drop(conn);
    let before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "a refused store is never touched");
    assert!(!root.0.join("data/settings.db-wal").exists(), "no sidecar was created");
}

/// A version-2 database with one row, standing in for `memory.db`.
fn other_database(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes(body) VALUES ('kept');
         PRAGMA user_version=2;",
    )
    .unwrap();
}

#[test]
fn settings_store_refuses_a_hard_linked_database_and_never_touches_the_other_file() {
    // The containment checks see paths: a second name for another database must be refused before SQLite opens it.
    let (root, context) = settings_fixture("settings hard link");
    let memory = root.0.join("data/memory.db");
    other_database(&memory);
    std::fs::hard_link(&memory, settings_db(&root)).unwrap();
    let before = std::fs::read(&memory).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert_eq!(next_use_quiet(&context), Some(3000));
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], "settings storage is unavailable");
    assert_eq!(std::fs::read(&memory).unwrap(), before, "the other database keeps every byte");
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/memory.db{suffix}")).exists(), "memory.db{suffix} was created");
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "settings.db{suffix} was created");
    }
}

#[test]
fn settings_store_refuses_a_hard_linked_sidecar() {
    let (root, context) = settings_fixture("settings hard linked sidecar");
    let memory = root.0.join("data/memory.db");
    other_database(&memory);
    std::fs::write(settings_db(&root), b"").unwrap();
    let journal = root.0.join("data/settings.db-journal");
    std::fs::hard_link(&memory, &journal).unwrap();
    let before = std::fs::read(&memory).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert!(journal.is_file(), "the linked sidecar is left in place");
    assert_eq!(std::fs::read(&memory).unwrap(), before, "the other database keeps every byte");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), b"", "the store is left as it was");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], "settings storage is unavailable");
}

#[test]
fn settings_store_refuses_a_version_one_store_with_other_columns_and_leaves_it_unchanged() {
    let (root, context) = settings_fixture("settings other columns");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    conn.execute_batch("CREATE TABLE settings (x); CREATE TABLE settings_audit (x); PRAGMA user_version=1;").unwrap();
    drop(conn);
    let before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "a refused store is never touched");
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "settings.db{suffix} was created");
    }
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], "settings storage is unavailable");
}

#[test]
fn settings_store_refusal_leaves_uncheckpointed_frames_alone() {
    // A newer release's store whose process ended with committed frames still in the WAL.
    let (root, context) = settings_fixture("settings newer frames");
    let wal = root.0.join("data/settings.db-wal");
    let shm = root.0.join("data/settings.db-shm");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).unwrap();
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)).unwrap();
    assert_eq!(mode, "wal");
    conn.execute_batch("CREATE TABLE later (x INTEGER); INSERT INTO later VALUES (1); PRAGMA user_version=2;").unwrap();
    drop(conn);
    let db_before = std::fs::read(settings_db(&root)).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    assert!(!wal_before.is_empty() && shm.is_file(), "the fixture leaves frames in the WAL");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], crate::settings::NEWER_SCHEMA_ERROR);
    assert_eq!(next_use_quiet(&context), Some(3000));
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], crate::settings::NEWER_SCHEMA_ERROR);
    assert_eq!(std::fs::read(settings_db(&root)).ok(), Some(db_before), "the database keeps every byte");
    assert_eq!(std::fs::read(&wal).ok(), Some(wal_before), "the WAL keeps every frame");
    assert!(shm.is_file(), "the index is not removed");
    assert!(!root.0.join("data/settings.db-journal").exists(), "no journal was created");
}

#[test]
fn settings_store_refuses_a_newer_store_with_a_journal_and_leaves_both() {
    let (root, context) = settings_fixture("settings newer journal");
    let journal = root.0.join("data/settings.db-journal");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    conn.execute_batch("CREATE TABLE later (x INTEGER); PRAGMA user_version=2;").unwrap();
    drop(conn);
    std::fs::write(&journal, vec![0xff_u8; 512]).unwrap();
    let db_before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], crate::settings::NEWER_SCHEMA_ERROR);
    assert_eq!(next_use_quiet(&context), Some(3000));
    assert_eq!(std::fs::read(&journal).ok(), Some(vec![0xff_u8; 512]), "the journal is never rolled back or removed");
    assert_eq!(std::fs::read(settings_db(&root)).ok(), Some(db_before), "the database keeps every byte");
}

#[test]
fn settings_store_refuses_its_own_store_left_with_a_journal() {
    // A leftover journal makes the store unusable: readers report it, and a writer is refused without recovering it,
    // so the store and its journal keep every byte.
    let (root, context) = settings_fixture("settings own journal");
    let journal = root.0.join("data/settings.db-journal");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    conn.execute_batch("CREATE TABLE scratch (x INTEGER); DROP TABLE scratch;").unwrap();
    drop(conn);
    std::fs::write(&journal, vec![0xff_u8; 512]).unwrap();
    let db_before = std::fs::read(settings_db(&root)).unwrap();
    assert_eq!(next_use_quiet(&context), Some(3000));
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], LEFTOVER_JOURNAL_ERROR);
    assert!(journal.is_file(), "readers leave the journal alone");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], LEFTOVER_JOURNAL_ERROR);
    assert_eq!(std::fs::read(&journal).unwrap(), vec![0xff_u8; 512], "the journal keeps every byte");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), db_before, "the store keeps every byte");
    assert_eq!(next_use_quiet(&context), Some(3000), "a leftover journal applies nothing");
}

#[test]
fn settings_store_never_makes_a_journal_copy() {
    // A journal is never recovered on a copy of the store: a store with one is refused and no copy appears.
    let (root, context) = settings_fixture("settings no journal copy");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let journal = root.0.join("data/settings.db-journal");
    std::fs::write(&journal, vec![0xff_u8; 512]).unwrap();
    let db_before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], LEFTOVER_JOURNAL_ERROR);
    assert_eq!(std::fs::read(&journal).unwrap(), vec![0xff_u8; 512], "the journal keeps every byte");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), db_before, "the store keeps every byte");
    let copies: Vec<String> = std::fs::read_dir(root.0.join("data"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("settings-inspect.db"))
        .collect();
    assert!(copies.is_empty(), "no journal copy was made: {copies:?}");
}

#[test]
fn settings_store_creates_over_a_journal_beside_an_empty_database() {
    // An empty database has no page a journal could restore: such a journal is not a leftover to refuse.
    let (root, context) = settings_fixture("settings empty store journal");
    let journal = root.0.join("data/settings.db-journal");
    std::fs::write(settings_db(&root), b"").unwrap();
    std::fs::write(&journal, vec![0xff_u8; 512]).unwrap();
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.get("storeError").is_none(), "readers see an empty store: {body}");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert!(!journal.exists(), "SQLite discarded the journal of the empty database");
}

#[test]
fn settings_store_refuses_a_hard_linked_sidecar_without_a_database() {
    // SQLite opens, truncates or removes the sidecars even when it creates the database, so each is checked first.
    let (root, context) = settings_fixture("settings linked sidecar no store");
    let memory = root.0.join("data/memory.db");
    other_database(&memory);
    let before = std::fs::read(&memory).unwrap();
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = root.0.join(format!("data/settings.db{suffix}"));
        std::fs::hard_link(&memory, &sidecar).unwrap();
        let (status, body) = set_quiet(&context, json!(4500));
        assert_eq!(status, 503, "{suffix}: {body}");
        assert_eq!(body["error"], "settings storage is unavailable", "{suffix}");
        assert!(!settings_db(&root).exists(), "{suffix}: the store was created");
        assert!(sidecar.is_file(), "{suffix}: the linked sidecar is left in place");
        assert_eq!(std::fs::read(&memory).unwrap(), before, "{suffix}: the other database keeps every byte");
        std::fs::remove_file(&sidecar).unwrap();
    }
}

#[test]
fn settings_store_refuses_a_hard_linked_gate() {
    // The store gate is opened read-write and locked: a second name for another database must never be opened.
    let (root, context) = settings_fixture("settings linked gate");
    let memory = root.0.join("data/memory.db");
    other_database(&memory);
    std::fs::create_dir_all(root.0.join("data/locks")).unwrap();
    std::fs::hard_link(&memory, root.0.join("data/locks/settings-store.lock")).unwrap();
    let before = std::fs::read(&memory).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert!(!settings_db(&root).exists(), "the store was created");
    assert_eq!(std::fs::read(&memory).unwrap(), before, "the other database keeps every byte");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], "settings storage is unavailable");
}

#[test]
fn settings_store_refuses_a_malformed_store_with_a_journal_and_leaves_both() {
    // A leftover journal makes the store unusable, whatever its schema: nothing recovers it or copies it.
    let (root, context) = settings_fixture("settings malformed journal");
    let journal = root.0.join("data/settings.db-journal");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    conn.execute_batch("CREATE TABLE settings (x); CREATE TABLE settings_audit (x); PRAGMA user_version=1;").unwrap();
    drop(conn);
    std::fs::write(&journal, vec![0xff_u8; 512]).unwrap();
    let db_before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], LEFTOVER_JOURNAL_ERROR);
    assert_eq!(std::fs::read(&journal).ok(), Some(vec![0xff_u8; 512]), "the journal is never rolled back or removed");
    assert_eq!(std::fs::read(settings_db(&root)).ok(), Some(db_before), "the database keeps every byte");
    for suffix in ["", "-journal", "-wal", "-shm"] {
        assert!(!root.0.join(format!("data/settings-inspect.db{suffix}")).exists(), "settings-inspect.db{suffix} was left");
    }
}

#[test]
fn settings_store_refuses_a_version_one_store_without_its_constraints_and_leaves_it_unchanged() {
    // The right column names without the primary key `ON CONFLICT(key)` needs are not this release's schema.
    let (root, context) = settings_fixture("settings no constraints");
    let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
    conn.execute_batch(
        "CREATE TABLE settings (key, value, revision, updated_ms, updated_by);
         CREATE TABLE settings_audit (id, at_ms, actor, source, scope, key, old_value, new_value, request_id);
         PRAGMA user_version=1;",
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "a refused store is never touched");
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "settings.db{suffix} was created");
    }
}

#[test]
fn settings_store_refuses_its_schema_with_an_extra_object() {
    let (root, context) = settings_fixture("settings extra object");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    conn.execute_batch("CREATE INDEX settings_by_value ON settings (value);").unwrap();
    drop(conn);
    let before = std::fs::read(settings_db(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "a refused store is never touched");
    assert_eq!(next_use_quiet(&context), Some(3000), "a store that is not exactly this release's applies nothing");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["storeError"], "settings storage is unavailable");
}

#[test]
fn settings_registry_is_complete_and_agrees_with_its_reader() {
    let registry = crate::settings::REGISTRY;
    assert!(!registry.is_empty(), "the registry has settings");
    let mut keys: Vec<&str> = registry.iter().map(|setting| setting.key).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), registry.len(), "registry keys are unique");
    for setting in registry {
        assert!(!setting.label.is_empty() && !setting.hint.is_empty() && !setting.group.is_empty(), "{}", setting.key);
        assert!(matches!(setting.applies, "live" | "next wake" | "restart"), "{}", setting.key);
        assert!(setting.pointer.starts_with('/') && !setting.pointer.contains('~'), "{}", setting.pointer);
        assert!(setting.pointer.split('/').skip(1).all(|name| !name.is_empty()), "{}", setting.pointer);
        match setting.kind {
            crate::settings::Kind::Integer { min, max, default, file } => {
                assert!(min <= default && default <= max, "{}", setting.key);
                assert_eq!(setting.normalize(&json!(min)), Some(json!(min)), "{}", setting.key);
                assert_eq!(setting.normalize(&json!(max)), Some(json!(max)), "{}", setting.key);
                assert_eq!(setting.normalize(&json!(min - 1)), None, "{}", setting.key);
                assert_eq!(setting.normalize(&json!(max + 1)), None, "{}", setting.key);
                assert_eq!(setting.default_value(), json!(default));
                assert_eq!(setting.integer(&json!({})), default, "{}: a missing value reads as the default", setting.key);
                if file == crate::settings::FileRule::Clamp {
                    assert!(min >= 0, "{}: a clamped setting has non-negative bounds", setting.key);
                }
            }
            crate::settings::Kind::ModelId { default } => {
                if let Some(id) = default {
                    assert_eq!(setting.normalize(&json!(id)), Some(json!(id)), "{}: the default is a model id", setting.key);
                }
                assert_eq!(setting.default_value(), default.map_or(Value::Null, |id| json!(id)), "{}", setting.key);
                assert_eq!(setting.text(&json!({})), default.map(str::to_string), "{}", setting.key);
            }
        }
    }
    // The registry owns the default and bounds the reader applies to the file (`wake_cfg_of`).
    let quiet = crate::settings::find(QUIET_KEY).expect("reply.wakeQuietMs is registered");
    let crate::settings::Kind::Integer { min, max, default, .. } = quiet.kind else {
        panic!("reply.wakeQuietMs is an integer setting");
    };
    for ms in [min - 1, min, default, max, max + 1] {
        let config = json!({"socialV2": {"reply": {"wakeQuietMs": ms}}});
        let expected = quiet.normalize(&json!(ms)).and_then(|value| value.as_i64()).unwrap_or(default);
        assert_eq!(crate::wake_cfg_of(&config).reply_wake_quiet_ms, Some(expected), "{ms}");
    }
    assert_eq!(crate::wake_cfg_of(&json!({})).reply_wake_quiet_ms, Some(default));
}

#[test]
fn no_registry_setting_is_cached_on_the_config_stamp() {
    // `RuntimeContext::config_stamp` sees only the stored file. Values cached on it: `reserved.rs` (`social`),
    // `console_access.rs` (`allow`, `deny`, `allowAllWhenEmpty`, `ownerQQ`) and `history_log.rs` (`history`).
    let registry = crate::settings::REGISTRY;
    assert!(!registry.is_empty(), "the registry has settings");
    for setting in registry {
        for cached in ["/social", "/allow", "/deny", "/allowAllWhenEmpty", "/ownerQQ", "/history"] {
            assert!(
                setting.pointer != cached && !setting.pointer.starts_with(&format!("{cached}/")),
                "{} is cached on the config stamp ({cached}); key that cache on the store too",
                setting.key
            );
        }
    }
}

#[test]
fn settings_page_lists_the_registry_and_sets_through_the_route() {
    let (root, context) = settings_fixture("settings page");
    let granted = || -> Result<Option<std::fs::File>, (u16, String)> { Ok(None) };
    let (status, content_type, page) = crate::console_settings::route(&context, "GET", "/settings", "", false, &granted);
    assert_eq!(status, 200, "{page}");
    assert_eq!(content_type, "text/html; charset=utf-8");
    let (status, _) = admin_request(&context, "GET", "/settings", None);
    assert_eq!(status, 200, "the page is served behind the console front");
    let (status, body) = agent_request(&context, "GET", "/settings", None, "fixture-agent-token");
    assert_eq!(status, 403, "{body}");
    // The page renders what the list route returns and sends what the set route reads.
    let view = quiet_view(&context);
    for field in ["key", "label", "hint", "value", "default", "applies"] {
        assert!(page.contains(&format!("s.{field}")), "the page shows {field}");
        assert!(!view[field].is_null(), "the list route returns {field}");
    }
    assert!(page.contains("s.kind.min") && page.contains("s.kind.max"), "the input takes the bounds");
    assert_eq!(view["kind"]["min"], 1);
    assert!(page.contains("s.kind.type==='integer'"), "the page picks the input by the kind");
    assert!(!page.contains("maxLength"), "a model id input sets no UTF-16 length: the set route counts characters");
    assert!(page.contains("input.value=s.value==null?'':s.value"), "a model id that is not configured shows empty");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    let model = body["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == "harness.model").unwrap();
    assert_eq!(model["kind"], json!({"type": "model id", "maxChars": 200}));
    assert!(page.contains("api('/api/settings')"), "the page lists through the route");
    assert!(
        page.contains("api('/api/settings/set',{setting:s.key,value:integer?Number(input.value):input.value})"),
        "the page sets through the route: a number for an integer setting, the text for a model id"
    );
    assert!(page.contains("'x-console-token'"), "the page sends the console credential");
    assert!(!page.contains("innerHTML"), "text is never parsed as HTML");
    assert!(!settings_db(&root).exists(), "showing the page creates no store");
    let request = json!({"setting": QUIET_KEY, "value": 4500});
    let (status, body) = admin_request(&context, "POST", "/api/settings/set", Some(&request));
    assert_eq!(status, 200, "{body}");
    assert_eq!(quiet_view(&context)["value"], 4500, "the page's next list shows the new value");
}

// Model and cost settings (#150): each is read through the registry by its real reader.

const MODEL_KEYS: [&str; 6] = [
    "harness.model",
    "harness.maxSteps",
    "harness.maxTokens",
    "memory.digest.model",
    "memory.digest.timeoutSecs",
    "webSearch.model",
];

fn set_setting(context: &RuntimeContext, key: &str, value: Value) -> (u16, Value) {
    admin_request(context, "POST", "/api/settings/set", Some(&json!({"setting": key, "value": value})))
}

/// The `key` entry of `GET /api/settings`.
fn setting_view(context: &RuntimeContext, key: &str) -> Value {
    let (status, body) = admin_request(context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    body["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] == key)
        .unwrap_or_else(|| panic!("{key} is listed"))
        .clone()
}

/// A fixture runtime whose relay route can be built: the fixture has the `model` dependency, this adds the credential.
fn model_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    std::fs::write(root.0.join("config/harness-api-key"), "fixture-harness-credential-150").unwrap();
    (root, context)
}

/// The model id the next harness turn uses (`ModelRoute::from_runtime` on the loader's configuration), or its error.
fn next_turn_model(context: &RuntimeContext) -> Result<String, String> {
    crate::harness::ModelRoute::from_runtime(context, &context.load_config().unwrap())
        .map(|route| route.model_name().to_string())
}

/// The step and token budgets the next harness turn captures.
fn next_turn_budgets(context: &RuntimeContext) -> (usize, u32) {
    crate::harness::Limits::from_config(&context.load_config().unwrap()).budgets()
}

/// The model id and the request timeout, in seconds, of the next digest run.
fn next_digest(context: &RuntimeContext) -> (Option<String>, u64) {
    let cfg = context.load_config().unwrap();
    (crate::digest::digest_model_id(&cfg), crate::digest::digest_timeout(&cfg).as_secs())
}

/// The model the next web search asks (`aux_mcp` reads the loader's document).
fn next_search_model(context: &RuntimeContext) -> String {
    crate::aux_search::load_search_config(&context.load_config_document().unwrap()).model
}

#[test]
fn model_and_cost_settings_are_live_on_the_next_use() {
    let (root, context) = model_fixture("settings model live");
    // Nothing in the file or the store: the registry's defaults.
    assert_eq!(next_turn_model(&context), Err("harness model is not configured".to_string()));
    assert_eq!(next_turn_budgets(&context), (8, 32_768));
    assert_eq!(next_digest(&context), (None, 120));
    assert_eq!(next_search_model(&context), "deepseek-v4-flash");
    // The file's values while the store holds none.
    write_config(&root, |config| {
        config["harness"] = json!({"model": "file-model", "maxSteps": 3, "maxTokens": 5000});
        config["memory"] = json!({"digest": {"timeoutSecs": 30}});
        config["webSearch"] = json!({"model": "file-search"});
    });
    let config_path = root.0.join("config/config.json");
    let stored_file = std::fs::read(&config_path).unwrap();
    assert_eq!(next_turn_model(&context), Ok("file-model".to_string()));
    assert_eq!(next_turn_budgets(&context), (3, 5000));
    assert_eq!(next_digest(&context), (Some("file-model".to_string()), 30), "the digest falls back to the conversation model");
    assert_eq!(next_search_model(&context), "file-search");

    let changes = [
        ("harness.model", json!("store-model")),
        ("harness.maxSteps", json!(12)),
        ("harness.maxTokens", json!(64_000)),
        ("memory.digest.model", json!("store-digest")),
        ("memory.digest.timeoutSecs", json!(45)),
        ("webSearch.model", json!("store-search")),
    ];
    for (key, value) in &changes {
        let (status, body) = set_setting(&context, key, value.clone());
        assert_eq!(status, 200, "{key}: {body}");
        assert_eq!(body, json!({"ok": true, "setting": key, "value": value, "revision": 1, "changed": true}), "{key}");
    }
    assert_eq!(next_turn_model(&context), Ok("store-model".to_string()));
    assert_eq!(next_turn_budgets(&context), (12, 64_000));
    assert_eq!(next_digest(&context), (Some("store-digest".to_string()), 45));
    assert_eq!(next_search_model(&context), "store-search");
    assert_eq!(std::fs::read(&config_path).unwrap(), stored_file, "the store never writes config.json");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), changes.len(), "{rows:?}");
    for (row, (key, value)) in rows.iter().zip(&changes) {
        assert_eq!(
            (row["key"].as_str(), row["source"].as_str(), row["scope"].as_str()),
            (Some(*key), Some("console"), Some("global"))
        );
        assert_eq!(row["new"], json!(value.to_string()), "{key}");
    }
    for key in MODEL_KEYS {
        let view = setting_view(&context, key);
        assert_eq!((view["group"].as_str(), view["applies"].as_str()), (Some("model and cost"), Some("live")), "{key}");
        assert_eq!(view["stored"], view["value"], "{key}: the next use reads the stored value");
    }
    assert_eq!(setting_view(&context, "harness.model")["kind"], json!({"type": "model id", "maxChars": 200}));
    assert_eq!(setting_view(&context, "harness.model")["default"], Value::Null);
    assert_eq!(setting_view(&context, "webSearch.model")["default"], "deepseek-v4-flash");
    assert_eq!(setting_view(&context, "harness.maxSteps")["kind"], json!({"type": "integer", "min": 1, "max": 64}));
    assert_eq!(setting_view(&context, "harness.maxTokens")["default"], 32_768);
    let request = json!({"setting": "harness.maxTokens", "value": 1_000_000});
    let (status, body) = agent_request(&context, "POST", "/api/settings/set", Some(&request), "fixture-agent-token");
    assert_eq!(status, 403, "{body}");
    assert_eq!(next_turn_budgets(&context), (12, 64_000), "an agent never changes its own budget");
}

#[test]
fn model_and_cost_settings_refuse_invalid_values_and_keep_the_old_value() {
    let (root, context) = model_fixture("settings model refused");
    let accepted = [
        ("harness.model", json!("x".repeat(200)), json!("x".repeat(200))),
        ("harness.model", json!("  padded-model  "), json!("padded-model")),
        ("harness.model", json!("模型".repeat(100)), json!("模型".repeat(100))),
        ("harness.maxSteps", json!(1), json!(1)),
        ("harness.maxSteps", json!(64), json!(64)),
        ("harness.maxTokens", json!(1), json!(1)),
        ("harness.maxTokens", json!(1_000_000), json!(1_000_000)),
        ("memory.digest.timeoutSecs", json!(1), json!(1)),
        ("memory.digest.timeoutSecs", json!(600), json!(600)),
    ];
    for (key, value, stored) in &accepted {
        let (status, body) = set_setting(&context, key, value.clone());
        assert_eq!(status, 200, "{key} {value}: {body}");
        assert_eq!(&body["value"], stored, "{key} {value}: stored trimmed");
    }
    let model = "harness.model must be a model id of 1 to 200 characters without control characters";
    let tokens = format!("harness.maxTokens must be an integer from 1 to {}", 1_000_000);
    let refused = [
        ("harness.model", json!(""), model),
        ("harness.model", json!("   "), model),
        ("harness.model", json!("x".repeat(201)), model),
        ("harness.model", json!("模".repeat(201)), model),
        ("harness.model", json!("line\nbreak"), model),
        ("harness.model", json!("bell\u{7}"), model),
        ("harness.model", json!(7), model),
        ("harness.model", Value::Null, model),
        ("harness.model", json!(true), model),
        ("harness.model", json!(["m"]), model),
        ("harness.model", json!({"id": "m"}), model),
        ("webSearch.model", json!(""), "webSearch.model must be a model id of 1 to 200 characters without control characters"),
        (
            "memory.digest.model",
            json!("tab\tinside"),
            "memory.digest.model must be a model id of 1 to 200 characters without control characters",
        ),
        ("harness.maxSteps", json!(0), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxSteps", json!(65), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxSteps", json!(8.5), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxSteps", json!("8"), "harness.maxSteps must be an integer from 1 to 64"),
        ("harness.maxTokens", json!(0), tokens.as_str()),
        ("harness.maxTokens", json!(1_000_001), tokens.as_str()),
        ("memory.digest.timeoutSecs", json!(0), "memory.digest.timeoutSecs must be an integer from 1 to 600"),
        ("memory.digest.timeoutSecs", json!(601), "memory.digest.timeoutSecs must be an integer from 1 to 600"),
    ];
    for (key, value, error) in &refused {
        let (status, body) = set_setting(&context, key, value.clone());
        assert_eq!(status, 400, "{key} {value}: {body}");
        assert_eq!(body["error"], *error, "{key} {value}");
    }
    assert_eq!(next_turn_model(&context), Ok("模型".repeat(100)), "every refusal keeps the old value");
    assert_eq!(next_turn_budgets(&context), (64, 1_000_000));
    assert_eq!(next_digest(&context), (Some("模型".repeat(100)), 600));
    assert_eq!(next_search_model(&context), "deepseek-v4-flash");
    assert_eq!(audit_rows(&root).len(), accepted.len(), "a refusal writes no audit row");
}

#[test]
fn model_and_cost_readers_take_defaults_and_bounds_from_the_registry() {
    let registered = |key: &str| crate::settings::find(key).unwrap_or_else(|| panic!("{key} is registered"));
    let steps = registered("harness.maxSteps");
    let tokens = registered("harness.maxTokens");
    let timeout = registered("memory.digest.timeoutSecs");
    let harness_model = registered("harness.model");
    registered("memory.digest.model");
    registered("webSearch.model");
    // A `config.json` integer outside the bounds is clamped and any other value reads as the default: each reader's
    // rule before the registry owned it.
    for (value, expected) in [
        (json!(0), 1),
        (json!(1), 1),
        (json!(8), 8),
        (json!(64), 64),
        (json!(65), 64),
        (json!(u64::MAX), 64),
        (json!(-1), 8),
        (json!(5.5), 8),
        (json!("8"), 8),
        (Value::Null, 8),
    ] {
        let cfg = json!({"harness": {"maxSteps": value}});
        assert_eq!(crate::harness::Limits::from_config(&cfg).budgets().0, expected, "maxSteps {value}");
        assert_eq!(steps.integer(&cfg), expected as i64, "maxSteps {value}");
    }
    for (value, expected) in [
        (json!(0), 1),
        (json!(1), 1),
        (json!(32_768), 32_768),
        (json!(1_000_000), 1_000_000),
        (json!(1_000_001), 1_000_000),
        (json!(-5), 32_768),
        (json!("x"), 32_768),
    ] {
        let cfg = json!({"harness": {"maxTokens": value}});
        assert_eq!(crate::harness::Limits::from_config(&cfg).budgets().1, expected, "maxTokens {value}");
        assert_eq!(tokens.integer(&cfg), i64::from(expected), "maxTokens {value}");
    }
    for (value, expected) in
        [(json!(0), 1), (json!(1), 1), (json!(600), 600), (json!(601), 600), (json!(-1), 120), (json!(2.5), 120)]
    {
        let cfg = json!({"memory": {"digest": {"timeoutSecs": value}}});
        assert_eq!(crate::digest::digest_timeout(&cfg).as_secs(), expected, "timeoutSecs {value}");
        assert_eq!(timeout.integer(&cfg), expected as i64, "timeoutSecs {value}");
    }
    assert_eq!(crate::harness::Limits::from_config(&json!({})).budgets(), (8, 32_768));
    assert_eq!(crate::harness::Limits::from_config(&json!({"harness": "x"})).budgets(), (8, 32_768));
    assert_eq!(crate::digest::digest_timeout(&json!({})).as_secs(), 120);
    // A model id is trimmed; empty, not text, too long or with a control character reads as not configured.
    for (value, expected) in [
        (json!(" m1 "), Some("m1")),
        (json!(""), None),
        (json!("  "), None),
        (json!(5), None),
        (json!("a\nb"), None),
        (json!("x".repeat(201)), None),
    ] {
        let cfg = json!({"harness": {"model": value}});
        assert_eq!(harness_model.text(&cfg).as_deref(), expected, "harness.model {value}");
        assert_eq!(crate::digest::digest_model_id(&cfg).as_deref(), expected, "digest fallback {value}");
    }
    let digest = |model: Value| {
        crate::digest::digest_model_id(&json!({"memory": {"digest": {"model": model}}, "harness": {"model": "h"}}))
    };
    assert_eq!(digest(json!(" d ")).as_deref(), Some("d"));
    assert_eq!(digest(json!("")).as_deref(), Some("h"), "an empty digest model uses the conversation model");
    assert_eq!(digest(json!("a\u{1}b")).as_deref(), Some("h"));
    assert_eq!(crate::digest::digest_model_id(&json!({})), None);
    let search = |config: Value| crate::aux_search::load_search_config(&config).model;
    assert_eq!(search(json!({})), "deepseek-v4-flash");
    assert_eq!(search(json!({"webSearch": {"model": " m "}})), "m");
    assert_eq!(search(json!({"webSearch": []})), "deepseek-v4-flash");
    assert_eq!(search(json!({"webSearch": {"model": "a\u{1}b"}})), "deepseek-v4-flash");
    assert_eq!(search(json!({"webSearch": {"model": "x".repeat(201)}})), "deepseek-v4-flash");
    // The route a turn builds carries the trimmed id; a blank id is not configured.
    let (_root, context) = model_fixture("settings model reader");
    let turn_model = |model: &str| {
        crate::harness::ModelRoute::from_runtime(&context, &json!({"harness": {"model": model}}))
            .map(|built| built.model_name().to_string())
    };
    assert_eq!(turn_model(" padded "), Ok("padded".to_string()));
    assert_eq!(turn_model("  "), Err("harness model is not configured".to_string()));
}

#[test]
fn model_and_cost_settings_have_no_reader_default() {
    // Each reader reads its setting through the registry (`settings.rs`), never by its own pointer or default.
    let readers = [
        ("harness.rs", include_str!("harness.rs")),
        ("digest/model.rs", include_str!("digest/model.rs")),
        ("aux_search.rs", include_str!("aux_search.rs")),
    ];
    let owned = [
        "/harness/model",
        "/harness/maxSteps",
        "/harness/maxTokens",
        "/memory/digest/model",
        "/memory/digest/timeoutSecs",
        "/webSearch/model",
        "deepseek-v4-flash",
    ];
    for (file, source) in readers {
        for needle in owned {
            assert!(!source.contains(needle), "{file} reads {needle} itself; read it through settings::REGISTRY");
        }
    }
    let registry = include_str!("settings.rs");
    for needle in owned {
        assert!(registry.contains(needle), "settings.rs owns {needle}");
    }
}

#[test]
fn stored_model_and_cost_rows_the_registry_refuses_are_not_applied() {
    // Rows written by another release or by hand are checked again when overlaid, and never clamped.
    let (root, context) = model_fixture("settings model old rows");
    for (key, value) in [("harness.model", json!("store-model")), ("harness.maxSteps", json!(12)), ("memory.digest.timeoutSecs", json!(45))] {
        let (status, body) = set_setting(&context, key, value);
        assert_eq!(status, 200, "{key}: {body}");
    }
    assert_eq!(next_turn_model(&context), Ok("store-model".to_string()));
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    conn.execute("UPDATE settings SET value=?1 WHERE key='harness.model'", [json!("bad\nid").to_string()]).unwrap();
    conn.execute_batch(
        "UPDATE settings SET value='65' WHERE key='harness.maxSteps';
         UPDATE settings SET value='not json' WHERE key='memory.digest.timeoutSecs';
         INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES('harness.maxTokens', '\"4096\"', 1, 0, 'test');",
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        next_turn_model(&context),
        Err("harness model is not configured".to_string()),
        "a stored model id with a control character is not applied"
    );
    assert_eq!(next_turn_budgets(&context), (8, 32_768), "an out-of-range or mistyped stored value is not applied, not clamped");
    assert_eq!(next_digest(&context), (None, 120));
    for key in ["harness.model", "harness.maxSteps", "harness.maxTokens", "memory.digest.timeoutSecs"] {
        assert_eq!(setting_view(&context, key)["stored"], Value::Null, "{key}");
    }
    assert_eq!(setting_view(&context, "harness.model")["value"], Value::Null, "not configured");
    let (status, body) = set_setting(&context, "harness.model", json!("fixed-model"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2);
    assert_eq!(next_turn_model(&context), Ok("fixed-model".to_string()));
    assert_eq!(audit_rows(&root).last().unwrap()["old"], json!("bad\nid").to_string(), "the audit keeps what was stored");
}

#[test]
fn model_id_length_counts_characters_not_utf16_units() {
    // The limit is 200 characters (`settings.rs` `model_id`), not UTF-16 units: a character outside the BMP is two units.
    let (_root, context) = model_fixture("settings model length");
    let astral = |count: usize| "\u{1F600}".repeat(count);
    let (status, body) = set_setting(&context, "harness.model", json!(astral(101)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], json!(astral(101)), "101 characters (202 UTF-16 units) are accepted");
    assert_eq!(next_turn_model(&context), Ok(astral(101)));
    let (status, body) = set_setting(&context, "harness.model", json!(astral(201)));
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["error"],
        "harness.model must be a model id of 1 to 200 characters without control characters"
    );
    assert_eq!(next_turn_model(&context), Ok(astral(101)), "a refused id keeps the old value");
}

#[test]
fn model_and_cost_settings_written_at_once_all_commit() {
    let (root, context) = model_fixture("settings model concurrent");
    let apply = |key: &str, value: Value| crate::settings::apply(&context, key, &value, "administrator", "console");
    let applied = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            for _ in 0..20 {
                let (steps, tokens) = next_turn_budgets(&context);
                assert!(steps == 8 && matches!(tokens, 32_768 | 4096), "{steps} {tokens}");
            }
        });
        let writers = [
            scope.spawn(|| apply("harness.model", json!("m-a"))),
            scope.spawn(|| apply("harness.maxTokens", json!(4096))),
            scope.spawn(|| apply("webSearch.model", json!("s-a"))),
        ];
        reader.join().unwrap();
        writers.map(|writer| writer.join().unwrap().unwrap())
    });
    for (index, applied) in applied.iter().enumerate() {
        assert!(applied.changed && applied.revision == 1, "writer {index}");
    }
    assert_eq!(next_turn_model(&context), Ok("m-a".to_string()));
    assert_eq!(next_turn_budgets(&context), (8, 4096));
    assert_eq!(next_search_model(&context), "s-a");
    let mut keys: Vec<String> = audit_rows(&root).iter().map(|row| row["key"].as_str().unwrap().to_string()).collect();
    keys.sort();
    assert_eq!(keys, ["harness.maxTokens", "harness.model", "webSearch.model"]);
}

#[test]
fn model_setting_cut_off_before_its_audit_row_leaves_the_old_model() {
    let (root, context) = model_fixture("settings model rollback");
    let (status, body) = set_setting(&context, "harness.model", json!("first-model"));
    assert_eq!(status, 200, "{body}");
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let (status, body) = set_setting(&context, "harness.model", json!("second-model"));
    assert_eq!(status, 503, "{body}");
    assert_eq!(next_turn_model(&context), Ok("first-model".to_string()), "the next turn uses the last committed model");
    assert_eq!(audit_rows(&root).len(), 1, "and no audit row");
    let (status, body) = set_setting(&context, "harness.model", json!("second-model"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2);
}

#[test]
fn a_store_from_before_the_model_settings_is_read_and_extended() {
    // A store an earlier release wrote holds older keys only; a full restore brings such a store back byte for byte.
    let (root, context) = model_fixture("settings model older store");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "settings.db{suffix} was left");
    }
    let older = std::fs::read(settings_db(&root)).unwrap();
    // This release reads it: the older key applies, the new keys read the file or their defaults.
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(next_turn_budgets(&context), (8, 32_768));
    assert_eq!(next_turn_model(&context), Err("harness model is not configured".to_string()));
    // And extends it: the same schema version, the older row untouched.
    let (status, body) = set_setting(&context, "harness.maxSteps", json!(12));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(next_turn_budgets(&context), (12, 32_768));
    assert_eq!(next_use_quiet(&context), Some(4500));
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, 1, "these settings need no new schema");
    drop(conn);
    // Restoring the older store over it brings back exactly what it held.
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "settings.db{suffix} was left");
    }
    std::fs::write(settings_db(&root), &older).unwrap();
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(next_turn_budgets(&context), (8, 32_768), "the restored store has no model settings");
    let (status, body) = set_setting(&context, "harness.model", json!("after-restore"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(next_turn_model(&context), Ok("after-restore".to_string()));
    assert_eq!(audit_rows(&root).len(), 2, "the restored audit rows and the new one");
}

/// The chat's name for `reply.wakeQuietMs`: its configuration path.
const CHAT_NAME: &str = "socialV2.reply.wakeQuietMs";
/// The fixture's owner-private conversation (`ownerQQ` 7001) and its agent token.
const OWNER_CHAT: &str = "private:7001";
const OWNER_CHAT_TOKEN: &str = "fixture-owner-chat-token-149";
const GROUP_CHAT_TOKEN: &str = "fixture-group-chat-token-149";
const CHAT_WRITE_REFUSED: &str = "该项不可通过对话修改（只有 settings_get 列出的设置可以改；白名单/令牌/路径类必须本地改）";
const CHAT_STORE_UNAVAILABLE: &str = "设置存储暂时不可用，这次没有改动";
const CHAT_OWNER_ONLY: &str = "配置读写只允许管理员私聊";

/// A settings fixture whose owner-private chat and one group chat hold agent tokens.
fn chat_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    let state = json!({"conversations": {
        OWNER_CHAT: {"agentToken": OWNER_CHAT_TOKEN},
        "group:101": {"agentToken": GROUP_CHAT_TOKEN},
    }});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    (root, context)
}

/// The owner-private chat's arguments with `fields` added.
fn owner_chat_args(fields: Value) -> Value {
    let mut args = json!({"key": OWNER_CHAT, "token": OWNER_CHAT_TOKEN});
    args.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    args
}

/// One settings action from the owner's private chat, as the chat tools run it (`memory_actions::tool_action`).
fn chat(context: &RuntimeContext, action: &str, fields: Value) -> Result<Value, String> {
    memory_tool_action(context, action, &owner_chat_args(fields))
}

/// What the model reads back from one chat settings tool call, as the MCP server renders it.
fn chat_text(context: &RuntimeContext, tool: &str, fields: Value) -> String {
    let outcome = crate::mcp::call_prepared(tool, &owner_chat_args(fields), context);
    outcome["content"][0]["text"].as_str().unwrap().to_string()
}

#[test]
fn chat_settings_set_validates_and_audits_through_the_registry() {
    let (root, context) = chat_fixture("chat settings set");
    let config_path = root.0.join("config/config.json");
    let stored_file = std::fs::read(&config_path).unwrap();
    let text = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 4500}));
    assert_eq!(text, format!("已改 {CHAT_NAME} = 4500。下次唤醒时生效"));
    assert_eq!(next_use_quiet(&context), Some(4500), "the next use reads the chat's value");
    assert_eq!(quiet_view(&context)["stored"], 4500, "the console shows the chat's value");
    assert_eq!(std::fs::read(&config_path).unwrap(), stored_file, "a registry setting is written to the store, not config.json");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["actor"].clone(), rows[0]["source"].clone()), (json!("owner"), json!("chat")));
    assert_eq!((rows[0]["scope"].clone(), rows[0]["key"].clone()), (json!("global"), json!(QUIET_KEY)));
    assert_eq!((rows[0]["old"].clone(), rows[0]["new"].clone()), (Value::Null, json!("4500")));

    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4500})).unwrap();
    assert_eq!((reply["value"].clone(), reply["changed"].clone(), reply["hot"].clone()), (json!(4500), json!(false), json!(true)));
    assert_eq!(audit_rows(&root).len(), 1, "the same value again is not a change");
    let padded = format!(" {CHAT_NAME} ");
    let reply = chat(&context, "settings-set", json!({"settingKey": padded, "value": 1})).unwrap();
    assert_eq!((reply["key"].clone(), reply["value"].clone(), reply["changed"].clone()), (json!(CHAT_NAME), json!(1), json!(true)));
    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 30000})).unwrap();
    assert_eq!(reply["value"], 30000);

    // An invalid value is refused under the chat's name with the registry's bounds, and changes nothing.
    let requirement = format!("{CHAT_NAME} 必须是 1 到 30000 的整数");
    for value in [
        json!(0),
        json!(30001),
        json!(-1),
        json!(4500.5),
        json!(3000.0),
        json!("4500"),
        json!(true),
        Value::Null,
        json!([4500]),
        json!({"ms": 4500}),
        json!(1e30),
        json!(u64::MAX),
        json!("x".repeat(100_000)),
    ] {
        let refused = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": value.clone()}));
        assert_eq!(refused, Err(requirement.clone()), "{}", &value.to_string()[..value.to_string().len().min(40)]);
    }
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME})), Err(requirement.clone()), "no value");
    let refusal = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 0}));
    assert_eq!(refusal, format!("改配置失败：{requirement}"));
    for model_text in [&text, &refusal] {
        assert!(crate::upstream_capture::denied_terms(model_text).is_empty(), "{model_text}");
    }
    // Only the configuration path names it: not the registry key, the pointer, another case, a parent or a child.
    for name in [QUIET_KEY, "/socialV2/reply/wakeQuietMs", "SOCIALV2.REPLY.WAKEQUIETMS", "socialV2.reply", "socialV2", "socialV2.reply.wakeQuietMs.x"] {
        let refused = chat(&context, "settings-set", json!({"settingKey": name, "value": 4500}));
        assert_eq!(refused, Err(CHAT_WRITE_REFUSED.to_string()), "{name}");
    }
    // Only the owner's private chat writes: not a group chat's token, nor that token presented for the owner's chat.
    for (key, token) in [("group:101", GROUP_CHAT_TOKEN), (OWNER_CHAT, GROUP_CHAT_TOKEN), (OWNER_CHAT, "")] {
        let args = json!({"key": key, "token": token, "settingKey": CHAT_NAME, "value": 4500});
        assert_eq!(memory_tool_action(&context, "settings-set", &args), Err(CHAT_OWNER_ONLY.to_string()), "{key}");
    }
    assert_eq!(audit_rows(&root).len(), 3, "a refusal writes no audit row");
    assert_eq!(next_use_quiet(&context), Some(30000), "every refusal keeps the old value");
    assert_eq!(std::fs::read(&config_path).unwrap(), stored_file);
}

#[test]
fn chat_settings_get_reads_registry_settings_as_the_next_use_and_hides_everything_else() {
    let (root, context) = chat_fixture("chat settings get");
    // No stored and no file value: the registry default, in the list and alone.
    let all = chat(&context, "settings-get", json!({})).unwrap();
    assert_eq!(all["settings"][CHAT_NAME], 3000, "{all}");
    assert!(all["writable"].as_array().unwrap().contains(&json!(CHAT_NAME)), "{all}");
    let one = chat(&context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap();
    assert_eq!(one, json!({"ok": true, "key": CHAT_NAME, "writable": true, "value": 3000}));
    assert_eq!(chat_text(&context, "settings_get", json!({"settingKey": CHAT_NAME})), format!("{CHAT_NAME} = 3000"));
    let listed = chat_text(&context, "settings_get", json!({}));
    assert!(listed.lines().any(|line| line == format!("{CHAT_NAME} = 3000")), "{listed}");

    // The file's value, a stored value over it, then a stored value the registry refuses: always what the next use reads.
    let read = |context: &RuntimeContext| chat(context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap()["value"].clone();
    let listed = |context: &RuntimeContext| chat(context, "settings-get", json!({})).unwrap()["settings"][CHAT_NAME].clone();
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000, "apiKey": "fixture-reply-value-149"}}));
    assert_eq!((read(&context), listed(&context)), (json!(9000), json!(9000)));
    assert_eq!(next_use_quiet(&context), Some(9000));
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!((read(&context), listed(&context)), (json!(4500), json!(4500)), "a stored value wins over the file's");
    assert_eq!(next_use_quiet(&context), Some(4500));
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    conn.execute_batch("UPDATE settings SET value='50000' WHERE key='reply.wakeQuietMs';").unwrap();
    drop(conn);
    assert_eq!((read(&context), listed(&context)), (json!(9000), json!(9000)), "a stored value the registry refuses is not applied");
    assert_eq!(next_use_quiet(&context), Some(9000));
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 90000, "apiKey": "fixture-reply-value-149"}}));
    assert_eq!((read(&context), listed(&context)), (json!(3000), json!(3000)), "an out-of-bounds file value reads as the default");
    assert_eq!(next_use_quiet(&context), Some(3000));

    // Nothing else reads: not the registry key, the pointer, a parent, a sibling, a secret beside it or a long name.
    let long = "x".repeat(10_000);
    for name in [
        QUIET_KEY,
        "/socialV2/reply/wakeQuietMs",
        "SOCIALV2.REPLY.WAKEQUIETMS",
        "socialV2.reply",
        "socialV2",
        "socialV2.reply.apiKey",
        "socialV2.reply.quietMinMs",
        long.as_str(),
    ] {
        let refused = chat(&context, "settings-get", json!({"settingKey": name}));
        assert_eq!(refused, Err(SETTINGS_READ_REFUSED.to_string()), "{}", &name[..name.len().min(40)]);
    }
    let listed = chat_text(&context, "settings_get", json!({}));
    assert!(!listed.contains("fixture-") && !listed.contains("apiKey"), "{listed}");
    let all = chat(&context, "settings-get", json!({})).unwrap();
    assert!(!all.to_string().contains("fixture-"), "{all}");
    for name in all["writable"].as_array().unwrap() {
        assert!(!super::super::setting_is_secret(name.as_str().unwrap()), "{name}");
    }

    // A legacy (no-root) context has no settings store: no registry setting is offered, read or written there.
    let legacy = RuntimeContext::legacy();
    let cfg = json!({"socialV2": {"reply": {"wakeQuietMs": 9000}}});
    let all = super::super::settings_action("settings-get", &json!({}), &cfg, &legacy).unwrap();
    assert!(all["settings"].get(CHAT_NAME).is_none(), "{all}");
    assert!(!all["writable"].as_array().unwrap().contains(&json!(CHAT_NAME)), "{all}");
    let one = super::super::settings_action("settings-get", &json!({"settingKey": CHAT_NAME}), &cfg, &legacy);
    assert_eq!(one, Err((403, SETTINGS_READ_REFUSED.to_string())));
    let set = super::super::settings_action("settings-set", &json!({"settingKey": CHAT_NAME, "value": 4500}), &cfg, &legacy);
    assert_eq!(set, Err((403, CHAT_WRITE_REFUSED.to_string())));
}

#[test]
fn chat_settings_answer_store_trouble_without_its_details() {
    // A store this release cannot use: not a database, a newer schema, a leftover journal.
    for trouble in ["garbage", "newer", "journal"] {
        let (root, context) = chat_fixture(&format!("chat settings trouble {trouble}"));
        match trouble {
            "garbage" => std::fs::write(settings_db(&root), vec![b'x'; 4096]).unwrap(),
            "newer" => {
                let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
                conn.execute_batch("CREATE TABLE later (x INTEGER); PRAGMA user_version=2;").unwrap();
            }
            _ => {
                let conn = rusqlite::Connection::open(settings_db(&root)).unwrap();
                conn.execute_batch("CREATE TABLE scratch (x INTEGER); DROP TABLE scratch;").unwrap();
                drop(conn);
                std::fs::write(root.0.join("data/settings.db-journal"), vec![0xff_u8; 512]).unwrap();
            }
        }
        let before = std::fs::read(settings_db(&root)).unwrap();
        let text = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 4500}));
        assert_eq!(text, format!("改配置失败：{CHAT_STORE_UNAVAILABLE}"), "{trouble}");
        assert!(crate::upstream_capture::denied_terms(&text).is_empty(), "{trouble}: {text}");
        assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "{trouble}: the store keeps every byte");
        let read = chat(&context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap();
        assert_eq!(read["value"], 3000, "{trouble}: a read shows what the next use reads");
        assert_eq!(next_use_quiet(&context), Some(3000), "{trouble}");
    }
    // A change cut off before its audit row (a crash midway) leaves no value and no row; the next change continues.
    let (root, context) = chat_fixture("chat settings cut off");
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4500})).unwrap()["value"], 4500);
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let cut = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 5000}));
    assert_eq!(cut, Err(CHAT_STORE_UNAVAILABLE.to_string()));
    assert_eq!(next_use_quiet(&context), Some(4500), "the cut-off change left no value");
    assert_eq!(audit_rows(&root).len(), 1, "and no audit row");
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 5000})).unwrap()["value"], 5000);
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[1]["old"].clone(), rows[1]["new"].clone()), (json!("4500"), json!("5000")));
}

#[test]
fn chat_settings_and_the_console_write_one_setting_at_once() {
    let (root, context) = chat_fixture("chat settings concurrent");
    let (from_chat, from_console) = std::thread::scope(|scope| {
        let from_chat = scope.spawn(|| chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4000})));
        let from_console =
            scope.spawn(|| crate::settings::apply(&context, QUIET_KEY, &json!(5000), "administrator", "console"));
        (from_chat.join().unwrap(), from_console.join().unwrap())
    });
    assert_eq!(from_chat.unwrap()["changed"], true);
    assert!(from_console.unwrap().changed);
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let mut sources: Vec<String> = rows.iter().map(|row| row["source"].as_str().unwrap().to_string()).collect();
    sources.sort();
    assert_eq!(sources, ["chat", "console"]);
    assert_eq!(rows[1]["old"], rows[0]["new"], "the later writer saw the earlier commit");
    let last: i64 = rows[1]["new"].as_str().unwrap().parse().unwrap();
    assert_eq!(next_use_quiet(&context), Some(last));
    assert_eq!(chat(&context, "settings-get", json!({"settingKey": CHAT_NAME})).unwrap()["value"], last);
}

#[test]
fn chat_settings_legacy_set_replies_without_the_value() {
    let (_root, context) = chat_fixture("chat settings legacy reply");
    // A config.json setting the owner writes: the model's reply names the key, never the value the owner sent.
    let text = chat_text(&context, "settings_set", json!({"settingKey": "sendDelayMs", "value": "OpenAI"}));
    assert_eq!(text, "已改 sendDelayMs。已立即生效", "{text}");
    assert!(!text.contains("OpenAI"), "{text}");
    assert!(crate::upstream_capture::denied_terms(&text).is_empty(), "{text}");
}

#[test]
fn chat_settings_come_from_the_registry_and_are_never_secret() {
    let (_root, context) = chat_fixture("chat settings registry");
    let all = chat(&context, "settings-get", json!({})).unwrap();
    let offered: Vec<String> =
        all["writable"].as_array().unwrap().iter().map(|name| name.as_str().unwrap().to_string()).collect();
    let registry = crate::settings::REGISTRY;
    assert!(registry.iter().any(|setting| setting.chat), "the registry has a chat setting");
    for setting in registry.iter().filter(|setting| setting.chat) {
        let name = setting.chat_name();
        assert!(offered.contains(&name), "{name} is marked chat but settings_get does not offer it: {offered:?}");
        assert_eq!(crate::settings::chat_setting(&name).map(|found| found.key), Some(setting.key), "{name}");
        assert!(
            setting.pointer.split('/').skip(1).all(|part| !part.contains('.')),
            "{}: a dot in a member name makes the chat name ambiguous",
            setting.pointer
        );
        assert!(!super::super::setting_is_secret(&name) && !super::super::setting_is_secret(setting.key), "{name} is a secret");
        let lower = name.to_lowercase();
        for refused in ["allow", "token", "secret", "password", "path", "ownerqq", "consoletoken"] {
            assert!(!lower.contains(refused), "{name} names {refused}");
        }
        assert!(!name.starts_with("dsh.") && !name.starts_with("snowluma."), "{name}");
    }
    for setting in registry.iter().filter(|setting| !setting.chat) {
        let name = setting.chat_name();
        assert!(!offered.contains(&name) && crate::settings::chat_setting(&name).is_none(), "{} is not for the chat", setting.key);
    }
    // A path in both lists would be written to config.json under the store's value, and lost.
    for path in SETTINGS_WRITABLE {
        let pointer = format!("/{}", path.replace('.', "/"));
        assert!(
            registry.iter().all(|setting| setting.pointer != pointer),
            "{path} is registered: mark its registry setting chat and delete it from SETTINGS_WRITABLE"
        );
    }
}
