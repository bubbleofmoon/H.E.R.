// #218: settings kept per conversation (schema version 2) and the global default they fall back to; the turn engine as
// the first of each, stored but not read to route turns yet; their console page and routes; the migration from version
// 1; the clear of a conversation's value when it loses access; and the cases the plan's threats name.
use super::*;
use crate::social_state::inbound_admitted;

const ENGINE: &str = "conversation.engine";
const ENGINE_DEFAULT_KEY: &str = "engine.default";
/// #162: the model route kept per conversation.
const ROUTE: &str = "conversation.route";
const NOT_ADMITTED: &str = "conversation is not admitted; admit it first";
const CONVERSATION_REFUSED: &str = "conversation must be group:<id> or private:<id>";
const ENGINE_REFUSED: &str = "conversation.engine must be one of dsh, harness";
const CHAT_WRITE_REFUSED_TEXT: &str = "禁止在对话中更改此项（仅允许调整 settings_get 列出的配置；白名单、访问令牌及路径类须本地修改）";
const KEPT_GROUP: &str = "group:7001";
const LOST_GROUP: &str = "group:7002";

fn set_engine(context: &RuntimeContext, conversation: &str, value: Value) -> (u16, Value) {
    post(context, "/api/settings/conversation/set", json!({"conversation": conversation, "setting": ENGINE, "value": value}))
}

fn reset_engine(context: &RuntimeContext, conversation: &str) -> (u16, Value) {
    post(context, "/api/settings/conversation/reset", json!({"conversation": conversation, "setting": ENGINE}))
}

/// The engine the next turn of `conversation` runs on today: the turn engine's reader on the loaded configuration.
fn next_turn_engine(context: &RuntimeContext, conversation: &str) -> Result<&'static str, String> {
    crate::turn_engine::Engine::selected(context, &context.load_config().unwrap(), conversation, "chat").map(crate::turn_engine::Engine::id)
}

/// `GET /api/settings/conversations`.
fn conversations_view(context: &RuntimeContext) -> Value {
    let (status, body) = admin_request(context, "GET", "/api/settings/conversations", None);
    assert_eq!(status, 200, "{body}");
    body
}

/// The `conversation` entry of `GET /api/settings/conversations`, `None` when it is not listed.
fn conversation_entry(context: &RuntimeContext, conversation: &str) -> Option<Value> {
    conversations_view(context)["conversations"].as_array().unwrap().iter().find(|entry| entry["conversation"] == conversation).cloned()
}

/// The stored conversations of `conversation_settings`, in order.
fn stored_conversations(root: &FixtureRoot) -> Vec<String> {
    StoreProbe::at(&root.0).conversation_rows().into_iter().map(|row| row.0).collect()
}

/// Sets `allow.groups` in `config.json` by hand (the fixture admits only the owner's private chat, `private:7001`).
fn admit_groups(root: &FixtureRoot, groups: Value) {
    write_config(root, |config| config["allow"]["groups"] = groups);
}

/// A reserved2 settings fixture whose config allows groups 7001 and 7002 (both served by `rollout.rustGroups`, both
/// with an agent token) and the owner's private chat 7001.
fn access_fixture(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, context) = settings_fixture(label);
    write_config(&root, |config| {
        config["allow"] = json!({"private": ["7001"], "groups": [7001, 7002]});
        config["deny"] = json!({"private": [], "groups": []});
        config["rollout"] = json!({"enabled": true, "rustGroups": [KEPT_GROUP, LOST_GROUP]});
    });
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"reserved2"}"#).unwrap();
    let state = json!({"conversations": {
        KEPT_GROUP: {"agentToken": "fixture-conversation-token-7001"},
        LOST_GROUP: {"agentToken": "fixture-conversation-token-7002"},
    }});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    (root, context)
}

#[test]
fn conversation_settings_set_is_stored_for_that_conversation_only_and_routes_nothing() {
    let (root, context) = settings_fixture("conversation settings stored");
    admit_groups(&root, json!([101]));
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"ok": true, "conversation": OWNER_CHAT, "setting": ENGINE, "value": "harness", "revision": 1, "changed": true})
    );
    let owner = conversation_entry(&context, OWNER_CHAT).expect("the owner's private chat is listed");
    assert_eq!(owner["settings"][ENGINE], json!({"stored": "harness", "value": "harness"}), "the stored choice is listed");
    let group = conversation_entry(&context, "group:101").expect("an admitted group is listed");
    assert_eq!(group["settings"][ENGINE], json!({"stored": null, "value": "dsh"}), "another conversation keeps the default");
    assert_eq!(next_turn_engine(&context, OWNER_CHAT), Ok("harness"), "the conversation choice wins");
    assert_eq!(owner["engineToday"], "harness", "the page shows the next engine choice");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "a conversation's value is never written into config.json");
    let rows = audit_rows(&root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0]["actor"].clone(), rows[0]["source"].clone()), (json!("administrator"), json!("console")));
    assert_eq!((rows[0]["scope"].clone(), rows[0]["key"].clone()), (json!(OWNER_CHAT), json!(ENGINE)), "the audit scope is the conversation");
    assert_eq!((rows[0]["old"].clone(), rows[0]["new"].clone()), (Value::Null, json!("\"harness\"")));
    assert_eq!(
        StoreProbe::at(&root.0).conversation_rows(),
        vec![(OWNER_CHAT.to_string(), ENGINE.to_string(), "\"harness\"".to_string(), 1, "administrator".to_string())]
    );
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!((status, body["revision"].clone(), body["changed"].clone()), (200, json!(1), json!(false)), "{body}");
    assert_eq!(audit_rows(&root).len(), 1, "the same value again is not a change");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("dsh"));
    assert_eq!((status, body["revision"].clone()), (200, json!(2)), "{body}");
    assert_eq!(conversation_entry(&context, OWNER_CHAT).unwrap()["settings"][ENGINE]["stored"], "dsh");
}

#[test]
fn conversation_settings_global_default_applies_to_conversations_without_their_own_choice() {
    let (root, context) = settings_fixture("conversation settings global default");
    admit_groups(&root, json!([101]));
    let global = setting_view(&context, ENGINE_DEFAULT_KEY);
    assert_eq!(
        (global["scope"].clone(), global["applies"].clone(), global["default"].clone(), global["kind"].clone()),
        (json!("global"), json!("next wake"), json!("dsh"), json!({"type": "choice", "options": ["dsh", "harness"]}))
    );
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": ENGINE_DEFAULT_KEY, "value": "harness"}));
    assert_eq!(status, 200, "{body}");
    let entry = conversation_entry(&context, "group:101").unwrap();
    assert_eq!(entry["settings"][ENGINE], json!({"stored": null, "value": "harness"}), "a conversation without a choice uses the global default");
    assert_eq!(next_turn_engine(&context, "group:101"), Ok("harness"), "admission reads the global default");
    let (status, body) = set_engine(&context, "group:101", json!("dsh"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(conversation_entry(&context, "group:101").unwrap()["settings"][ENGINE]["value"], "dsh", "its own choice wins");
    assert_eq!(conversation_entry(&context, OWNER_CHAT).unwrap()["settings"][ENGINE]["value"], "harness");
    // A reset stores the default for that conversation, so the global setting no longer decides it.
    let (status, body) = reset_engine(&context, OWNER_CHAT);
    assert_eq!((status, body["value"].clone(), body["changed"].clone()), (200, json!("dsh"), json!(true)), "{body}");
    assert_eq!(conversation_entry(&context, OWNER_CHAT).unwrap()["settings"][ENGINE], json!({"stored": "dsh", "value": "dsh"}));
    // The global default is written back and audited globally.
    assert_eq!(stored_config(&root)["engineDefault"], "harness");
    assert!(stored_config(&root).get("conversationEngines").is_none(), "no conversation's value reaches config.json");
    assert_eq!(next_turn_engine(&context, "group:101"), Ok("dsh"), "the scoped choice takes precedence");
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": ENGINE_DEFAULT_KEY, "value": "other"}));
    assert_eq!((status, body["error"].clone()), (400, json!("engine.default must be one of dsh, harness")));
}

#[test]
fn conversation_settings_global_writes_keep_a_version_one_store() {
    let (root, context) = settings_fixture("conversation settings v1 stays");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let store = StoreProbe::at(&root.0);
    assert_eq!(
        store.user_version(),
        1,
        "a store that never held a conversation's value stays at version 1, which the previous release opens"
    );
    assert_eq!(store.objects_named("conversation_settings"), 0, "and has no part of version 2");
    let entry = conversation_entry(&context, OWNER_CHAT).expect("the owner's private chat is listed");
    assert_eq!(entry["settings"][ENGINE], json!({"stored": null, "value": "dsh"}), "a version-1 store is readable");
    // An entry of a conversation scope in a version-1 store (a hand edit or a later release's row) cannot be undone.
    let id = store.insert_audit(&[AuditRow {
        at_ms: 1,
        actor: "test",
        source: "console",
        scope: "private:7001",
        key: ENGINE,
        old: None,
        new: Some("\"harness\"".to_string()),
        request: "v1-scoped".to_string(),
    }]);
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": id}));
    assert_eq!((status, body["error"].clone()), (409, json!("this entry cannot be undone")), "{body}");
    assert_eq!(store.user_version(), 1, "an undo never migrates the store");
}

#[test]
fn conversation_settings_first_conversation_write_migrates_a_version_one_store_once() {
    let (root, context) = settings_fixture("conversation settings migrate");
    for value in [4500, 5000] {
        let (status, body) = set_quiet(&context, json!(value));
        assert_eq!(status, 200, "{body}");
    }
    let store = StoreProbe::at(&root.0);
    let global_rows = audit_rows(&root);
    assert_eq!((store.user_version(), global_rows.len()), (1, 2), "global writes leave version 1");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(store.user_version(), 2, "the first conversation write migrates the store");
    assert_eq!(store.objects_named("conversation_settings"), 1);
    assert_eq!(&audit_rows(&root)[..2], &global_rows[..], "the audit history is kept as it was");
    assert_eq!(next_use_quiet(&context), Some(5000), "the global values are kept");
    let (entries, _) = history_page(&context, "");
    let latest_global = entries.iter().find(|entry| entry["scope"] == "global").unwrap()["entry"].clone();
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": latest_global}));
    assert_eq!(status, 200, "a global entry from before the migration is still undone: {body}");
    assert_eq!(next_use_quiet(&context), Some(4500));
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("dsh"));
    assert_eq!((status, body["revision"].clone()), (200, json!(2)), "{body}");
    let (status, body) = set_quiet(&context, json!(6000));
    assert_eq!(status, 200, "{body}");
    assert_eq!(store.user_version(), 2, "a global write keeps version 2");
    assert_eq!(next_use_quiet(&context), Some(6000));
}

#[test]
fn conversation_settings_migration_cut_off_leaves_version_one() {
    // A crash between creating `conversation_settings` and the commit: the migration is part of the write's transaction.
    let (root, context) = settings_fixture("conversation settings migration cut off");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let rows_before = audit_rows(&root);
    crate::settings::FAIL_MIGRATION.with(|fail| fail.set(true));
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    crate::settings::FAIL_MIGRATION.with(|fail| fail.set(false));
    assert_eq!(status, 503, "a migration cut off midway refuses the write: {body}");
    let store = StoreProbe::at(&root.0);
    assert_eq!(store.user_version(), 1, "a migration cut off midway leaves version 1");
    assert_eq!(store.objects_named("conversation_settings"), 0, "and no part of version 2");
    assert_eq!(audit_rows(&root), rows_before, "and no audit row");
    assert_eq!(next_use_quiet(&context), Some(4500), "the store still applies");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "the next write migrates: {body}");
    assert_eq!(store.user_version(), 2);
}

#[test]
fn conversation_settings_store_refuses_a_version_two_store_that_is_not_exactly_its_schema() {
    for (label, edit) in [
        ("extra index", "CREATE INDEX conversation_by_value ON conversation_settings (value);"),
        ("no table", "DROP TABLE conversation_settings;"),
    ] {
        let (root, context) = settings_fixture(&format!("conversation settings v2 {label}"));
        let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
        assert_eq!(status, 200, "{label}: {body}");
        StoreProbe::at(&root.0).edit(edit);
        let before = std::fs::read(settings_db(&root)).unwrap();
        let (status, body) = set_engine(&context, OWNER_CHAT, json!("dsh"));
        assert_eq!((status, body["error"].clone()), (503, json!("settings storage is unavailable")), "{label}: refused");
        let (status, body) = set_quiet(&context, json!(4500));
        assert_eq!(status, 503, "{label}: a global write is refused too: {body}");
        assert_eq!(std::fs::read(settings_db(&root)).unwrap(), before, "{label}: a refused store is never touched");
        let body = conversations_view(&context);
        assert_eq!(body["storeError"], "settings storage is unavailable", "{label}");
    }
    // A newer release's store (version 4) is refused as newer by a conversation write too, and never migrated down.
    let (root, context) = settings_fixture("conversation settings v4");
    StoreProbe::at(&root.0).build("CREATE TABLE later (x INTEGER); PRAGMA user_version=4;");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!((status, body["error"].clone()), (503, json!(crate::settings::NEWER_SCHEMA_ERROR)), "{body}");
    assert_eq!(StoreProbe::at(&root.0).user_version(), 4, "a newer store is never migrated down");
}

#[test]
fn conversation_settings_skip_rows_with_a_non_canonical_conversation() {
    let (root, context) = settings_fixture("conversation settings rows");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "{body}");
    let store = StoreProbe::at(&root.0);
    for (conversation, key, text) in [
        ("group:1/x", ENGINE, "\"harness\""),
        ("group:~1", ENGINE, "\"harness\""),
        ("private:07001", ENGINE, "\"harness\""),
        ("", ENGINE, "\"harness\""),
        ("/conversationEngines", ENGINE, "\"harness\""),
        ("group:5", "conversation.retired", "\"harness\""),
        ("group:6", ENGINE, "\"other\""),
        ("group:7", ENGINE, "not json"),
    ] {
        store.insert_conversation_value(conversation, key, text);
    }
    let listed: Vec<Value> =
        conversations_view(&context)["conversations"].as_array().unwrap().iter().map(|entry| entry["conversation"].clone()).collect();
    assert_eq!(listed, vec![json!(OWNER_CHAT)], "only the canonical, accepted row is listed");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let after = stored_config(&root);
    assert!(after.get("conversationEngines").is_none() && after.get("group:1").is_none(), "no row reaches config.json: {after}");
    assert_ne!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "the global write-back still runs");
    for conversation in ["group:1/x", "group:~0", "group:~1"] {
        let (status, body) = set_engine(&context, conversation, json!("harness"));
        assert_eq!((status, body["error"].clone()), (400, json!(CONVERSATION_REFUSED)), "{conversation}");
        let (status, body) = admin_request(&context, "GET", &format!("/api/settings/history?conversation={conversation}"), None);
        assert_eq!((status, body["error"].clone()), (400, json!(CONVERSATION_REFUSED)), "history {conversation}");
    }
    let id = store.insert_audit(&[AuditRow {
        at_ms: 1,
        actor: "test",
        source: "console",
        scope: "group:1/x",
        key: ENGINE,
        old: None,
        new: Some("\"harness\"".to_string()),
        request: "odd-scope".to_string(),
    }]);
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": id}));
    assert_eq!((status, body["error"].clone()), (409, json!("this entry cannot be undone")), "{body}");
}

#[test]
fn conversation_settings_refuse_malformed_unknown_and_unadmitted_requests() {
    let owner_set = r#"{"conversation":"private:7001","setting":"conversation.engine","value":"harness"}"#;
    let owner_reset = r#"{"conversation":"private:7001","setting":"conversation.engine"}"#;
    let (root, context) = settings_fixture("conversation settings refusals");
    admit_groups(&root, json!([101]));
    let revoked = crate::console_front::revoked_admin_context(&context);
    for (path, body) in [("/api/settings/conversation/set", owner_set), ("/api/settings/conversation/reset", owner_reset)] {
        let (status, _, reply, _) = crate::console_settings::route(
            crate::console_front::AdminCaller::new(&revoked),
            "POST",
            path,
            &HashMap::new(),
            body,
        );
        assert_eq!(status, 401, "{path}: the authority re-checks the credential before anything is written: {reply}");
    }
    let mut requests = Vec::new();
    for conversation in [json!(null), json!(7001), json!(""), json!("group:"), json!("group:abc"), json!("friend:1"),
        json!("group:1/x"), json!("group:~0"), json!("group:0"), json!("group:-1"), json!("group:1.5")]
    {
        requests.push((json!({"conversation": conversation, "setting": ENGINE, "value": "harness"}), 400, CONVERSATION_REFUSED));
    }
    requests.extend([
        (json!({"setting": ENGINE, "value": "harness"}), 400, CONVERSATION_REFUSED),
        (json!({"conversation": OWNER_CHAT, "setting": "conversation.other", "value": "harness"}), 400, "unknown setting"),
        (json!({"conversation": OWNER_CHAT, "setting": ENGINE_DEFAULT_KEY, "value": "harness"}), 400, "unknown setting"),
        (json!({"conversation": OWNER_CHAT, "value": "harness"}), 400, "setting is required"),
        (json!({"conversation": OWNER_CHAT, "setting": ENGINE}), 400, "value is required"),
        (json!({"conversation": OWNER_CHAT, "setting": ENGINE, "value": "harness", "pad": 1}), 400, "unknown settings field"),
        (json!({"conversation": "group:999", "setting": ENGINE, "value": "harness"}), 409, NOT_ADMITTED),
        (json!({"conversation": "group:999", "setting": ENGINE, "value": "other"}), 409, NOT_ADMITTED),
    ]);
    for value in [json!("other"), json!("DSH"), json!(" dsh"), json!(null), json!(1), json!(true), json!(["harness"]), json!({"engine": "harness"})] {
        requests.push((json!({"conversation": "group:101", "setting": ENGINE, "value": value}), 400, ENGINE_REFUSED));
    }
    for (request, status, error) in requests {
        let (got, body) = post(&context, "/api/settings/conversation/set", request.clone());
        assert_eq!((got, body["error"].clone()), (status, json!(error)), "{request}");
    }
    let (status, body) = reset_engine(&context, "group:999");
    assert_eq!((status, body["error"].clone()), (409, json!(NOT_ADMITTED)), "a reset of a conversation that is not admitted");
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": ENGINE, "value": "harness"}));
    assert_eq!((status, body["error"].clone()), (400, json!("unknown setting")), "the global set route takes no conversation setting");
    let (status, body) = post(
        &context,
        "/api/settings/conversation/set",
        json!({"conversation": OWNER_CHAT, "setting": ENGINE, "value": "harness", "pad": "x".repeat(1_000_001)}),
    );
    assert_eq!(status, 413, "{body}");
    for (method, path, body) in [
        ("GET", "/api/settings/conversations", None),
        ("POST", "/api/settings/conversation/set", Some(json!({"conversation": OWNER_CHAT, "setting": ENGINE, "value": "harness"}))),
        ("POST", "/api/settings/conversation/reset", Some(json!({"conversation": OWNER_CHAT, "setting": ENGINE}))),
    ] {
        let (status, reply) = agent_request(&context, method, path, body.as_ref(), "fixture-agent-token");
        assert_eq!(status, 403, "{path}: {reply}");
    }
    assert!(!settings_db(&root).exists(), "no refusal created the store");
    // A conversation key is canonicalized, as the persona routes do.
    let (status, body) = set_engine(&context, "group:0101", json!("harness"));
    assert_eq!((status, body["conversation"].clone()), (200, json!("group:101")), "{body}");
    assert_eq!(stored_conversations(&root), vec!["group:101".to_string()]);
    assert_eq!(audit_rows(&root).len(), 1, "no refusal wrote an audit row");
}

#[test]
fn conversation_settings_removing_a_conversation_from_its_allow_list_clears_its_choice() {
    let (root, context) = access_fixture("conversation settings removal");
    // Scope removes the served group. Whitelist can then remove the other allowed group.
    write_config(&root, |config| config["rollout"]["rustGroups"] = json!([LOST_GROUP]));
    for conversation in [KEPT_GROUP, LOST_GROUP, OWNER_CHAT] {
        let (status, body) = set_engine(&context, conversation, json!("harness"));
        assert_eq!(status, 200, "{conversation}: {body}");
    }
    // The scope route: removing the group clears its choice in the access change's own transaction.
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": LOST_GROUP, "action": "remove", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        stored_conversations(&root),
        vec![KEPT_GROUP.to_string(), OWNER_CHAT.to_string()],
        "the removed conversation's choice is cleared"
    );
    let rows = audit_rows(&root);
    let cleared = rows.iter().find(|row| row["scope"] == LOST_GROUP && row["source"] == "access").expect("the clear is audited").clone();
    assert_eq!(
        (cleared["actor"].clone(), cleared["key"].clone(), cleared["old"].clone(), cleared["new"].clone()),
        (json!("system"), json!(ENGINE), json!("\"harness\""), Value::Null)
    );
    let (entries, _) = history_page(&context, &format!("?conversation={LOST_GROUP}"));
    assert_eq!(entries[0]["source"], "access");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!((status, body["error"].clone()), (409, json!("this entry cannot be undone")), "a clear is never undone: {body}");
    // The whitelist route clears the same way; the owner's private chat stays the owner's whatever the lists say.
    let (status, body) = post(&context, "/api/whitelist", json!({"allow": {"groups": [], "private": []}, "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_conversations(&root), vec![OWNER_CHAT.to_string()], "every conversation that lost access is cleared");
    // Admitted again, a conversation has no choice until one is set.
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": LOST_GROUP, "action": "admit", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(conversation_entry(&context, LOST_GROUP).unwrap()["settings"][ENGINE]["stored"], Value::Null);
}

#[test]
fn conversation_settings_access_change_on_the_settings_page_clears_too_and_keeps_the_configured_owner() {
    let (root, context) = access_fixture("conversation settings access page");
    write_config(&root, |config| {
        config["allow"]["private"] = json!([]);
        config["rollout"]["rustGroups"] = json!([KEPT_GROUP]);
    });
    for conversation in [KEPT_GROUP, LOST_GROUP, OWNER_CHAT] {
        let (status, body) = set_engine(&context, conversation, json!("harness"));
        assert_eq!(status, 200, "{conversation}: {body}");
    }
    // The admin agent mode puts another owner in force; the configured owner's chat still keeps its choice.
    context.write_atomic(RuntimeArea::Data, "mode.json", br#"{"mode":"closed-agent"}"#).unwrap();
    context.write_atomic(RuntimeArea::Data, "closed-agent-owner.json", br#"{"ownerQQ":515151}"#).unwrap();
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        stored_conversations(&root),
        vec![KEPT_GROUP.to_string(), OWNER_CHAT.to_string()],
        "a settings-page access change clears the lost conversation and keeps the configured owner's chat"
    );
    // A change that leaves every conversation admitted clears nothing, and a non-access change never looks.
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "allow.groups", "value": [7001, 7003], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_conversations(&root), vec![KEPT_GROUP.to_string(), OWNER_CHAT.to_string()]);
    assert_eq!(audit_rows(&root).iter().filter(|row| row["source"] == "access").count(), 1, "one clear, one audit row");
}

#[test]
fn conversation_settings_clear_is_atomic_with_its_access_change() {
    let (root, context) = access_fixture("conversation settings clear atomic");
    write_config(&root, |config| config["rollout"]["rustGroups"] = json!([KEPT_GROUP]));
    let (status, body) = set_engine(&context, LOST_GROUP, json!("harness"));
    assert_eq!(status, 200, "{body}");
    let rows_before = audit_rows(&root);
    crate::settings::FAIL_AFTER_CLEAR.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
    crate::settings::FAIL_AFTER_CLEAR.with(|fail| fail.set(false));
    assert_eq!(status, 503, "an access change whose clear fails is refused whole: {body}");
    assert_eq!(stored_conversations(&root), vec![LOST_GROUP.to_string()], "the choice stays when the access change does not commit");
    assert_eq!(audit_rows(&root), rows_before, "and no audit row is written");
    assert!(inbound_admitted(&context.load_config().unwrap(), LOST_GROUP), "the access change did not happen either");
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "allow.groups", "value": [7001], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert!(stored_conversations(&root).is_empty(), "the next access change clears it");
}

#[test]
fn conversation_settings_a_hand_edited_allow_list_leaves_values_listed_until_the_next_access_change() {
    // Hundreds of stored values of conversations a hand edit removed: listed as not admitted, then cleared in one go.
    let (root, context) = access_fixture("conversation settings many");
    let (status, body) = set_engine(&context, KEPT_GROUP, json!("harness"));
    assert_eq!(status, 200, "{body}");
    let store = StoreProbe::at(&root.0);
    for id in 10_000..10_500 {
        store.insert_conversation_value(&format!("group:{id}"), ENGINE, "\"harness\"");
    }
    let view = conversations_view(&context);
    let rows = view["conversations"].as_array().unwrap();
    assert_eq!(rows.len(), 503, "the stored values and the admitted conversations are listed");
    assert_eq!(rows.iter().filter(|row| row["admitted"] == false).count(), 500, "the hand-edited ones as not admitted");
    // A set to the list in effect is no change and clears nothing; a real change clears every one in one transaction.
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "allow.groups", "value": [7001, 7002], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_conversations(&root).len(), 501, "no access change, no clear");
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "allow.groups", "value": [7001, 7002, 7003], "confirm": true}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_conversations(&root), vec![KEPT_GROUP.to_string()], "the next access change clears every one");
    assert_eq!(audit_rows(&root).iter().filter(|row| row["source"] == "access").count(), 500);
}

#[test]
fn conversation_settings_undo_and_history_are_kept_per_conversation() {
    let (root, context) = settings_fixture("conversation settings undo");
    admit_groups(&root, json!([101]));
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, &format!("?conversation={OWNER_CHAT}"));
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(
        (entries[0]["scope"].clone(), entries[0]["setting"].clone(), entries[0]["new"].clone()),
        (json!(OWNER_CHAT), json!(ENGINE), json!("harness"))
    );
    // Undoing a first change removes the stored value: the global default applies again.
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "the latest entry of a conversation is undone");
    assert_eq!(body, json!({"ok": true, "entry": entries[0]["entry"], "setting": ENGINE, "conversation": OWNER_CHAT, "stored": null}));
    assert!(stored_conversations(&root).is_empty(), "the undo removed the stored value");
    // Revisions are never reused: two audit rows before, so the next change is revision 3.
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!((status, body["revision"].clone()), (200, json!(3)), "{body}");
    for (conversation, value) in [(OWNER_CHAT, "dsh"), ("group:101", "harness")] {
        let (status, body) = set_engine(&context, conversation, json!(value));
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) = set_quiet(&context, json!(4500));
    assert_eq!(status, 200, "{body}");
    let (owner, _) = history_page(&context, &format!("?conversation={OWNER_CHAT}"));
    assert_eq!(owner.len(), 4, "{owner:?}");
    assert!(owner.iter().all(|entry| entry["scope"] == OWNER_CHAT), "{owner:?}");
    let (group, _) = history_page(&context, "?conversation=group:0101");
    assert_eq!(group.len(), 1, "a conversation key is canonicalized: {group:?}");
    let (engine, _) = history_page(&context, &format!("?setting={ENGINE}"));
    assert_eq!(engine.len(), 5, "{engine:?}");
    let (global, _) = history_page(&context, &format!("?setting={QUIET_KEY}"));
    assert_eq!(global.len(), 1, "{global:?}");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": owner[1]["entry"]}));
    assert_eq!((status, body["error"].clone()), (409, json!("the setting changed after this entry; undo the later change first")));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": owner[0]["entry"]}));
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["conversation"].clone(), body["stored"].clone()), (json!(OWNER_CHAT), json!("harness")));
    assert_eq!(conversation_entry(&context, OWNER_CHAT).unwrap()["settings"][ENGINE]["stored"], "harness", "the restored value is stored");
    assert_eq!(conversation_entry(&context, "group:101").unwrap()["settings"][ENGINE]["stored"], "harness", "another conversation is untouched");
    assert_eq!(next_use_quiet(&context), Some(4500), "and so is the global setting");
    // A stored value changed behind the store's back no longer matches the latest entry.
    StoreProbe::at(&root.0).edit("UPDATE conversation_settings SET value='\"dsh\"' WHERE conversation='group:101'");
    let (group, _) = history_page(&context, "?conversation=group:101");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": group[0]["entry"]}));
    assert_eq!((status, body["error"].clone()), (409, json!("the setting changed after this entry; undo the later change first")));
}

#[test]
fn conversation_settings_change_cut_off_before_its_audit_row_changes_nothing() {
    let (root, context) = settings_fixture("conversation settings cut off");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "{body}");
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("dsh"));
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(false));
    assert_eq!(status, 503, "a change cut off before its audit row is refused: {body}");
    assert_eq!(StoreProbe::at(&root.0).conversation_rows()[0].2, "\"harness\"", "the cut-off change left no value");
    assert_eq!(audit_rows(&root).len(), 1, "and no audit row");
    let (entries, _) = history_page(&context, "");
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(true));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    crate::settings::FAIL_BEFORE_AUDIT.with(|fail| fail.set(false));
    assert_eq!(status, 503, "{body}");
    assert_eq!(StoreProbe::at(&root.0).conversation_rows().len(), 1, "the cut-off undo left the stored value");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("dsh"));
    assert_eq!((status, body["revision"].clone()), (200, json!(2)), "the next change continues: {body}");
}

#[test]
fn conversation_settings_concurrent_sets_each_store_one_change() {
    let (root, context) = settings_fixture("conversation settings concurrent");
    admit_groups(&root, json!([101, 102]));
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let writes = [(OWNER_CHAT, "harness"), ("group:101", "harness"), ("group:102", "harness"), (OWNER_CHAT, "dsh"), ("group:101", "dsh")];
    let context = &context;
    std::thread::scope(|scope| {
        let running: Vec<_> = writes
            .iter()
            .map(|(conversation, value)| scope.spawn(move || set_engine(context, conversation, json!(value))))
            .collect();
        for writer in running {
            let (status, body) = writer.join().unwrap();
            assert_eq!(status, 200, "{body}");
        }
    });
    let stored = StoreProbe::at(&root.0).conversation_rows();
    assert_eq!(stored.len(), 3, "{stored:?}");
    let view = conversations_view(context);
    for (conversation, _, text, _, _) in &stored {
        let value: Value = serde_json::from_str(text).unwrap();
        let entry = view["conversations"].as_array().unwrap().iter().find(|entry| entry["conversation"] == conversation.as_str()).unwrap();
        assert_eq!(entry["settings"][ENGINE]["stored"], value, "{conversation}: the list shows the stored value");
    }
    assert_eq!(audit_rows(&root).len(), 5, "every write was one change with one audit row");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "no write touched config.json");
}

#[test]
fn conversation_settings_writes_take_the_configuration_gate_before_the_store_gate() {
    let (_root, context) = settings_fixture("conversation settings gate order");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "{body}");
    for writer in ["set", "reset", "undo"] {
        let (entries, _) = history_page(&context, "");
        let latest = entries[0]["entry"].clone();
        let config_gate = acquire_social_state_gate(&context).unwrap();
        let status = std::thread::scope(|scope| {
            let running = scope.spawn(|| match writer {
                "set" => set_engine(&context, OWNER_CHAT, json!("dsh")).0,
                "reset" => reset_engine(&context, OWNER_CHAT).0,
                _ => post(&context, "/api/settings/undo", json!({"entry": latest})).0,
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
    assert_eq!(conversation_entry(&context, OWNER_CHAT).unwrap()["settings"][ENGINE]["stored"], "harness", "the undo restored the value before the set");
}

#[test]
fn conversation_settings_are_never_chat_visible_or_writable() {
    let (root, context) = chat_fixture("conversation settings chat");
    let listed = chat(&context, "settings-get", json!({})).unwrap();
    for name in listed["writable"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        assert!(!name.starts_with("conversation") && !name.starts_with("engine"), "{name} is chat-writable");
    }
    assert!(listed["settings"].as_object().unwrap().keys().all(|name| !name.starts_with("conversation") && !name.starts_with("engine")), "{listed}");
    for name in ["conversationEngines", "conversationEngines.private:7001", ENGINE, "engineDefault", ENGINE_DEFAULT_KEY] {
        let set = chat(&context, "settings-set", json!({"settingKey": name, "value": "harness"}));
        assert_eq!(set, Err(CHAT_WRITE_REFUSED_TEXT.to_string()), "{name}");
        let get = chat(&context, "settings-get", json!({"settingKey": name}));
        assert_eq!(get, Err(SETTINGS_READ_REFUSED.to_string()), "{name}");
    }
    assert!(!settings_db(&root).exists(), "no chat call created the store");
    assert!(!crate::settings::ENGINE_DEFAULT.chat, "the global default is never chat-writable");
}

#[test]
fn conversation_settings_registry_agrees_with_the_engine_reader() {
    let globals: Vec<_> = crate::settings::registry().collect();
    let scoped: Vec<_> = crate::settings::conversation_registry().collect();
    assert!(!scoped.is_empty(), "the conversation scope has settings");
    for setting in &scoped {
        assert!(globals.iter().all(|global| global.key != setting.key), "{} is not a global key", setting.key);
        assert!(crate::settings::find(setting.key).is_none(), "{} is not a global setting", setting.key);
        assert_eq!(crate::settings::find_conversation(setting.key).map(|found| found.key), Some(setting.key));
        assert!(globals.iter().any(|global| global.key == setting.global.key), "{}: its global setting is registered", setting.key);
        assert!(!setting.global.chat, "{}: never chat-writable", setting.key);
        assert!(!setting.label.is_empty() && !setting.hint.is_empty(), "{}", setting.key);
    }
    let mut keys: Vec<&str> = scoped.iter().map(|setting| setting.key).collect();
    let all = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), all, "conversation keys are unique");
    // The engine settings' options and default agree with the turn engine's parser.
    let engine = &crate::settings::CONVERSATION_ENGINE;
    assert_eq!(engine.global.key, crate::settings::ENGINE_DEFAULT.key, "the conversation engine falls back to engine.default");
    let options = engine.global.kind.view()["options"].as_array().unwrap().clone();
    assert_eq!(options, vec![json!("dsh"), json!("harness")]);
    let selected = |value: Value| crate::turn_engine::Engine::parse(&value).map(crate::turn_engine::Engine::id);
    for option in &options {
        assert_eq!(selected(option.clone()), Ok(option.as_str().unwrap()));
    }
    assert_eq!(selected(engine.global.default_value()), Ok("dsh"));
    for refused in [json!("other"), json!("HARNESS"), json!(" dsh"), json!(1), Value::Null] {
        assert_eq!(engine.normalize(&refused), None, "{refused}");
        assert!(selected(refused.clone()).is_err(), "the reader fails closed on {refused}");
    }
}

#[test]
fn conversation_settings_page_lists_and_sets_through_the_routes() {
    let (root, context) = settings_fixture("conversation settings page");
    let (status, _) = admin_request(&context, "GET", "/settings/conversations", None);
    assert_eq!(status, 302, "legacy path redirects behind the console front");
    let (status, _) = admin_request(&context, "GET", "/conversations", None);
    assert_eq!(status, 200, "canonical SPA route is served behind the console front");
    let (status, body) = agent_request(&context, "GET", "/settings/conversations", None, "fixture-agent-token");
    assert_eq!(status, 403, "{body}");
    let body = conversations_view(&context);
    assert_eq!(
        body["settings"],
        json!([{
            "key": ENGINE, "scope": "conversation", "group": "turn engine", "label": "Turn engine",
            "hint": crate::settings::CONVERSATION_ENGINE.hint, "applies": "next wake",
            "kind": {"type": "choice", "options": ["dsh", "harness"]}, "default": "dsh", "global": ENGINE_DEFAULT_KEY,
        }, {
            "key": ROUTE, "scope": "conversation", "group": "model and cost", "label": "Model route",
            "hint": crate::settings::CONVERSATION_ROUTE.hint, "applies": "live",
            "kind": {"type": "route id", "maxChars": 64}, "default": null, "global": "harness.firstRoute",
        }]),
        "the list names the conversation scope"
    );
    assert_eq!(
        body["conversations"],
        json!([{"conversation": OWNER_CHAT, "admitted": true, "engineToday": "dsh", "settings": {
            ENGINE: {"stored": null, "value": "dsh"},
            ROUTE: {"stored": null, "value": null},
        }}])
    );
    assert!(body["note"].as_str().unwrap().starts_with("Switch waits"), "the page explains the drain: {body}");
    assert!(!settings_db(&root).exists(), "listing conversation settings creates no store");
    let (status, body) = set_engine(&context, OWNER_CHAT, json!("harness"));
    assert_eq!(status, 200, "{body}");
    let entry = conversation_entry(&context, OWNER_CHAT).unwrap();
    assert_eq!(entry["settings"][ENGINE], json!({"stored": "harness", "value": "harness"}), "the next list shows the new value");
    let routes = include_str!("../../console_settings/conversation.rs");
    assert!(!routes.contains("write_atomic(RuntimeArea::Config"), "console_settings/conversation.rs writes config.json itself");
}

#[test]
fn legacy_engine_choices_import_once_before_routing() {
    let (root, context) = access_fixture("engine choices import");
    assert_eq!(set_engine(&context, KEPT_GROUP, json!("dsh")).0, 200);
    let old = json!({KEPT_GROUP: "harness", LOST_GROUP: "harness", "group:91": "harness"});
    write_config(&root, |config| config["conversationEngines"] = old.clone());
    crate::config_tx::import_engine_choices(&context).unwrap();
    assert_eq!(crate::settings::engine_choice(&context, &context.load_config().unwrap(), LOST_GROUP, true).unwrap(), "harness", "legacy choice must be imported");
    assert_eq!(crate::settings::engine_choice(&context, &context.load_config().unwrap(), KEPT_GROUP, true).unwrap(), "dsh", "the administrator's stored choice wins");
    assert_eq!(stored_conversations(&root), [KEPT_GROUP, LOST_GROUP]);
    assert!(stored_config(&root).get("conversationEngines").is_none());

    // A crash before the raw key was removed must not recreate an audited choice after access is cleared.
    assert_eq!(post(&context, "/api/conversations/scope", json!({"conversation": LOST_GROUP,"action":"remove","confirm":true})).0, 200);
    assert_eq!(post(&context, "/api/conversations/scope", json!({"conversation": LOST_GROUP,"action":"admit","confirm":true})).0, 200);
    write_config(&root, |config| config["conversationEngines"] = old);
    crate::config_tx::import_engine_choices(&context).unwrap();
    assert_eq!(stored_conversations(&root), [KEPT_GROUP], "an access clear must survive upgrade replay");
}

#[test]
fn legacy_engine_import_skips_and_audits_one_invalid_choice() {
    let (root, context) = access_fixture("engine choices invalid entry");
    let old = json!({KEPT_GROUP: {"invalid": true}, LOST_GROUP: "harness"});
    write_config(&root, |config| config["conversationEngines"] = old.clone());
    assert!(
        crate::config_tx::import_engine_choices(&context).is_ok(),
        "one invalid choice must not abort the import"
    );
    let cfg = context.load_config().unwrap();
    assert_eq!(crate::settings::engine_choice(&context, &cfg, KEPT_GROUP, true).unwrap(), "dsh");
    assert_eq!(crate::settings::engine_choice(&context, &cfg, LOST_GROUP, true).unwrap(), "harness");
    assert_eq!(stored_conversations(&root), [LOST_GROUP]);
    assert!(stored_config(&root).get("conversationEngines").is_none());
    let (before, _) = history_page(&context, &format!("?conversation={KEPT_GROUP}"));
    assert_eq!(before.len(), 1, "the invalid entry must be audited");
    assert_eq!(before[0]["actor"], "system");
    assert_eq!(before[0]["source"], "migration");
    assert!(before[0]["old"].is_null() && before[0]["new"].is_null());

    write_config(&root, |config| config["conversationEngines"] = old);
    crate::config_tx::import_engine_choices(&context).unwrap();
    let (after, _) = history_page(&context, &format!("?conversation={KEPT_GROUP}"));
    assert_eq!(before, after, "replaying a skipped entry must not audit it twice");
}

#[test]
fn conversation_settings_undo_never_stores_a_value_for_a_conversation_that_is_not_admitted() {
    let (root, context) = access_fixture("conversation settings undo admission");
    let (status, body) = set_engine(&context, LOST_GROUP, json!("harness"));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, &format!("?conversation={LOST_GROUP}"));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "the set is undone while the conversation is admitted: {body}");
    // The conversation leaves its allow list with no value stored, so the clear has nothing to remove.
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": LOST_GROUP, "action": "remove", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, &format!("?conversation={LOST_GROUP}"));
    assert_eq!(entries.len(), 2, "the removal wrote no clear: {entries:?}");
    let audit = audit_rows(&root).len();
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!((status, body["error"].clone()), (409, json!(NOT_ADMITTED)), "an undo that would store a value for a conversation that is not admitted: {body}");
    assert!(stored_conversations(&root).is_empty(), "no value is stored for a conversation that is not admitted");
    assert_eq!(audit_rows(&root).len(), audit, "and the refused undo wrote no audit row");
    // Admitted again, the same undo stores the value.
    let (status, body) = post(&context, "/api/conversations/scope", json!({"conversation": LOST_GROUP, "action": "admit", "confirm": true}));
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "an admitted conversation gets its value back: {body}");
    assert_eq!(stored_conversations(&root), vec![LOST_GROUP.to_string()]);
    // An undo that removes a value stays allowed after a hand edit took the conversation off its allow list.
    admit_groups(&root, json!([7001]));
    let (entries, _) = history_page(&context, &format!("?conversation={LOST_GROUP}"));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!(status, 200, "removing a value needs no admission: {body}");
    assert!(stored_conversations(&root).is_empty(), "the value is removed");
}

/// #162: the refusal of a value that is not a route id.
const ROUTE_REFUSED: &str = "conversation.route must be a route id of 1 to 64 ASCII letters, digits, '.', '_' or '-', not default";

/// #162: the `conversation` row of `GET /api/settings/models`.
fn model_row(body: &Value, conversation: &str) -> Value {
    body["conversations"].as_array().unwrap().iter().find(|row| row["conversation"] == conversation).cloned().unwrap_or_else(|| panic!("{conversation} is listed: {body}"))
}

#[test]
fn model_route_list_shows_the_chain_health_prices_and_each_conversations_order_with_no_dsh_running() {
    // The fixture's DSH address has nothing listening: the list reads only the configuration, the store and route health.
    let (root, context) = settings_fixture("model route list");
    admit_groups(&root, json!([101]));
    write_config(&root, |config| {
        config["harness"] = json!({"routes": [
            {"id": "a", "model": "deepseek-v4-pro", "reasoningEffort": "high"},
            {"id": "b", "model": "m-synthetic-model-key-162", "maxOutputTokens": 1000, "retry429": false},
        ]});
    });
    std::fs::write(root.0.join("config/harness-api-key"), "synthetic-model-key-162").unwrap();
    let chain = crate::harness::routes::chain(&context.load_config().unwrap()).unwrap();
    crate::harness::route_health::observe(&context, &chain, 1, crate::harness::route_health::Seen::Failed { class: "quota" });
    let (status, body) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!(status, 200, "{body}");
    let routes = body["routes"].as_array().unwrap().clone();
    assert_eq!((routes[1]["health"]["state"].clone(), routes[1]["health"]["class"].clone()), (json!("failed"), json!("quota")), "{body}");
    assert!(routes[1]["health"]["since"].is_i64(), "{body}");
    assert_eq!(routes[0]["health"], json!({"state": "unknown"}), "nothing observed for route a");
    assert_eq!(
        (routes[0]["id"].clone(), routes[0]["model"].clone(), routes[0]["reasoningEffort"].clone(), routes[0]["maxOutputTokens"].clone(), routes[0]["retry429"].clone()),
        (json!("a"), json!("deepseek-v4-pro"), json!("high"), json!(4096), json!(true))
    );
    assert_eq!(
        (routes[1]["model"].clone(), routes[1]["maxOutputTokens"].clone(), routes[1]["retry429"].clone()),
        (json!("m-[REDACTED]"), json!(1000), json!(false)),
        "the harness credential is never returned"
    );
    assert!(!body.to_string().contains("synthetic-model-key-162"), "{body}");
    assert_eq!((routes[0]["price"]["label"].clone(), routes[0]["price"]["estimated"].clone()), (json!("DeepSeek-V4-Pro-0813"), json!(false)));
    assert_eq!(routes[1]["price"]["estimated"], true, "a model the price table lacks is priced as an estimate");
    assert_eq!((body["allFailed"].clone(), body["firstRoute"].clone(), body["setting"].clone()), (json!(false), Value::Null, json!(ROUTE)));
    assert_eq!(
        model_row(&body, OWNER_CHAT),
        json!({"conversation": OWNER_CHAT, "admitted": true, "stored": null, "route": null, "inChain": false, "order": ["a", "b"]})
    );
    // A conversation's route is set through the settings routes and listed with the order its next turn walks.
    let (status, reply) = post(&context, "/api/settings/conversation/set", json!({"conversation": "group:101", "setting": ROUTE, "value": "b"}));
    assert_eq!(status, 200, "{reply}");
    let (_, body) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!(
        model_row(&body, "group:101"),
        json!({"conversation": "group:101", "admitted": true, "stored": "b", "route": "b", "inChain": true, "order": ["b", "a"]})
    );
    assert_eq!(model_row(&body, OWNER_CHAT)["order"], json!(["a", "b"]), "another conversation keeps the configured order");
    // The global first route applies to a conversation without its own.
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "harness.firstRoute", "value": "b"}));
    assert_eq!(status, 200, "{reply}");
    let (_, body) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!((body["firstRoute"].clone(), model_row(&body, OWNER_CHAT)["order"].clone()), (json!("b"), json!(["b", "a"])));
    // The setting was seeded on the first settings write. A hand edit cannot replace its chain.
    write_config(&root, |config| config["harness"]["routes"][1]["id"] = json!("b2"));
    let (_, body) = admin_request(&context, "GET", "/api/settings/models", None);
    assert_eq!(model_row(&body, "group:101")["order"], json!(["b", "a"]));
    // When a settings write removes a conversation's selected route, the selection is still listed and ignored.
    let routes = stored_config(&root)["harness"]["routes"].clone();
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "harness.routes", "value": routes}));
    assert_eq!(status, 200, "{reply}");
    let (_, body) = admin_request(&context, "GET", "/api/settings/models", None);
    let row = model_row(&body, "group:101");
    assert_eq!((row["route"].clone(), row["inChain"].clone(), row["order"].clone()), (json!("b"), json!(false), json!(["a", "b2"])));
    // The legacy path redirects, canonical SPA route is served, and the list requires admin + selected runtime.
    let (status, _) = admin_request(&context, "GET", "/settings/models", None);
    assert_eq!(status, 302, "legacy path redirects behind the console front");
    let (status, _) = admin_request(&context, "GET", "/models", None);
    assert_eq!(status, 200, "canonical SPA route is served behind the console front");
    for path in ["/settings/models", "/api/settings/models"] {
        let (status, reply) = agent_request(&context, "GET", path, None, "fixture-agent-token");
        assert_eq!(status, 403, "{path}: {reply}");
    }
    let source = include_str!("../../console_settings/models.rs");
    assert!(!source.contains("write_atomic(RuntimeArea::Config") && !source.contains("dsh"), "the list writes nothing and asks no DSH");

    let (legacy_root, legacy) = settings_fixture("model route list with legacy engine map");
    write_config(&legacy_root, |config| config["conversationEngines"] = json!({OWNER_CHAT: "harness"}));
    let config_path = legacy_root.0.join("config/config.json");
    let before = std::fs::read(&config_path).unwrap();
    let (status, _) = admin_request(&legacy, "GET", "/api/settings/models", None);
    assert_eq!(status, 200);
    assert_eq!(std::fs::read(config_path).unwrap(), before, "model route reads must leave the legacy map alone");
    assert!(!settings_db(&legacy_root).exists(), "model route reads must leave the store absent");
}

#[test]
fn model_route_setting_is_kept_per_conversation_and_a_reset_stores_none_that_an_undo_restores() {
    let (root, context) = settings_fixture("model route setting");
    let set = |value: Value| post(&context, "/api/settings/conversation/set", json!({"conversation": OWNER_CHAT, "setting": ROUTE, "value": value}));
    let (status, body) = set(json!("b"));
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&context, "/api/settings/conversation/reset", json!({"conversation": OWNER_CHAT, "setting": ROUTE}));
    assert_eq!((status, body["value"].clone(), body["changed"].clone()), (200, Value::Null, json!(true)), "{body}");
    assert_eq!(crate::settings::conversation_value(&context, OWNER_CHAT, ROUTE), Some(Value::Null), "a reset stores none");
    let (status, body) = set(json!("c"));
    assert_eq!(status, 200, "{body}");
    let (entries, _) = history_page(&context, &format!("?conversation={OWNER_CHAT}"));
    let (status, body) = post(&context, "/api/settings/undo", json!({"entry": entries[0]["entry"]}));
    assert_eq!((status, body["stored"].clone()), (200, Value::Null), "undoing the change after a reset restores none: {body}");
    assert_eq!(crate::settings::conversation_value(&context, OWNER_CHAT, ROUTE), Some(Value::Null));
    assert_eq!(model_row(&admin_request(&context, "GET", "/api/settings/models", None).1, OWNER_CHAT)["route"], Value::Null);
    for value in [json!("default"), json!("a b"), json!(""), json!(" a"), json!("x".repeat(65)), Value::Null, json!(1)] {
        let (status, body) = set(value.clone());
        assert_eq!((status, body["error"].clone()), (400, json!(ROUTE_REFUSED)), "{value}");
    }
    // The global setting follows the global rules (written back into config.json); a conversation's value never is.
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "harness.firstRoute", "value": "b"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_config(&root)["harness"]["firstRoute"], "b");
    assert!(!std::fs::read_to_string(root.0.join("config/config.json")).unwrap().contains(ROUTE), "no conversation's route reaches config.json");
    assert_eq!(crate::settings::conversation_value(&context, "group:101", ROUTE), None, "a conversation without a value");
    assert_eq!(crate::settings::conversation_value(&context, "group:1/x", ROUTE), None, "not a conversation key");
    assert_eq!(crate::settings::conversation_value(&context, OWNER_CHAT, ENGINE_DEFAULT_KEY), None, "not a conversation setting");
    assert_eq!(setting_view(&context, "harness.firstRoute")["kind"], json!({"type": "route id", "maxChars": 64}));
    let (status, body) = post(&context, "/api/settings/set", json!({"setting": "harness.firstRoute", "value": "a/b"}));
    assert_eq!((status, body["error"].clone()), (400, json!("harness.firstRoute must be a route id of 1 to 64 ASCII letters, digits, '.', '_' or '-', not default")));
    // Old and newer stores: a version-1 store holds no conversation value, and a store this release cannot read gives
    // none, so the global setting applies and no turn fails.
    let (old_root, old) = settings_fixture("model route old store");
    let (status, body) = set_quiet(&old, json!(4500));
    assert_eq!(status, 200, "{body}");
    assert_eq!((StoreProbe::at(&old_root.0).user_version(), crate::settings::conversation_value(&old, OWNER_CHAT, ROUTE)), (1, None));
    let (newer_root, newer) = settings_fixture("model route newer store");
    StoreProbe::at(&newer_root.0).build("CREATE TABLE later (x INTEGER); PRAGMA user_version=4;");
    assert_eq!(crate::settings::conversation_value(&newer, OWNER_CHAT, ROUTE), None, "a newer store reads as no value");
}
