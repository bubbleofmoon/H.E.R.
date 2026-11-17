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
            crate::settings::Kind::Integer { min, max, default } => {
                assert!(min <= default && default <= max, "{}", setting.key);
                assert_eq!(setting.normalize(&json!(min)), Some(json!(min)), "{}", setting.key);
                assert_eq!(setting.normalize(&json!(max)), Some(json!(max)), "{}", setting.key);
                assert_eq!(setting.normalize(&json!(min - 1)), None, "{}", setting.key);
                assert_eq!(setting.normalize(&json!(max + 1)), None, "{}", setting.key);
                assert_eq!(setting.default_value(), json!(default));
            }
            crate::settings::Kind::Text { max_chars, default } => {
                // The compiled default passes its own check, so the fallback is never a refused text.
                assert_eq!(setting.normalize(&json!(default)), Some(json!(default)), "{}", setting.key);
                assert_eq!(setting.default_value(), json!(default));
                let longest = "鲸".repeat(max_chars);
                assert_eq!(setting.normalize(&json!(longest)), Some(json!(longest)), "{}", setting.key);
                assert_eq!(setting.normalize(&json!(format!("{longest}鲸"))), None, "{}", setting.key);
                assert_eq!(setting.normalize(&json!("")), Some(json!("")), "{}", setting.key);
                for refused in [json!("AI"), json!("ai"), json!("ＤＳＨ"), json!("a\rb"), json!(1), Value::Null] {
                    assert_eq!(setting.normalize(&refused), None, "{}: {refused}", setting.key);
                }
                // The reader (`prompt_text::text`) agrees: the default without a value, the configured text when
                // accepted, the default when refused.
                let configured = |text: &str| {
                    let mut document = json!(text);
                    for name in setting.pointer.split('/').skip(1).collect::<Vec<_>>().into_iter().rev() {
                        let mut object = serde_json::Map::new();
                        object.insert(name.to_string(), document);
                        document = Value::Object(object);
                    }
                    document
                };
                assert_eq!(crate::prompt_text::text(&json!({}), setting.key), default, "{}", setting.key);
                assert_eq!(crate::prompt_text::text(&configured("鲸 140"), setting.key), "鲸 140", "{}", setting.key);
                assert_eq!(crate::prompt_text::text(&configured("AI 140"), setting.key), default, "{}", setting.key);
                // Prompt text is never chat-writable: the chat tool's list names no prompt pointer (#149 keeps it so).
                assert!(setting.pointer.starts_with("/prompts/"), "{}", setting.key);
            }
        }
    }
    // The registry owns the default and bounds the reader applies to the file (`wake_cfg_of`).
    let quiet = crate::settings::find(QUIET_KEY).expect("reply.wakeQuietMs is registered");
    let crate::settings::Kind::Integer { min, max, default } = quiet.kind else {
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
    assert!(page.contains("api('/api/settings')"), "the page lists through the route");
    assert!(
        page.contains("api('/api/settings/set',{setting:s.key,value:text?input.value:Number(input.value)})"),
        "the page sets through the route"
    );
    assert!(page.contains("'x-console-token'"), "the page sends the console credential");
    assert!(!page.contains("innerHTML"), "text is never parsed as HTML");
    assert!(!settings_db(&root).exists(), "showing the page creates no store");
    let request = json!({"setting": QUIET_KEY, "value": 4500});
    let (status, body) = admin_request(&context, "POST", "/api/settings/set", Some(&request));
    assert_eq!(status, 200, "{body}");
    assert_eq!(quiet_view(&context)["value"], 4500, "the page's next list shows the new value");
}

// #140: prompt text as live data.

/// A settings fixture whose memory database exists, so a prompt render can recall.
fn prompt_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    drop(initialize_fixture_memory_database(&context));
    (root, context)
}

fn set_text(context: &RuntimeContext, key: &str, value: Value) -> (u16, Value) {
    admin_request(context, "POST", "/api/settings/set", Some(&json!({"setting": key, "value": value})))
}

/// What the next compatibility render of a group conversation sends; it carries the behaviour and sticker rules.
fn next_render(context: &RuntimeContext) -> String {
    crate::prompt::build_with_runtime(context, "group:101", "atMention", "小鲸鱼", "9001")
}

fn open_store(root: &FixtureRoot) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(settings_db(root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap()
}

#[test]
fn prompt_text_edit_is_live_on_the_next_wake_as_a_section_replacement() {
    let (_root, context) = prompt_fixture("prompt text next wake");
    let key = "group:101";
    context.write_atomic(RuntimeArea::Data, "social-v2.json", json!({"conversations":{
        key:{"agentToken":"epoch-140","bootstrapSent":true,"lastUnreadSeq":1,"lastReadThroughSeq":0,"lastIncomingAt":1_000,
            "unread":[{"seq":1,"sender":"甲","userId":"1001","text":"今天天气不错","time":1_000,"isSelf":false}],
            "recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,"confirmedAt":1,"triggers":{"atMention":true}}}}})
        .to_string().as_bytes()).unwrap();
    crate::session_map::bind(&context, key, "sid-140", None).unwrap();
    let mut allowed = context.load_config().unwrap();
    allowed["allow"]["groups"] = json!([101]);
    context.write_atomic(RuntimeArea::Config, "config.json", allowed.to_string().as_bytes()).unwrap();
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    let open: crate::persona::Authority<'static> = &crate::persona::open_authority;
    let id = crate::persona::create(&context, &json!({"name": "小鲸鱼", "core": "你是一只住在群里的小鲸鱼，爱聊天。"}), open)
        .unwrap()["id"].as_str().unwrap().to_string();
    crate::persona::approve(&context, &json!({"id": id, "version": 1}), open).unwrap();
    crate::persona::assign(&context, &json!({"default": id}), open).unwrap();
    // Each wake's message packet comes from the console route, served here twice.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let context = context.with_console_port(listener.local_addr().unwrap().port());
    let (served, served_cfg) = (context.clone(), context.load_config().unwrap());
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            handle_conn(stream, served_cfg.clone(), served.clone());
        }
    });
    let (briefing, _) = crate::prompt::prepare_dsh_wake(&context, &context.load_config().unwrap(), key, "sid-140", "epoch-140",
        "atMention", "rpc-140-1", "").unwrap();
    assert!(briefing.contains("【简报节 behaviour】\n【别像客服】你不是客服："), "{briefing}");
    crate::session_context::observe(&context, key, "sid-140", "epoch-140", &json!({
        "type": "user/message", "seq": 1, "data": {"source": {"rpcId": "rpc-140-1"}},
    })).unwrap();
    let edited = "【别像客服】改过的规则 140";
    crate::settings::apply(&context, "prompt.notAClerk", &json!(edited), "administrator", "console").unwrap();
    let (delta, _) = crate::prompt::prepare_dsh_wake(&context, &context.load_config().unwrap(), key, "sid-140", "epoch-140",
        "atMention", "rpc-140-2", "").unwrap();
    server.join().unwrap();
    assert!(delta.contains("【唤醒增量】"), "{delta}");
    assert!(delta.contains(&format!("【替换简报节 behaviour】完整替换旧节：\n{edited}\n\n【积极性】")), "{delta}");
    assert!(!delta.contains("你不是客服"), "the old rule is gone: {delta}");
    assert!(!delta.contains("【替换简报节 stickers】"), "only the edited section is replaced: {delta}");
}

#[test]
fn prompt_text_save_is_checked_and_a_refusal_keeps_the_last_good_version() {
    let (root, context) = prompt_fixture("prompt text refused");
    assert!(next_render(&context).contains("【积极性】不要习惯性潜水"), "the compiled default renders");
    let (status, body) = set_text(&context, "prompt.proactive", json!("【积极性】看心情接话 140"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": "prompt.proactive", "value": "【积极性】看心情接话 140", "revision": 1, "changed": true}));
    assert!(next_render(&context).contains("【积极性】看心情接话 140\n\n【在群里怎么说话】"), "the next render reads it");
    let shape = "prompt.proactive must be text of at most 2000 characters, with no control character but line feed and tab";
    for (value, error) in [
        (json!("你是一个AI，要积极"), "prompt.proactive names a denied term: AI"),
        (json!("我是ai"), "prompt.proactive names a denied term: AI"),
        (json!("别提 DeepSeek 和 机器人"), "prompt.proactive names a denied term: deepseek, 机器人"),
        (json!("ＤＳＨ 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("D\u{200b}SH 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("D\u{61c}SH 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("D\u{180e}SH 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("DSH\u{61c}Ready 里的规则"), "prompt.proactive names a denied term: dsh"),
        (json!("x".repeat(2001)), shape),
        (json!("一行\r\n两行"), shape),
        (json!("响铃\u{7}"), shape),
        (json!(5), shape),
        (Value::Null, shape),
        (json!(["【积极性】"]), shape),
    ] {
        let (status, body) = set_text(&context, "prompt.proactive", value.clone());
        assert_eq!(status, 400, "{value}: {body}");
        assert_eq!(body["error"], error, "{value}");
    }
    assert!(next_render(&context).contains("【积极性】看心情接话 140\n\n"), "every refusal keeps the last good version");
    assert_eq!(audit_rows(&root).len(), 1, "a refusal writes no audit row");
    // The boundaries: 2000 characters (a Han character counts once) render whole, since a rule section is never cut;
    // line feed and tab pass.
    let longest = "鲸".repeat(2000);
    let (status, body) = set_text(&context, "prompt.proactive", json!(longest));
    assert_eq!(status, 200, "{body}");
    assert!(next_render(&context).contains(&format!("{longest}\n\n【在群里怎么说话】")), "the longest text renders whole");
    let (status, body) = set_text(&context, "prompt.proactive", json!("第一行\n\t第二行"));
    assert_eq!(status, 200, "{body}");
    assert!(next_render(&context).contains("第一行\n\t第二行\n\n【在群里怎么说话】"));
    // An emptied text renders nothing; the other rules stay.
    let (status, body) = set_text(&context, "prompt.proactive", json!(""));
    assert_eq!(status, 200, "{body}");
    let text = next_render(&context);
    assert!(!text.contains("【积极性】") && text.contains("【别像客服】你不是客服") && text.contains("【在群里怎么说话】"), "{text}");
}

#[test]
fn prompt_text_refused_at_load_falls_back_to_its_last_good_version() {
    let (root, context) = prompt_fixture("prompt text load guard");
    for text in ["【别像客服】第一版 140", "【别像客服】第二版 140"] {
        let (status, body) = set_text(&context, "prompt.notAClerk", json!(text));
        assert_eq!(status, 200, "{body}");
    }
    let conn = open_store(&root);
    // A row changed outside `apply`, or one holding a term a later release denies, is refused at load.
    conn.execute("UPDATE settings SET value=?1 WHERE key='prompt.notAClerk'", [json!("【别像客服】我是 AI 140").to_string()])
        .unwrap();
    // Newer audit rows holding refused text too (old data: many of them) are passed over.
    conn.execute_batch("BEGIN").unwrap();
    for n in 0..500 {
        conn.execute(
            "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
             VALUES(0, 'test', 'test', 'global', 'prompt.notAClerk', NULL, ?1, ?2)",
            rusqlite::params![json!(format!("【别像客服】AI {n}")).to_string(), format!("refused-{n}")],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    let text = next_render(&context);
    assert!(text.contains("【别像客服】第二版 140\n\n") && !text.contains("我是 AI"), "the newest accepted audited value: {text}");
    let (status, view) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{view}");
    let entry = view["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == "prompt.notAClerk").unwrap().clone();
    assert_eq!(entry["stored"], "【别像客服】第二版 140", "{entry}");
    assert_eq!(entry["value"], "【别像客服】第二版 140", "{entry}");
    // A reset (an audit row without a new value) ends the search: the default applies.
    conn.execute(
        "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(0, 'test', 'test', 'global', 'prompt.notAClerk', NULL, NULL, 'reset')",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(next_render(&context).contains("【别像客服】你不是客服："), "the default after a reset");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】第三版 140"));
    assert_eq!(status, 200, "{body}");
    assert!(next_render(&context).contains("【别像客服】第三版 140\n\n"), "a later save is in effect");

    // Without an accepted stored or audited value, the file's value applies when accepted, else the default.
    let (other_root, other) = prompt_fixture("prompt text file guard");
    write_config(&other_root, |config| config["prompts"] = json!({"behaviour": {"notAClerk": "【别像客服】文件里的 AI 140"}}));
    assert!(next_render(&other).contains("【别像客服】你不是客服："), "a refused file value renders the default");
    write_config(&other_root, |config| config["prompts"] = json!({"behaviour": {"notAClerk": "【别像客服】文件里的版本 140"}}));
    assert!(next_render(&other).contains("【别像客服】文件里的版本 140\n\n"), "an accepted file value renders");
    let (status, body) = set_quiet(&other, json!(4500));
    assert_eq!(status, 200, "{body}");
    let conn = open_store(&other_root);
    conn.execute(
        "INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES('prompt.notAClerk', ?1, 1, 0, 'test')",
        [json!("【别像客服】库里的 AI 140").to_string()],
    )
    .unwrap();
    drop(conn);
    assert!(next_render(&other).contains("【别像客服】文件里的版本 140\n\n"), "a refused row with no audited value leaves the file's");
}

#[test]
fn prompt_text_save_cut_off_before_its_audit_row_keeps_the_old_text() {
    let (root, context) = prompt_fixture("prompt text cut off");
    crate::settings::apply(&context, "prompt.groupConduct", &json!("【在群里怎么说话】旧版 140"), "administrator", "console")
        .unwrap();
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let cut = crate::settings::apply(&context, "prompt.groupConduct", &json!("【在群里怎么说话】新版 140"), "administrator", "console");
    assert_eq!(cut.err(), Some((503, "settings storage is unavailable".to_string())));
    let text = next_render(&context);
    assert!(text.contains("【在群里怎么说话】旧版 140\n\n") && !text.contains("新版 140"), "{text}");
    assert_eq!(audit_rows(&root).len(), 1, "the cut-off save left no audit row");
}

#[test]
fn prompt_text_readers_see_one_whole_version_while_writers_save() {
    let (root, context) = prompt_fixture("prompt text concurrent");
    let first = format!("【别像客服】{}", "甲".repeat(1990));
    let second = format!("【别像客服】{}", "乙".repeat(1990));
    let (a, b) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            for _ in 0..20 {
                let text = next_render(&context);
                let seen = [
                    text.contains("【别像客服】你不是客服："),
                    text.contains(&format!("{first}\n\n【积极性】")),
                    text.contains(&format!("{second}\n\n【积极性】")),
                ];
                assert_eq!(seen.iter().filter(|seen| **seen).count(), 1, "one whole version: {text}");
            }
        });
        let a = scope.spawn(|| crate::settings::apply(&context, "prompt.notAClerk", &json!(first.as_str()), "administrator", "console"));
        let b = scope.spawn(|| crate::settings::apply(&context, "prompt.notAClerk", &json!(second.as_str()), "administrator", "console"));
        reader.join().unwrap();
        (a.join().unwrap().err(), b.join().unwrap().err())
    });
    assert_eq!((a, b), (None, None));
    assert_eq!(audit_rows(&root).len(), 2);
}

#[test]
fn prompt_text_works_on_a_store_written_before_it() {
    // A store as #146 wrote it: schema version 1 with integer rows only. No migration is needed.
    let (root, context) = prompt_fixture("prompt text old store");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert!(next_render(&context).contains("【别像客服】你不是客服："), "the compiled defaults render");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】新版 140"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert!(next_render(&context).contains("【别像客服】新版 140\n\n"));
    assert_eq!(next_use_quiet(&context), Some(4500), "the integer setting is untouched");
    let version: i64 = open_store(&root).query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, 1, "the schema version is unchanged");
}

#[test]
fn prompt_text_settings_page_edits_text_in_a_text_area() {
    let (root, context) = settings_fixture("prompt text page");
    let granted = || -> Result<Option<std::fs::File>, (u16, String)> { Ok(None) };
    let (status, _, page) = crate::console_settings::route(&context, "GET", "/settings", "", false, &granted);
    assert_eq!(status, 200, "{page}");
    let (status, body) = admin_request(&context, "GET", "/api/settings", None);
    assert_eq!(status, 200, "{body}");
    let entry = |key: &str| body["settings"].as_array().unwrap().iter().find(|entry| entry["key"] == key).cloned();
    let mode = entry("prompt.mode.chat").unwrap_or_else(|| panic!("prompt.mode.chat is listed: {body}"));
    assert_eq!(mode["kind"], json!({"type": "text", "maxChars": 8000}));
    assert_eq!((mode["default"].clone(), mode["stored"].clone(), mode["value"].clone()), (json!(""), Value::Null, json!("")));
    assert_eq!(mode["group"], "prompt text");
    let rule = entry("prompt.notAClerk").unwrap_or_else(|| panic!("prompt.notAClerk is listed: {body}"));
    assert_eq!(rule["kind"], json!({"type": "text", "maxChars": 2000}));
    assert!(rule["default"].as_str().unwrap().starts_with("【别像客服】你不是客服："), "{rule}");
    assert!(
        page.contains("s.kind.type==='text'") && page.contains("createElement(text?'textarea':'input')"),
        "a text setting is edited in a text area"
    );
    assert!(
        page.contains("value:text?input.value:Number(input.value)"),
        "a text setting is sent as typed, an integer as a number"
    );
    assert!(!page.contains("innerHTML"), "text is never parsed as HTML");
    assert!(!settings_db(&root).exists(), "showing the page creates no store");
}

#[test]
fn prompt_text_is_never_writable_from_chat() {
    // The model must not rewrite its own prompt: the chat settings tool refuses every prompt pointer (#149 keeps it so).
    let (root, context) = settings_fixture("prompt text chat");
    context.write_atomic(RuntimeArea::Data, "social-v2.json", br#"{"conversations":{"private:7001":{"agentToken":"owner-token-140"}}}"#)
        .unwrap();
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    for pointer in ["prompts.behaviour.notAClerk", "prompts.mode.chat", "prompts.mode.closedAgent", "prompts", "prompts.mode"] {
        let reply = crate::mcp::call_prepared("settings_set",
            &json!({"key": "private:7001", "token": "owner-token-140", "settingKey": pointer, "value": "x"}), &context).to_string();
        assert!(reply.contains("该项不可通过对话修改"), "{pointer}: {reply}");
    }
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing was written");
}
