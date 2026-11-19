// The console's writers of `config.json` with registry settings: its one write path, its configuration pages (#177),
// the audit undo of a whole file, and the gate order of every writer of the store.
use super::*;

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
    // A save without the member keeps the stored value in config.json. The store then changes the value (a settings write
    // this record does not hold), and the undo keeps the value the store holds now.
    config_post(json!({"reply": {}, "autoReplyCheckMs": 6000}));
    assert_eq!(file()["socialV2"]["reply"]["wakeQuietMs"], 8000, "the stored value stays in config.json");
    let (status, body) = set_quiet(&context, json!(9000));
    assert_eq!(status, 200, "{body}");
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": latest_config_entry()})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["settingsKept"], json!([QUIET_KEY]), "{reply}");
    assert_eq!(file()["socialV2"]["reply"]["wakeQuietMs"], 9000, "the registry setting keeps its current value");
    assert_eq!(file()["socialV2"]["autoReplyCheckMs"], 4000, "other members are restored");
    // An undo whose backup holds the current registry values keeps nothing.
    config_post(json!({"autoReplyCheckMs": 6000}));
    let (status, reply) = admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": latest_config_entry()})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["settingsKept"], json!([]), "{reply}");
    assert_eq!(file()["socialV2"]["autoReplyCheckMs"], 4000);
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

#[test]
fn settings_config_writers_keep_the_stored_values_in_config_json() {
    let (root, context) = settings_fixture("settings config writer");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let mut edited = stored_config(&root);
    edited["socialV2"] = json!({"enabled": true});
    edited["dsh"]["model"] = json!("fixture-model-edited");
    let written = write_selected_config_text(&context, &serde_json::to_string_pretty(&edited).unwrap());
    let _ = take_config_writes();
    written.unwrap();
    let file = stored_config(&root);
    assert_eq!(
        file["socialV2"],
        json!({"enabled": true, "reply": {"wakeQuietMs": 4500}}),
        "a writer without the member keeps the stored value"
    );
    assert_eq!(file["dsh"]["model"], "fixture-model-edited", "the writer's own change is kept");
    // #177: a writer that changes the member in the file itself is refused and writes nothing.
    edited["socialV2"] = json!({"enabled": true, "reply": {"wakeQuietMs": 9000}});
    edited["dsh"]["model"] = json!("fixture-model-other");
    let written = write_selected_config_text(&context, &serde_json::to_string_pretty(&edited).unwrap());
    let _ = take_config_writes();
    assert_eq!(written, Err("reply.wakeQuietMs changes only through the settings store".to_string()));
    assert_eq!(stored_config(&root), file, "nothing is written");
}

#[test]
fn settings_console_writer_keeps_the_copy_before_it_mirrors() {
    let (root, context) = settings_fixture("settings console copy");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let _ = std::fs::remove_file(pre_settings_copy(&root));
    std::fs::create_dir(pre_settings_copy(&root)).unwrap();
    // config.json lacks the stored value (a write-back that did not finish), so this write must put it in.
    write_config(&root, |config| config["socialV2"]["reply"] = json!({}));
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let text = json!({"socialV2": {"reply": {}}}).to_string();
    assert_eq!(
        write_selected_config_text(&context, &text),
        Err("config.json.pre-settings cannot be written".to_string()),
        "a console write that would mirror a stored value is refused without its copy"
    );
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "config.json is untouched");
    assert!(pre_settings_copy(&root).is_dir(), "what is there is left alone");
}

#[test]
fn settings_writers_take_the_configuration_gate_before_the_store_gate() {
    // Gate order: every writer of the store holds the configuration gate (`console::config_gate`: the routes'
    // authority, the chat tools' `memory_actions::handle`) and takes the store gate inside it. The settings module
    // never takes the configuration gate, so no writer holds the store gate while it waits for the configuration gate.
    for (file, source) in SETTINGS_SOURCES {
        for needle in ["acquire_social_state_gate", "config_gate("] {
            assert!(!source.contains(needle), "{file} takes the configuration gate ({needle})");
        }
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
    config_post(json!({"reply": {}, "autoReplyCheckMs": 5000}));
    assert_eq!(file_quiet(&root), Some(4500), "a console config write without the member keeps the stored value");
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
fn settings_console_config_page_changes_a_registry_setting_through_the_store() {
    let (root, context) = settings_fixture("settings console config write");
    let config_post = |body: Value| admin_request(&context, "POST", "/api/socialV2/config", Some(&body));
    // Without a store: the page's value is stored with one audit row, written back, and read on the next use.
    let (status, reply) = config_post(json!({"reply": {"wakeQuietMs": 4500}, "autoReplyCheckMs": 4000}));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(quiet_view(&context)["stored"], 4500, "the page's value is in the store");
    assert_eq!(next_use_quiet(&context), Some(4500), "the next use reads the page's value");
    assert_eq!(file_quiet(&root), Some(4500), "written back into config.json");
    assert_eq!(stored_config(&root)["socialV2"]["autoReplyCheckMs"], 4000, "the page's other members are written");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0]["actor"].clone(), rows[0]["source"].clone(), rows[0]["key"].clone()),
        (json!("administrator"), json!("console"), json!(QUIET_KEY))
    );
    assert_eq!((rows[0]["old"].clone(), rows[0]["new"].clone()), (Value::Null, json!("4500")));
    // Over a stored value: the page's new value replaces it.
    let (status, reply) = config_post(json!({"reply": {"wakeQuietMs": 8000}}));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(next_use_quiet(&context), Some(8000));
    assert_eq!(file_quiet(&root), Some(8000));
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[1]["old"].clone(), rows[1]["new"].clone()), (json!("4500"), json!("8000")));
    // The stored value again, or a save without the member or with it null, is no change: no audit row.
    for body in [
        json!({"reply": {"wakeQuietMs": 8000}}),
        json!({"autoReplyCheckMs": 5000}),
        json!({"reply": {}}),
        json!({"reply": null}),
        json!({"reply": {"wakeQuietMs": null}}),
    ] {
        let (status, reply) = config_post(body.clone());
        assert_eq!(status, 200, "{body}: {reply}");
        assert_eq!(next_use_quiet(&context), Some(8000), "{body}");
        assert_eq!(file_quiet(&root), Some(8000), "{body}");
    }
    assert_eq!(audit_rows(&root).len(), 2, "no change, no audit row");
    // The page's change is undone on the settings page like any other.
    let (entries, _) = history_page(&context, "");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_use_quiet(&context), Some(4500));
    assert_eq!(file_quiet(&root), Some(4500));
}

#[test]
fn settings_console_config_page_refuses_an_invalid_registry_value_and_writes_nothing() {
    let (root, context) = settings_fixture("settings console config refused");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    for value in [json!(0), json!(30001), json!("4500"), json!(4500.5), json!(true), json!([4500])] {
        let body = json!({"reply": {"wakeQuietMs": value}, "autoReplyCheckMs": 4000});
        let (status, reply) = admin_request(&context, "POST", "/api/socialV2/config", Some(&body));
        assert_eq!(status, 400, "{value}: {reply}");
        assert_eq!(reply["error"], "reply.wakeQuietMs must be an integer from 1 to 30000", "{value}");
        assert_eq!(
            std::fs::read(root.0.join("config/config.json")).unwrap(),
            before,
            "{value}: nothing is written, the page's other members included"
        );
    }
    assert!(!settings_db(&root).exists(), "a refusal creates no store");
    // The default is no change: nothing is stored and the file gets no member.
    let (status, reply) = admin_request(&context, "POST", "/api/socialV2/config", Some(&json!({"reply": {"wakeQuietMs": 3000}})));
    assert_eq!(status, 200, "{reply}");
    assert!(!settings_db(&root).exists(), "what the next use already reads is no change");
    assert_eq!(
        stored_config(&root).pointer("/socialV2/reply/wakeQuietMs"),
        None,
        "the file gets no registry member from the page"
    );
    // A store that cannot take the change: 503 and nothing written, the page's other members included.
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    std::fs::create_dir(pre_settings_copy(&root)).unwrap();
    let body = json!({"reply": {"wakeQuietMs": 4500}, "autoReplyCheckMs": 4000});
    let (status, reply) = admin_request(&context, "POST", "/api/socialV2/config", Some(&body));
    assert_eq!(status, 503, "{reply}");
    assert_eq!(reply["error"], "config.json.pre-settings cannot be written");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing is written");
    assert_eq!(next_use_quiet(&context), Some(3000));
}

#[test]
fn settings_console_config_page_takes_the_configuration_gate_before_the_store_gate() {
    let (root, context) = settings_fixture("settings console config gate order");
    let config_gate = acquire_social_state_gate(&context).unwrap();
    let status = std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            admin_request(&context, "POST", "/api/socialV2/config", Some(&json!({"reply": {"wakeQuietMs": 7000}}))).0
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!running.is_finished(), "the page went ahead while another writer held the configuration gate");
        assert!(!settings_db(&root).exists(), "the page wrote the store before it held the configuration gate");
        let store_gate = context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock");
        assert!(store_gate.is_ok(), "the page holds the store gate while it waits for the configuration gate");
        drop(store_gate);
        drop(config_gate);
        running.join().unwrap()
    });
    assert_eq!(status, 200);
    assert_eq!(next_use_quiet(&context), Some(7000));
}

#[test]
fn settings_no_console_writer_changes_a_registry_setting_in_config_json_itself() {
    // Every registry setting, through the console's one config.json write path: a document that changes its member is
    // refused and nothing is written; the same document with the member as the file holds it is written.
    let probes = [
        json!(1),
        json!(2),
        json!(7),
        // #152: an integer range that starts at 1000.
        json!(5000),
        json!("06:30"),
        json!("Asia/Shanghai"),
        json!("probe-model-a"),
        json!("probe-model-b"),
        json!("plain probe text one"),
        json!("plain probe text two"),
        // #154: an id list and a switch.
        json!([1]),
        json!(false),
        // #218: a choice.
        json!("harness"),
    ];
    for setting in crate::settings::registry() {
        let (root, context) = settings_fixture("settings direct write refused");
        let now = setting.read(&context.load_config_document().unwrap());
        let value = probes
            .iter()
            .find(|probe| setting.normalize(probe).is_some_and(|value| value != now))
            .unwrap_or_else(|| panic!("{}: no probe value", setting.key))
            .clone();
        let before = std::fs::read(root.0.join("config/config.json")).unwrap();
        let mut edited = stored_config(&root);
        let mut slot = &mut edited;
        for name in setting.pointer.split('/').skip(1) {
            slot = &mut slot[name];
        }
        *slot = value;
        let written = write_selected_config_text(&context, &serde_json::to_string_pretty(&edited).unwrap());
        let _ = take_config_writes();
        assert_eq!(written, Err(format!("{} changes only through the settings store", setting.key)));
        assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "{}: untouched", setting.key);
        assert!(!settings_db(&root).exists(), "{}", setting.key);
        let mut unchanged = stored_config(&root);
        unchanged["notes"] = json!("console writer");
        let written = write_selected_config_text(&context, &serde_json::to_string_pretty(&unchanged).unwrap());
        let _ = take_config_writes();
        assert_eq!(written, Ok(()), "{}", setting.key);
    }
    // That write path is the only product write of config.json in the console and the modules it calls.
    let console = include_str!("../../console.rs");
    // Split on the test module's header only (no line ending in the pattern, so a CRLF checkout splits the same).
    let product = console.split("mod tests {").next().unwrap();
    assert_eq!(product.matches("write_atomic(RuntimeArea::Config").count(), 1, "console.rs writes config.json once");
    for (name, source) in [
        ("console_access.rs", include_str!("../../console_access.rs")),
        ("console_agent.rs", include_str!("../../console_agent.rs")),
        ("console_feedback.rs", include_str!("../../console_feedback.rs")),
        ("console_persona.rs", include_str!("../../console_persona.rs")),
        ("console_reply.rs", include_str!("../../console_reply.rs")),
        ("console_schedule.rs", include_str!("../../console_schedule.rs")),
        ("console_send_guard.rs", include_str!("../../console_send_guard.rs")),
        ("console_settings.rs", include_str!("../../console_settings.rs")),
        ("console_tasks.rs", include_str!("../../console_tasks.rs")),
        ("console_voice.rs", include_str!("../../console_voice.rs")),
        ("memory_actions.rs", include_str!("../../memory_actions.rs")),
    ] {
        assert!(!source.contains("write_atomic(RuntimeArea::Config"), "{name} writes config.json itself");
    }
}

#[test]
fn settings_console_config_page_takes_the_posted_value_when_the_file_lags_the_store() {
    let (root, context) = settings_fixture("settings console config lagging file");
    let (status, body) = set_quiet(&context, json!(8000));
    assert_eq!(status, 200, "{body}");
    // config.json lags the store (an unfinished write-back, or a hand edit): the page's value differs from the value in effect.
    write_config(&root, |config| config["socialV2"]["reply"]["wakeQuietMs"] = json!(4500));
    let (status, reply) =
        admin_request(&context, "POST", "/api/socialV2/config", Some(&json!({"reply": {"wakeQuietMs": 4500}})));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(quiet_view(&context)["stored"], 4500, "the posted value replaces the stored one");
    assert_eq!(next_use_quiet(&context), Some(4500), "the next use reads the posted value");
    assert_eq!(file_quiet(&root), Some(4500), "written back into config.json");
}

#[test]
fn settings_console_config_page_refuses_a_non_canonical_file_before_the_store_takes_a_change() {
    let (root, context) = settings_fixture("settings console config non canonical");
    write_config(&root, |config| config["ownerQQ"] = json!(true));
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    // The router's front check refuses such a file before any console handler runs, so the handler is called directly:
    // its own check must refuse before the store takes the change.
    let reply = handle_social_config_post(&context, &json!({"reply": {"wakeQuietMs": 4500}}).to_string());
    assert_eq!(reply, Err((503, "runtime config is invalid".to_string())));
    assert!(!settings_db(&root).exists(), "nothing is stored");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing is written");
}

#[test]
fn settings_console_config_page_saves_without_a_registry_change_on_a_non_canonical_file() {
    let (root, context) = settings_fixture("settings console config non canonical plain save");
    write_config(&root, |config| config["ownerQQ"] = json!(true));
    // Called directly, as above: the handler runs no canonical check of its own when no registry setting changes.
    let reply = handle_social_config_post(&context, &json!({"autoReplyCheckMs": 5000}).to_string());
    assert!(reply.is_ok(), "{reply:?}");
    assert_eq!(stored_config(&root)["socialV2"]["autoReplyCheckMs"], 5000);
    assert!(!settings_db(&root).exists(), "no registry change, no store");
}
