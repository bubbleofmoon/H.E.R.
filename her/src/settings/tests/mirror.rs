// Seeding from `config.json` and writing the stored values back into it (#176), with reset, undo and the console's
// start.
use super::*;

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
    let version = StoreProbe::at(&root.0).user_version();
    assert_eq!(version, 1, "seeding keeps schema version 1, which the previous release opens");

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
