// Ticket 24 hub tests: access changes, the whole reset and manual wakes act on queued work.
use super::*;

fn abort_all(hub: &mut Hub) -> HashMap<String, String> {
    let ran = hub.active.iter().map(|(k, a)| (k.clone(), a.reason.clone())).collect();
    for active in hub.active.values() { active.task.abort(); }
    hub.active.clear();
    ran
}

#[tokio::test]
async fn a_revoked_then_regranted_conversation_never_runs_its_queued_wake() {
    let (root, mut hub) = queued_hub("t24-revoke-undo").await;
    let (revoked, kept) = ("group:420042", "group:420043");
    // Both conversations have a queued wake and awaited ingress. Revoke one, then undo it, all
    // before the scheduler's next tick.
    let (status, reply) = crate::console::admin_request(&hub.context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420043]}})));
    assert_eq!(status, 200, "{reply}");
    let (_, recent) = crate::console::admin_request(&hub.context, "GET", "/api/audit/recent", None);
    let id = recent["entries"][0]["id"].clone();
    let (status, reply) = crate::console::admin_request(&hub.context, "POST", "/api/audit/undo", Some(&json!({"id": id})));
    assert_eq!(status, 200, "{reply}");
    assert!(crate::console::key_allowed(&hub.context.load_config().unwrap(), revoked), "undo restored access");
    hub.tick("bot", 9001).await.unwrap();
    let ran = abort_all(&mut hub);
    let event = format!("{revoked}|message:1");
    let stage = hub.journal.run(move |j| j.stage_of(&event)).await.unwrap();
    drop(hub);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(ran.get(kept).map(String::as_str), Some("atMention"), "the untouched conversation keeps its wake");
    assert_eq!(ran.get(revoked), None, "the wake queued before the revocation ran after the undo");
    assert_eq!(stage.as_deref(), Some("handled"), "the revoked conversation's queued ingress must be settled, not replayed");
}

#[tokio::test]
async fn the_whole_reset_drops_queued_held_and_retried_work_including_token_less_conversations() {
    let (root, mut hub) = queued_hub("t24-whole-reset").await;
    let (a, b) = ("group:420042", "group:420043");
    // b was imported without a token (no epoch minted yet); a also has a held and a retried wake.
    let mut state = hub.state().unwrap();
    state["conversations"][b].as_object_mut().unwrap().remove("agentToken");
    state["paused"] = json!(true);
    hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    hub.tokens.insert(b.into(), String::new());
    hub.held.insert(a.into(), "timeout".into());
    hub.retries.insert(a.into(), (0, "replyCheck".into()));
    let (status, reply) = crate::console::admin_request(&hub.context, "POST", "/api/socialV2/reset", Some(&json!({})));
    assert_eq!((status, reply.clone()), (200, json!({"ok": true})));
    let state = hub.state().unwrap();
    assert_eq!(state["conversations"], json!({}));
    assert_eq!(state.get("paused"), None, "the reset clears the pause (JS paused=false)");
    hub.tick("bot", 9001).await.unwrap();
    hub.tick("bot", 9001).await.unwrap();
    let ran = abort_all(&mut hub);
    let stages = {
        let mut out = Vec::new();
        for key in [a, b] {
            let event = format!("{key}|message:1");
            out.push(hub.journal.run(move |j| j.stage_of(&event)).await.unwrap());
        }
        out
    };
    let after = hub.state().unwrap();
    let held_or_retried = !hub.held.is_empty() || !hub.retries.is_empty() || !hub.ready.is_empty() || !hub.awaiting.is_empty();
    drop(hub);
    std::fs::remove_dir_all(root).unwrap();
    assert!(ran.is_empty(), "work queued before the reset ran: {ran:?}");
    assert!(!held_or_retried, "the scheduler still holds work of the reset epoch");
    assert_eq!(stages, vec![Some("handled".to_string()), Some("handled".to_string())]);
    assert_eq!(after["conversations"], json!({}), "nothing recreated a conversation the reset removed");
}

#[tokio::test]
async fn a_manual_wake_bootstraps_once_then_wakes_as_admin_and_buffers_while_busy() {
    let (root, mut hub) = queued_hub("t24-manual-wake").await;
    let key = "group:420042";
    hub.ready.clear();
    hub.awaiting.clear();
    let mut state = hub.state().unwrap();
    state["conversations"][key]["bootstrapSent"] = json!(false);
    hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    hub.tick("bot", 9001).await.unwrap();
    let wake = |context: &RuntimeContext| crate::console::admin_request(context, "POST", "/api/socialV2/wake", Some(&json!({"key": key, "reason": "operator"})));
    let (status, reply) = wake(&hub.context);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply, json!({"ok": true, "key": key, "reason": "bootstrap", "requestedReason": "operator", "firstWake": true}));
    hub.tick("bot", 9001).await.unwrap();
    assert_eq!(hub.active.get(key).map(|a| a.reason.as_str()), Some("bootstrap"), "the first manual wake is a bootstrap wake");
    // Keep that turn running: the fixture DSH is unreachable and would end it at once.
    let first = hub.active.remove(key).unwrap();
    first.task.abort();
    hub.active.insert(key.into(), Active { task: tokio::spawn(async { std::future::pending::<()>().await; (Ok(0), false) }), ..first });
    // While that turn runs, two identical manual wakes are buffered once (JS busy queue).
    let (_, reply) = wake(&hub.context);
    assert_eq!(reply["reason"], "operator");
    assert_eq!(reply["firstWake"], false);
    hub.tick("bot", 9001).await.unwrap();
    wake(&hub.context);
    hub.tick("bot", 9001).await.unwrap();
    let pending: Vec<String> = hub.drivers[key].sched.pending.iter().map(|p| p.reason.clone()).collect();
    assert_eq!(pending, vec!["operator".to_string()]);
    // A request made under an epoch that ended is dropped.
    abort_all(&mut hub);
    wake(&hub.context);
    console::ensure_or_rotate_agent_token(&hub.context, key, true).unwrap();
    hub.tick("bot", 9001).await.unwrap();
    let ran = abort_all(&mut hub);
    let queued = hub.state().unwrap()["conversations"][key].get("adminWakes").cloned();
    drop(hub);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(ran.get(key), None, "a manual wake of the previous epoch ran");
    assert_eq!(queued, None, "the stale request must be consumed");
}

/// S2: a whole reset while a turn runs keeps that turn attached (cancelled, ended through the
/// normal path), and no second turn of the same conversation starts meanwhile.
#[tokio::test]
async fn a_whole_reset_keeps_a_running_turn_attached_until_it_ends() {
    let (root, mut hub) = queued_hub("t24-reset-running").await;
    let key = "group:420042";
    hub.ready.retain(|(k, _)| k != key);
    let event = hub.awaiting.remove(key).unwrap();
    let state = hub.state().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    hub.active.insert(key.into(), Active {
        task: tokio::spawn(async move { let _ = wait.await; (Ok(0), false) }),
        inbound: event.clone(), reason: "operatorMarker".into(), previous: state["conversations"][key].clone(),
        at: crate::now_ms() as i64, token: "test-token".into(), mode_stamp: hub.mode_stamp.clone(), session: Default::default(), minted: Default::default(), cap_abort_at: None, text_only: false,
    });
    let (status, reply) = crate::console::admin_request(&hub.context, "POST", "/api/socialV2/reset", Some(&json!({})));
    assert_eq!(status, 200, "{reply}");
    hub.tick("bot", 9001).await.unwrap();
    let attached = hub.active.contains_key(key);
    // A new message of the same conversation arrives while the old turn still runs.
    let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,"message_id":24001,"message":[{"type":"at","data":{"qq":"9001"}},{"type":"text","data":{"text":" after reset"}}]});
    let ev: crate::Event = serde_json::from_value(frame.clone()).unwrap();
    let _ = hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await;
    hub.tick("bot", 9001).await.unwrap();
    let concurrent = hub.active.get(key).map(|a| a.reason.clone());
    let _ = release.send(());
    for active in hub.active.values() { active.task.abort(); }
    drop(hub);
    std::fs::remove_dir_all(root).unwrap();
    assert!(attached, "the reset detached the running turn (its task keeps running unobserved)");
    assert_eq!(concurrent.as_deref(), Some("operatorMarker"), "the running turn must stay the only one of {key}");
}

/// Codex 5: a wake that passed the tick's validity check and then waits for the journal must not
/// be dispatched under a token minted meanwhile (revoke and re-grant during the wait).
#[tokio::test]
async fn a_wake_whose_token_changed_while_it_waited_for_the_journal_is_not_dispatched() {
    let (root, mut hub) = queued_hub("t24-claim-epoch").await;
    let (key, other) = ("group:420042", "group:420043");
    hub.ready.retain(|(k, _)| k == key);
    hub.awaiting.remove(other);
    hub.tokens.insert(key.into(), "test-token".into());
    let journal = hub.journal.clone();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel::<()>();
    let blocker = tokio::spawn(async move {
        journal
            .run(move |_| {
                let _ = locked_tx.send(());
                let _ = hold_rx.recv_timeout(std::time::Duration::from_secs(20));
                Ok(())
            })
            .await
    });
    locked_rx.await.unwrap();
    let context = hub.context.clone();
    let admin = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let result = tokio::task::spawn_blocking(move || {
            let (status, reply) = crate::console::admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": [420043]}})));
            assert_eq!(status, 200, "{reply}");
            let (_, recent) = crate::console::admin_request(&context, "GET", "/api/audit/recent", None);
            let id = recent["entries"][0]["id"].clone();
            let (status, reply) = crate::console::admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": id})));
            assert_eq!(status, 200, "{reply}");
        })
        .await;
        let _ = hold_tx.send(());
        result
    });
    hub.tick("bot", 9001).await.unwrap();
    admin.await.unwrap().unwrap();
    blocker.await.unwrap().unwrap();
    let ran = abort_all(&mut hub);
    let event = format!("{key}|message:1");
    let stage = hub.journal.run(move |j| j.stage_of(&event)).await.unwrap();
    drop(hub);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(ran.get(key), None, "the wake queued under the revoked token was dispatched under the new one");
    assert_eq!(stage.as_deref(), Some("handled"), "its ingress must be settled, not replayed");
}

/// Codex 8: the reset cleanup must not cancel a session that replaced the reset one before the
/// scheduler observed the reset (a new turn after a switch to chat).
#[tokio::test]
async fn a_reset_observed_late_never_cancels_the_replacement_session() {
    let (root, mut hub) = queued_hub("t24-reset-replacement").await;
    let key = "group:420042";
    hub.ready.retain(|(k, _)| k == key);
    hub.tokens.insert(key.into(), "test-token".into());
    // A DSH credential, so the scheduler's cleanup pipeline can be built (the DSH stays unreachable).
    std::fs::write(root.join("user/dsh/.credentials.yaml"), "secret: \"JCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQ\"\n").unwrap();
    // A turn of the old epoch runs (the fixture DSH is unreachable; it stays attached until the
    // scheduler collects it).
    hub.tick("bot", 9001).await.unwrap();
    assert!(hub.active.contains_key(key));
    let (status, reply) = crate::console::admin_request(&hub.context, "POST", "/api/socialV2/reset", Some(&json!({})));
    assert_eq!(status, 200, "{reply}");
    // Before the scheduler's next tick a new turn binds a replacement session and token.
    let token = console::ensure_or_rotate_agent_token(&hub.context, key, false).unwrap();
    crate::session_map::bind(&hub.context, key, "replacement-session", None).unwrap();
    let turn = crate::turn_ledger::Recorder::begin(&hub.context, key, "dsh", "chat", "epoch", &[], None).unwrap();
    hub.tick("bot", 9001).await.unwrap();
    abort_all(&mut hub);
    let token_after = console::conversation_token(&hub.context, key).unwrap();
    let mapping = crate::session_map::lookup(&hub.context, key).unwrap();
    let (turns, _) = crate::turn_ledger::fold(&crate::turn_ledger::read_records(&hub.context).unwrap());
    let state = turns.iter().find(|t| t.id == turn.id).unwrap().state;
    drop(hub);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(token_after.as_deref(), Some(token.as_str()), "the reset cleanup rotated the replacement session's token");
    assert_eq!(mapping.as_deref(), Some("replacement-session"));
    assert!(state.is_open(), "the reset cleanup cancelled the replacement session's turn: {state:?}");
}
