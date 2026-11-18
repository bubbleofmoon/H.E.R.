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

fn pre_settings_copy(root: &FixtureRoot) -> std::path::PathBuf {
    root.0.join("config/config.json.pre-settings")
}

/// `config.json` as stored, without the settings overlay: what a release without the store reads.
fn stored_config(root: &FixtureRoot) -> Value {
    serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap()
}

/// The quiet time a release without the store reads: `config.json` alone, through the same reader.
fn file_quiet(root: &FixtureRoot) -> Option<i64> {
    crate::wake_cfg_of(&stored_config(root)).reply_wake_quiet_ms
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
    let mut mirrored: Value = serde_json::from_slice(&stored_file).unwrap();
    mirrored["socialV2"] = json!({"reply": {"wakeQuietMs": 4500}});
    assert_eq!(stored_config(&root), mirrored, "the change is written back into config.json and nothing else changes");
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), stored_file, "config.json from before the first write is kept");
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
    // The store and the pre-settings copy exist already, so these writers, which hold no configuration gate here,
    // race only in the store.
    crate::settings::create_store(&context).unwrap();
    std::fs::copy(root.0.join("config/config.json"), pre_settings_copy(&root)).unwrap();
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
    assert_eq!(next_use_quiet(&context), Some(4500), "an out-of-range stored value is not applied; the written-back file value is read");
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
    assert!(!pre_settings_copy(&root).exists(), "nor copies config.json");
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
    assert_eq!(next_use_quiet(&context), Some(4500), "a store that is not exactly this release's applies nothing; the file holds the last written-back value");
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
            crate::settings::Kind::Text { max_chars, default } => {
                // The compiled default passes its own check, so the fallback is never a refused text.
                assert_eq!(setting.normalize(&json!(default)), Some(json!(default)), "{}", setting.key);
                assert_eq!(setting.default_value(), json!(default));
                let missing = setting.text(&json!({}));
                assert_eq!(missing.as_deref(), Some(default), "{}: a missing value reads as the default", setting.key);
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
                // Prompt text is never chat-writable, so the model cannot rewrite its own prompt.
                assert!(setting.pointer.starts_with("/prompts/"), "{}", setting.key);
                assert!(!setting.chat, "{}: prompt text is never chat-writable", setting.key);
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
    assert_eq!(body["revision"], 2, "the first write seeded the file's value as revision 1");
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": QUIET_KEY, "value": 3000, "revision": 3, "changed": true}));
    assert_eq!(next_use_quiet(&context), Some(3000), "the default wins over the file's value");
    assert_eq!(quiet_view(&context)["stored"], 3000);
    assert_eq!(file_quiet(&root), Some(3000), "the default is written back into config.json");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!((rows[0]["source"].clone(), rows[0]["new"].clone()), (json!("migration"), json!("9000")));
    assert_eq!(rows[2]["actor"], "administrator");
    assert_eq!(
        (rows[2]["source"].clone(), rows[2]["old"].clone(), rows[2]["new"].clone()),
        (json!("reset"), json!("4500"), json!("3000"))
    );
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["changed"], false, "a stored default is not a change");
    assert_eq!(audit_rows(&root).len(), 3);
    // Without a stored value a reset first seeds the file's value, then stores the default over it, so the file's
    // value stops applying and config.json gets the default.
    let (fresh_root, fresh) = settings_fixture("settings reset fresh");
    write_config(&fresh_root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000}}));
    let (status, body) = post(&fresh, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["revision"].clone(), body["changed"].clone()), (json!(2), json!(true)));
    assert_eq!(next_use_quiet(&fresh), Some(3000));
    assert_eq!(file_quiet(&fresh_root), Some(3000));
    let rows = audit_rows(&fresh_root);
    assert_eq!((rows[0]["source"].clone(), rows[0]["old"].clone()), (json!("migration"), Value::Null));
    assert_eq!(
        (rows[1]["source"].clone(), rows[1]["old"].clone(), rows[1]["new"].clone()),
        (json!("reset"), json!("9000"), json!("3000"))
    );
}

#[test]
fn settings_reset_of_a_model_id_without_a_default_stores_not_configured_over_the_file() {
    let model = &crate::settings::HARNESS_MODEL;
    let next_use = |context: &RuntimeContext| model.text(&context.load_config_document().unwrap());
    let (root, context) = settings_fixture("settings reset model");
    write_config(&root, |config| config["harness"] = json!({"model": "file-model"}));
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": model.key, "value": "model-a"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_use(&context).as_deref(), Some("model-a"));
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": model.key}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": model.key, "value": null, "revision": 3, "changed": true}));
    assert_eq!(next_use(&context), None, "the missing default wins over the file's value");
    assert_eq!(stored_config(&root)["harness"]["model"], Value::Null, "a stored null is written back as null");
    assert_eq!(model.text(&stored_config(&root)), None, "a release without the store reads not configured too");
    let rows = audit_rows(&root);
    assert_eq!((rows[0]["source"].clone(), rows[0]["new"].clone()), (json!("migration"), json!("\"file-model\"")));
    assert_eq!(
        (rows[2]["source"].clone(), rows[2]["old"].clone(), rows[2]["new"].clone()),
        (json!("reset"), json!("\"model-a\""), json!("null"))
    );
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": model.key}));
    assert_eq!((status, body["changed"].clone()), (200, json!(false)), "a stored default is not a change");
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": model.key, "value": null}));
    assert_eq!(status, 400, "a set still refuses null: {body}");
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_use(&context).as_deref(), Some("model-a"), "undoing the reset restores the old value");
    assert_eq!(stored_config(&root)["harness"]["model"], "model-a", "and writes it back");
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "undoing the undo stores the missing default again: {body}");
    assert_eq!(next_use(&context), None);
    assert_eq!(stored_config(&root)["harness"]["model"], Value::Null);
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
    // No value in config.json, so nothing is seeded and the change is the setting's first.
    let (root, context) = settings_fixture("settings undo first");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(file_quiet(&root), Some(4500), "the change is written back");
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stored"], Value::Null, "{body}");
    assert_eq!(quiet_view(&context)["stored"], Value::Null);
    assert!(
        stored_config(&root).pointer("/socialV2/reply/wakeQuietMs").is_none(),
        "the undo removed the written-back value from config.json"
    );
    assert_eq!(next_use_quiet(&context), Some(3000), "the default applies again");
    assert_eq!(file_quiet(&root), Some(3000), "a release without the store reads the default too");
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
    // The first write seeds every value the file holds, in registry order, as revision 1; the digest model is not in
    // the file, so its change is its first.
    let seeded = ["harness.model", "harness.maxSteps", "harness.maxTokens", "memory.digest.timeoutSecs", "webSearch.model"];
    for (key, value) in &changes {
        let (status, body) = set_setting(&context, key, value.clone());
        assert_eq!(status, 200, "{key}: {body}");
        let revision = if seeded.contains(key) { 2 } else { 1 };
        assert_eq!(body, json!({"ok": true, "setting": key, "value": value, "revision": revision, "changed": true}), "{key}");
    }
    assert_eq!(next_turn_model(&context), Ok("store-model".to_string()));
    assert_eq!(next_turn_budgets(&context), (12, 64_000));
    assert_eq!(next_digest(&context), (Some("store-digest".to_string()), 45));
    assert_eq!(next_search_model(&context), "store-search");
    let file = stored_config(&root);
    for (key, value) in &changes {
        let pointer = crate::settings::find(key).unwrap().pointer;
        assert_eq!(file.pointer(pointer), Some(value), "{key} is written back into config.json");
    }
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), stored_file, "config.json from before the first write is kept");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), seeded.len() + changes.len(), "{rows:?}");
    for (row, key) in rows.iter().zip(seeded) {
        assert_eq!((row["key"].as_str(), row["source"].as_str(), row["actor"].as_str()), (Some(key), Some("migration"), Some("system")));
    }
    for (row, (key, value)) in rows[seeded.len()..].iter().zip(&changes) {
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
    let config_path = root.0.join("config/config.json");
    let file_before = std::fs::read(&config_path).unwrap();
    for (key, value) in [("harness.model", json!("store-model")), ("harness.maxSteps", json!(12)), ("memory.digest.timeoutSecs", json!(45))] {
        let (status, body) = set_setting(&context, key, value);
        assert_eq!(status, 200, "{key}: {body}");
    }
    assert_eq!(next_turn_model(&context), Ok("store-model".to_string()));
    // config.json as it was before the write-backs, so only the store could supply these values.
    std::fs::write(&config_path, &file_before).unwrap();
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
    assert_eq!(setting_view(&context, "harness.maxSteps")["stored"], Value::Null, "the restored store has no model settings");
    assert_eq!(next_turn_budgets(&context), (12, 32_768), "config.json holds the last written-back value");
    let (status, body) = set_setting(&context, "harness.model", json!("after-restore"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(next_turn_model(&context), Ok("after-restore".to_string()));
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 3, "the restored audit row, the seeded copy of the file's harness.maxSteps and the new one: {rows:?}");
    assert_eq!(
        (rows[1]["key"].clone(), rows[1]["source"].clone(), rows[1]["new"].clone()),
        (json!("harness.maxSteps"), json!("migration"), json!("12"))
    );
    assert_eq!(next_turn_budgets(&context), (12, 32_768));
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
    assert_eq!(file_quiet(&root), Some(4500), "a registry setting is written to the store and back into config.json");
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), stored_file, "config.json from before the first write is kept");
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
    assert!(reply.get("mirrorError").is_none(), "a write-back that succeeded adds nothing: {reply}");

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
    assert_eq!(file_quiet(&root), Some(30000), "config.json holds the last stored value");
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
    assert_eq!(
        (read(&context), listed(&context)),
        (json!(4500), json!(4500)),
        "a stored value the registry refuses is not applied; the file's written-back value is read"
    );
    assert_eq!(next_use_quiet(&context), Some(4500));
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
    // An audit row without a new value (an undo that removed the stored value writes one) ends the search: the
    // default applies.
    conn.execute(
        "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(0, 'test', 'test', 'global', 'prompt.notAClerk', NULL, NULL, 'removed')",
        [],
    )
    .unwrap();
    drop(conn);
    // The search ends, and the file's value applies when the registry accepts it: the file holds the last text a write
    // stored (#176 writes the stored values back), so the second version still renders.
    assert!(next_render(&context).contains("【别像客服】第二版 140\n\n"), "the file's value after a removal");
    // A file with no prompt text leaves the default.
    write_config(&root, |config| config["prompts"] = json!({}));
    assert!(next_render(&context).contains("【别像客服】你不是客服："), "the default after a removal, with nothing in the file");
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
    // The set above seeded the file's text (#176); replace that row and its migration row, so the store holds a
    // refused value with no audited one.
    conn.execute("DELETE FROM settings_audit WHERE key='prompt.notAClerk'", []).unwrap();
    conn.execute(
        "INSERT INTO settings(key, value, revision, updated_ms, updated_by) VALUES('prompt.notAClerk', ?1, 1, 0, 'test')
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
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
    // The reader starts once a commit has returned, then reads until both writers have; every such read must show a
    // whole committed version, never the compiled default.
    let committed = std::sync::atomic::AtomicUsize::new(0);
    let (a, b, reads_after_commit) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            while committed.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            let mut reads_after_commit = 0;
            loop {
                let done = committed.load(std::sync::atomic::Ordering::SeqCst);
                let text = next_render(&context);
                let default = text.contains("【别像客服】你不是客服：");
                let seen = [
                    default,
                    text.contains(&format!("{first}\n\n【积极性】")),
                    text.contains(&format!("{second}\n\n【积极性】")),
                ];
                assert_eq!(seen.iter().filter(|seen| **seen).count(), 1, "one whole version: {text}");
                assert!(!default, "a read after a commit shows the default, not the committed text: {text}");
                reads_after_commit += 1;
                if done == 2 {
                    break;
                }
            }
            reads_after_commit
        });
        let a = scope.spawn(|| {
            let saved = crate::settings::apply(&context, "prompt.notAClerk", &json!(first.as_str()), "administrator", "console");
            committed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            saved.err()
        });
        let b = scope.spawn(|| {
            let saved = crate::settings::apply(&context, "prompt.notAClerk", &json!(second.as_str()), "administrator", "console");
            committed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            saved.err()
        });
        let reads = reader.join().unwrap();
        (a.join().unwrap(), b.join().unwrap(), reads)
    });
    assert_eq!((a, b), (None, None));
    assert!(reads_after_commit > 0, "no read ran after a commit");
    let settled = next_render(&context);
    assert!(settled.contains(&format!("{first}\n\n【积极性】")) || settled.contains(&format!("{second}\n\n【积极性】")), "{settled}");
    assert_eq!(audit_rows(&root).len(), 2);
}

#[test]
fn denied_term_split_by_format_characters_is_refused_on_save_and_load() {
    // Invisible format characters read as joiners and as word boundaries at once: each spelling reads as `DSH`.
    let rule = crate::settings::find("prompt.notAClerk").unwrap();
    for split in ["x\u{200b}D\u{200b}SH", "x\u{200b}DS\u{200b}H", "x\u{200b}D\u{200b}S\u{200b}H"] {
        assert!(crate::settings::denied_in_text(split).contains(&"dsh".to_string()), "{split:?}");
        assert!(rule.normalize(&json!(split)).is_none(), "{split:?} is accepted on load");
        let (_root, context) = prompt_fixture("prompt text split denied");
        let (status, body) = set_text(&context, "prompt.notAClerk", json!(split));
        assert_eq!(status, 400, "{split:?}: {body}");
    }
    // A split that a letter or digit touches is one word, not the term.
    for clean in ["x\u{200b}DSHy", "XDSHX", "x\u{200b}DSH1"] {
        assert!(!crate::settings::denied_in_text(clean).contains(&"dsh".to_string()), "{clean:?}");
    }
}

#[test]
fn prompt_text_rule_a_strip_turns_into_a_denied_term_falls_back_to_the_last_good_text() {
    let (root, context) = prompt_fixture("prompt text strip denied");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】新版 140"));
    assert_eq!(status, 200, "{body}");
    // Accepted on save, when the strip has no term to remove; the strip then makes `DSH` of it.
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】D鲸SH"));
    assert_eq!(status, 200, "{body}");
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["鲸"]}));
    let text = next_render(&context);
    assert!(text.contains("【别像客服】新版 140\n\n"), "the last good version renders: {text}");
    assert!(!text.contains("DSH") && !text.contains("鲸"), "a denied term reached the model: {text}");
}

#[test]
fn prompt_text_render_stripped_again_on_delivery_never_makes_a_denied_term() {
    let (root, context) = prompt_fixture("prompt text double strip");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】新版 140"));
    assert_eq!(status, 200, "{body}");
    // Accepted on save; one strip pass reads `D鲸鱼SH` and a second one `DSH`.
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】D鲸鲸鱼鱼SH"));
    assert_eq!(status, 200, "{body}");
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["鲸鱼"]}));
    let render = next_render(&context);
    let config = context.load_config_document().unwrap();
    // The delivery strips the render again, as the DSH and CLI paths and the harness do.
    let delivered = crate::model_view::strip_text_with_runtime(&context, &render, &config);
    for text in [&render, &delivered] {
        assert!(!crate::settings::denied_in_text(text).contains(&"dsh".to_string()), "a denied term reached the model: {text}");
        assert!(text.contains("【别像客服】新版 140\n\n"), "the last good version renders: {text}");
        assert!(!text.contains("D鲸"), "{text}");
    }
}

#[test]
fn prompt_text_strip_joining_rules_across_their_boundary_never_makes_a_denied_term() {
    let (root, context) = prompt_fixture("prompt text boundary strip");
    let (status, body) = set_text(&context, "prompt.notAClerk", json!("【别像客服】D"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_text(&context, "prompt.proactive", json!("SH【积极性】"));
    assert_eq!(status, 200, "{body}");
    // The rules are joined by a blank line; stripping it from the whole render would make `DSH` of the two.
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["\n\n"]}));
    let render = next_render(&context);
    assert!(!crate::settings::denied_in_text(&render).contains(&"dsh".to_string()), "{render}");
    let config = context.load_config_document().unwrap();
    let delivered = crate::model_view::strip_text_with_runtime(&context, &render, &config);
    assert!(!crate::settings::denied_in_text(&delivered).contains(&"dsh".to_string()), "a denied term reached the model: {delivered}");
}

#[test]
fn prompt_text_sticker_rules_a_strip_turns_into_a_denied_term_fall_back_to_the_default() {
    let (root, context) = prompt_fixture("prompt text sticker strip denied");
    let (status, body) = set_text(&context, "prompt.stickerStrategy", json!("【表情包策略】D鲸SH 140"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_text(&context, "prompt.stickerSoften", json!("【用表情缓和语气】D鲸SH 140"));
    assert_eq!(status, 200, "{body}");
    write_config(&root, |config| config["socialV2"]["context"] = json!({"stripFromModelText": ["鲸"]}));
    // The strategy is printed without a sticker inventory; the strip's `DSH` never reaches the model, the default does.
    let text = next_render(&context);
    assert!(!text.contains("DSH") && text.contains(crate::prompt_text::STICKER_STRATEGY), "{text}");
    // The softening rule is printed only with an inventory, so it is checked directly.
    let config = context.load_config_document().unwrap();
    assert_eq!(
        crate::prompt_text::model_text(&context, &config, crate::prompt_text::STICKER_SOFTEN_KEY),
        crate::prompt_text::STICKER_SOFTEN
    );
    assert_eq!(
        crate::prompt_text::model_text(&context, &config, crate::prompt_text::STICKER_STRATEGY_KEY),
        crate::prompt_text::STICKER_STRATEGY
    );
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
    let (status, _, page) = crate::console_settings::route(&context, "GET", "/settings", &HashMap::new(), "", false, &granted);
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
        page.contains("value:integer?Number(input.value):input.value"),
        "a text setting is sent as typed, an integer as a number"
    );
    assert!(!page.contains("innerHTML"), "text is never parsed as HTML");
    assert!(!settings_db(&root).exists(), "showing the page creates no store");
}

#[test]
fn prompt_text_is_never_writable_from_chat() {
    // The model must not rewrite its own prompt: no text setting is marked `chat`, so the chat settings tool refuses
    // every prompt pointer.
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

#[test]
fn prompt_text_reset_stores_the_default_and_undo_restores_only_accepted_text() {
    // #140 with #147: a reset stores a text setting's compiled default over the file's value (a mode prompt's empty
    // default included), and an undo restores an old text only while the registry still accepts it.
    let (root, context) = prompt_fixture("prompt text reset undo");
    write_config(&root, |config| {
        config["prompts"] = json!({
            "behaviour": {"notAClerk": "【别像客服】文件里的版本 140"},
            "mode": {"chat": "文件里的模式说明 140"},
        })
    });
    let edited = "【别像客服】改过的 140";
    let (status, body) = set_text(&context, "prompt.notAClerk", json!(edited));
    assert_eq!(status, 200, "{body}");
    // The file holds prompt text, so the first write seeded the store with it (revision 1, one migration row each);
    // the save is revision 2 and the reset below revision 3.
    let seeded: Vec<Value> = audit_rows(&root).into_iter().filter(|row| row["source"] == "migration").collect();
    for (key, text) in [("prompt.notAClerk", "【别像客服】文件里的版本 140"), ("prompt.mode.chat", "文件里的模式说明 140")] {
        assert!(
            seeded.iter().any(|row| row["key"] == key && row["new"] == json!(json!(text).to_string())),
            "{key} was not seeded from the file: {seeded:?}"
        );
    }
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": "prompt.notAClerk"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"ok": true, "setting": "prompt.notAClerk", "value": crate::prompt_text::NOT_A_CLERK, "revision": 3, "changed": true})
    );
    assert!(next_render(&context).contains("【别像客服】你不是客服："), "the default wins over the file's value");
    assert_eq!(setting_view(&context, "prompt.notAClerk")["stored"], crate::prompt_text::NOT_A_CLERK);
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": "prompt.notAClerk"}));
    assert_eq!((status, body["changed"].clone()), (200, json!(false)), "a stored default is not a change");
    // Undoing the reset restores the saved text through the registry's check.
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stored"], edited, "{body}");
    assert!(next_render(&context).contains(&format!("{edited}\n\n【积极性】")), "the next render reads the restored text");
    // An entry whose old value the registry now refuses (a term a later release denies) is not undone.
    let conn = open_store(&root);
    conn.execute(
        "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(0, 'test', 'test', 'global', 'prompt.notAClerk', ?1, ?2, 'older-release')",
        rusqlite::params![json!("【别像客服】我是 AI 140").to_string(), json!(edited).to_string()],
    )
    .unwrap();
    let entry = conn.last_insert_rowid();
    drop(conn);
    let rows = audit_rows(&root).len();
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entry}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "the old value of this entry is no longer accepted");
    assert!(next_render(&context).contains(&format!("{edited}\n\n【积极性】")), "a refused undo keeps the text");
    assert_eq!(audit_rows(&root).len(), rows, "a refused undo writes no audit row");
    // A mode prompt's default is empty: a reset stores the empty text over the file's, and no mode prompt is sent.
    let mode = |context: &RuntimeContext| {
        crate::prompt_text::mode_prompt(&context.load_config_document().unwrap(), crate::prompt_text::MODE_CHAT_KEY)
    };
    assert_eq!(mode(&context), "文件里的模式说明 140", "the file's mode prompt applies before any change");
    let (status, body) = set_text(&context, "prompt.mode.chat", json!("群聊模式说明 140"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": "prompt.mode.chat"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": "prompt.mode.chat", "value": "", "revision": 3, "changed": true}));
    let view = setting_view(&context, "prompt.mode.chat");
    assert_eq!((view["stored"].clone(), view["value"].clone()), (json!(""), json!("")));
    assert_eq!(mode(&context), "", "the stored empty default wins over the file's mode prompt");
}

#[test]
fn settings_first_write_seeds_the_store_from_config_json_once() {
    let (root, context) = settings_fixture("settings seed");
    write_config(&root, |config| config["socialV2"] = json!({"enabled": true, "reply": {"wakeQuietMs": 7000}}));
    let original = std::fs::read(root.0.join("config/config.json")).unwrap();
    assert_eq!(next_use_quiet(&context), Some(7000), "before the store exists the file's value is read");
    assert!(!settings_db(&root).exists() && !pre_settings_copy(&root).exists());

    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": QUIET_KEY, "value": 4500, "revision": 2, "changed": true}));
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "one migration row, then the change: {rows:?}");
    assert_eq!(rows[0]["actor"], "system");
    assert_eq!(rows[0]["source"], "migration");
    assert_eq!(rows[0]["scope"], "global");
    assert_eq!(rows[0]["key"], QUIET_KEY);
    assert_eq!((rows[0]["old"].clone(), rows[0]["new"].clone()), (Value::Null, json!("7000")));
    assert_eq!(rows[1]["source"], "console");
    assert_eq!((rows[1]["old"].clone(), rows[1]["new"].clone()), (json!("7000"), json!("4500")));
    assert_ne!(rows[0]["request"], rows[1]["request"]);
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), original, "config.json from before the first write is kept");
    assert_eq!(stored_config(&root)["socialV2"], json!({"enabled": true, "reply": {"wakeQuietMs": 4500}}));
    let conn = rusqlite::Connection::open_with_flags(settings_db(&root), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, 1, "seeding keeps schema version 1, which the previous release opens");
    drop(conn);

    // A restart: a fresh context over the same root never seeds again, even when the file holds another value.
    let context = RuntimeContext::from_root(&root.0).unwrap();
    write_config(&root, |config| config["socialV2"]["reply"]["wakeQuietMs"] = json!(9000));
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 3);
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 3, "no second migration row: {rows:?}");
    assert_eq!((rows[2]["old"].clone(), rows[2]["new"].clone()), (json!("4500"), json!("5000")));
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), original, "the copy is never replaced");
    assert_eq!(file_quiet(&root), Some(5000), "the file's value is replaced by the stored one");
}

#[test]
fn settings_seeding_copies_only_values_the_registry_accepts() {
    for (file_value, seeded) in [
        (json!(1), true),
        (json!(30000), true),
        (json!(0), false),
        (json!(30001), false),
        (json!("7000"), false),
        (json!(7000.5), false),
        (Value::Null, false),
    ] {
        let (root, context) = settings_fixture("settings seed bounds");
        write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": file_value.clone()}}));
        let (status, body) = set_quiet(&context, json!(4500));
        assert_eq!(status, 200, "{file_value}: {body}");
        let migrated: Vec<Value> = audit_rows(&root).into_iter().filter(|row| row["source"] == "migration").collect();
        if seeded {
            assert_eq!(migrated.len(), 1, "{file_value}: {migrated:?}");
            assert_eq!(migrated[0]["new"], json!(file_value.to_string()), "{file_value}");
            assert_eq!(body["revision"], 2, "{file_value}");
        } else {
            assert!(migrated.is_empty(), "{file_value} is not copied: {migrated:?}");
            assert_eq!(body["revision"], 1, "{file_value}");
        }
        assert_eq!(file_quiet(&root), Some(4500), "{file_value}: the stored value is written back over the file's");
    }
}

#[test]
fn settings_a_set_to_the_seeded_value_keeps_the_migration_and_changes_nothing() {
    let (root, context) = settings_fixture("settings seed same");
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 7000}}));
    let (status, body) = set_quiet(&context, json!(7000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": QUIET_KEY, "value": 7000, "revision": 1, "changed": false}));
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["source"], "migration");
    assert_eq!(quiet_view(&context)["stored"], 7000, "the migration commits although the value did not change");
}

#[test]
fn settings_a_store_cut_off_before_its_first_change_is_seeded_by_the_next_write() {
    // A crash after the schema commit and before the first change leaves a version-1 store with no value.
    let (root, context) = settings_fixture("settings seed cut off");
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 7000}}));
    let original = std::fs::read(root.0.join("config/config.json")).unwrap();
    crate::settings::create_store(&context).unwrap();
    assert!(settings_db(&root).is_file());
    assert!(audit_rows(&root).is_empty());
    assert_eq!(quiet_view(&context)["stored"], Value::Null);
    assert_eq!(next_use_quiet(&context), Some(7000));
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[0]["source"].clone(), rows[0]["new"].clone()), (json!("migration"), json!("7000")));
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), original);
}

#[test]
fn settings_a_release_without_the_store_reads_the_current_values() {
    let (root, context) = settings_fixture("settings older release");
    for value in [4500, 5000] {
        let (status, body) = set_quiet(&context, json!(value));
        assert_eq!(status, 200, "{body}");
        assert_eq!(file_quiet(&root), Some(value), "config.json holds the current value");
    }
    // A release before the store reads config.json alone.
    for name in ["settings.db", "settings.db-wal", "settings.db-shm"] {
        let _ = std::fs::remove_file(root.0.join("data").join(name));
    }
    assert_eq!(next_use_quiet(&context), Some(5000));
}

#[test]
fn settings_a_failed_write_back_keeps_the_change_and_is_finished_later() {
    let (root, context) = settings_fixture("settings write back failure");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(file_quiet(&root), Some(4500));
    // A full disk, or a crash, between the store commit and the config.json write.
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "the stored change stands: {body}");
    assert_eq!((body["revision"].clone(), body["changed"].clone()), (json!(2), json!(true)));
    assert_eq!(body["mirrorError"], "runtime config is unavailable", "{body}");
    assert_eq!(next_use_quiet(&context), Some(5000), "this release reads the store");
    assert_eq!(file_quiet(&root), Some(4500), "config.json still holds the earlier value");
    finish_settings_write_back(&context);
    assert_eq!(file_quiet(&root), Some(5000), "the console start finished the write-back");
    // The next change finishes it too, even one that changes nothing.
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let (status, body) = set_quiet(&context, json!(6000));
    assert_eq!(status, 200, "{body}");
    assert!(body.get("mirrorError").is_some(), "{body}");
    assert_eq!(file_quiet(&root), Some(5000));
    let (status, body) = set_quiet(&context, json!(6000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "setting": QUIET_KEY, "value": 6000, "revision": 3, "changed": false}));
    assert_eq!(file_quiet(&root), Some(6000), "the next settings change finished the write-back");
}

#[test]
fn settings_console_start_writes_back_stored_values_and_creates_nothing_without_a_store() {
    let (root, context) = settings_fixture("settings start no store");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    finish_settings_write_back(&context);
    assert!(!settings_db(&root).exists(), "the start never creates the store");
    assert!(!pre_settings_copy(&root).exists());
    assert!(!root.0.join("data/locks/social-v2.lock").exists(), "no gate is taken without a stored value");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before);

    // A store an earlier build wrote without writing config.json back, and without the copy.
    let (root, context) = settings_fixture("settings start old store");
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 7000}}));
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    write_config(&root, |config| config["socialV2"]["reply"]["wakeQuietMs"] = json!(7000));
    let _ = std::fs::remove_file(pre_settings_copy(&root));
    let unmirrored = std::fs::read(root.0.join("config/config.json")).unwrap();
    let rows = audit_rows(&root).len();
    finish_settings_write_back(&context);
    assert_eq!(file_quiet(&root), Some(4500), "the stored value is written back at start");
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), unmirrored, "the copy is made before the first write-back");
    assert_eq!(audit_rows(&root).len(), rows, "the start writes no audit row");
}

#[test]
fn settings_console_start_waits_for_the_configuration_gate_and_never_writes_without_it() {
    let (root, context) = settings_fixture("settings start gate");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    // Another writer holds the gate briefly: the start waits for it, then writes back.
    write_config(&root, |config| config["socialV2"]["reply"]["wakeQuietMs"] = json!(9000));
    let (held, wait) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let holder = scope.spawn(|| {
            let gate = acquire_social_state_gate(&context).unwrap();
            held.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(gate);
        });
        wait.recv().unwrap();
        finish_settings_write_back(&context);
        holder.join().unwrap();
    });
    assert_eq!(file_quiet(&root), Some(4500), "written back once the gate was free");
    // A gate that stays busy: the start gives up after the gate's wait and writes nothing.
    write_config(&root, |config| config["socialV2"]["reply"]["wakeQuietMs"] = json!(9000));
    let gate = acquire_social_state_gate(&context).unwrap();
    let started = std::time::Instant::now();
    finish_settings_write_back(&context);
    assert!(started.elapsed() >= std::time::Duration::from_secs(5), "the start waited for the gate");
    assert_eq!(file_quiet(&root), Some(9000), "nothing is written without the gate");
    drop(gate);
}

#[test]
fn settings_config_writers_keep_the_stored_values_in_config_json() {
    let (root, context) = settings_fixture("settings config writer");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let mut edited = stored_config(&root);
    edited["socialV2"] = json!({"enabled": true, "reply": {"wakeQuietMs": 9000}});
    edited["dsh"]["model"] = json!("fixture-model-edited");
    let written = write_selected_config_text(&context, &serde_json::to_string_pretty(&edited).unwrap());
    let _ = take_config_writes();
    written.unwrap();
    let file = stored_config(&root);
    assert_eq!(file["socialV2"], json!({"enabled": true, "reply": {"wakeQuietMs": 4500}}), "no writer puts back another value");
    assert_eq!(file["dsh"]["model"], "fixture-model-edited", "the writer's own change is kept");
}

#[test]
fn settings_write_back_leaves_a_non_object_in_place_and_says_so() {
    let (root, context) = settings_fixture("settings write back blocked");
    write_config(&root, |config| config["socialV2"] = json!({"reply": "x"}));
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["mirrorError"], "config.json cannot hold reply.wakeQuietMs", "{body}");
    assert!(audit_rows(&root).iter().all(|row| row["source"] != "migration"), "nothing to copy behind a non-object");
    assert_eq!(stored_config(&root)["socialV2"], json!({"reply": "x"}), "the file's member is never replaced");
    assert_eq!(quiet_view(&context)["stored"], 4500);
}

#[test]
fn settings_refuse_a_change_when_config_json_or_its_copy_cannot_be_used() {
    let (root, context) = settings_fixture("settings copy refused");
    let config_before = std::fs::read(root.0.join("config/config.json")).unwrap();
    std::fs::create_dir(pre_settings_copy(&root)).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "config.json.pre-settings cannot be written");
    assert!(audit_rows(&root).is_empty(), "nothing was stored");
    assert_eq!(next_use_quiet(&context), Some(3000));
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), config_before, "config.json is untouched");
    assert!(pre_settings_copy(&root).is_dir(), "what is there is left alone");

    #[cfg(unix)]
    {
        let (root, context) = settings_fixture("settings copy linked");
        let target = root.0.join("elsewhere.json");
        std::fs::write(&target, b"kept").unwrap();
        std::os::unix::fs::symlink(&target, pre_settings_copy(&root)).unwrap();
        let (status, body) = set_quiet(&context, json!(4500));
        assert_eq!(status, 503, "{body}");
        assert_eq!(body["error"], "config.json.pre-settings cannot be written");
        assert_eq!(std::fs::read(&target).unwrap(), b"kept", "the link target is never written");
    }

    // A config.json the store cannot seed from: nothing is created.
    let (root, context) = settings_fixture("settings config unreadable");
    std::fs::write(root.0.join("config/config.json"), b"{not json").unwrap();
    let applied = crate::settings::apply(&context, QUIET_KEY, &json!(4500), "administrator", "console");
    assert_eq!(applied.err(), Some((503, "runtime config is unavailable".to_string())));
    assert!(!settings_db(&root).exists(), "no store without a readable config.json");
}

#[test]
fn settings_concurrent_console_sets_leave_config_json_on_the_stored_value() {
    let (root, context) = settings_fixture("settings concurrent write back");
    for round in 0..5 {
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| set_quiet(&context, json!(1000 + round)));
            let second = scope.spawn(|| set_quiet(&context, json!(2000 + round)));
            (first.join().unwrap(), second.join().unwrap())
        });
        assert_eq!((first.0, second.0), (200, 200), "{first:?} {second:?}");
        let stored = quiet_view(&context)["stored"].as_i64();
        assert!(stored.is_some(), "round {round}");
        assert_eq!(file_quiet(&root), stored, "round {round}: config.json ends on the stored value");
    }
    assert_eq!(audit_rows(&root).len(), 10);
}

#[test]
fn settings_write_back_keeps_a_large_config_json_whole() {
    let (root, context) = settings_fixture("settings large config");
    let notes = "n".repeat(512 * 1024);
    write_config(&root, |config| config["notes"] = json!(notes));
    let original = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let file = stored_config(&root);
    assert_eq!(file["notes"].as_str().map(str::len), Some(512 * 1024), "every other member is kept");
    assert_eq!(file_quiet(&root), Some(4500));
    assert_eq!(std::fs::read(pre_settings_copy(&root)).unwrap(), original);
}

#[test]
fn settings_console_writer_keeps_the_copy_before_it_mirrors() {
    let (root, context) = settings_fixture("settings console copy");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let _ = std::fs::remove_file(pre_settings_copy(&root));
    std::fs::create_dir(pre_settings_copy(&root)).unwrap();
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let text = json!({"socialV2": {"reply": {"wakeQuietMs": 9000}}}).to_string();
    assert_eq!(
        write_selected_config_text(&context, &text),
        Err("config.json.pre-settings cannot be written".to_string()),
        "a console write that would mirror a stored value is refused without its copy"
    );
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "config.json is untouched");
    assert!(pre_settings_copy(&root).is_dir(), "what is there is left alone");
}

// Seeding and write-back with reset, undo and the chat settings tools (#176 merged with #147 and #149).

#[test]
fn settings_undo_of_a_migration_entry_is_refused_and_revisions_continue_after_the_seed() {
    let (root, context) = settings_fixture("settings undo migration");
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000}}));
    // A change of another setting seeds the quiet time: its migration entry is the quiet time's latest entry.
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "harness.model", "value": "model-a"}));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "");
    let seeded = entries.iter().find(|entry| entry["source"] == "migration").unwrap().clone();
    assert_eq!((seeded["setting"].clone(), seeded["new"].clone()), (json!(QUIET_KEY), json!(9000)));
    // Undoing the copy of config.json would only drop the store's copy of the file's own value: refused.
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": seeded["entry"]}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "this entry cannot be undone");
    assert_eq!(audit_rows(&root).len(), 2, "a refused undo writes nothing");
    assert_eq!(quiet_view(&context)["stored"], 9000);
    assert_eq!(file_quiet(&root), Some(9000), "config.json keeps the owner's value");
    // A change after the seed: undoing it restores the seeded value, in the store and in config.json.
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 2, "the seeded value is revision 1");
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "entry": entries[0]["entry"], "setting": QUIET_KEY, "stored": 9000}));
    assert_eq!(next_use_quiet(&context), Some(9000));
    assert_eq!(file_quiet(&root), Some(9000), "config.json holds the restored value");
    let (status, body) = set_quiet(&context, json!(5000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revision"], 4, "the seed, the change and the undo each took a revision");
}

#[test]
fn settings_undo_that_cannot_remove_the_written_back_value_changes_nothing() {
    let (root, context) = settings_fixture("settings undo removal refused");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "");
    // A full disk, or a crash, before config.json loses the value: the undo is refused and the store keeps it.
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(false));
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], "runtime config is unavailable");
    assert_eq!(quiet_view(&context)["stored"], 4500, "the store keeps the value");
    assert_eq!(audit_rows(&root).len(), 1, "and writes no audit row");
    assert_eq!(file_quiet(&root), Some(4500), "config.json is unchanged");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "the undo goes through once config.json can be written: {body}");
    assert_eq!(next_use_quiet(&context), Some(3000));
    assert_eq!(file_quiet(&root), Some(3000));

    // A crash after the removal and before the commit: config.json lacks the value while the store still holds it.
    // The overlay applies the stored value, and the console's start puts it back into config.json.
    let (root, context) = settings_fixture("settings undo removal cut off");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    write_config(&root, |config| config["socialV2"]["reply"] = json!({}));
    assert_eq!(next_use_quiet(&context), Some(4500), "the store still applies");
    finish_settings_write_back(&context);
    assert_eq!(file_quiet(&root), Some(4500), "the console's start wrote the stored value back");
}

#[test]
fn settings_reset_and_undo_replies_carry_a_failed_write_back() {
    let (root, context) = settings_fixture("settings mirror error replies");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    // A full disk, or a crash, between a reset's commit and its config.json write: the reset stands and says so.
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "the reset stands: {body}");
    assert_eq!((body["value"].clone(), body["changed"].clone()), (json!(3000), json!(true)));
    assert_eq!(body["mirrorError"], "runtime config is unavailable", "{body}");
    assert_eq!(next_use_quiet(&context), Some(3000));
    assert_eq!(file_quiet(&root), Some(4500), "config.json still holds the earlier value");
    finish_settings_write_back(&context);
    assert_eq!(file_quiet(&root), Some(3000), "the console's start finished the write-back");
    // The same for an undo.
    let (entries, _) = history_page(&context, "");
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "the undo stands: {body}");
    assert_eq!(body["stored"], 4500, "{body}");
    assert_eq!(body["mirrorError"], "runtime config is unavailable", "{body}");
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(file_quiet(&root), Some(3000), "config.json still holds the earlier value");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["changed"], false);
    assert!(body.get("mirrorError").is_none(), "{body}");
    assert_eq!(file_quiet(&root), Some(4500), "the next settings change finished the write-back");
}

#[test]
fn chat_settings_set_reply_carries_a_failed_write_back_as_a_fixed_text() {
    let (root, context) = chat_fixture("chat settings mirror error");
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 4500})).unwrap()["value"], 4500);
    // The write-back's own error names files, so the chat gets one fixed text; the change stands.
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 5000})).unwrap();
    assert_eq!((reply["value"].clone(), reply["changed"].clone()), (json!(5000), json!(true)));
    assert_eq!(reply["mirrorError"], SETTINGS_MIRROR_UNFINISHED, "{reply}");
    assert_eq!(reply["note"], format!("下次唤醒时生效；{SETTINGS_MIRROR_UNFINISHED}"), "{reply}");
    assert_eq!(next_use_quiet(&context), Some(5000));
    assert_eq!(file_quiet(&root), Some(4500), "config.json still holds the earlier value");
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let text = chat_text(&context, "settings_set", json!({"settingKey": CHAT_NAME, "value": 6000}));
    assert_eq!(text, format!("已改 {CHAT_NAME} = 6000。下次唤醒时生效；{SETTINGS_MIRROR_UNFINISHED}"));
    assert!(!text.contains("config.json") && !text.contains("runtime config"), "{text}");
    assert!(crate::upstream_capture::denied_terms(&text).is_empty(), "{text}");
    let reply = chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 6000})).unwrap();
    assert_eq!(reply["changed"], false);
    assert!(reply.get("mirrorError").is_none(), "{reply}");
    assert_eq!(file_quiet(&root), Some(6000), "the next settings change finished the write-back");
}

#[test]
fn settings_writers_take_the_configuration_gate_before_the_store_gate() {
    // Gate order: every writer of the store holds the configuration gate (`console::config_gate`: the routes'
    // authority, the chat tools' `memory_actions::handle`) and takes the store gate inside it. settings.rs never takes
    // the configuration gate, so no writer holds the store gate while it waits for the configuration gate.
    let source = include_str!("settings.rs");
    for needle in ["acquire_social_state_gate", "config_gate("] {
        assert!(!source.contains(needle), "settings.rs takes the configuration gate ({needle})");
    }
    let (_root, context) = chat_fixture("settings gate order");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    for writer in ["set", "reset", "undo", "chat"] {
        let (entries, _) = history_page(&context, "");
        let latest = entries[0]["entry"].clone();
        let config_gate = acquire_social_state_gate(&context).unwrap();
        let status = std::thread::scope(|scope| {
            let running = scope.spawn(|| match writer {
                "set" => set_quiet(&context, json!(5000)).0,
                "reset" => post(&context, "/api/settings/reset", json!({"setting": QUIET_KEY})).0,
                "undo" => post(&context, "/api/settings/undo", json!({"entry": latest})).0,
                _ => chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 6000})).map_or(503, |_| 200),
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!running.is_finished(), "{writer} went ahead while another writer held the configuration gate");
            let store_gate = context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock");
            assert!(store_gate.is_ok(), "{writer} holds the store gate while it waits for the configuration gate");
            drop(store_gate);
            drop(config_gate);
            running.join().unwrap()
        });
        assert_eq!(status, 200, "{writer}");
    }
    assert_eq!(next_use_quiet(&context), Some(6000));
}

#[test]
fn audit_undo_keeps_a_stored_value_and_its_write_back() {
    // A whole-file audit undo with a stored value: config.json holds the written-back value, a console write never
    // puts another over it, and the undo keeps it.
    let (root, context) = settings_fixture("settings audit undo stored");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(file_quiet(&root), Some(4500));
    let config_post = |body: Value| {
        let (status, reply) = admin_request(&context, "POST", "/api/socialV2/config", Some(&body));
        assert_eq!(status, 200, "{reply}");
    };
    config_post(json!({"autoReplyCheckMs": 4000}));
    config_post(json!({"reply": {"wakeQuietMs": 8000}, "autoReplyCheckMs": 5000}));
    assert_eq!(file_quiet(&root), Some(4500), "a console config write never puts a value over a stored one");
    let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
    assert_eq!(status, 200, "{recent}");
    let latest = recent["entries"].as_array().unwrap().iter().find(|entry| entry["path"] == "/api/socialV2/config").unwrap()
        ["id"]
        .clone();
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": latest})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["settingsKept"], json!([]), "the backup and the current file both hold the stored value: {reply}");
    assert_eq!(stored_config(&root)["socialV2"]["autoReplyCheckMs"], 4000, "other members are restored");
    assert_eq!(file_quiet(&root), Some(4500), "the stored value stays in config.json");
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(audit_rows(&root).len(), 1, "an audit undo writes nothing to the store");
}

#[test]
fn audit_undo_survives_a_write_back_into_a_null_or_empty_parent() {
    // A null or an empty object on a registry pointer's path reads as absent: the write-back fills it, and that is no
    // later change of the file the audit record describes. Each case runs on its own and fails the test at the end.
    const SOCIAL: usize = 0;
    const SLANG: usize = 1;
    // The console route, its body and the member its write adds, which the undo removes again.
    let consoles: [(&str, Value, &str, Value); 2] = [
        ("/api/socialV2/config", json!({"autoReplyCheckMs": 4000}), "/socialV2/autoReplyCheckMs", json!(4000)),
        ("/api/slang/config", json!({"injectMax": 5}), "/slang/injectMax", json!(5)),
    ];
    // Where the file starts with a null or an empty object, the setting written afterwards and the console route.
    let cases: [(&str, Value, &str, Value, usize); 7] = [
        ("/webSearch", Value::Null, "webSearch.model", json!("model-x"), SOCIAL),
        ("/harness", Value::Null, "harness.model", json!("model-x"), SOCIAL),
        ("/memory", Value::Null, "memory.digest.model", json!("model-x"), SOCIAL),
        ("/socialV2/reply", Value::Null, QUIET_KEY, json!(4500), SOCIAL),
        ("/memory/digest", Value::Null, "memory.digest.model", json!("model-x"), SLANG),
        ("/memory", json!({}), "memory.digest.model", json!("model-x"), SLANG),
        ("/socialV2/reply", Value::Null, QUIET_KEY, json!(4500), SLANG),
    ];
    let mut failures = Vec::new();
    for (at, initial, key, value, console) in cases {
        let (route, body, marker, marker_value) = &consoles[console];
        let label = format!("{at} {initial} then {key} through {route}");
        let (root, context) = settings_fixture("settings audit undo null parent");
        write_config(&root, |config| {
            let mut slot = config;
            for name in at.split('/').skip(1) {
                slot = &mut slot[name];
            }
            *slot = initial.clone();
        });
        let (status, reply) = admin_request(&context, "POST", route, Some(body));
        assert_eq!(status, 200, "{label}: {reply}");
        assert_eq!(stored_config(&root).pointer(at), Some(&initial), "{label}: the console write keeps it");
        let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
        assert_eq!(status, 200, "{recent}");
        let entry = recent["entries"].as_array().unwrap().iter().find(|entry| entry["path"] == *route).unwrap()["id"].clone();
        let (status, reply) = admin_request(&context, "POST", "/api/settings/set", Some(&json!({"setting": key, "value": value})));
        assert_eq!(status, 200, "{label}: {reply}");
        assert!(stored_config(&root).pointer(at).is_some_and(Value::is_object), "{label}: the write-back made an object");
        let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry})));
        if status != 200 {
            failures.push(format!("{label}: undo answered {status}: {reply}"));
        } else if stored_config(&root).pointer(marker) == Some(marker_value) {
            failures.push(format!("{label}: the console member was not restored"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn audit_undo_of_a_console_write_survives_a_later_settings_write_back() {
    // A settings write rewrites config.json with no audit record of its own; the audit undo of an earlier console
    // write must still apply, because registry members belong to the store and are never undone through the audit.
    let (root, context) = settings_fixture("settings audit undo write-back");
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
    config_post(json!({"autoReplyCheckMs": 4000}));
    let entry = latest_config_entry();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(file_quiet(&root), Some(4500), "the write-back put the stored value in config.json");
    assert_eq!(stored_config(&root)["socialV2"]["autoReplyCheckMs"], 4000);
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry})));
    assert_eq!(status, 200, "a registry write-back is no later change of the record's file: {reply}");
    assert_ne!(stored_config(&root)["socialV2"]["autoReplyCheckMs"], 4000, "the console member is restored");
    assert_eq!(file_quiet(&root), Some(4500), "the stored registry value stays in config.json");
    assert_eq!(next_use_quiet(&context), Some(4500));
}

#[test]
fn audit_undo_is_still_refused_after_a_later_non_registry_change() {
    let (root, context) = settings_fixture("settings audit undo later change");
    let config_post = |body: Value| {
        let (status, reply) = admin_request(&context, "POST", "/api/socialV2/config", Some(&body));
        assert_eq!(status, 200, "{reply}");
    };
    config_post(json!({"autoReplyCheckMs": 4000}));
    let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
    assert_eq!(status, 200, "{recent}");
    let entry = recent["entries"].as_array().unwrap().iter().find(|entry| entry["path"] == "/api/socialV2/config").unwrap()
        ["id"]
        .clone();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    config_post(json!({"autoReplyCheckMs": 5000}));
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": entry})));
    assert_eq!(status, 409, "{reply}");
    assert_eq!(stored_config(&root)["socialV2"]["autoReplyCheckMs"], 5000, "a refused undo changes nothing");
    assert_eq!(file_quiet(&root), Some(4500));
}

#[test]
fn settings_every_writer_closes_the_store_without_sidecars() {
    // Each writer reads the stored values for its write-back on its own read-write connection, so that connection's
    // close is the store's last and removes the WAL and its index: no `-wal` or `-shm` is left after any write.
    let (root, context) = chat_fixture("settings writers leave no sidecar");
    write_config(&root, |config| config["socialV2"] = json!({"reply": {"wakeQuietMs": 9000}}));
    let no_sidecars = |writer: &str| {
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!root.0.join(format!("data/settings.db{suffix}")).exists(), "{writer} left settings.db{suffix}");
        }
    };
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    no_sidecars("a set that seeds");
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": QUIET_KEY}));
    assert_eq!(status, 200, "{body}");
    no_sidecars("a reset");
    // The history read is a reader; the undo's own close comes after it.
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    no_sidecars("an undo");
    assert_eq!(chat(&context, "settings-set", json!({"settingKey": CHAT_NAME, "value": 6000})).unwrap()["value"], 6000);
    no_sidecars("a chat set");
    assert_eq!(file_quiet(&root), Some(6000), "the write-back still ran");
}
