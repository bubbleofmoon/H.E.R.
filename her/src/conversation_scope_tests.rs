// #186 console tests: the inbound gate reads allow and deny per event, and one action admits or removes a
// conversation in its allow list and `rollout.rustGroups` at once.
use super::*;

const GROUP: &str = "group:420042";

/// A selected fixture runtime in reserved2 mode whose config has the given top-level fields replaced.
fn scoped(label: &str, fields: Value) -> (FixtureRoot, RuntimeContext) {
    let (root, _) = fixture_runtime(label, "fixture-t186-scope-token-abcdefghij");
    let path = root.0.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (field, value) in fields.as_object().unwrap() {
        config[field] = value.clone();
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    std::fs::write(root.0.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    let context = RuntimeContext::from_root(&root.0).unwrap();
    (root, context)
}

/// The raw `config.json` as written.
fn file(root: &FixtureRoot) -> Value {
    serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap()
}

fn scope(context: &RuntimeContext, conversation: &str, action: &str) -> (u16, Value) {
    admin_request(context, "POST", "/api/conversations/scope", Some(&json!({"conversation": conversation, "action": action})))
}

#[test]
fn t186_inbound_gate_admits_listed_and_not_denied_conversations_only() {
    let cfg = json!({
        "allow": {"groups": [420042, " 420043 "], "private": ["7001"]},
        "deny": {"groups": ["420043"]},
        "allowAllWhenEmpty": true
    });
    assert!(inbound_admitted(&cfg, "group:420042"));
    assert!(!inbound_admitted(&cfg, "group:420043"), "a denied conversation must be refused even when it is listed");
    assert!(!inbound_admitted(&cfg, "group:420044"), "an unlisted conversation is refused");
    assert!(inbound_admitted(&cfg, "private:7001"));
    assert!(!inbound_admitted(&cfg, "private:7002"));
    assert!(!inbound_admitted(&cfg, "channel:420042") && !inbound_admitted(&cfg, "group:"), "malformed keys are refused");
    assert!(
        !inbound_admitted(&json!({"allow": {"groups": []}, "allowAllWhenEmpty": true}), "group:420042"),
        "allowAllWhenEmpty never widens the inbound gate"
    );
    assert_eq!(admitted_ids(&cfg, "group"), ["420042"]);
    assert_eq!(admitted_ids(&cfg, "private"), ["7001"]);
    assert!(admitted_ids(&json!({}), "group").is_empty());
}

#[test]
fn t186_admit_writes_both_keys_once_and_reports_the_live_restart() {
    let (root, context) = scoped(
        "t186 admit",
        json!({"allow": {"groups": [420042], "private": []}, "rollout": {"enabled": true, "rustGroups": [GROUP]}}),
    );
    // A live owner started for the old list holds the live gate.
    std::fs::create_dir_all(root.0.join("data/logs")).unwrap();
    let gate = crate::lifecycle::OwnerGate::new(root.0.join("data/logs").join(crate::lifecycle::LIVE_LOCK));
    let me = crate::lifecycle::current_identity().unwrap();
    let crate::lifecycle::Acquire::Acquired(owner) =
        gate.acquire("live", me.clone(), Some(vec![GROUP.to_string()]), 1, std::time::Duration::ZERO)
    else {
        panic!("the fixture live gate is free")
    };
    let (status, reply) = scope(&context, "group:420043", "admit");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["changed"], json!(["allow.groups", "rollout.rustGroups"]), "{reply}");
    let config = file(&root);
    assert_eq!(config["allow"]["groups"], json!([420042, 420043]));
    assert_eq!(config["rollout"]["rustGroups"], json!([GROUP, "group:420043"]));
    assert_eq!(reply["liveRestart"]["state"], "required", "{reply}");
    assert_eq!(reply["liveRestart"]["serving"], json!([GROUP]));
    assert_eq!(reply["scope"], json!({"notAllowed": [], "notServed": []}), "both keys agree after the action");
    assert!(inbound_admitted(&context.load_config().unwrap(), "group:420043"), "admitted for the next event");
    // The change is in the console change history, so it can be undone there.
    let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
    assert_eq!(status, 200, "{recent}");
    assert!(
        recent["entries"].as_array().unwrap().iter().any(|e| e["path"] == "/api/conversations/scope" && e["ok"] == true),
        "{recent}"
    );
    // Again: nothing left to change, and the live owner still serves the old list.
    let (status, again) = scope(&context, "group:420043", "admit");
    assert_eq!((status, again["changed"].clone()), (200, json!([])), "{again}");
    assert_eq!(again["liveRestart"]["state"], "required");
    // A live owner that serves the new list needs no restart; neither does a runtime without one.
    drop(owner);
    let crate::lifecycle::Acquire::Acquired(owner) = gate.acquire(
        "live",
        me,
        Some(vec!["group:420043".to_string(), GROUP.to_string()]),
        2,
        std::time::Duration::ZERO,
    ) else {
        panic!("the fixture live gate is free again")
    };
    assert_eq!(scope(&context, "group:420043", "admit").1["liveRestart"]["state"], "none");
    drop(owner);
    assert_eq!(scope(&context, "group:420043", "admit").1["liveRestart"]["state"], "none", "the next start serves the new list");
}

#[test]
fn t186_remove_revokes_at_once_and_refused_requests_change_nothing() {
    let (root, context) = scoped(
        "t186 remove",
        json!({
            "allow": {"groups": [420042], "private": []},
            "deny": {"groups": [420045]},
            "rollout": {"enabled": true, "rustGroups": [GROUP]}
        }),
    );
    std::fs::write(
        root.0.join("data/social-v2.json"),
        json!({"conversations": {GROUP: {"agentToken": "old-scope-token-abcdefghij", "bootstrapSent": true}}}).to_string(),
    )
    .unwrap();
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    for (body, expected) in [
        (json!({"conversation": "group:abc", "action": "admit"}), 400),
        (json!({"conversation": "group:420042,private:7001", "action": "admit"}), 400),
        (json!({"conversation": "group:0", "action": "admit"}), 400),
        (json!({"conversation": "channel:420042", "action": "admit"}), 400),
        (json!({"conversation": GROUP, "action": "drop"}), 400),
        (json!({"key": GROUP, "action": "remove"}), 400),
        (json!({"conversation": "group:420045", "action": "admit"}), 409),
    ] {
        let (status, reply) = admin_request(&context, "POST", "/api/conversations/scope", Some(&body));
        assert_eq!(status, expected, "{body} -> {reply}");
        assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "a refused request changes nothing: {body}");
    }
    let (status, reply) = scope(&context, GROUP, "remove");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["changed"], json!(["allow.groups", "rollout.rustGroups"]));
    assert_eq!(reply["revoked"], json!([GROUP]), "the removed conversation is revoked before the reply");
    let config = file(&root);
    assert_eq!(config["allow"]["groups"], json!([]));
    assert_eq!(config["rollout"]["rustGroups"], json!([]));
    assert_ne!(
        conversation_token(&context, GROUP).unwrap().as_deref(),
        Some("old-scope-token-abcdefghij"),
        "the conversation token is rotated"
    );
    assert!(!inbound_admitted(&context.load_config().unwrap(), GROUP), "refused for the next event");
    // Without an explicit rollout.rustGroups array nothing is written.
    let mut no_rollout = file(&root);
    no_rollout.as_object_mut().unwrap().remove("rollout");
    std::fs::write(root.0.join("config/config.json"), serde_json::to_vec_pretty(&no_rollout).unwrap()).unwrap();
    let unchanged = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, reply) = scope(&context, "group:420043", "admit");
    assert_eq!(status, 409, "{reply}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), unchanged);
}

#[test]
fn t186_cli_entry_uses_the_same_write_and_revocation() {
    let (root, context) = scoped(
        "t186 cli entry",
        json!({"allow": {"groups": [420042], "private": []}, "rollout": {"enabled": true, "rustGroups": [GROUP]}}),
    );
    std::fs::write(
        root.0.join("data/social-v2.json"),
        json!({"conversations": {GROUP: {"agentToken": "old-cli-token-abcdefghij", "bootstrapSent": true}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.0.join("data/sessions.json"),
        json!({"sessions": {GROUP: "dsh-old-session"}, "sessionPolicies": {GROUP: "[\"reserved2\",\"qq-chat-v2\",null]"}}).to_string(),
    )
    .unwrap();
    let reply = conversation_scope_change(&context, GROUP, "remove").expect("the CLI entry applies the change");
    assert_eq!(reply["changed"], json!(["allow.groups", "rollout.rustGroups"]), "{reply}");
    assert_eq!(reply["revoked"], json!([GROUP]));
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None, "the DSH mapping is retired");
    assert!(take_config_writes().is_none(), "no request write record is left behind");
    assert_eq!(file(&root)["allow"]["groups"], json!([]));
    assert_eq!(conversation_scope_change(&context, "group:abc", "admit").unwrap_err().0, 400);
}

#[test]
fn t186_cli_entry_waits_for_background_session_stops_within_its_limit() {
    let (_root, context) = scoped(
        "t186 cli wait",
        json!({"allow": {"groups": [420042], "private": []}, "rollout": {"enabled": true, "rustGroups": [GROUP]}}),
    );
    // A background stop that ends after 300 ms: the CLI entry returns only after it.
    let stop = access::BackgroundStopForTest::start();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(stop);
    });
    let started = std::time::Instant::now();
    let reply = conversation_scope_change(&context, "group:420043", "admit").expect("the admit applies");
    assert!(started.elapsed() >= std::time::Duration::from_millis(300), "returned before the background stop finished");
    assert!(reply.get("sessionStops").is_none(), "{reply}");
    release.join().unwrap();
    // A stop that outlives the wait is reported, never waited for without end.
    let stuck = access::BackgroundStopForTest::start();
    let reply = conversation_scope_change_waiting(&context, "group:420043", "admit", std::time::Duration::from_millis(100))
        .expect("the admit applies again");
    assert_eq!(reply["sessionStops"], "unfinished: check the retired sessions' tasks in DSH", "{reply}");
    drop(stuck);
    assert!(access::wait_background_stops(std::time::Duration::from_secs(10)));
}

#[test]
fn t186_concurrent_admits_lose_no_update() {
    use std::time::Duration;
    let (root, context) = scoped(
        "t186 concurrent",
        json!({"allow": {"groups": [], "private": []}, "rollout": {"enabled": true, "rustGroups": []}}),
    );
    // The first admit stops between its read and its write until released; the second reports when it has read.
    let (first_read_tx, first_read) = std::sync::mpsc::channel::<()>();
    let (second_read_tx, second_read) = std::sync::mpsc::channel::<()>();
    let (release_tx, release) = std::sync::mpsc::channel::<()>();
    let release = std::sync::Mutex::new(release);
    *access::SCOPE_AFTER_READ.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::sync::Arc::new(move |key: &str| match key {
        "group:420051" => {
            first_read_tx.send(()).unwrap();
            let _ = release.lock().unwrap_or_else(|p| p.into_inner()).recv_timeout(Duration::from_secs(10));
        }
        "group:420052" => {
            let _ = second_read_tx.send(());
        }
        _ => {}
    }));
    let (first, second, second_read_early) = std::thread::scope(|threads| {
        let first_context = context.clone();
        let first = threads.spawn(move || conversation_scope_change(&first_context, "group:420051", "admit"));
        first_read.recv_timeout(Duration::from_secs(10)).expect("the first admit reached the point between its read and its write");
        let second_context = context.clone();
        let mut second = Some(threads.spawn(move || conversation_scope_change(&second_context, "group:420052", "admit")));
        // Serialized, the second admit waits on the gates and cannot read yet; unserialized it reads the old file now.
        let second_read_early = second_read.recv_timeout(Duration::from_secs(2)).is_ok();
        // Unserialized: let the second write land first, so the held first write is the stale one.
        let early = if second_read_early { Some(second.take().unwrap().join().unwrap()) } else { None };
        release_tx.send(()).unwrap();
        let first = first.join().unwrap();
        let second = match early {
            Some(result) => result,
            None => second.take().unwrap().join().unwrap(),
        };
        (first, second, second_read_early)
    });
    *access::SCOPE_AFTER_READ.lock().unwrap_or_else(|p| p.into_inner()) = None;
    let config = file(&root);
    for key in ["group:420051", "group:420052"] {
        assert!(config["rollout"]["rustGroups"].as_array().unwrap().contains(&json!(key)), "a concurrent admit lost an update: {key} {config}");
        assert!(inbound_admitted(&context.load_config().unwrap(), key), "a concurrent admit lost an update: {key} {config}");
    }
    assert_eq!(config["allow"]["groups"], json!([420051, 420052]), "the second admit read after the first one wrote");
    assert!(!second_read_early, "the second admit read the configuration while the first was between its read and its write");
    for reply in [first, second] {
        assert_eq!(reply.expect("each admit applies")["changed"], json!(["allow.groups", "rollout.rustGroups"]));
    }
}

#[test]
fn t186_a_busy_gate_refuses_without_writing() {
    let (root, context) = scoped(
        "t186 busy gate",
        json!({"allow": {"groups": [420042], "private": []}, "rollout": {"enabled": true, "rustGroups": [GROUP]}}),
    );
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    // Another writer holds the social gate past the wait budget.
    let held = acquire_social_state_gate(&context).unwrap();
    let (status, _) = conversation_scope_change(&context, "group:420043", "admit").unwrap_err();
    assert_eq!(status, 503);
    drop(held);
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "nothing is written when the gate is busy");
    let reply = conversation_scope_change(&context, "group:420043", "admit").expect("the free gate admits");
    assert_eq!(reply["changed"], json!(["allow.groups", "rollout.rustGroups"]));
}

#[test]
fn t186_rerun_completes_a_half_applied_change() {
    // An edit that stopped halfway (one key changed) is flagged, and running the action again finishes it.
    let (root, context) = scoped(
        "t186 half applied",
        json!({"allow": {"groups": [420042, 420043], "private": []}, "rollout": {"enabled": true, "rustGroups": [GROUP]}}),
    );
    let gaps = crate::watchdog::rollout_scope_gaps(&context.load_config().unwrap());
    assert_eq!(gaps.not_served, ["group:420043"], "the half-applied admission is flagged");
    let (status, reply) = scope(&context, "group:420043", "admit");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["changed"], json!(["rollout.rustGroups"]), "only the missing key is written");
    assert_eq!(file(&root)["allow"]["groups"], json!([420042, 420043]), "the present key is left as it is");
    assert_eq!(reply["scope"], json!({"notAllowed": [], "notServed": []}));
    // The other half: served but no longer allowed; removing finishes it.
    let mut half = file(&root);
    half["allow"]["groups"] = json!([420042]);
    std::fs::write(root.0.join("config/config.json"), serde_json::to_vec_pretty(&half).unwrap()).unwrap();
    assert_eq!(crate::watchdog::rollout_scope_gaps(&context.load_config().unwrap()).not_allowed, ["group:420043"]);
    let (status, reply) = scope(&context, "group:420043", "remove");
    assert_eq!((status, reply["changed"].clone()), (200, json!(["rollout.rustGroups"])), "{reply}");
    assert_eq!(file(&root)["rollout"]["rustGroups"], json!([GROUP]));
}

#[test]
fn t186_old_spellings_and_large_lists_keep_their_meaning() {
    let ids: Vec<Value> = (0..1000).map(|n| json!(100000 + n)).collect();
    let (root, context) = scoped(
        "t186 old spellings",
        json!({
            "allow": {"group": ids, "privates": ["7001"]},
            "rollout": {"enabled": false, "rustGroups": ["group:100000", " group:100001 "]}
        }),
    );
    // The alias list moves to its canonical name with every entry kept, and the new id is appended.
    let (status, reply) = scope(&context, GROUP, "admit");
    assert_eq!(status, 200, "{reply}");
    let config = file(&root);
    let groups = config["allow"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1001);
    assert_eq!(groups[0], json!(100000));
    assert_eq!(groups[1000], json!(420042));
    assert!(config["allow"].get("group").is_none(), "the alias spelling is folded into allow.groups");
    assert_eq!(config["allow"]["privates"], json!(["7001"]), "the other kind keeps its spelling");
    assert_eq!(config["rollout"]["rustGroups"], json!(["group:100000", " group:100001 ", GROUP]));
    let effective = context.load_config().unwrap();
    assert!(inbound_admitted(&effective, "group:100999") && inbound_admitted(&effective, GROUP));
    // A served entry in another spelling is matched in canonical form.
    let (status, reply) = scope(&context, "group:100001", "remove");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(file(&root)["rollout"]["rustGroups"], json!(["group:100000", GROUP]));
    assert_eq!(file(&root)["allow"]["groups"].as_array().unwrap().len(), 1000);
}

#[test]
fn t186_agent_credentials_cannot_change_the_scope() {
    let (root, context) = scoped(
        "t186 agent",
        json!({"allow": {"groups": [420042], "private": []}, "rollout": {"enabled": true, "rustGroups": [GROUP]}}),
    );
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, reply) = agent_request(
        &context,
        "POST",
        "/api/conversations/scope",
        Some(&json!({"conversation": "group:420043", "action": "admit"})),
        "agent-scope-token-abcdefghij",
    );
    assert_eq!(status, 403, "{reply}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before);
}
