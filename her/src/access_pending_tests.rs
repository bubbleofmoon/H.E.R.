// #192 console tests: an access change records its revocation before it writes `config.json`; a revocation cut off by
// a crash (or a failed step) is finished at console start or before the next access change, by its recorded policies.
use super::*;

const GROUP: &str = "group:420042";
const OLD_TOKEN: &str = "old-pending-token-abcdefghij";

/// A selected reserved2 fixture runtime: `allow.groups` and `rollout.rustGroups` list `GROUP`, which has an agent
/// token and a DSH mapping.
fn admitted(label: &str) -> (FixtureRoot, RuntimeContext) {
    let (root, _) = fixture_runtime(label, "fixture-t192-pending-token-abcdefghij");
    let path = root.0.join("config/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["allow"] = json!({"groups": [420042], "private": []});
    config["rollout"] = json!({"enabled": true, "rustGroups": [GROUP]});
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    std::fs::write(root.0.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
    std::fs::write(
        root.0.join("data/social-v2.json"),
        json!({"conversations": {GROUP: {"agentToken": OLD_TOKEN, "bootstrapSent": true}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.0.join("data/sessions.json"),
        json!({"sessions": {GROUP: "dsh-old-session"}, "sessionPolicies": {GROUP: "[\"reserved2\",\"qq-chat-v2\",null]"}}).to_string(),
    )
    .unwrap();
    let context = RuntimeContext::from_root(&root.0).unwrap();
    (root, context)
}

/// The raw `config.json` as written.
fn file(root: &FixtureRoot) -> Value {
    serde_json::from_slice(&std::fs::read(root.0.join("config/config.json")).unwrap()).unwrap()
}

/// The recorded pending revocations (none without the record file).
fn pending(root: &FixtureRoot) -> Vec<Value> {
    match std::fs::read(root.0.join("data").join(access::PENDING_REVOCATIONS)) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap()["pending"].as_array().cloned().unwrap(),
        Err(_) => Vec::new(),
    }
}

/// The `config.json` paths whose access changes stop (panic) between their configuration write and their revocation.
static CRASHING: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Run `change` with `context`'s access changes stopping between their configuration write and their revocation, as
/// a crash there would. True when the change stopped there.
fn crash_after_write<T>(context: &RuntimeContext, change: impl FnOnce() -> T) -> bool {
    let config = context.config_file().to_path_buf();
    *access::AFTER_ACCESS_WRITE.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(std::sync::Arc::new(|path: &std::path::Path| {
            if CRASHING.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|crashing| crashing == path) {
                panic!("t192: simulated crash between the configuration write and the revocation");
            }
        }));
    CRASHING.lock().unwrap_or_else(|p| p.into_inner()).push(config.clone());
    let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(change)).is_err();
    CRASHING.lock().unwrap_or_else(|p| p.into_inner()).retain(|crashing| crashing != &config);
    stopped
}

#[test]
fn t192_a_crash_between_the_write_and_the_revocation_is_finished_at_console_start() {
    let (root, context) = admitted("t192 crash scope");
    assert_eq!(
        access::finish_pending_revocations(&context).unwrap(),
        (Vec::new(), Vec::new()),
        "a runtime without the record has nothing pending"
    );
    assert!(crash_after_write(&context, || conversation_scope_change(&context, GROUP, "remove")), "the change stopped after its write");
    assert_eq!(file(&root)["allow"]["groups"], json!([]), "the configuration write happened");
    assert_eq!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN), "the crash came before the revocation");
    let recorded = pending(&root);
    assert_eq!(recorded.len(), 1, "the revocation was recorded before the write: {recorded:?}");
    assert_eq!(recorded[0]["why"], "access-revoked");
    assert_eq!(recorded[0]["mode"], "reserved2");
    assert_eq!(recorded[0]["before"]["allow"]["groups"], json!(["420042"]));
    assert_eq!(recorded[0]["after"]["allow"]["groups"], json!([]));
    // Console start finishes it.
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert_eq!((revoked, failures), (vec![GROUP.to_string()], Vec::<String>::new()));
    let rotated = conversation_token(&context, GROUP).unwrap();
    assert_ne!(rotated.as_deref(), Some(OLD_TOKEN), "the conversation token is rotated");
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None, "the DSH mapping is retired");
    assert!(pending(&root).is_empty(), "a finished revocation is no longer pending");
    // Running it again changes nothing.
    assert_eq!(access::finish_pending_revocations(&context).unwrap(), (Vec::new(), Vec::new()));
    // A re-grant keeps the fresh epoch; the old one never comes back.
    let reply = conversation_scope_change(&context, GROUP, "admit").expect("the re-grant applies");
    assert_eq!(reply["revoked"], json!([]), "{reply}");
    assert_eq!(conversation_token(&context, GROUP).unwrap(), rotated, "the re-grant keeps the fresh token");
    assert!(pending(&root).is_empty(), "a finished change leaves nothing pending");
}

#[test]
fn t192_a_whitelist_write_and_an_audit_undo_cut_off_after_their_write_are_finished_the_same_way() {
    let (root, context) = admitted("t192 crash whitelist undo");
    let body = json!({"allow": {"groups": [], "private": []}, "confirm": true}).to_string();
    assert!(crash_after_write(&context, || access::whitelist_post(&context, &body)), "the whitelist write stopped after its write");
    assert_eq!(file(&root)["allow"]["groups"], json!([]));
    assert_eq!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN));
    assert_eq!(pending(&root).len(), 1, "the whitelist write recorded its revocation first");
    // The next access change finishes it before its own write.
    let reply = conversation_scope_change(&context, "group:420043", "admit").expect("the next change applies");
    assert_eq!(reply["changed"], json!(["allow.groups", "rollout.rustGroups"]), "{reply}");
    assert_ne!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN), "the earlier revocation ran first");
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None);
    assert!(pending(&root).is_empty());

    // An audited admit, then its undo cut off after the restored configuration was written.
    let (status, reply) =
        admin_request(&context, "POST", "/api/conversations/scope", Some(&json!({"conversation": "group:420044", "action": "admit", "confirm": true})));
    assert_eq!(status, 200, "{reply}");
    mutate_social_state_for(&context, |state| {
        state["conversations"]["group:420044"] = json!({"agentToken": "old-undo-token-abcdefghij", "bootstrapSent": true});
        Ok(())
    })
    .unwrap();
    let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
    assert_eq!(status, 200, "{recent}");
    let id = recent["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "/api/conversations/scope" && e["ok"] == true)
        .and_then(|e| e["id"].as_str())
        .expect("the admit is in the change history")
        .to_string();
    let undo = json!({"id": id, "confirm": true}).to_string();
    assert!(crash_after_write(&context, || audit_undo(&context, &undo)), "the undo stopped after its write");
    assert!(!file(&root)["allow"]["groups"].as_array().unwrap().contains(&json!(420044)), "the undo wrote the restored configuration");
    let recorded = pending(&root);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["why"], "audit-undo");
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert_eq!((revoked, failures), (vec!["group:420044".to_string()], Vec::<String>::new()));
    assert_ne!(conversation_token(&context, "group:420044").unwrap().as_deref(), Some("old-undo-token-abcdefghij"));
    assert!(pending(&root).is_empty());
}

#[test]
fn t192_a_crash_before_the_write_revokes_with_a_fresh_epoch_and_keeps_the_access() {
    let (root, context) = admitted("t192 crash before write");
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    // #154: the change's first write after recording its revocation is the settings store's commit of the allow list.
    crate::settings::PANIC_BEFORE_COMMIT.with(|panic| panic.set(true));
    let stopped =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| conversation_scope_change(&context, GROUP, "remove"))).is_err();
    crate::settings::PANIC_BEFORE_COMMIT.with(|panic| panic.set(false));
    assert!(stopped, "the change stopped at its configuration write");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "the configuration was not written");
    assert_eq!(pending(&root).len(), 1, "the revocation was recorded before the write");
    // Finished as recorded (fail closed): the conversation keeps its access with a fresh epoch.
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert_eq!((revoked, failures), (vec![GROUP.to_string()], Vec::<String>::new()));
    assert!(inbound_admitted(&context.load_config().unwrap(), GROUP), "the configuration still admits it");
    assert_ne!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN));
    assert!(pending(&root).is_empty());
}

#[test]
fn t192_a_change_is_refused_while_an_earlier_revocation_cannot_finish() {
    let (root, context) = admitted("t192 refused");
    assert!(crash_after_write(&context, || conversation_scope_change(&context, GROUP, "remove")));
    // A damaged session map stops the revocation: its mappings cannot be read or retired.
    let sessions = root.0.join("data/sessions.json");
    let mapping = std::fs::read(&sessions).unwrap();
    std::fs::write(&sessions, "{ damaged").unwrap();
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let (status, reply) =
        admin_request(&context, "POST", "/api/conversations/scope", Some(&json!({"conversation": GROUP, "action": "admit", "confirm": true})));
    assert_eq!(status, 503, "{reply}");
    assert!(reply.to_string().contains("did not finish"), "{reply}");
    let (status, reply) = admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420042]}, "confirm": true})));
    assert_eq!(status, 503, "{reply}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before, "a refused change writes nothing");
    assert_eq!(pending(&root).len(), 1, "the unfinished revocation stays recorded");
    let (_, failures) = access::finish_pending_revocations(&context).unwrap();
    assert!(!failures.is_empty(), "console start reports the failure");
    assert_eq!(pending(&root).len(), 1);
    // Repaired, the next change finishes it first and then applies.
    std::fs::write(&sessions, &mapping).unwrap();
    let reply = conversation_scope_change(&context, GROUP, "admit").expect("the change applies once the revocation finished");
    assert_eq!(reply["changed"], json!(["allow.groups", "rollout.rustGroups"]), "{reply}");
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None, "the earlier revocation retired the mapping first");
    assert_ne!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN));
    assert!(pending(&root).is_empty());
    // A record file that is not a pending-revocation record refuses every access change.
    std::fs::write(root.0.join("data").join(access::PENDING_REVOCATIONS), "not json").unwrap();
    let before = std::fs::read(root.0.join("config/config.json")).unwrap();
    let error = conversation_scope_change(&context, GROUP, "remove").unwrap_err();
    assert_eq!(error.0, 503, "{error:?}");
    assert!(error.1.contains(access::PENDING_REVOCATIONS), "{error:?}");
    assert_eq!(std::fs::read(root.0.join("config/config.json")).unwrap(), before);
}

#[test]
fn t192_an_entry_without_mode_or_reason_is_finished_in_the_current_mode() {
    let (root, context) = admitted("t192 bare entry");
    std::fs::write(
        root.0.join("data").join(access::PENDING_REVOCATIONS),
        json!({"pending": [{"id": "revoke-t192-bare", "before": {"allow": {"groups": [420042]}}, "after": {"allow": {"groups": []}}}]})
            .to_string(),
    )
    .unwrap();
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert_eq!((revoked, failures), (vec![GROUP.to_string()], Vec::<String>::new()));
    assert_ne!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN));
    assert!(pending(&root).is_empty());
}

#[test]
fn t192_console_start_keeps_the_record_while_the_interaction_gate_is_busy() {
    let (root, context) = admitted("t192 busy gate");
    assert!(crash_after_write(&context, || conversation_scope_change(&context, GROUP, "remove")));
    // Another holder keeps the closed-agent interaction gate past the five-second wait.
    let held = crate::closed_agent::lock(&context).unwrap();
    let error = access::finish_pending_revocations(&context).unwrap_err();
    assert!(error.contains("closed-agent interaction lock unavailable"), "{error}");
    assert_eq!(pending(&root).len(), 1, "nothing is finished without the gate");
    assert_eq!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN));
    drop(held);
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert_eq!((revoked, failures), (vec![GROUP.to_string()], Vec::<String>::new()));
    assert!(pending(&root).is_empty());
}

/// Fail the `nth` guarded-write step named `failing` on this thread. A first change of the allow list writes, in
/// order: the pending record, the settings store's `config.json.pre-settings` copy, the store's write-back of
/// `config.json` and the change's own `config.json` (#154).
fn fail_config_write_at(failing: &'static str, nth: u32) {
    let mut seen = 0u32;
    crate::guarded_file::STEP_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |step: &'static str| -> std::io::Result<()> {
            if step == failing {
                seen += 1;
                if seen == nth {
                    return Err(std::io::Error::other("t192: injected write failure"));
                }
            }
            Ok(())
        }))
    });
}

fn clear_write_failures() {
    crate::guarded_file::STEP_HOOK.with(|slot| *slot.borrow_mut() = None);
}

#[test]
fn t192_a_failed_config_write_keeps_its_revocation_unless_nothing_was_published() {
    let (root, context) = admitted("t192 failed write");
    let config = root.0.join("config/config.json");
    let before = std::fs::read(&config).unwrap();
    // Before the store takes the change (the pre-settings copy's temporary file sync): nothing changed, so nothing stays
    // pending.
    fail_config_write_at("temp/sync", 2);
    let refused = conversation_scope_change(&context, GROUP, "remove");
    clear_write_failures();
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(std::fs::read(&config).unwrap(), before, "nothing was published");
    assert!(pending(&root).is_empty(), "a write that never landed leaves nothing pending");
    // After the store committed the change (the change's own `config.json` folder sync): the new policy is in force, so
    // its revocation stays pending.
    fail_config_write_at("dir/sync", 4);
    let failed = conversation_scope_change(&context, GROUP, "remove");
    clear_write_failures();
    let error = failed.unwrap_err();
    assert!(error.1.contains("revocation stays pending"), "{error:?}");
    assert_eq!(file(&root)["allow"]["groups"], json!([]), "the configuration was published");
    assert_eq!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN));
    assert_eq!(pending(&root).len(), 1, "the published change keeps its pending revocation");
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert_eq!((revoked, failures), (vec![GROUP.to_string()], Vec::<String>::new()));
    assert_ne!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN));
    assert!(pending(&root).is_empty());
}

#[test]
fn t192_an_entry_without_usable_policies_refuses_access_changes() {
    let (root, context) = admitted("t192 unusable entry");
    let record = root.0.join("data").join(access::PENDING_REVOCATIONS);
    let config = root.0.join("config/config.json");
    for entry in [
        json!({}),
        json!({"id": "revoke-t192-half", "before": {"allow": {"groups": [420042]}}}),
        json!({"id": "revoke-t192-list", "before": {"allow": [420042]}, "after": {}}),
    ] {
        let text = json!({"pending": [entry.clone()]}).to_string();
        std::fs::write(&record, &text).unwrap();
        let before = std::fs::read(&config).unwrap();
        let error = conversation_scope_change(&context, "group:420043", "admit").unwrap_err();
        assert_eq!(error.0, 503, "{entry}: {error:?}");
        assert!(error.1.contains("no usable before/after policy"), "{entry}: {error:?}");
        assert_eq!(std::fs::read(&config).unwrap(), before, "{entry}: nothing is written");
        assert!(access::finish_pending_revocations(&context).is_err(), "{entry}: console start reports it");
        assert_eq!(std::fs::read_to_string(&record).unwrap(), text, "{entry}: the record is kept for repair");
        assert_eq!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN), "{entry}: nothing is revoked");
    }
}

/// The recorded DSH session stops that have not finished.
fn stops(root: &FixtureRoot) -> Vec<Value> {
    match std::fs::read(root.0.join("data").join(access::PENDING_REVOCATIONS)) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap()["stops"].as_array().cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn t192_a_retired_session_stays_recorded_until_its_stop_finished() {
    const SESSION: &str = "dsh-t192-recorded-stop";
    static STOP_WORKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let (root, context) = admitted("t192 recorded stop");
    std::fs::write(
        root.0.join("data/sessions.json"),
        json!({"sessions": {GROUP: SESSION}, "sessionPolicies": {GROUP: "[\"reserved2\",\"qq-chat-v2\",null]"}}).to_string(),
    )
    .unwrap();
    // Only this test's session: its DSH stop fails until the test lets it succeed.
    *access::STOP_SEAM.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::sync::Arc::new(|session: &str| {
        (session == SESSION).then(|| {
            if STOP_WORKS.load(std::sync::atomic::Ordering::SeqCst) { Ok(()) } else { Err("t192: DSH unreachable".to_string()) }
        })
    }));
    // The CLI entry waits for its background stops, so the failed stop has ended when it returns.
    let reply = conversation_scope_change(&context, GROUP, "remove").expect("the remove applies");
    assert_eq!(reply["revoked"], json!([GROUP]), "{reply}");
    assert!(pending(&root).is_empty(), "the revocation itself finished");
    assert_eq!(crate::session_map::lookup(&context, GROUP).unwrap(), None, "the mapping is retired");
    assert_eq!(stops(&root), [json!({"key": GROUP, "session": SESSION})], "the failed stop stays recorded");
    // Console start runs it again; once it succeeds, the record is gone.
    STOP_WORKS.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(access::finish_pending_revocations(&context).unwrap(), (Vec::new(), Vec::new()));
    assert!(access::wait_background_stops(std::time::Duration::from_secs(15)), "the stop ended");
    assert!(stops(&root).is_empty(), "a finished stop is no longer recorded");
}

#[test]
fn t192_an_owner_change_retried_after_a_crash_still_selects_a_retired_mapping() {
    let (root, context) = admitted("t192 owner retry");
    // State a crash leaves between `retire_if` and the token rotation: an owner change in closed-agent mode, the other
    // conversation's mapping already retired and its stop recorded, its token still the old one.
    std::fs::write(
        root.0.join("data").join(access::PENDING_REVOCATIONS),
        json!({
            "pending": [{"id": "revoke-t192-owner", "why": "access-revoked", "mode": "closed-agent",
                         "before": {"ownerQQ": 8808}, "after": {"ownerQQ": 8809}}],
            "stops": [{"key": "group:420046", "session": "dsh-t192-owner-stop"}]
        })
        .to_string(),
    )
    .unwrap();
    mutate_social_state_for(&context, |state| {
        state["conversations"]["group:420046"] = json!({"agentToken": "old-owner-token-abcdefghij", "bootstrapSent": true});
        Ok(())
    })
    .unwrap();
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert!(failures.is_empty(), "{failures:?}");
    assert!(revoked.contains(&"group:420046".to_string()), "the recorded stop still selects it: {revoked:?}");
    assert_ne!(
        conversation_token(&context, "group:420046").unwrap().as_deref(),
        Some("old-owner-token-abcdefghij"),
        "its token is rotated before the entry clears"
    );
    assert!(pending(&root).is_empty());
}

#[test]
fn t192_revocation_targets_are_recorded_before_the_steps_and_kept_until_all_succeed() {
    let (root, context) = admitted("t192 targets kept");
    assert!(crash_after_write(&context, || conversation_scope_change(&context, GROUP, "remove")));
    assert!(pending(&root)[0].get("targets").is_none(), "no revocation attempt ran yet");
    // A damaged session map makes the attempt fail after its selection.
    let sessions = root.0.join("data/sessions.json");
    let mapping = std::fs::read(&sessions).unwrap();
    std::fs::write(&sessions, "{ damaged").unwrap();
    let (_, failures) = access::finish_pending_revocations(&context).unwrap();
    assert!(!failures.is_empty(), "the attempt failed");
    assert_eq!(pending(&root)[0]["targets"], json!([GROUP]), "its targets were recorded before its steps and stay after the failure");
    std::fs::write(&sessions, &mapping).unwrap();
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert_eq!((revoked, failures), (vec![GROUP.to_string()], Vec::<String>::new()));
    assert!(pending(&root).is_empty(), "every step succeeded: the entry and its targets are gone");
}

#[test]
fn t192_a_retried_owner_change_selects_its_recorded_targets_after_the_stop_cleared() {
    let (root, context) = admitted("t192 owner targets");
    // What the gate review describes: an attempt recorded `group:420046` as a target, retired its mapping, failed to
    // rotate its token, and its DSH stop then finished, so neither a mapping nor a stop names it any more.
    std::fs::write(
        root.0.join("data").join(access::PENDING_REVOCATIONS),
        json!({"pending": [{"id": "revoke-t192-targets", "why": "access-revoked", "mode": "closed-agent",
                            "before": {"ownerQQ": 8808}, "after": {"ownerQQ": 8809}, "targets": ["group:420046"]}]})
        .to_string(),
    )
    .unwrap();
    mutate_social_state_for(&context, |state| {
        state["conversations"]["group:420046"] = json!({"agentToken": "old-target-token-abcdefghij", "bootstrapSent": true});
        Ok(())
    })
    .unwrap();
    let (revoked, failures) = access::finish_pending_revocations(&context).unwrap();
    assert!(failures.is_empty(), "{failures:?}");
    assert!(revoked.contains(&"group:420046".to_string()), "the recorded target is selected again: {revoked:?}");
    assert_ne!(
        conversation_token(&context, "group:420046").unwrap().as_deref(),
        Some("old-target-token-abcdefghij"),
        "its token is rotated before the entry clears"
    );
    assert!(pending(&root).is_empty());
}

#[test]
fn t192_a_malformed_stop_record_refuses_access_changes() {
    let (root, context) = admitted("t192 malformed stops");
    let record = root.0.join("data").join(access::PENDING_REVOCATIONS);
    let config = root.0.join("config/config.json");
    for stops in [
        json!([{"key": "group:420046"}]),
        json!({"key": "group:420046", "session": "dsh-t192-malformed"}),
        json!([{"key": 420046, "session": "dsh-t192-malformed"}]),
    ] {
        let text = json!({"pending": [], "stops": stops.clone()}).to_string();
        std::fs::write(&record, &text).unwrap();
        let before = std::fs::read(&config).unwrap();
        let error = conversation_scope_change(&context, "group:420043", "admit").unwrap_err();
        assert_eq!(error.0, 503, "{stops}: {error:?}");
        assert!(error.1.contains("malformed stop record"), "{stops}: {error:?}");
        assert_eq!(std::fs::read(&config).unwrap(), before, "{stops}: nothing is written");
        assert!(access::finish_pending_revocations(&context).is_err(), "{stops}: console start reports it");
        assert_eq!(std::fs::read_to_string(&record).unwrap(), text, "{stops}: the record is kept for repair");
    }
    // A record without the field (every record written before run 6) stays accepted.
    std::fs::write(&record, json!({"pending": []}).to_string()).unwrap();
    conversation_scope_change(&context, "group:420043", "admit").expect("a record without stops accepts changes");
}

#[test]
fn t192_duplicate_pending_ids_refuse_access_changes() {
    let (root, context) = admitted("t192 duplicate ids");
    let record = root.0.join("data").join(access::PENDING_REVOCATIONS);
    let config = root.0.join("config/config.json");
    // Two entries share the id "same": the first selects nothing, the second selects `GROUP` for its token rotation.
    let text = json!({"pending": [
        {"id": "same", "mode": "reserved2", "before": {}, "after": {}},
        {"id": "same", "mode": "reserved2", "before": {"allow": {"groups": [420042]}}, "after": {"allow": {"groups": []}},
         "targets": [GROUP]}
    ]})
    .to_string();
    std::fs::write(&record, &text).unwrap();
    let before = std::fs::read(&config).unwrap();
    let error = conversation_scope_change(&context, "group:420043", "admit").unwrap_err();
    assert_eq!(error.0, 503, "{error:?}");
    assert_eq!(std::fs::read(&config).unwrap(), before, "nothing is written");
    assert_eq!(std::fs::read_to_string(&record).unwrap(), text, "the record is kept for repair");
    assert_eq!(conversation_token(&context, GROUP).unwrap().as_deref(), Some(OLD_TOKEN), "nothing is revoked");
    assert!(access::finish_pending_revocations(&context).is_err(), "console start reports it");
}

#[test]
fn t192_a_noncanonical_target_refuses_access_changes() {
    let (root, context) = admitted("t192 noncanonical target");
    let record = root.0.join("data").join(access::PENDING_REVOCATIONS);
    let config = root.0.join("config/config.json");
    // A hand-edited target with a trailing space names no conversation the token lookup finds.
    let text = json!({"pending": [{
        "id": "one", "mode": "closed-agent", "before": {"ownerQQ": 8808}, "after": {"ownerQQ": 8809},
        "targets": ["group:420046 "]
    }]})
    .to_string();
    std::fs::write(&record, &text).unwrap();
    mutate_social_state_for(&context, |state| {
        state["conversations"]["group:420046"] = json!({"agentToken": "old-noncanon-token-abcdefghij", "bootstrapSent": true});
        Ok(())
    })
    .unwrap();
    let before = std::fs::read(&config).unwrap();
    let error = conversation_scope_change(&context, "group:420043", "admit").unwrap_err();
    assert_eq!(error.0, 503, "{error:?}");
    assert_eq!(std::fs::read(&config).unwrap(), before, "nothing is written");
    assert_eq!(std::fs::read_to_string(&record).unwrap(), text, "the record is kept for repair");
    assert_eq!(
        conversation_token(&context, "group:420046").unwrap().as_deref(),
        Some("old-noncanon-token-abcdefghij"),
        "nothing is revoked"
    );
}
