// Reset, undo and the history: the routes, their refusals and their gate.
use super::*;

/// One request straight to `console_settings::route`, past the console front.
fn direct_route(context: &RuntimeContext, method: &str, path: &str, body: &str) -> (u16, Value) {
    let (status, _, reply) =
        crate::console_settings::route(crate::console_front::AdminCaller::new(context), method, path, &HashMap::new(), body);
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
    let store = StoreProbe::at(&root.0);
    store.set_value("reply.wakeQuietMs", "50000");
    store.insert_audit(&[
        AuditRow {
            at_ms: 1,
            actor: "older release",
            source: "console",
            scope: "global",
            key: "reply.retired",
            old: Some("not json".to_string()),
            new: Some("1".to_string()),
            request: "retired-1".to_string(),
        },
        AuditRow {
            at_ms: 2,
            actor: "older release",
            source: "console",
            scope: "conversation:group:1",
            key: "reply.wakeQuietMs",
            old: None,
            new: Some("7".to_string()),
            request: "scoped-1".to_string(),
        },
    ]);
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
    let legacy = RuntimeContext::legacy();
    for (method, path, body) in [
        ("GET", "/api/settings/history", ""),
        ("POST", "/api/settings/reset", r#"{"setting":"reply.wakeQuietMs"}"#),
        ("POST", "/api/settings/undo", r#"{"entry":1}"#),
    ] {
        let (status, reply) = direct_route(&legacy, method, path, body);
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
    let revoked = crate::console_front::revoked_admin_context(&context);
    for (path, body) in [("/api/settings/reset", r#"{"setting":"reply.wakeQuietMs"}"#), ("/api/settings/undo", r#"{"entry":1}"#)] {
        let (status, reply) = direct_route(&revoked, "POST", path, body);
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
