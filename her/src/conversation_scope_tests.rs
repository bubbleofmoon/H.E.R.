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
    admin_request(context, "POST", "/api/conversations/scope", Some(&json!({"conversation": conversation, "action": action, "confirm": true})))
}

#[test]
fn t186_inbound_gate_admits_listed_and_not_denied_conversations_only() {
    let cfg = json!({
        "allow": {"groups": [420042, " 420043 "], "private": ["7001"]},
        "deny": {"groups": ["420043"]},
        "allowAllWhenEmpty": true
    });
    assert!(crate::social_state::inbound_admitted(&cfg, "group:420042"));
    assert!(
        !crate::social_state::inbound_admitted(&cfg, "group:420043"),
        "a denied conversation must be refused even when it is listed"
    );
    assert!(
        !crate::social_state::inbound_admitted(&cfg, "group:420044"),
        "an unlisted conversation is refused"
    );
    assert!(crate::social_state::inbound_admitted(&cfg, "private:7001"));
    assert!(!crate::social_state::inbound_admitted(&cfg, "private:7002"));
    assert!(
        !crate::social_state::inbound_admitted(&cfg, "channel:420042")
            && !crate::social_state::inbound_admitted(&cfg, "group:"),
        "malformed keys are refused"
    );
    assert!(
        !crate::social_state::inbound_admitted(
            &json!({"allow": {"groups": []}, "allowAllWhenEmpty": true}),
            "group:420042"
        ),
        "allowAllWhenEmpty never widens the inbound gate"
    );
    assert_eq!(crate::social_state::admitted_ids(&cfg, "group"), ["420042"]);
    assert_eq!(crate::social_state::admitted_ids(&cfg, "private"), ["7001"]);
    assert!(crate::social_state::admitted_ids(&json!({}), "group").is_empty());
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
    assert!(
        crate::social_state::inbound_admitted(&context.load_config().unwrap(), "group:420043"),
        "admitted for the next event"
    );
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
fn scope_commits_both_keys_for_the_watchdog_even_if_the_mirror_fails() {
    let (root, context) = scoped(
        "scope settings transaction",
        json!({"allow": {"groups": [420042], "private": []}, "backends": {"mcpRustBridge": true},
            "rollout": {"rustGroups": [GROUP]}}),
    );
    let before = file(&root);
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(true));
    let (status, reply) = scope(&context, "group:420043", "admit");
    crate::settings::FAIL_WRITE_BACK.with(|fail| fail.set(false));
    assert_eq!(status, 200, "{reply}");
    assert!(reply["mirrorError"].is_string(), "{reply}");
    let history = crate::settings::history(&context, None, None, None).unwrap();
    let changes: Vec<_> = history["entries"].as_array().unwrap().iter()
        .filter(|entry| entry["source"] == "console")
        .map(|entry| (entry["setting"].clone(), entry["new"].clone()))
        .collect();
    assert_eq!(
        changes,
        [(json!("rollout.rustGroups"), json!([GROUP, "group:420043"])), (json!("allow.groups"), json!([420042, 420043]))],
        "scope records both settings changes"
    );
    assert_eq!(file(&root)["rollout"], before["rollout"], "the forced mirror failure leaves the old target on disk");
    let effective = context.load_config().unwrap();
    assert!(crate::social_state::inbound_admitted(&effective, "group:420043"));
    let target = crate::watchdog::live_target_from_config(&effective).unwrap();
    assert_eq!(target.groups, [GROUP, "group:420043"], "the watchdog uses the committed launch order");
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
    assert!(
        !crate::social_state::inbound_admitted(&context.load_config().unwrap(), GROUP),
        "refused for the next event"
    );
    // A runtime with no rollout in the file or store refuses the scope request.
    let (root, context) = scoped("t186 missing rollout", json!({}));
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
    assert!(
        !root.0.join("data/audit/audit.jsonl").exists(),
        "CLI scope change does not write an HTTP audit record"
    );
    assert_eq!(file(&root)["allow"]["groups"], json!([]));
    assert_eq!(conversation_scope_change(&context, "group:abc", "admit").unwrap_err().0, 400);
}

#[test]
fn t186_cli_entry_waits_for_background_session_stops_within_its_limit() {
    let (_root, context) = scoped(
        "t186 cli wait",
        json!({"allow": {"groups": [420042], "private": []}, "rollout": {"enabled": true, "rustGroups": [GROUP]}}),
    );
    let tracker = std::sync::Arc::new(crate::social_state::StopTracker::new());
    // A background stop that ends after 300 ms: the CLI entry returns only after it.
    let stop = tracker.start_for_test();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(stop);
    });
    let started = std::time::Instant::now();
    let reply = crate::config_tx::change_conversation_scope_waiting_with(
        &context,
        "group:420043",
        "admit",
        std::time::Duration::from_secs(10),
        &tracker,
        crate::social_state::default_stopper(),
    )
    .expect("the admit applies");
    assert!(started.elapsed() >= std::time::Duration::from_millis(300), "returned before the background stop finished");
    assert!(reply.get("sessionStops").is_none(), "{reply}");
    release.join().unwrap();
    // A stop that outlives the wait is reported, never waited for without end.
    let stuck = tracker.start_for_test();
    let reply = crate::config_tx::change_conversation_scope_waiting_with(
        &context,
        "group:420043",
        "admit",
        std::time::Duration::from_millis(100),
        &tracker,
        crate::social_state::default_stopper(),
    )
    .expect("the admit applies again");
    assert_eq!(reply["sessionStops"], "unfinished: check the retired sessions' tasks in DSH", "{reply}");
    drop(stuck);
    assert!(tracker.wait(std::time::Duration::from_secs(10)));
}

#[test]
fn t186_concurrent_admits_lose_no_update() {
    enum Signal {
        Reached,
        Finished,
    }
    /// Sends `Finished` when the first admit's thread ends, also while unwinding.
    struct FinishGuard(std::sync::mpsc::Sender<Signal>);
    impl Drop for FinishGuard {
        fn drop(&mut self) {
            let _ = self.0.send(Signal::Finished);
        }
    }
    let (root, context) = scoped(
        "t186 concurrent",
        json!({"allow": {"groups": [], "private": []}, "rollout": {"enabled": true, "rustGroups": []}}),
    );
    // The first admit stops between its read and its write, holding both gates, until released.
    let (signal_tx, signal) = std::sync::mpsc::channel::<Signal>();
    let (release_tx, release) = std::sync::mpsc::channel::<()>();
    let release = std::sync::Mutex::new(release);
    let reached = signal_tx.clone();
    let hook = std::sync::Arc::new(move |key: &str| {
        if key == "group:420051" {
            let _ = reached.send(Signal::Reached);
            // Returns when released, or when the test drops the sender while unwinding.
            let _ = release.lock().unwrap_or_else(|p| p.into_inner()).recv();
        }
    });
    *crate::social_state::SCOPE_AFTER_READ
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(hook);
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (first, second, during) = std::thread::scope(|threads| {
        // Owned by this closure, so a panic here drops it and frees the held admit.
        let release_tx = release_tx;
        let finished = FinishGuard(signal_tx);
        let first_context = context.clone();
        let first = threads.spawn(move || {
            let _finished = finished;
            conversation_scope_change(&first_context, "group:420051", "admit")
        });
        let held = matches!(signal.recv(), Ok(Signal::Reached));
        // While the first admit holds the gates, the second runs synchronously on this thread.
        let second = if held { Some(conversation_scope_change(&context, "group:420052", "admit")) } else { None };
        let during = std::fs::read(root.0.join("config/config.json")).unwrap();
        let _ = release_tx.send(());
        (first.join().unwrap(), second, during)
    });
    *crate::social_state::SCOPE_AFTER_READ.lock().unwrap_or_else(|p| p.into_inner()) = None;
    let second = second.expect("the first admit ended before it reached the point between its read and its write");
    assert!(second.is_err(), "the second admit must be refused while the first holds the gates: {second:?}");
    assert_eq!(second.unwrap_err().0, 503, "a busy gate refuses with 503");
    assert_eq!(during, before, "the refused second admit wrote nothing");
    assert_eq!(first.expect("the first admit applies")["changed"], json!(["allow.groups", "rollout.rustGroups"]));
    let config = file(&root);
    assert_eq!(config["allow"]["groups"], json!([420051]), "only the first admit was written");
    assert_eq!(config["rollout"]["rustGroups"], json!(["group:420051"]));
    let retry = conversation_scope_change(&context, "group:420052", "admit").expect("the retried second admit applies");
    assert_eq!(retry["changed"], json!(["allow.groups", "rollout.rustGroups"]));
    let config = file(&root);
    assert_eq!(config["allow"]["groups"], json!([420051, 420052]), "no update was lost");
    assert_eq!(config["rollout"]["rustGroups"], json!(["group:420051", "group:420052"]));
    assert!(crate::social_state::inbound_admitted(&context.load_config().unwrap(), "group:420052"));
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
    let original = file(&root);
    let (status, reply) = scope(&context, GROUP, "admit");
    assert_eq!(status, 400, "an admit cannot persist the malformed legacy rollout: {reply}");
    assert_eq!(file(&root), original);
    // Removal can match the old spelling and leave a valid list for the next launch.
    let (status, reply) = scope(&context, "group:100001", "remove");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(file(&root)["rollout"]["rustGroups"], json!(["group:100000"]));
    // The changed allow list moves to its canonical name and admission appends the new id.
    let (status, reply) = scope(&context, GROUP, "admit");
    assert_eq!(status, 200, "{reply}");
    let config = file(&root);
    let groups = config["allow"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1000);
    assert_eq!(groups[0], json!(100000));
    assert_eq!(groups[1], json!(100002));
    assert_eq!(groups[999], json!(420042));
    assert!(config["allow"].get("group").is_none(), "the alias spelling is folded into allow.groups");
    assert_eq!(config["allow"]["privates"], json!(["7001"]), "the other kind keeps its spelling");
    assert_eq!(config["rollout"]["rustGroups"], json!(["group:100000", GROUP]));
    let effective = context.load_config().unwrap();
    assert!(
        crate::social_state::inbound_admitted(&effective, "group:100999")
            && crate::social_state::inbound_admitted(&effective, GROUP)
    );
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
