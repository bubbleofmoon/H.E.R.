// #154: the security settings (allow and deny lists, owner, outbound guard switches): confirmation on every route that
// changes one, the access change's side effects, undo on the settings page and through the console audit, the old
// routes through the store, the admin agent mode's owner, the seed and the registry.
use super::*;

const SECURITY_KEYS: [&str; 8] = [
    "allow.private",
    "allow.groups",
    "deny.private",
    "deny.groups",
    "ownerQQ",
    "security.interceptNotify",
    "security.outboundGuard",
    "rollout.rustGroups",
];
const KEPT: &str = "group:7001";
const LOST: &str = "group:7002";
const LOST_TOKEN: &str = "fixture-security-token-7002";

/// A reserved2 settings fixture whose config allows groups 7001 and 7002, both with an agent token, and whose
/// `rollout.rustGroups` serves the group kept by the ordinary access tests.
fn security_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    write_config(&root, |config| {
        config["allow"] = json!({"private": ["7001"], "groups": [7001, 7002]});
        config["deny"] = json!({"private": [], "groups": []});
        config["rollout"] = json!({"enabled": true, "rustGroups": [KEPT]});
    });
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    let state = json!({"conversations": {
        KEPT: {"agentToken": "fixture-security-token-7001"},
        LOST: {"agentToken": LOST_TOKEN},
    }});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    (root, context)
}

fn set(context: &RuntimeContext, body: Value) -> (u16, Value) {
    post(context, "/api/settings/set", body)
}

/// The recorded pending revocations.
fn pending_revocations(root: &FixtureRoot) -> Vec<Value> {
    match std::fs::read(root.0.join("data").join(access::PENDING_REVOCATIONS)) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap()["pending"].as_array().cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The id of the newest console audit record of `path`.
fn audit_id(context: &RuntimeContext, path: &str) -> Value {
    let (status, recent) = admin_request(context, "GET", "/api/audit/recent?limit=20", None);
    assert_eq!(status, 200, "{recent}");
    recent["entries"].as_array().unwrap().iter().find(|entry| entry["path"] == path).expect("audited")["id"].clone()
}

fn confirmation_error(key: &str) -> Value {
    json!(format!("{key} is a security setting: send the change again with \"confirm\": true"))
}

#[test]
fn settings_security_change_needs_confirmation() {
    let (root, context) = security_fixture("settings security confirm");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, body) = set(&context, json!({"setting": "security.outboundGuard", "value": false}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], confirmation_error("security.outboundGuard"));
    let (status, body) = set(&context, json!({"setting": "security.outboundGuard", "value": false, "confirm": false}));
    assert_eq!(status, 409, "{body}");
    let (status, body) = set(&context, json!({"setting": "security.outboundGuard", "value": false, "confirm": "yes"}));
    assert_eq!((status, body["error"].clone()), (400, json!("confirm must be true or false")), "{body}");
    for key in SECURITY_KEYS {
        let (status, body) = post(&context, "/api/settings/reset", json!({"setting": key}));
        assert_eq!((status, body["error"].clone()), (409, confirmation_error(key)), "{key}: {body}");
    }
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing is written");
    assert!(!settings_db(&root).exists(), "no store is created");
    let (status, body) = set(&context, json!({"setting": "security.outboundGuard", "value": false, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert!(body.get("revoked").is_none(), "a switch is no access change: {body}");
    assert!(!crate::pipeline::outbound_guard_enabled(&context.load_config().unwrap()), "the next reply reads it");
    assert_eq!(stored_config(&root)["security"]["outboundGuard"], false, "written back");
    // A setting outside the security group takes `confirm` and needs none.
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set(&context, json!({"setting": QUIET_KEY, "value": 4600, "confirm": true}));
    assert_eq!(status, 200, "{body}");
}

#[test]
fn settings_access_change_revokes_like_a_whitelist_write_and_can_be_undone() {
    let (root, context) = security_fixture("settings access change");
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], json!([7001]));
    assert_eq!(body["revoked"], json!([LOST]), "{body}");
    assert_ne!(conversation_token(&context, LOST).unwrap().as_deref(), Some(LOST_TOKEN), "the lost conversation's token is rotated");
    assert!(!inbound_admitted(&context.load_config().unwrap(), LOST), "the next event is refused");
    assert!(inbound_admitted(&context.load_config().unwrap(), KEPT));
    assert!(pending_revocations(&root).is_empty(), "no revocation is left pending");
    assert_eq!(stored_config(&root)["allow"]["groups"], json!([7001]), "written back");
    // The undo needs confirmation too, and restores the list without bringing the old token back.
    let (entries, _) = history_page(&context, "?setting=allow.groups");
    let entry = entries[0]["entry"].clone();
    let rotated = conversation_token(&context, LOST).unwrap();
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entry}));
    assert_eq!((status, body["error"].clone()), (409, confirmation_error("allow.groups")), "{body}");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entry, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revoked"], json!([]), "a re-grant revokes nothing: {body}");
    assert_eq!(setting_view(&context, "allow.groups")["value"], json!([7001, 7002]));
    assert!(inbound_admitted(&context.load_config().unwrap(), LOST), "the undo grants access again");
    assert_eq!(conversation_token(&context, LOST).unwrap(), rotated, "the undo grants a new epoch only");
    // Refused values change nothing.
    for refused in [json!([0]), json!(["x"]), json!("7001"), json!((1..=10_001u64).collect::<Vec<_>>())] {
        let (status, body) = set(&context, json!({"setting": "allow.groups", "value": refused, "confirm": true}));
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"], "allow.groups must be a list of at most 10000 QQ ids (positive integers)");
    }
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": (1..=10_000u64).collect::<Vec<_>>(), "confirm": true}));
    assert_eq!(status, 200, "the largest list is accepted: {}", body["error"]);
}

#[test]
fn settings_old_access_routes_need_confirmation_and_store_their_change() {
    let (root, context) = security_fixture("settings old routes");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, body) = post(&context, "/api/whitelist", json!({"allow": {"groups": [7001]}}));
    assert_eq!((status, body["error"].clone()), (409, confirmation_error("allow.groups")), "{body}");
    let (status, body) = post(&context, "/api/security", json!({"interceptNotify": false}));
    assert_eq!((status, body["error"].clone()), (409, confirmation_error("security.interceptNotify")), "{body}");
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": LOST, "action": "remove"}));
    assert_eq!((status, body["error"].clone()), (409, confirmation_error("allow.groups")), "{body}");
    let (status, body) = post(&context, "/api/whitelist", json!({"ownerQQ": "abc"}));
    assert_eq!(status, 400, "{body}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "a refused request writes nothing");
    assert_eq!(conversation_token(&context, LOST).unwrap().as_deref(), Some(LOST_TOKEN));
    // A request that changes no security setting needs no confirmation.
    let (status, body) = post(&context, "/api/whitelist", json!({"allow": {"groups": [7001, 7002]}}));
    assert_eq!(status, 200, "{body}");
    // Confirmed, each route stores its change and keeps its side effects.
    let (status, body) = post(&context, "/api/whitelist", json!({"allow": {"groups": [7001]}, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["allow"]["groups"], json!([7001]));
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001]), "the whitelist write is stored");
    assert_ne!(conversation_token(&context, LOST).unwrap().as_deref(), Some(LOST_TOKEN), "the revocation still runs");
    let (status, body) = admin_request(&context, "GET", "/api/whitelist", None);
    assert_eq!(status, 200, "{body}");
    let shown: Vec<String> =
        body["allow"]["groups"].as_array().unwrap().iter().map(|id| id.to_string().trim_matches('"').to_string()).collect();
    assert_eq!(shown, ["7001"], "the whitelist view sees the stored change");
    let (status, body) = post(&context, "/api/security", json!({"interceptNotify": false, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert!(body["security"].get("confirm").is_none(), "confirm is no security member: {body}");
    assert_eq!(setting_view(&context, "security.interceptNotify")["stored"], false);
    let (status, body) = post(&context, "/api/security", json!({"outboundGuard": "off", "confirm": true}));
    assert_eq!((status, body["error"].clone()), (400, json!("security.outboundGuard must be true or false")), "{body}");
    let file = stored_config(&root);
    assert_eq!(file["allow"]["groups"], json!([7001]), "written back");
    assert_eq!(file["security"], json!({"interceptNotify": false}), "written back, and confirm is not written");
    // The command line confirms with its own `--confirm`.
    let reply = conversation_scope_change(&context, KEPT, "remove").expect("the command line confirms");
    assert_eq!(reply["changed"], json!(["allow.groups", "rollout.rustGroups"]), "{reply}");
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([]));
}

#[test]
fn settings_owner_yields_to_the_admin_agent_owner_while_that_mode_is_on() {
    let (root, context) = security_fixture("settings owner closed agent");
    let (status, body) = set(&context, json!({"setting": "ownerQQ", "value": " 424242 ", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], 424242, "a digit string is taken as its number");
    assert_eq!(context.load_config().unwrap()["ownerQQ"], 424242);
    assert!(setting_view(&context, "ownerQQ").get("overriddenBy").is_none());
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"closed-agent"}"#).unwrap();
    context.write_atomic(RuntimeArea::Data, "closed-agent-owner.json", br#"{"ownerQQ":515151}"#).unwrap();
    assert_eq!(context.load_config().unwrap()["ownerQQ"], 515151, "the admin agent's owner wins while its mode is on");
    let view = setting_view(&context, "ownerQQ");
    assert_eq!(view["value"], 424242, "the setting keeps its own value");
    assert_eq!(view["overriddenBy"], json!({"source": "admin agent mode", "value": 515151}));
    // A list-only whitelist write never copies that owner into the setting.
    let (status, body) = post(&context, "/api/whitelist", json!({"allow": {"groups": [7001]}, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_config(&root)["ownerQQ"], 424242);
    let (status, body) = post(&context, "/api/settings/reset", json!({"setting": "ownerQQ", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["value"], Value::Null, "a reset is no owner");
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    assert_eq!(context.load_config().unwrap().get("ownerQQ").cloned().unwrap_or(Value::Null), Value::Null, "no owner after the mode ends");
    let (status, body) = set(&context, json!({"setting": "ownerQQ", "value": null, "confirm": true}));
    assert_eq!(status, 400, "{body}");
}

#[test]
fn settings_security_settings_are_registered_and_never_chat_writable() {
    let keys: Vec<&str> =
        crate::settings::registry().filter(|setting| setting.group == "security").map(|setting| setting.key).collect();
    assert_eq!(keys, SECURITY_KEYS);
    for setting in crate::settings::registry().filter(|setting| setting.group == "security") {
        assert!(!setting.chat, "{}", setting.key);
        assert!(crate::settings::chat_setting(&setting.chat_name()).is_none(), "{}", setting.key);
    }
    // The owner's chat can neither change nor list them.
    let (root, context) = chat_fixture("settings security chat");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    for (name, value) in [("allow.groups", json!([1])), ("allow.private", json!([1])), ("deny.groups", json!([])), ("ownerQQ", json!(1)), ("security.outboundGuard", json!(false)), ("security.interceptNotify", json!(false))] {
        let refused = chat(&context, "settings-set", json!({"settingKey": name, "value": value}));
        assert!(refused.is_err(), "{name}: {refused:?}");
    }
    let all = chat(&context, "settings-get", json!({})).unwrap();
    for name in SECURITY_KEYS {
        assert!(!all["writable"].as_array().unwrap().iter().any(|writable| writable == name), "{name}: {all}");
    }
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "the chat wrote nothing");
    assert!(!settings_db(&root).exists());
}

#[test]
fn settings_a_write_of_another_setting_never_takes_the_access_lists_over() {
    let (root, context) = security_fixture("settings security seed");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let seeded: Vec<Value> = audit_rows(&root)
        .into_iter()
        .filter(|row| row["key"] != "rollout.rustGroups" && SECURITY_KEYS.iter().any(|key| row["key"] == *key))
        .collect();
    assert!(seeded.is_empty(), "other security settings seed on a write of themselves: {seeded:?}");
    // So a hand edit of a list still applies until the list is first changed on the console.
    write_config(&root, |config| config["allow"]["groups"] = json!([7001]));
    assert!(!inbound_admitted(&context.load_config().unwrap(), LOST));
}

#[test]
fn settings_first_change_seeds_the_list_from_its_alias_spelling_and_undoes_to_it() {
    let (root, context) = security_fixture("settings seed alias");
    write_config(&root, |config| {
        config["allow"] = json!({"private": ["7001"], "group": ["7003", "x"]});
        config["rollout"]["rustGroups"] = json!([]);
    });
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": [7004], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "?setting=allow.groups");
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!((entries[1]["source"].clone(), entries[1]["new"].clone()), (json!("migration"), json!([7003])), "seeded through the alias, usable ids only");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert!(inbound_admitted(&context.load_config().unwrap(), "group:7003"), "the undo restores the file's list");
    assert!(!inbound_admitted(&context.load_config().unwrap(), "group:7004"));
    // A file list the store cannot keep refuses the first change, so its undo can never lose it.
    let (root, context) = security_fixture("settings seed over limit");
    write_config(&root, |config| config["deny"]["groups"] = json!((1..=10_001u64).collect::<Vec<_>>()));
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, body) = set(&context, json!({"setting": "deny.groups", "value": [1], "confirm": true}));
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().starts_with("config.json holds a value of deny.groups the settings store cannot keep"), "{body}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing is written");
    assert!(pending_revocations(&root).is_empty(), "the refused change leaves nothing pending");
    assert!(audit_rows(&root).iter().all(|row| row["key"] != "deny.groups"), "nothing is stored");
}

#[test]
fn settings_audit_undo_restores_security_settings_through_the_store_and_needs_confirmation() {
    let (_root, context) = security_fixture("settings audit undo security");
    let (status, body) = post(&context, "/api/whitelist", json!({"allow": {"groups": [7001]}, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let id = audit_id(&context, "/api/whitelist");
    let (status, reply) = post(&context, "/api/audit/undo", json!({"id": id}));
    assert_eq!((status, reply["error"].clone()), (409, confirmation_error("allow.groups")), "{reply}");
    let (status, reply) = post(&context, "/api/audit/undo", json!({"id": id, "confirm": true}));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001, 7002]), "restored through the store");
    assert!(!reply["settingsKept"].as_array().unwrap().iter().any(|key| key == "allow.groups"), "{reply}");
    assert!(inbound_admitted(&context.load_config().unwrap(), LOST));
    // A later change of the list on the settings page makes an older record stale.
    let (status, body) = post(&context, "/api/security", json!({"interceptNotify": false, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let id = audit_id(&context, "/api/security");
    let (status, body) = set(&context, json!({"setting": "security.interceptNotify", "value": true, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let (status, reply) = post(&context, "/api/audit/undo", json!({"id": id, "confirm": true}));
    assert_eq!(status, 409, "{reply}");
    assert_eq!(setting_view(&context, "security.interceptNotify")["value"], true);
}

#[test]
fn settings_access_change_takes_the_configuration_gate_before_the_store_gate() {
    let (root, context) = security_fixture("settings access gate order");
    let config = root.0.join("config/config.json");
    let before = std::fs::read(&config).unwrap();
    let config_gate = acquire_social_state_gate(&context).unwrap();
    let (status, body) = std::thread::scope(|scope| {
        let running = scope.spawn(|| set(&context, json!({"setting": "allow.groups", "value": [7001], "confirm": true})));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!running.is_finished(), "the access change went ahead while another writer held the configuration gate");
        // The write-back of the store's commit is the change's own configuration write: none may happen under another
        // writer's gate. (The revocation after the commit also waits for that gate, so `is_finished` alone cannot see it.)
        assert_eq!(std::fs::read(&config).unwrap(), before, "the access change wrote config.json while another writer held the configuration gate");
        let store_gate = context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock");
        assert!(store_gate.is_ok(), "the access change holds the store gate while it waits for the configuration gate");
        drop(store_gate);
        drop(config_gate);
        running.join().unwrap()
    });
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revoked"], json!([LOST]), "{body}");
}

#[test]
fn settings_access_change_that_fails_before_its_commit_leaves_nothing_pending() {
    let (root, context) = security_fixture("settings access store failure");
    // The first write seeds the store, so the failing write below finds it.
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(false));
    assert_eq!(status, 503, "{body}");
    assert!(pending_revocations(&root).is_empty(), "a change that was never stored leaves nothing pending");
    assert_eq!(conversation_token(&context, LOST).unwrap().as_deref(), Some(LOST_TOKEN), "nothing is revoked");
    assert!(inbound_admitted(&context.load_config().unwrap(), LOST));
    // A held interaction gate refuses the change before anything is written.
    let held = crate::closed_agent::lock(&context).unwrap();
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
    drop(held);
    assert_eq!(status, 503, "{body}");
    assert!(inbound_admitted(&context.load_config().unwrap(), LOST));
    assert!(pending_revocations(&root).is_empty());
}

/// Group 7003, which the lists below grant and then lose.
const EXTRA: &str = "group:7003";
const EXTRA_TOKEN: &str = "fixture-security-token-7003";

#[test]
fn settings_security_undo_whose_first_look_fails_is_refused_without_confirm_and_revokes_with_it() {
    let (root, context) = security_fixture("settings undo failed look");
    let state = json!({"conversations": {
        KEPT: {"agentToken": "fixture-security-token-7001"},
        LOST: {"agentToken": LOST_TOKEN},
        EXTRA: {"agentToken": EXTRA_TOKEN},
    }});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": [7001, 7002, 7003], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "?setting=allow.groups");
    let entry = entries[0]["entry"].clone();
    // The store cannot be read for the first look: which setting the entry changes is unknown, so no confirmation means
    // no undo.
    crate::settings::FAIL_UNDO_LOOK.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entry}));
    assert_eq!(status, 503, "an unconfirmed undo is refused when its entry cannot be looked at: {body}");
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001, 7002, 7003]), "nothing is undone");
    assert_eq!(conversation_token(&context, EXTRA).unwrap().as_deref(), Some(EXTRA_TOKEN));
    assert!(pending_revocations(&root).is_empty());
    // Confirmed, it takes the access path, which looks again under the configuration gate, and revokes.
    crate::settings::FAIL_UNDO_LOOK.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entry, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revoked"], json!([EXTRA]), "{body}");
    assert_ne!(conversation_token(&context, EXTRA).unwrap().as_deref(), Some(EXTRA_TOKEN), "the lost conversation's token is rotated");
    assert!(!inbound_admitted(&context.load_config().unwrap(), EXTRA));
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001, 7002]));
    assert!(pending_revocations(&root).is_empty());
}

#[test]
fn settings_whitelist_write_that_names_the_owner_null_or_blank_clears_it_under_confirmation() {
    for (label, owner) in [("null", Value::Null), ("blank", json!(" "))] {
        let (root, context) = security_fixture(&format!("settings whitelist clears owner {label}"));
        let before = std::fs::read(root.0.join("config/config.json")).unwrap();
        let (status, body) = post(&context, "/api/whitelist", json!({"ownerQQ": owner}));
        assert_eq!((status, body["error"].clone()), (409, confirmation_error("ownerQQ")), "{label}: {body}");
        assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "{label}: nothing is written");
        assert_eq!(context.load_config().unwrap()["ownerQQ"], 7001, "{label}");
        let (status, body) = post(&context, "/api/whitelist", json!({"ownerQQ": owner, "confirm": true}));
        assert_eq!(status, 200, "{label}: {body}");
        assert_eq!(body["ownerQQ"], Value::Null, "{label}: {body}");
        assert_eq!(stored_config(&root)["ownerQQ"], Value::Null, "{label}: the file is cleared");
        assert_eq!(context.load_config().unwrap().get("ownerQQ").cloned().unwrap_or(Value::Null), Value::Null, "{label}: no owner is in effect");
        let rows: Vec<Value> = audit_rows(&root).into_iter().filter(|row| row["key"] == "ownerQQ" && row["source"] == "console").collect();
        assert_eq!(rows.len(), 1, "{label}: {rows:?}");
        assert_eq!(rows[0]["new"], json!("null"), "{label}: the store holds no owner");
        assert_eq!(setting_view(&context, "ownerQQ")["value"], Value::Null, "{label}");
        assert!(pending_revocations(&root).is_empty(), "{label}");
        // Clearing an owner that is cleared already changes nothing.
        let (status, body) = post(&context, "/api/whitelist", json!({"ownerQQ": owner}));
        assert_eq!(status, 200, "{label}: {body}");
    }
}

#[test]
fn settings_id_lists_compare_ids_as_the_runtime_does() {
    let (root, context) = security_fixture("settings id list leading zero");
    write_config(&root, |config| config["deny"]["private"] = json!(["07001"]));
    // The runtime compares strings: "07001" denies no conversation, so the list holds no usable id.
    assert_eq!(setting_view(&context, "deny.private")["value"], json!([]));
    assert!(inbound_admitted(&context.load_config().unwrap(), "private:7001"), "07001 is not 7001 to the runtime");
    let (status, body) = set(&context, json!({"setting": "deny.private", "value": ["07001"], "confirm": true}));
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "deny.private must be a list of at most 10000 QQ ids (positive integers)");
    // Denying 7001 changes the list from what it reads, and the file then holds 7001.
    let (status, body) = post(&context, "/api/whitelist", json!({"deny": {"private": [7001]}, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(setting_view(&context, "deny.private")["stored"], json!([7001]));
    assert_eq!(stored_config(&root)["deny"]["private"], json!([7001]), "written back");
    assert!(!inbound_admitted(&context.load_config().unwrap(), "private:7001"), "7001 is denied");
}

#[test]
fn settings_scope_change_works_on_an_allow_list_with_unusable_items() {
    let (root, context) = security_fixture("settings scope unusable items");
    write_config(&root, |config| {
        config["allow"]["groups"] = json!([7001, 7002, "7003a", 0, 9_007_199_254_740_992u64]);
        config["rollout"]["rustGroups"] = json!([KEPT, LOST]);
    });
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": LOST, "action": "remove", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revoked"], json!([LOST]), "{body}");
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001]));
    assert_eq!(stored_config(&root)["allow"]["groups"], json!([7001]), "the unusable items are dropped");
    assert_ne!(conversation_token(&context, LOST).unwrap().as_deref(), Some(LOST_TOKEN), "the lost conversation's token is rotated");
    assert!(!inbound_admitted(&context.load_config().unwrap(), LOST));
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": LOST, "action": "admit", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001, 7002]));
}

#[test]
fn settings_undo_of_an_entry_missing_at_the_first_look_never_undoes_a_security_setting_unconfirmed() {
    let (_root, context) = security_fixture("settings undo entry created meanwhile");
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": [7001, 7002, 7003], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "");
    let unknown = entries[0]["entry"].as_i64().unwrap() + 1;
    // Another administrator's confirmed change of the list becomes that entry after the undo's first look.
    let other = context.clone();
    crate::console_settings::AFTER_UNDO_LOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let (status, body) = set(&other, json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
            assert_eq!(status, 200, "{body}");
        }));
    });
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": unknown}));
    assert_eq!(status, 409, "a security setting the first look did not name is not undone by the plain path: {body}");
    assert_eq!(body["error"], "allow.groups is a security setting: send the undo again");
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001]), "the entry made meanwhile is not undone unconfirmed");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": unknown + 100}));
    assert_eq!(status, 404, "an entry that never exists is still 404: {body}");
}

#[test]
fn settings_scope_admit_is_denied_only_by_an_id_the_deny_list_reads() {
    let (root, context) = security_fixture("settings scope deny leading zero");
    write_config(&root, |config| config["deny"]["groups"] = json!(["07004"]));
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": "group:7004", "action": "admit", "confirm": true}));
    assert_eq!(status, 200, "07004 denies nothing the runtime matches: {body}");
    assert_eq!(setting_view(&context, "allow.groups")["stored"], json!([7001, 7002, 7004]));
    assert!(inbound_admitted(&context.load_config().unwrap(), "group:7004"));
    // A usable id of the deny list still refuses the admit.
    write_config(&root, |config| config["deny"]["groups"] = json!([7005]));
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": "group:7005", "action": "admit", "confirm": true}));
    assert_eq!(status, 409, "{body}");
}

#[test]
fn rollout_seed_only_copies_a_valid_subset_of_effective_allow_lists() {
    let (root, context) = settings_fixture("rollout seed");
    write_config(&root, |cfg| {
        cfg["allow"] = json!({"groups": [7001], "privates": [" 7002 "]});
        cfg["deny"] = json!({"private": [7002]});
        cfg.as_object_mut().unwrap().remove("rollout");
    });
    let (status, body) = set(&context, json!({"setting": "allow.groups", "value": [7001, 7003], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    for (index, legacy) in [Value::Null, json!(["group:07001"]), json!(["group:7004"]), json!(["private:7003"])]
        .into_iter()
        .enumerate()
    {
        // The raw file would admit 7004, but the store's allow.groups wins.
        write_config(&root, |cfg| {
            cfg["allow"]["groups"] = json!([7004]);
            cfg["rollout"] = json!({"rustGroups": legacy.clone()});
        });
        let (status, body) = set_quiet(&context, json!(4500 + index));
        assert_eq!(status, 200, "{body}");
        assert!(history_page(&context, "?setting=rollout.rustGroups").0.is_empty(), "a legacy rollout gap must stay file-backed");
        assert_eq!(context.load_config().unwrap()["rollout"]["rustGroups"], legacy);
    }
    let valid = json!(["private:7002", "group:7003"]);
    write_config(&root, |cfg| {
        cfg["allow"]["groups"] = json!([7004]);
        cfg["rollout"]["rustGroups"] = valid.clone();
    });
    let (status, body) = set_quiet(&context, json!(4600));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, "?setting=rollout.rustGroups");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["source"], "migration");
    assert_eq!(entries[0]["new"], valid, "valid rollout seeds through stored allow and aliases even when denied");
}

#[test]
fn rollout_subset_refusals_keep_values_history_and_revocations_unchanged() {
    let (root, context) = settings_fixture("rollout refused writes");
    write_config(&root, |cfg| {
        cfg["allow"] = json!({"groups": [7001], "privates": [7002]});
        cfg["rollout"] = json!({"rustGroups": ["group:7001", "private:7002"]});
    });
    let history = history_page(&context, "").0;
    let file = stored_config(&root);
    let effective = context.load_config().unwrap();
    for (path, body, allow) in [
        ("/api/settings/set", json!({"setting": "allow.groups", "value": [], "confirm": true}), "allow.groups"),
        ("/api/settings/reset", json!({"setting": "allow.private", "confirm": true}), "allow.private"),
        ("/api/settings/set", json!({"setting": "rollout.rustGroups", "value": ["private:7001"], "confirm": true}), "allow.private"),
        ("/api/whitelist", json!({"allow": {"groups": [], "private": [7002]}, "confirm": true}), "allow.groups"),
    ] {
        let (status, reply) = post(&context, path, body);
        assert_eq!(status, 409, "a scope write must refuse a rollout outside its matching allow list: {path}: {reply}");
        assert!(reply["error"].as_str().unwrap().contains(allow), "{reply}");
        assert_eq!(history_page(&context, "").0, history, "a refusal rolls the seed and audit back");
        assert_eq!(stored_config(&root), file, "a refused proposal is never mirrored");
        assert_eq!(context.load_config().unwrap(), effective, "both effective lists stand");
        assert!(pending_revocations(&root).is_empty());
    }

    let (status, reply) = post(&context, "/api/settings/reset", json!({"setting": "rollout.rustGroups", "confirm": true}));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(stored_config(&root)["rollout"]["rustGroups"], json!([]), "reset mirrors an explicit empty list");
    let (entries, _) = history_page(&context, "?setting=rollout.rustGroups");
    let reset = &entries[0]["entry"];
    let (status, reply) = set(&context, json!({"setting": "allow.groups", "value": [], "confirm": true}));
    assert_eq!(status, 200, "{reply}");
    let history = history_page(&context, "").0;
    let (status, reply) = post(&context, "/api/settings/undo", json!({"entry": reset, "confirm": true}));
    assert_eq!(status, 409, "undo must validate the restored rollout against current allow lists: {reply}");
    assert_eq!(history_page(&context, "").0, history);
    assert_eq!(context.load_config().unwrap()["rollout"]["rustGroups"], json!([]));
}

#[test]
fn rollout_writes_preserve_legacy_gaps_outside_the_changed_allow_kind() {
    let (root, context) = settings_fixture("rollout inherited gaps");
    write_config(&root, |cfg| {
        cfg["allow"] = json!({"groups": [7001, 7002], "private": [7004, 7005]});
        cfg["rollout"] = json!({"rustGroups": ["group:7001", "private:7003"]});
    });
    let (status, reply) = set(&context, json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
    assert_eq!(status, 200, "a group change can leave an inherited private gap in place: {reply}");
    let (status, reply) = set(&context, json!({"setting": "allow.private", "value": [7005, 7004], "confirm": true}));
    assert_eq!(status, 200, "reordering allow without changing its membership keeps the inherited gap: {reply}");
    let (status, reply) = set(&context, json!({"setting": "allow.private", "value": [7002], "confirm": true}));
    assert_eq!(status, 409, "a private membership change must repair the private gap: {reply}");
    let (status, reply) = set(&context, json!({"setting": "rollout.rustGroups", "value": ["private:7003", "group:7001"], "confirm": true}));
    assert_eq!(status, 409, "a rollout change checks both allow lists: {reply}");
    let (status, reply) = post(&context, "/api/conversations/scope", json!({"conversation": "private:7003", "action": "admit", "confirm": true}));
    assert_eq!(status, 200, "the scope action repairs the inherited gap: {reply}");
    assert_eq!(reply["scope"]["notAllowed"], json!([]));
}
