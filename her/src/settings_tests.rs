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
    let (status, _, reply) =
        crate::console_settings::route(&context, "POST", "/api/settings/set", &HashMap::new(), &body, false, &revoked);
    assert_eq!(status, 401, "{reply}");
    assert!(!settings_db(&root).exists(), "a revoked credential never writes");
    assert_eq!(next_use_quiet(&context), Some(3000));
    let granted = || -> Result<Option<std::fs::File>, (u16, String)> { Ok(None) };
    let (status, _, reply) =
        crate::console_settings::route(&context, "POST", "/api/settings/set", &HashMap::new(), &body, false, &granted);
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
    for (method, path) in [
        ("POST", "/api/settings"),
        ("GET", "/api/settings/set"),
        ("GET", "/api/settings/reset"),
        ("GET", "/api/settings/undo"),
        ("POST", "/api/settings/history"),
    ] {
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
        let crate::settings::Kind::Integer { min, max, default } = setting.kind;
        assert!(min <= default && default <= max, "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min)), Some(json!(min)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max)), Some(json!(max)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min - 1)), None, "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max + 1)), None, "{}", setting.key);
        assert_eq!(setting.default_value(), json!(default));
    }
    // The registry owns the default and bounds the reader applies to the file (`wake_cfg_of`).
    let quiet = crate::settings::find(QUIET_KEY).expect("reply.wakeQuietMs is registered");
    let crate::settings::Kind::Integer { min, max, default } = quiet.kind;
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
    let (status, content_type, page) =
        crate::console_settings::route(&context, "GET", "/settings", &HashMap::new(), "", false, &granted);
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
        page.contains("api('/api/settings/set',{setting:s.key,value:Number(input.value)})"),
        "the page sets through the route"
    );
    assert!(page.contains("api('/api/settings/reset',{setting:s.key})"), "the page resets through the route");
    assert!(page.contains("api('/api/settings/undo',{entry:e.entry})"), "the page undoes through the route");
    assert!(
        page.contains("'/api/settings/history'") && page.contains("'?before='+next"),
        "the page lists the history one page at a time"
    );
    for field in ["entry", "atMs", "setting", "old", "new", "source", "actor"] {
        assert!(page.contains(&format!("e.{field}")), "the history shows {field}");
    }
    assert!(page.contains("'x-console-token'"), "the page sends the console credential");
    assert!(!page.contains("innerHTML"), "text is never parsed as HTML");
    assert!(!settings_db(&root).exists(), "showing the page creates no store");
    let request = json!({"setting": QUIET_KEY, "value": 4500});
    let (status, body) = admin_request(&context, "POST", "/api/settings/set", Some(&request));
    assert_eq!(status, 200, "{body}");
    assert_eq!(quiet_view(&context)["value"], 4500, "the page's next list shows the new value");
}

fn post(context: &RuntimeContext, path: &str, body: Value) -> (u16, Value) {
    admin_request(context, "POST", path, Some(&body))
}

/// One page of `GET /api/settings/history<query>`: its entries, newest first, and its `next`.
fn history_page(context: &RuntimeContext, query: &str) -> (Vec<Value>, Value) {
    let (status, body) = admin_request(context, "GET", &format!("/api/settings/history{query}"), None);
    assert_eq!(status, 200, "{body}");
    (body["entries"].as_array().unwrap().clone(), body["next"].clone())
}

/// One request straight to `console_settings::route`, past the console front.
fn direct_route(
    context: &RuntimeContext, method: &str, path: &str, body: &str, authority: crate::persona::Authority<'_>,
) -> (u16, Value) {
    let (status, _, reply) = crate::console_settings::route(context, method, path, &HashMap::new(), body, false, authority);
    (status, serde_json::from_str(&reply).unwrap_or(Value::Null))
}

#[test]
fn settings_reset_stores_the_default_over_the_file_and_is_audited() {
    let (root, context) = settings_fixture("settings reset");
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000}}));
    assert_eq!(next_use_quiet(&context), Some(9000), "the file's value applies before any change");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": QUIET_KEY, "value": 3000, "revision": 2, "changed": true}));
    assert_eq!(next_use_quiet(&context), Some(3000), "the default wins over the file's value");
    assert_eq!(quiet_view(&context)["stored"], 3000);
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1]["actor"], "administrator");
    assert_eq!(
        (rows[1]["source"].clone(), rows[1]["old"].clone(), rows[1]["new"].clone()),
        (json!("reset"), json!("4500"), json!("3000"))
    );
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["changed"], false, "a stored default is not a change");
    assert_eq!(audit_rows(&root).len(), 2);
    // Without a stored value a reset still stores the default, so the file's value stops applying.
    let (fresh_root, fresh) = settings_fixture("settings reset fresh");
    write_config(&fresh_root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000}}));
    let (status, body) = post(&fresh, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["revision"].clone(), body["changed"].clone()), (json!(1), json!(true)));
    assert_eq!(next_use_quiet(&fresh), Some(3000));
    assert_eq!(audit_rows(&fresh_root)[0]["old"], Value::Null);
}

#[test]
fn settings_undo_restores_the_old_value_and_is_refused_after_a_later_change() {
    let (root, context) = settings_fixture("settings undo");
    for value in [4500, 5000] {
        let (status, body) = set_quiet(&context, json!(value));
        assert_eq!(status, 200, "{body}");
    }
    let (entries, _) = history_page(&context, "");
    let (latest, first) = (entries[0]["entry"].clone(), entries[1]["entry"].clone());
    // The first change has a later one: undoing it would overwrite 5000.
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": first}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "the setting changed after this entry; undo the later change first");
    assert_eq!(next_use_quiet(&context), Some(5000));
    assert_eq!(audit_rows(&root).len(), 2, "a refused undo writes no audit row");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": latest}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "entry": latest, "setting": QUIET_KEY, "stored": 4500}));
    assert_eq!(next_use_quiet(&context), Some(4500), "the next use reads the restored value");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[2]["actor"], "administrator");
    assert_eq!(
        (rows[2]["source"].clone(), rows[2]["old"].clone(), rows[2]["new"].clone()),
        (json!("undo"), json!("5000"), json!("4500"))
    );
    // The undone entry is no longer the latest change; undoing the undo redoes the change.
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": latest}));
    assert_eq!(status, 409, "{body}");
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_use_quiet(&context), Some(5000));
    // A change back to the same value is still a later change: the value matches, the undo is refused.
    for value in [6000, 5000] {
        let (status, body) = set_quiet(&context, json!(value));
        assert_eq!(status, 200, "{body}");
    }
    let (entries, _) = history_page(&context, "");
    let redo = entries.iter().find(|entry| entry["source"] == "undo" && entry["new"] == 5000).unwrap()["entry"].clone();
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": redo}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(next_use_quiet(&context), Some(5000));
    assert_eq!(audit_rows(&root).len(), 6);
}

#[test]
fn settings_undo_of_a_first_change_removes_the_stored_value_and_never_reuses_a_revision() {
    let (root, context) = settings_fixture("settings undo first");
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000}}));
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stored"], Value::Null, "{body}");
    assert_eq!(quiet_view(&context)["stored"], Value::Null);
    assert_eq!(next_use_quiet(&context), Some(9000), "the file's value applies again");
    let (entries, _) = history_page(&context, "");
    assert_eq!((entries[0]["old"].clone(), entries[0]["new"].clone()), (json!(4500), Value::Null));
    // The next change continues after both recorded changes.
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 3, "a revision is never reused for the same setting");
}

#[test]
fn settings_history_lists_changes_newest_first_one_page_at_a_time() {
    let (root, context) = settings_fixture("settings history");
    let (entries, next) = history_page(&context, "");
    assert!(entries.is_empty() && next.is_null(), "no store yet is an empty history");
    assert!(!settings_db(&root).exists(), "reading the history never creates the store");
    for ms in 1..=100 {
        crate::settings::apply(&context, QUIET_KEY, &json!(ms), "administrator", "console").unwrap();
    }
    let (entries, next) = history_page(&context, "");
    assert_eq!(entries.len(), 100);
    assert_eq!((entries[0]["old"].clone(), entries[0]["new"].clone()), (json!(99), json!(100)), "newest first");
    assert_eq!((entries[99]["old"].clone(), entries[99]["new"].clone()), (Value::Null, json!(1)));
    for entry in &entries {
        assert_eq!(
            (entry["actor"].clone(), entry["source"].clone(), entry["scope"].clone(), entry["setting"].clone()),
            (json!("administrator"), json!("console"), json!("global"), json!(QUIET_KEY))
        );
        assert!(entry["atMs"].as_i64().unwrap() > 0, "{entry}");
    }
    let ids: Vec<i64> = entries.iter().map(|entry| entry["entry"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|pair| pair[0] > pair[1]), "{ids:?}");
    // Exactly one full page: `next` points past it, and that page is empty and the last.
    assert_eq!(next, entries[99]["entry"]);
    let (older, last) = history_page(&context, &format!("?before={next}"));
    assert!(older.is_empty() && last.is_null(), "{older:?}");
    // One more change: the oldest moves to a second page.
    crate::settings::apply(&context, QUIET_KEY, &json!(101), "administrator", "console").unwrap();
    let (entries, next) = history_page(&context, "");
    assert_eq!((entries.len(), entries[0]["new"].clone(), entries[99]["new"].clone()), (100, json!(101), json!(2)));
    let (older, last) = history_page(&context, &format!("?before={next}"));
    assert_eq!(older.len(), 1, "{older:?}");
    assert_eq!(older[0]["new"], 1);
    assert!(last.is_null());
    // `setting` narrows the list to one registry key.
    let (narrowed, _) = history_page(&context, &format!("?setting={QUIET_KEY}"));
    assert_eq!(narrowed.len(), 100);
}

#[test]
fn settings_writes_wait_for_the_store_gate_and_give_up_after_five_seconds() {
    // Every write holds `locks/settings-store.lock` from its inspection until its connection closes, so no other
    // writer can leave a journal between a writer's inspection and its read-write open.
    let (root, context) = settings_fixture("settings gate every write");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let gate = context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock").unwrap();
    let (status, body) = std::thread::scope(|scope| {
        let writer = scope.spawn(|| set_quiet(&context, json!(5000)));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!writer.is_finished(), "a write went ahead while another writer held the store gate");
        // Readers never wait for the gate.
        assert_eq!(quiet_view(&context)["stored"], 4500);
        assert_eq!(next_use_quiet(&context), Some(4500));
        let (entries, _) = history_page(&context, "");
        assert_eq!(entries.len(), 1);
        drop(gate);
        writer.join().unwrap()
    });
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_use_quiet(&context), Some(5000));
    // Held past the wait, the write gives up and writes nothing.
    let gate = context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock").unwrap();
    let started = std::time::Instant::now();
    let (status, body) = set_quiet(&context, json!(6000));
    let waited = started.elapsed();
    drop(gate);
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "settings storage is unavailable");
    assert!(waited >= std::time::Duration::from_secs(5), "{waited:?}");
    assert_eq!(next_use_quiet(&context), Some(5000), "a write that gave up changed nothing");
    assert_eq!(audit_rows(&root).len(), 2);
}

#[test]
fn audit_undo_keeps_the_current_value_of_every_registry_setting() {
    // A whole-file audit undo restores `config.json`, but registry settings change and undo through the store only.
    let (root, context) = settings_fixture("settings audit undo");
    let config_post = |body: Value| {
        let (status, reply) = admin_request(&context, "POST", "/api/socialV2/config", Some(&body));
        assert_eq!(status, 200, "{reply}");
    };
    let latest_config_entry = || {
        let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
        assert_eq!(status, 200, "{recent}");
        recent["entries"].as_array().unwrap().iter().find(|entry| entry["path"] == "/api/socialV2/config").unwrap()["id"]
            .clone()
    };
    let file = || -> Value { serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap() };
    config_post(json!({"reply": {"wakeQuietMs": 7000}, "autoReplyCheckMs": 4000}));
    config_post(json!({"reply": {"wakeQuietMs": 8000}, "autoReplyCheckMs": 5000}));
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": latest_config_entry()})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["settingsKept"], json!([QUIET_KEY]), "{reply}");
    assert_eq!(file()["socialV2"]["autoReplyCheckMs"], 4000, "other members are restored");
    assert_eq!(file()["socialV2"]["reply"]["wakeQuietMs"], 8000, "the registry setting keeps its current value");
    // A registry value the current file lacks is removed from the restored file.
    config_post(json!({"reply": {}}));
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": latest_config_entry()})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["settingsKept"], json!([QUIET_KEY]), "{reply}");
    assert_eq!(file()["socialV2"]["reply"], json!({}));
    // An undo whose backup holds the current registry values keeps nothing.
    config_post(json!({"autoReplyCheckMs": 6000}));
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": latest_config_entry()})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["settingsKept"], json!([]), "{reply}");
    assert_eq!(file()["socialV2"]["autoReplyCheckMs"], 4000);
}

#[test]
fn settings_undo_never_overwrites_a_concurrent_change() {
    let (root, context) = settings_fixture("settings undo concurrent");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "");
    let entry = entries[0]["entry"].clone();
    // Two undos of the same entry: exactly one restores.
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| post(&context, "/api/settings/undo", json!({"entry": entry})));
        let second = scope.spawn(|| post(&context, "/api/settings/undo", json!({"entry": entry})));
        (first.join().unwrap(), second.join().unwrap())
    });
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(statuses, [200, 409], "{first:?} {second:?}");
    assert_eq!(audit_rows(&root).len(), 2);
    assert_eq!(quiet_view(&context)["stored"], Value::Null);
    // An undo racing a change from another writer, which takes no console gate: the change always survives.
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "");
    let entry = entries[0]["entry"].clone();
    let (undo, set) = std::thread::scope(|scope| {
        let undo = scope.spawn(|| post(&context, "/api/settings/undo", json!({"entry": entry})));
        let set = scope.spawn(|| crate::settings::apply(&context, QUIET_KEY, &json!(6000), "administrator", "console"));
        (undo.join().unwrap(), set.join().unwrap())
    });
    assert!(matches!(&set, Ok(applied) if applied.changed), "the change landed");
    assert!(matches!(undo.0, 200 | 409), "{undo:?}");
    assert_eq!(quiet_view(&context)["stored"], 6000, "the undo never overwrote the later change");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), if undo.0 == 200 { 5 } else { 4 }, "{rows:?}");
    assert_eq!(rows.last().unwrap()["new"], "6000", "the change is the last row either way");
}

#[test]
fn settings_undo_refuses_entries_it_cannot_restore() {
    let (root, context) = settings_fixture("settings undo old rows");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    // Rows an older release or another writer left: a stored value out of range, a retired setting's entry and an
    // entry of another scope.
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    conn.execute_batch(
        "UPDATE settings SET value='50000' WHERE key='reply.wakeQuietMs';
         INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(1, 'older release', 'console', 'global', 'reply.retired', 'not json', '1', 'retired-1');
         INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(2, 'older release', 'console', 'conversation:group:1', 'reply.wakeQuietMs', NULL, '7', 'scoped-1');",
    )
    .unwrap();
    drop(conn);
    // The stored value changed without an audit row: the first entry's new value is no longer what is stored.
    let (entries, _) = history_page(&context, "");
    let first = entries.iter().find(|entry| entry["new"] == 4500).unwrap()["entry"].clone();
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": first}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "the setting changed after this entry; undo the later change first");
    let (status, body) = set_quiet(&context, json!(4600));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "");
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert_eq!((entries[0]["old"].clone(), entries[0]["new"].clone()), (json!(50000), json!(4600)), "the stored text");
    assert_eq!(entries[1]["scope"], "conversation:group:1");
    assert_eq!(entries[2]["setting"], "reply.retired", "the history keeps a retired setting's entries");
    assert_eq!(entries[2]["old"], "not json", "stored text that is not JSON is shown as a string");
    for (entry, error) in [
        (&entries[0], "the old value of this entry is no longer accepted"),
        (&entries[1], "this entry cannot be undone"),
        (&entries[2], "this entry cannot be undone"),
    ] {
        let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entry["entry"]}));
        assert_eq!(status, 409, "{entry}: {body}");
        assert_eq!(body["error"], error, "{entry}");
    }
    assert_eq!(quiet_view(&context)["stored"], 4600);
    assert_eq!(audit_rows(&root).len(), 4, "a refused undo writes nothing");
}

#[test]
fn settings_undo_cut_off_before_its_audit_row_changes_nothing() {
    let (root, context) = settings_fixture("settings undo rollback");
    for value in [4500, 5000] {
        let (status, body) = set_quiet(&context, json!(value));
        assert_eq!(status, 200, "{body}");
    }
    let (entries, _) = history_page(&context, "");
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(false));
    assert_eq!(status, 503, "{body}");
    assert_eq!(quiet_view(&context)["stored"], 5000, "the cut-off undo left no value");
    assert_eq!(audit_rows(&root).len(), 2, "and no audit row");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(audit_rows(&root).len(), 3);
}

#[test]
fn settings_history_undo_and_reset_refuse_agents_legacy_contexts_and_bad_input() {
    let granted = || -> Result<Option<std::fs::File>, (u16, String)> { Ok(None) };
    let revoked = || -> Result<Option<std::fs::File>, (u16, String)> { Err((401, "console credential was revoked".to_string())) };
    let legacy = RuntimeContext::legacy();
    for (method, path, body) in [
        ("GET", "/api/settings/history", ""),
        ("POST", "/api/settings/reset", r#"{"setting":"reply.wakeQuietMs"}"#),
        ("POST", "/api/settings/undo", r#"{"entry":1}"#),
    ] {
        let (status, reply) = direct_route(&legacy, method, path, body, &granted);
        assert_eq!((status, reply["error"].clone()), (503, json!("settings require a selected runtime")), "{path}");
    }

    let (root, context) = settings_fixture("settings undo bad input");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": 1}));
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"], "unknown history entry");
    assert!(!settings_db(&root).exists(), "an undo without a store never creates one");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    // The authority re-checks the console credential under the configuration gate before anything is written.
    for (path, body) in [("/api/settings/reset", r#"{"setting":"reply.wakeQuietMs"}"#), ("/api/settings/undo", r#"{"entry":1}"#)] {
        let (status, reply) = direct_route(&context, "POST", path, body, &revoked);
        assert_eq!(status, 401, "{path}: {reply}");
    }
    for (request, status, error) in [
        (json!({"entry": 99999}), 404, "unknown history entry"),
        (json!({"entry": 0}), 400, "entry must be a positive integer"),
        (json!({"entry": -1}), 400, "entry must be a positive integer"),
        (json!({"entry": 1.5}), 400, "entry must be a positive integer"),
        (json!({"entry": "1"}), 400, "entry must be a positive integer"),
        (json!({"entry": u64::MAX}), 400, "entry must be a positive integer"),
        (json!({}), 400, "entry must be a positive integer"),
        (json!({"entry": 1, "setting": QUIET_KEY}), 400, "unknown settings field"),
        (json!([1]), 400, "body must be a JSON object"),
    ] {
        let (got, body) = post(&context, "/api/settings/undo", request.clone());
        assert_eq!((got, body["error"].clone()), (status, json!(error)), "{request}");
    }
    for (request, error) in [
        (json!({"setting": "reply.other"}), "unknown setting"),
        (json!({}), "setting is required"),
        (json!({"setting": QUIET_KEY, "value": 3000}), "unknown settings field"),
    ] {
        let (status, body) = post(&context, "/api/settings/reset", request.clone());
        assert_eq!((status, body["error"].clone()), (400, json!(error)), "{request}");
    }
    for query in ["?before=0", "?before=-1", "?before=abc", "?before=1.5", "?setting=reply.other"] {
        let (status, body) = admin_request(&context, "GET", &format!("/api/settings/history{query}"), None);
        assert_eq!(status, 400, "{query}: {body}");
    }
    for (method, path, body) in [
        ("GET", "/api/settings/history", None),
        ("POST", "/api/settings/reset", Some(json!({"setting": QUIET_KEY}))),
        ("POST", "/api/settings/undo", Some(json!({"entry": 1}))),
    ] {
        let (status, reply) = agent_request(&context, method, path, body.as_ref(), "fixture-agent-token");
        assert_eq!(status, 403, "{path}: {reply}");
    }
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": 1, "pad": "x".repeat(1_000_001)}));
    assert_eq!(status, 413, "{body}");
    assert_eq!(quiet_view(&context)["stored"], 4500, "no refusal changed the value");
    assert_eq!(audit_rows(&root).len(), 1, "and none wrote an audit row");

    // A store this release cannot read: every route answers 503 and the file keeps every byte.
    let (garbage_root, garbage) = settings_fixture("settings history garbage");
    let bytes = vec![b'x'; 4096];
    std::fs::write(settings_db(&garbage_root), &bytes).unwrap();
    for (method, path, body) in [
        ("GET", "/api/settings/history", None),
        ("POST", "/api/settings/undo", Some(json!({"entry": 1}))),
        ("POST", "/api/settings/reset", Some(json!({"setting": QUIET_KEY}))),
    ] {
        let (status, reply) = admin_request(&garbage, method, path, body.as_ref());
        assert_eq!((status, reply["error"].clone()), (503, json!("settings storage is unavailable")), "{path}");
    }
    assert_eq!(std::fs::read(settings_db(&garbage_root)).unwrap(), bytes);
}

#[test]
fn settings_undo_takes_the_store_gate_before_it_inspects_the_store() {
    // Every write holds the store gate before it inspects the store, even an undo that answers 404 when there is none.
    let (root, context) = settings_fixture("settings undo gate first");
    let gate = context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock").unwrap();
    let (status, body) = std::thread::scope(|scope| {
        let undo = scope.spawn(|| post(&context, "/api/settings/undo", json!({"entry": 1})));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!undo.is_finished(), "an undo answered before the store gate was free");
        drop(gate);
        undo.join().unwrap()
    });
    assert_eq!((status, body["error"].clone()), (404, json!("unknown history entry")), "{body}");
    assert!(!settings_db(&root).exists(), "an undo without a store still creates none");
}

#[test]
fn audit_undo_refuses_a_backup_that_cannot_hold_a_current_registry_value() {
    // A backup whose `reply` is not an object cannot hold `reply.wakeQuietMs`: the undo refuses and writes nothing,
    // instead of reporting a kept value that the restored file lacks.
    let (root, context) = settings_fixture("settings audit undo scalar");
    let config_post = |body: Value| {
        let (status, reply) = admin_request(&context, "POST", "/api/socialV2/config", Some(&body));
        assert_eq!(status, 200, "{reply}");
    };
    config_post(json!({"reply": false}));
    config_post(json!({"reply": {"wakeQuietMs": 8000}}));
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
    assert_eq!(status, 200, "{recent}");
    let latest = recent["entries"].as_array().unwrap().iter().find(|entry| entry["path"] == "/api/socialV2/config").unwrap()
        ["id"]
        .clone();
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": latest})));
    assert_eq!(status, 409, "{reply}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "a refused undo writes nothing");
}
