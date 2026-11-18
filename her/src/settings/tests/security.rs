// #154: the security settings (allow and deny lists, owner, outbound guard switches): confirmation on every route that
// changes one, the access change's side effects, undo on the settings page and through the console audit, the old
// routes through the store, the admin agent mode's owner, the seed and the registry.
use super::*;

const SECURITY_KEYS: [&str; 7] = [
    "allow.private",
    "allow.groups",
    "deny.private",
    "deny.groups",
    "ownerQQ",
    "security.interceptNotify",
    "security.outboundGuard",
];
const KEPT: &str = "group:7001";
const LOST: &str = "group:7002";
const LOST_TOKEN: &str = "fixture-security-token-7002";

/// A reserved2 settings fixture whose config allows groups 7001 and 7002, both with an agent token, and whose
/// `rollout.rustGroups` serves both.
fn security_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    write_config(&root, |config| {
        config["allow"] = json!({"private": ["7001"], "groups": [7001, 7002]});
        config["deny"] = json!({"private": [], "groups": []});
        config["rollout"] = json!({"enabled": true, "rustGroups": [KEPT, LOST]});
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
    let seeded: Vec<Value> = audit_rows(&root).into_iter().filter(|row| SECURITY_KEYS.iter().any(|key| row["key"] == *key)).collect();
    assert!(seeded.is_empty(), "a security setting is seeded only by a write of itself: {seeded:?}");
    // So a hand edit of a list still applies until the list is first changed on the console.
    write_config(&root, |config| config["allow"]["groups"] = json!([7001]));
    assert!(!inbound_admitted(&context.load_config().unwrap(), LOST));
}

#[test]
fn settings_first_change_seeds_the_list_from_its_alias_spelling_and_undoes_to_it() {
    let (root, context) = security_fixture("settings seed alias");
    write_config(&root, |config| config["allow"] = json!({"private": ["7001"], "group": ["7003", "x"]}));
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
