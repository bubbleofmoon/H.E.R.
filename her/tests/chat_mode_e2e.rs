//! ticket 12 external-behaviour acceptance: chat mode (ordinary chat in allowed groups and private
//! conversations), the public slash commands, status and reset, the automatic reply with its
//! duplicate-forward suppression, the failure paths that must stay out of groups, and the console
//! mode switch that has to change what the running bridge does.
//!
//! Everything runs against real `her` processes (live bridge + `--console-serve`) on a
//! synthetic runtime root, a fake DSH whose simulated model follows markers in the prompt, and a
//! fake OneBot gateway (WebSocket for events, HTTP for actions). No real QQ, model or DSH endpoint
//! is contacted.

include!("support/chat.rs");

// The fake records receipt arrival before replying. Await HER's published state
// separately: an approval is absent while its receipt is in flight, then retained.
fn wait_pending(rig: &Rig, expected: Value) {
    let mut actual = (0, Value::Null);
    let settled = wait_for(Duration::from_secs(10), || {
        actual = rig.console("GET", "/api/pending", None);
        actual.0 == 200 && actual.1 == expected
    });
    assert!(settled, "pending state did not settle: expected {expected}, got {actual:?}");
}

// ───────────────────────── AC1: filtering, input, model events, auto reply ─────────────────────────

#[test]
fn closed_agent_only_owner_private_uses_default_full_agent_preset() {
    let mut rig = Rig::new("closed-policy", Seed {
        mode: "closed-agent",
        // Closed-agent admission is the owner private policy, independent of chat allow lists.
        config: json!({"allow": {"groups": [GROUP_ID], "private": [STRANGER]}}),
        ..Seed::default()
    });
    rig.start();
    says(&rig, GROUP_KEY, OWNER, "owner", "group must not get full agent");
    says(&rig, "private:7707", STRANGER, "stranger", "private must not get full agent");
    says(&rig, OWNER_KEY, OWNER, "owner", "closed owner request");
    assert_eq!(rig.wait_texts(OWNER_KEY, 2), vec![ACK, "synthetic reply"]);
    assert_eq!(rig.dsh.prompts().len(), 1);
    assert_eq!(body_of(&rig.dsh.prompts()[0].text), "【管理员】closed owner request");
    let created = rig.dsh.calls("session/create");
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["request"]["agentPreset"], "ticket13-full");
    assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending": []})));
    assert!(rig.texts_to(GROUP_KEY).is_empty());
    assert!(rig.texts_to("private:7707").is_empty());
}

#[test]
fn closed_agent_question_round_trip_continues_original_turn() {
    let mut rig = Rig::new("closed-question", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(12), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("Choose a color"))), "question was not delivered: {:?}", rig.sends());
    assert_eq!(rig.console("GET", "/api/pending", None), (200, json!({"pending":[{"key":OWNER_KEY,"kind":"question","sessionId":"fake-session-1","questions":[{"id":"q1","question":"Choose a color","options":[{"label":"Blue"},{"label":"Red"}]}]}]})));
    says(&rig, OWNER_KEY, OWNER, "owner", "blue");
    assert!(wait_for(Duration::from_secs(12), || !rig.dsh.calls("$events/result").is_empty()), "answer did not reach DSH");
    assert_eq!(rig.dsh.calls("$events/result"), vec![json!({"clientId":"ticket13-client", "eventId":"question-13", "outcome":{"kind":"result","value":{"answers":[{"id":"q1","selected":["Blue"]}]}}})]);
    assert!(wait_for(Duration::from_secs(12), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")), "turn did not continue");
    assert_eq!(rig.dsh.prompts().len(), 1, "the answer must not start another turn");
    wait_pending(&rig, json!({"pending":[]}));
}

#[test]
fn closed_agent_approval_accept_and_reject_have_one_receipt_each() {
    for (tag, answer, outcome) in [("allow", "通过", "allowed-once"), ("deny", "拒绝", "rejected")] {
        let mut rig = Rig::new(&format!("closed-approval-{tag}"), Seed { mode: "closed-agent", ..Seed::default() });
        rig.start();
        says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
        assert!(wait_for(Duration::from_secs(12), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("file_write"))), "approval was not delivered");
        let (_, pending) = rig.console("GET", "/api/pending", None);
        // Frozen JS shows the reason in the message but stores only the tool name
        // in pending, so undefined reason/approvalId are omitted from its JSON.
        assert_eq!(pending, json!({"pending":[{"key":OWNER_KEY,"kind":"approval","sessionId":"fake-session-1","toolName":"file_write"}]}));
        says(&rig, OWNER_KEY, OWNER, "owner", answer);
        assert!(wait_for(Duration::from_secs(12), || rig.dsh.calls("$events/result").len() == 1), "approval receipt missing");
        assert_eq!(rig.dsh.calls("$events/result"), vec![json!({"clientId":"ticket13-client","eventId":"approval-1","outcome":{"kind":"result","value":outcome}})]);
        assert!(wait_for(Duration::from_secs(12), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")), "turn did not continue");
        let messages = rig.texts_to(OWNER_KEY);
        let interaction: Vec<_> = messages.iter().filter(|s| s.as_str() != ACK).map(String::as_str).collect();
        assert_eq!(interaction, vec!["🔐 agent 请求审批：file_write\n理由：save report\n回复「通过」或「拒绝」", "synthetic reply"], "the frozen JS sends no approval confirmation");
        assert_eq!(rig.dsh.prompts().len(), 1);
        wait_pending(&rig, pending); // The frozen JS retains answered approvals until timeout or reset.
    }
}

#[test]
fn closed_agent_answered_approval_can_be_answered_again_until_timeout() {
    let mut rig = Rig::new("closed-approval-lifetime", Seed { mode: "closed-agent", config: json!({"questionTimeoutMs":5000}), ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    let pending = rig.console("GET", "/api/pending", None).1;
    says(&rig, OWNER_KEY, OWNER, "owner", "通过");
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")));
    wait_pending(&rig, pending);
    says(&rig, OWNER_KEY, OWNER, "owner", "拒绝");
    assert!(wait_for(Duration::from_secs(3), || rig.dsh.calls("$events/result").len() == 2));
    assert_eq!(rig.dsh.calls("$events/result")[1]["outcome"]["value"], "rejected");
    assert_eq!(rig.dsh.prompts().len(), 1, "the retained approval consumes the next answer");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1 == json!({"pending":[]})), "the entry must expire");
}

#[test]
fn closed_agent_expired_question_is_cancelled_once_and_clears_console() {
    let mut rig = Rig::new("closed-expiry", Seed { mode: "closed-agent", config: json!({"questionTimeoutMs":1500}), ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)), "pending question never appeared");
    assert!(wait_for(Duration::from_secs(10), || !rig.dsh.calls("$events/result").is_empty()), "expiration did not cancel question");
    assert_eq!(rig.dsh.calls("$events/result"), vec![json!({"clientId":"ticket13-client","eventId":"question-13","outcome":{"kind":"result","value":{"answers":[]}}})]);
    wait_pending(&rig, json!({"pending":[]}));
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "⏰ 等待回答超时，已取消该请求（private:8808）")), "expiry notification missing");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(rig.dsh.calls("$events/result").len(), 1);
}

#[test]
fn closed_agent_duplicate_answer_event_sends_one_result() {
    let mut rig = Rig::new("closed-duplicate", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    let answer = private_event(131313, OWNER, "owner", vec![text_seg("blue")]);
    rig.push(answer.clone());
    rig.push(answer);
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")));
    assert_eq!(rig.dsh.calls("$events/result").len(), 1);
    assert_eq!(rig.dsh.prompts().len(), 1);
    wait_pending(&rig, json!({"pending":[]}));
}

#[test]
fn closed_agent_duplicate_approval_event_sends_one_result() {
    let mut rig = Rig::new("closed-duplicate-approval", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    let answer = private_event(131314, OWNER, "owner", vec![text_seg("通过")]);
    rig.push(answer.clone());
    rig.push(answer);
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")));
    let receipts = rig.dsh.calls("$events/result");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["outcome"]["value"], "allowed-once");
    assert_eq!(rig.dsh.prompts().len(), 1);
    wait_pending(&rig, json!({"pending":[{"key":OWNER_KEY,"kind":"approval","sessionId":"fake-session-1","toolName":"file_write"}]}));
}

#[test]
fn closed_agent_cancelled_turn_keeps_request_until_answer_or_timeout() {
    for (tag, marker, answer, expected) in [
        ("question", "#QUESTION#", "blue", json!({"answers":[{"id":"q1","selected":["Blue"]}]})),
        ("approval", "#APPROVAL#", "通过", json!("allowed-once")),
    ] {
        let mut rig = Rig::new(&format!("closed-cancelled-{tag}"), Seed { mode: "closed-agent", ..Seed::default() });
        rig.start();
        says(&rig, OWNER_KEY, OWNER, "owner", marker);
        assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)), "{tag} never appeared");
        let pending = rig.console("GET", "/api/pending", None).1;
        {
            // The external DSH cancels the active turn while its interaction is pending.
            let mut inner = lock(&rig.dsh.inner);
            let session = inner.sessions.get_mut("fake-session-1").unwrap();
            session.cancelled = true;
            event(session, "turn/end", json!({"turn": 1, "reason": {"kind":"aborted"}}));
        }
        assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "⏹️ 已停止")));
        assert!(wait_for(Duration::from_secs(10), || rig.ledger().iter().any(|r| r["k"] == "ended")), "{tag} turn did not finish: {:?}", rig.ledger());
        assert!(rig.dsh.calls("$events/result").is_empty(), "turn end alone must not send a receipt");
        assert_eq!(rig.console("GET", "/api/pending", None).1, pending);
        says(&rig, OWNER_KEY, OWNER, "owner", answer);
        assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
        assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], expected);
        assert_eq!(rig.dsh.prompts().len(), 1, "the pending answer must not start a new turn");
        wait_pending(&rig, if tag == "question" { json!({"pending":[]}) } else { pending });
        assert_eq!(rig.dsh.calls("$events/result").len(), 1);
    }
}

#[test]
fn closed_agent_pending_survives_interaction_stream_reconnect() {
    let mut rig = Rig::new("closed-stream-reconnect", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    let pending = rig.console("GET", "/api/pending", None).1;
    lock(&rig.dsh.inner).drop_interaction_stream = true;
    assert!(wait_for(Duration::from_secs(10), || lock(&rig.dsh.inner).event_clients >= 2), "interaction stream did not reconnect");
    assert_eq!(rig.console("GET", "/api/pending", None).1, pending);
    assert!(rig.dsh.calls("$events/result").is_empty(), "stream loss alone must not settle the request");
    says(&rig, OWNER_KEY, OWNER, "owner", "blue");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], json!({"answers":[{"id":"q1","selected":["Blue"]}]}));
}

#[test]
fn closed_agent_pending_expires_while_interaction_stream_is_disconnected() {
    let mut rig = Rig::new("closed-stream-expiry", Seed { mode: "closed-agent", config: json!({"questionTimeoutMs":1800}), ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    // Keep reconnect blocked until expiry is observed; host load must not decide
    // whether the expiry timer beats the jittered reconnect timer.
    struct ReleaseHandshake(Arc<Dsh>);
    impl Drop for ReleaseHandshake {
        fn drop(&mut self) { lock(&self.0.inner).stall_handshakes = false; }
    }
    let _release = ReleaseHandshake(rig.dsh.clone());
    {
        let mut inner = lock(&rig.dsh.inner);
        inner.stall_handshakes = true;
        inner.drop_interaction_stream = true;
    }
    assert!(wait_for(Duration::from_secs(10), || !lock(&rig.dsh.inner).drop_interaction_stream), "interaction stream did not disconnect");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1), "expiry must not wait for stream reconnection");
    assert_eq!(lock(&rig.dsh.inner).event_clients, 1, "receipt must precede reconnect");
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], json!({"answers":[]}));
    wait_pending(&rig, json!({"pending":[]}));
}

#[test]
fn closed_agent_delayed_receipt_does_not_block_next_waterfall_or_expiry() {
    let mut rig = Rig::new("closed-delayed-receipt", Seed { mode: "closed-agent", config: json!({"questionTimeoutMs":5000}), ..Seed::default() });
    #[cfg(feature = "deadlock-regression")]
    fs::create_dir(rig.root.join("deadlock-probe")).unwrap();
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION# #SLOW#");
    assert!(wait_for(Duration::from_secs(12), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("Choose a color"))));
    #[cfg(feature = "deadlock-regression")]
    {
        fs::write(rig.root.join("deadlock-probe/arm"), b"arm").unwrap();
        assert!(wait_for(Duration::from_secs(5), || rig.root.join("deadlock-probe/armed").exists()), "maintenance gate did not arm");
        eprintln!("witness: gate armed");
    }
    struct ReleaseReceipt(Arc<Dsh>);
    impl Drop for ReleaseReceipt { fn drop(&mut self) { lock(&self.0.inner).hold_result_response = false; } }
    let release = ReleaseReceipt(rig.dsh.clone());
    lock(&rig.dsh.inner).hold_result_response = true;
    says(&rig, OWNER_KEY, OWNER, "owner", "Blue");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1), "first receipt was not received");
    lock(&rig.dsh.inner).interactions.push(json!({"type":"waterfall", "event":"user-questions/request", "agentId":"fake-session-1", "eventId":"question-next", "request":{"questions":[{"id":"q2","question":"Next question"}]}}));
    #[cfg(feature = "deadlock-regression")]
    {
        let probe = rig.root.join("deadlock-probe");
        eprintln!("witness: first receipt held");
        assert!(wait_for(Duration::from_secs(5), || probe.join("register-pending").exists() || probe.join("register-ready").exists()), "register poll witness missing");
        assert!(probe.join("register-pending").exists(), "register did not queue");
        eprintln!("witness: register pending");
        fs::write(probe.join("maintenance"), b"poll").unwrap();
        assert!(wait_for(Duration::from_secs(5), || probe.join("maintenance-pending").exists() || probe.join("maintenance-ready").exists()), "maintenance poll witness missing");
        let expected = std::env::var("HER_DEADLOCK_EXPECT").unwrap_or_else(|_| "ready".into());
        assert!(matches!(expected.as_str(), "ready" | "pending"));
        assert!(probe.join(format!("maintenance-{expected}")).exists(), "unexpected maintenance poll result");
        eprintln!("witness: maintenance {expected}");
    }
    #[cfg(not(feature = "deadlock-regression"))]
    thread::sleep(Duration::from_millis(1000));
    drop(release);
    eprintln!("witness: receipt released");
    assert!(wait_for(Duration::from_secs(5), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("Next question"))), "next waterfall was blocked");
    assert!(wait_for(Duration::from_secs(8), || rig.dsh.calls("$events/result").iter().any(|c| c["eventId"] == "question-next")), "next question did not expire");
    let receipts = rig.dsh.calls("$events/result");
    assert_eq!(receipts.iter().filter(|c| c["eventId"] == "question-next").count(), 1);
    assert_eq!(receipts.iter().find(|c| c["eventId"] == "question-next").unwrap()["outcome"]["value"], json!({"answers":[]}));
    assert!(wait_for(Duration::from_secs(3), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("等待回答超时"))));
    wait_pending(&rig, json!({"pending":[]}));
    assert!(wait_for(Duration::from_secs(3), || {
        fs::read(rig.root.join("data/pending-interactions.json"))
            .ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) == Some(json!([]))
    }), "expired entry remained in the persisted snapshot");
}

#[test]
fn closed_agent_maintenance_continues_during_stalled_handshake() {
    let mut rig = Rig::new("closed-stalled-handshake", Seed { mode: "closed-agent", config: json!({"questionTimeoutMs":6000}), ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    {
        let mut inner = lock(&rig.dsh.inner);
        inner.stall_handshakes = true;
        inner.drop_interaction_stream = true;
    }
    assert!(wait_for(Duration::from_secs(5), || lock(&rig.dsh.inner).stalled_handshakes > 0), "reconnect handshake did not stall");
    let settings_calls = rig.dsh.calls("settings/describe").len();
    assert!(wait_for(Duration::from_secs(8), || rig.dsh.calls("$events/result").len() == 1), "expiry must proceed during the stalled handshake");
    assert!(wait_for(Duration::from_secs(6), || rig.dsh.calls("settings/describe").len() > settings_calls), "settings must refresh during the stalled handshake");
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], json!({"answers":[]}));
    wait_pending(&rig, json!({"pending":[]}));
    assert!(rig.texts_to(OWNER_KEY).iter().any(|s| s == "⏰ 等待回答超时，已取消该请求（private:8808）"));
    assert_eq!(rig.dsh.calls("$events/result").len(), 1);
    // Ticket 25: the event-stream reconnect is paced by the JS backoff (3 s, then 6 s after a failure without traffic), so the
    // second attempt starts about 19 s after the drop instead of 16 s; it must still happen.
    assert!(wait_for(Duration::from_secs(25), || lock(&rig.dsh.inner).stalled_handshakes >= 2), "timed-out handshake must be retried");
    lock(&rig.dsh.inner).stall_handshakes = false;
}

#[test]
fn closed_agent_reset_cancels_pending_and_old_event_cannot_answer_new_session() {
    let mut rig = Rig::new("closed-reset", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    assert_eq!(rig.console("POST", "/api/session/reset", Some(&json!({"key":OWNER_KEY}))).0, 200);
    assert_eq!(rig.console("GET", "/api/pending", None).1, json!({"pending":[]}));
    assert!(wait_for(Duration::from_secs(10), || !rig.dsh.calls("$events/result").is_empty()), "reset did not resolve pending question");
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], json!({"answers":[]}));
    says(&rig, OWNER_KEY, OWNER, "owner", "new session request");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 2));
    assert_eq!(rig.dsh.prompts()[1].session, "fake-session-2");
    lock(&rig.dsh.inner).interactions.push(json!({"type":"waterfall", "event":"user-questions/request", "agentId":"fake-session-1", "eventId":"old-question", "request":{"questions":[{"id":"stale","question":"old question"}]}}));
    thread::sleep(Duration::from_millis(800));
    assert_eq!(rig.console("GET", "/api/pending", None).1, json!({"pending":[]}));
    assert_eq!(rig.dsh.calls("$events/result").len(), 1);
    assert!(!rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("old question")));
}

#[test]
fn closed_agent_mode_switch_rejects_old_approval_before_new_turn() {
    let mut rig = Rig::new("closed-switch", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    assert_eq!(rig.console("POST", "/api/mode", Some(&json!({"mode":"chat"}))).0, 200);
    assert_eq!(rig.console("POST", "/api/mode", Some(&json!({"mode":"closed-agent"}))).0, 200);
    assert_eq!(rig.console("GET", "/api/pending", None).1, json!({"pending":[]}));
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], "rejected");
    says(&rig, OWNER_KEY, OWNER, "owner", "new session request");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 2));
    assert_eq!(rig.dsh.prompts()[1].session, "fake-session-2");
    lock(&rig.dsh.inner).interactions.push(json!({"type":"waterfall", "event":"approval/request", "agentId":"fake-session-1", "eventId":"old-approval", "request":{"toolName":"old_tool"}}));
    thread::sleep(Duration::from_millis(800));
    assert_eq!(rig.console("GET", "/api/pending", None).1, json!({"pending":[]}));
    assert_eq!(rig.dsh.calls("$events/result").len(), 1);
    assert!(!rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("old_tool")));
}

#[test]
fn closed_agent_preset_change_retires_pending_policy_before_answer() {
    let mut rig = Rig::new("closed-preset-change", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    assert_eq!(rig.console("POST", "/api/mode", Some(&json!({"mode":"closed-agent","closedAgentPreset":"ticket12-chat"}))).0, 200);
    assert_eq!(rig.console("GET", "/api/pending", None).1, json!({"pending":[]}));
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], "rejected");
    says(&rig, OWNER_KEY, OWNER, "owner", "after preset change");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 2));
    assert_eq!(rig.dsh.prompts()[1].session, "fake-session-2");
    assert_eq!(rig.dsh.calls("session/create")[1]["request"]["agentPreset"], "ticket12-chat");
}

#[test]
fn closed_agent_owner_can_reset_even_when_absent_from_chat_allowlist() {
    let mut rig = Rig::new("closed-reset-owner", Seed { mode: "closed-agent", config: json!({"allow":{"groups":[GROUP_ID],"private":[STRANGER]}}), ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    assert_eq!(rig.console("POST", "/api/session/reset", Some(&json!({"key":OWNER_KEY}))).0, 200);
    assert_eq!(rig.console("GET", "/api/pending", None).1, json!({"pending":[]}));
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], json!({"answers":[]}));
}

#[test]
fn closed_agent_owner_receives_agent_output_with_local_path() {
    let mut rig = Rig::new("closed-owner-output", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#PATH#");
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "文件在 C:\\Users\\tester\\secret.txt 里")), "owner did not receive full agent output: {:?}", rig.texts_to(OWNER_KEY));
}

#[test]
fn closed_agent_preset_resolution_uses_settings_and_fallbacks() {
    for (tag, configured, presets, settings, expected) in [
        ("settings", "", json!([{"id":"marked","isDefault":true},{"id":"from-user"}]), json!({"namespaces":[{"ns":"agent-presets","user":{"default":"from-user"},"value":{"default":"marked"}}]}), "from-user"),
        ("missing", "missing", json!([{"id":"ticket13-full","isDefault":true}]), json!({"namespaces":[]}), "ticket13-full"),
        ("unknown", "chosen", json!([]), json!({"namespaces":[]}), "chosen"),
    ] {
        let mut rig = Rig::new(&format!("closed-preset-{tag}"), Seed { mode: "closed-agent", ..Seed::default() });
        fs::write(rig.root.join("data/mode.json"), json!({"mode":"closed-agent","closedAgentPreset":configured}).to_string()).unwrap();
        { let mut inner = lock(&rig.dsh.inner); inner.presets = presets; inner.settings = settings; }
        rig.start();
        says(&rig, OWNER_KEY, OWNER, "owner", "test preset");
        assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 1));
        assert_eq!(rig.dsh.calls("session/create")[0]["request"]["agentPreset"], expected);
    }
}

#[test]
fn closed_agent_failed_preset_discovery_preserves_session() {
    let mut rig = Rig::new("closed-preset-discovery-failure", Seed { mode: "closed-agent", ..Seed::default() });
    {
        let mut inner = lock(&rig.dsh.inner);
        inner.presets = json!([{"id":"marked","isDefault":true},{"id":"selected"}]);
        inner.settings = json!({"namespaces":[{"ns":"agent-presets","user":{"default":"selected"}}]});
    }
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "first turn");
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")));
    assert_eq!(rig.dsh.calls("session/create")[0]["request"]["agentPreset"], "selected");
    lock(&rig.dsh.inner).fail_settings = true;
    says(&rig, OWNER_KEY, OWNER, "owner", "failed discovery");
    assert!(wait_for(Duration::from_secs(10), || {
        fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("synthetic settings failure")
            || !rig.dsh.calls("workspace/archiveSession").is_empty()
    }), "failed turn or session retirement was not observed");
    assert_eq!(rig.dsh.calls("session/create").len(), 1);
    assert!(rig.dsh.calls("workspace/archiveSession").is_empty());
    assert!(rig.dsh.calls("session/cancel").is_empty());
    assert_eq!(rig.dsh.prompts().len(), 1);
    lock(&rig.dsh.inner).fail_settings = false;
    says(&rig, OWNER_KEY, OWNER, "owner", "after recovery");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 2));
    assert_eq!(rig.dsh.prompts()[0].session, rig.dsh.prompts()[1].session);
    assert_eq!(rig.dsh.calls("session/create").len(), 1);
    assert!(rig.dsh.calls("workspace/archiveSession").is_empty());
}

#[test]
fn closed_agent_invalid_approval_keeps_request_until_valid_owner_answer() {
    let mut rig = Rig::new("closed-invalid-approval", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    says(&rig, OWNER_KEY, OWNER, "owner", "maybe");
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("请回复「通过」或「拒绝」"))));
    assert!(rig.dsh.calls("$events/result").is_empty());
    assert_eq!(rig.console("GET", "/api/pending", None).1["pending"].as_array().unwrap().len(), 1);
    says(&rig, OWNER_KEY, OWNER, "owner", "拒绝");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], "rejected");
}

#[test]
fn closed_agent_expired_approval_is_rejected_once() {
    let mut rig = Rig::new("closed-approval-expiry", Seed { mode: "closed-agent", config: json!({"questionTimeoutMs":1500}), ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], "rejected");
    wait_pending(&rig, json!({"pending":[]}));
    assert!(rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("等待回答超时")));
    thread::sleep(Duration::from_millis(500));
    assert_eq!(rig.dsh.calls("$events/result").len(), 1);
}

#[test]
fn closed_agent_definite_receipt_rejection_keeps_question_for_retry() {
    let mut rig = Rig::new("closed-receipt-rejection", Seed { mode: "closed-agent", ..Seed::default() });
    lock(&rig.dsh.inner).rejected_results = 1;
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    says(&rig, OWNER_KEY, OWNER, "owner", "blue");
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("回答提交失败"))), "failure was not reported");
    assert_eq!(rig.console("GET", "/api/pending", None).1["pending"].as_array().unwrap().len(), 1);
    says(&rig, OWNER_KEY, OWNER, "owner", "red");
    assert!(wait_for(Duration::from_secs(10), || lock(&rig.dsh.inner).accepted_results.len() == 1));
    assert_eq!(lock(&rig.dsh.inner).accepted_results[0]["outcome"]["value"], json!({"answers":[{"id":"q1","selected":["Red"]}]}));
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")));
    assert_eq!(rig.dsh.prompts().len(), 1);
}

#[test]
fn closed_agent_uses_dsh_owner_override_for_private_admission() {
    let mut rig = Rig::new("closed-dsh-owner", Seed { mode: "closed-agent", ..Seed::default() });
    lock(&rig.dsh.inner).settings = json!({"namespaces":[{"ns":"qq-mode","user":{"mode":"closed-agent","ownerQQ":"7707"}}]});
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "old-owner", "must not use full agent");
    says(&rig, "private:7707", STRANGER, "dsh-owner", "owner override request");
    assert!(wait_for(Duration::from_secs(12), || rig.texts_to("private:7707").iter().any(|s| s == "synthetic reply")), "DSH owner override was not admitted");
    assert_eq!(rig.dsh.prompts().len(), 1);
    assert_eq!(body_of(&rig.dsh.prompts()[0].text), "【管理员】owner override request");
    assert!(rig.texts_to(OWNER_KEY).is_empty());
}

#[test]
fn closed_agent_dsh_owner_change_cancels_old_approval_and_retires_old_session() {
    let mut rig = Rig::new("closed-dsh-owner-change", Seed { mode: "closed-agent", ..Seed::default() });
    lock(&rig.dsh.inner).settings = json!({"namespaces":[{"ns":"qq-mode","user":{"mode":"closed-agent","ownerQQ":"8808"}}]});
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "old-owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    lock(&rig.dsh.inner).settings = json!({"namespaces":[{"ns":"qq-mode","user":{"mode":"closed-agent","ownerQQ":"7707"}}]});
    assert!(wait_for(Duration::from_secs(12), || rig.dsh.calls("$events/result").len() == 1), "old approval was not cancelled");
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], "rejected");
    wait_pending(&rig, json!({"pending":[]}));
    says(&rig, "private:7707", STRANGER, "new-owner", "new owner request");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 2));
    assert_eq!(rig.dsh.prompts()[1].session, "fake-session-2");
    lock(&rig.dsh.inner).settings = json!({"namespaces":[{"ns":"qq-mode","user":{"mode":"closed-agent","ownerQQ":"8808"}}]});
    assert!(wait_for(Duration::from_secs(12), || rig.console("GET", "/api/status", None).1["ownerQQ"] == json!(8808)));
    says(&rig, OWNER_KEY, OWNER, "old-owner", "returning owner request");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 3));
    assert_eq!(rig.dsh.prompts()[2].session, "fake-session-3", "revoked owner must not regain the retired context");
}

#[test]
fn closed_agent_external_dsh_mode_change_retires_pending_approval() {
    let mut rig = Rig::new("closed-dsh-mode", Seed { mode: "closed-agent", ..Seed::default() });
    lock(&rig.dsh.inner).settings = json!({"namespaces":[{"ns":"qq-mode","user":{"mode":"closed-agent","ownerQQ":"8808"}}]});
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    lock(&rig.dsh.inner).settings = json!({"namespaces":[{"ns":"qq-mode","user":{"mode":"chat","ownerQQ":"8808"}}]});
    assert!(wait_for(Duration::from_secs(12), || rig.console("GET", "/api/status", None).1["mode"] == "chat"), "DSH mode change did not reach the bridge");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.calls("$events/result").len() == 1));
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], "rejected");
    wait_pending(&rig, json!({"pending":[]}));
}

#[test]
fn closed_agent_lost_receipt_response_is_visible_and_never_repeated() {
    let mut rig = Rig::new("closed-receipt-unknown", Seed { mode: "closed-agent", ..Seed::default() });
    lock(&rig.dsh.inner).drop_result_response = 1;
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    says(&rig, OWNER_KEY, OWNER, "owner", "blue");
    assert!(wait_for(Duration::from_secs(10), || lock(&rig.dsh.inner).accepted_results.len() == 1));
    assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"][0]["status"] == "outcome-unknown"), "unknown receipt result was not visible");
    assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")));
    says(&rig, OWNER_KEY, OWNER, "owner", "next message");
    assert!(wait_for(Duration::from_secs(10), || rig.dsh.prompts().len() == 2));
    assert_eq!(rig.dsh.calls("$events/result").len(), 1);
    wait_pending(&rig, json!({"pending":[]}));
}

/// Hold the console's mode write (and with it the interaction gate) inside DSH's settings write.
struct StalledModeWrite { dsh: Arc<Dsh>, post: Option<thread::JoinHandle<()>> }
impl StalledModeWrite {
    fn begin(rig: &Rig) -> Self {
        lock(&rig.dsh.inner).hold_settings_update = true;
        let port = rig.console_port;
        let post = thread::spawn(move || {
            let _ = http_call(port, "POST", "/api/mode", &[("x-console-token", CONSOLE_TOKEN)], Some(&json!({"mode":"closed-agent"})));
        });
        assert!(wait_for(Duration::from_secs(10), || !rig.dsh.calls("settings/update").is_empty()), "console mode write never reached DSH");
        Self { dsh: rig.dsh.clone(), post: Some(post) }
    }
    fn release(&mut self) {
        lock(&self.dsh.inner).hold_settings_update = false;
        if let Some(post) = self.post.take() { let _ = post.join(); }
    }
}
impl Drop for StalledModeWrite { fn drop(&mut self) { self.release(); } }

#[test]
fn closed_agent_answer_during_interaction_lock_stall_is_not_lost() {
    let mut rig = Rig::new("closed-lock-answer", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#QUESTION#");
    assert!(wait_for(Duration::from_secs(12), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    let mut stall = StalledModeWrite::begin(&rig);
    says(&rig, OWNER_KEY, OWNER, "owner", "blue");
    // Longer than the maintenance tick's gate wait plus the answer's own: the old code gave up silently.
    thread::sleep(Duration::from_secs(14));
    stall.release();
    assert!(wait_for(Duration::from_secs(15), || rig.dsh.calls("$events/result").len() == 1), "the answer given during the lock stall never reached DSH: {:?}", rig.sends());
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], json!({"answers":[{"id":"q1","selected":["Blue"]}]}));
    assert!(wait_for(Duration::from_secs(12), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "synthetic reply")), "turn did not continue");
    assert_eq!(rig.dsh.prompts().len(), 1, "the answer must not start another turn");
}

#[test]
fn closed_agent_question_arriving_during_interaction_lock_stall_is_delivered() {
    let mut rig = Rig::new("closed-lock-register", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#SLOW#");
    rig.wait_prompts(1);
    let mut stall = StalledModeWrite::begin(&rig);
    lock(&rig.dsh.inner).interactions.push(json!({"type":"waterfall", "event":"user-questions/request", "agentId":"fake-session-1", "eventId":"late-question", "request":{"questions":[{"id":"q2","question":"late question"}]}}));
    thread::sleep(Duration::from_secs(6));
    stall.release();
    assert!(wait_for(Duration::from_secs(12), || rig.texts_to(OWNER_KEY).iter().any(|s| s.contains("late question"))), "the question that arrived during the lock stall was dropped");
    assert_eq!(rig.console("GET", "/api/pending", None).1["pending"].as_array().map(Vec::len), Some(1));
    assert!(rig.dsh.calls("$events/result").is_empty(), "a retained question must not be cancelled");
}

#[test]
fn closed_agent_expiry_notice_is_sent_after_the_original_turn_ended() {
    for (tag, marker) in [("question", "#QUESTION#"), ("approval", "#APPROVAL#")] {
        let mut rig = Rig::new(&format!("closed-expiry-after-end-{tag}"), Seed { mode: "closed-agent", config: json!({"questionTimeoutMs":6000}), ..Seed::default() });
        rig.start();
        says(&rig, OWNER_KEY, OWNER, "owner", marker);
        assert!(wait_for(Duration::from_secs(10), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)), "{tag} never appeared");
        {
            let mut inner = lock(&rig.dsh.inner);
            let session = inner.sessions.get_mut("fake-session-1").unwrap();
            session.cancelled = true;
            event(session, "turn/end", json!({"turn": 1, "reason": {"kind":"aborted"}}));
        }
        assert!(wait_for(Duration::from_secs(4), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "⏹️ 已停止")), "{tag} turn did not end before expiry");
        assert!(wait_for(Duration::from_secs(10), || rig.texts_to(OWNER_KEY).iter().any(|s| s == "⏰ 等待回答超时，已取消该请求（private:8808）")), "{tag} expiry notice missing after the turn ended: {:?}", rig.sends());
        assert!(wait_for(Duration::from_secs(5), || rig.dsh.calls("$events/result").len() == 1), "{tag} expiry receipt missing");
        wait_pending(&rig, json!({"pending":[]}));
    }
}

#[test]
fn closed_agent_reset_discards_queued_instructions() {
    let mut rig = Rig::new("closed-reset-queue", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#SLOW# first instruction");
    rig.wait_prompts(1);
    says(&rig, OWNER_KEY, OWNER, "owner", "queued instruction");
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(rig.console("POST", "/api/session/reset", Some(&json!({"key":OWNER_KEY}))).0, 200);
    assert!(wait_for(Duration::from_secs(12), || !rig.dsh.calls("session/cancel").is_empty()), "the running turn was not stopped");
    thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.dsh.prompts().len(), 1, "the queued instruction ran after the reset: {:?}", rig.dsh.prompts());
    says(&rig, OWNER_KEY, OWNER, "owner", "fresh instruction");
    rig.wait_prompts(2);
    let prompts = rig.dsh.prompts();
    assert_eq!(body_of(&prompts[1].text), "【管理员】fresh instruction");
    assert_eq!(prompts[1].session, "fake-session-2");
    assert_eq!(rig.dsh.calls("session/create").len(), 2);
}

#[test]
fn closed_agent_unresolved_preset_keeps_a_js_session_with_the_empty_policy() {
    let mut rig = Rig::new(
        "closed-empty-preset-policy",
        Seed {
            mode: "closed-agent",
            sessions: Some(json!({"sessions": {OWNER_KEY: "fake-session-old"}, "sessionPolicies": {OWNER_KEY: "[\"closed-agent\",\"\",8808]"}})),
            ..Seed::default()
        },
    );
    {
        let mut inner = lock(&rig.dsh.inner);
        inner.presets = json!([]);
        inner.settings = json!({"namespaces":[]});
        inner.sessions.insert("fake-session-old".into(), Sess::default());
    }
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "keep the imported session");
    rig.wait_prompts(1);
    assert_eq!(rig.dsh.prompts()[0].session, "fake-session-old");
    assert!(rig.dsh.calls("session/create").is_empty());
    assert!(rig.dsh.calls("workspace/archiveSession").is_empty());
    assert!(rig.dsh.calls("session/cancel").is_empty());
}

#[test]
fn closed_agent_unresolved_preset_new_session_omits_preset_and_stores_the_empty_policy() {
    let mut rig = Rig::new("closed-empty-preset-create", Seed { mode: "closed-agent", ..Seed::default() });
    {
        let mut inner = lock(&rig.dsh.inner);
        inner.presets = json!([]);
        inner.settings = json!({"namespaces":[]});
    }
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "first");
    rig.wait_prompts(1);
    assert!(rig.dsh.calls("session/create")[0]["request"].get("agentPreset").is_none(), "{}", rig.dsh.calls("session/create")[0]);
    assert_eq!(rig.data_json("sessions.json")["sessionPolicies"][OWNER_KEY], json!("[\"closed-agent\",\"\",8808]"));
}

#[test]
fn a_new_group_chat_acknowledges_before_rename_and_final_reply() {
    let mut rig = Rig::new("relay-order", Seed::default());
    rig.start();
    rig.push(group_event(16101, 1001, "tester", "", vec![text_seg("relay order")]));
    assert_eq!(rig.wait_texts(GROUP_KEY, 2), vec![ACK.to_string(), "synthetic reply".to_string()]);
    let trace = lock(&rig.dsh.trace).clone();
    let rename = trace.iter().position(|s| s == "session/rename").expect("rename");
    let sends: Vec<_> = trace.iter().enumerate().filter(|(_, s)| *s == "send_group_msg").map(|(i, _)| i).collect();
    assert_eq!(sends.len(), 2, "{trace:?}");
    // Frozen JS bridge.js starts renameSessionForUi without awaiting its group-name lookup.
    // The live group-turn capture sends the prompt acknowledgement before that rename.
    assert!(sends[0] < rename && rename < sends[1], "relay order: {trace:?}");
}

#[test]
fn a_group_message_round_trips_with_ack_plain_reply_session_and_records() {
    let mut rig = Rig::new("roundtrip", Seed::default());
    rig.start();
    rig.push(group_event(1001, 1001, "tester", "", vec![text_seg("你好 世界")]));
    let texts = rig.wait_texts(GROUP_KEY, 2);
    assert_eq!(texts, vec![ACK.to_string(), "synthetic reply".to_string()], "ack first, then the reply");

    let prompts = rig.dsh.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(body_of(&prompts[0].text), "tester：你好 世界", "group prompt is `sender：text`, no admin tag");

    // Session: created once with the chat preset (not the reserved2 one), policy recorded the JS way.
    let creates = rig.dsh.calls("session/create");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].pointer("/request/agentPreset"), Some(&json!("ticket12-chat")), "{creates:?}");
    let sessions = rig.data_json("sessions.json");
    assert_eq!(sessions["sessions"][GROUP_KEY], json!("fake-session-1"));
    assert_eq!(sessions["sessionPolicies"][GROUP_KEY], json!("[\"chat\",\"ticket12-chat\",null]"));

    // The session gets the JS title through session/rename.
    assert!(wait_for(LONG, || !rig.dsh.calls("session/rename").is_empty()), "session was never titled");
    assert_eq!(rig.dsh.calls("session/rename")[0].pointer("/request/title"), Some(&json!("QQ 群 · 测试群")));

    // Records: one completed turn in the ledger, every sent item confirmed in the outbox.
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "completed")), "{:?}", rig.ledger());
    assert!(wait_for(LONG, || rig.data_lines("send-outbox.jsonl").iter()
        .filter(|r| r["k"] == "confirmed").count() == 2), "both send confirmations were not published");
    let outbox = rig.data_lines("send-outbox.jsonl");
    assert_eq!(outbox.iter().filter(|r| r["k"] == "intent").count(), 2, "{outbox:?}");
    assert_eq!(outbox.iter().filter(|r| r["k"] == "confirmed").count(), 2, "{outbox:?}");
}

#[test]
fn owner_prompts_carry_the_admin_tag_and_the_persona_core_in_the_old_layout() {
    let mut rig = Rig::new("owner", Seed::default());
    fs::write(rig.root.join("data/personas.json"), json!({"format": 1, "nextId": 2, "default": "p1", "overrides": {}, "importedRolesAt": null,
        "personas": {"p1": {"id": "p1", "activeVersion": 1, "versions": [{"version": 1, "approved": true, "name": "小鲸鱼", "core": "你是一只爱聊天的小鲸鱼。"}]}}}).to_string()).unwrap();
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "私聊你好");
    says(&rig, GROUP_KEY, OWNER, "owner", "群里你好");
    rig.wait_texts(OWNER_KEY, 2);
    rig.wait_texts(GROUP_KEY, 2);
    let bodies: Vec<String> = rig.dsh.prompts().iter().map(|p| body_of(&p.text).to_string()).collect();
    let private = bodies.iter().find(|b| b.contains("私聊你好")).expect("private prompt");
    let group = bodies.iter().find(|b| b.contains("群里你好")).expect("group prompt");
    assert_eq!(private, "【人设】小鲸鱼\n你是一只爱聊天的小鲸鱼。\n\n【管理员】私聊你好", "persona core, blank line, admin tag, text");
    assert_eq!(group, "【人设】小鲸鱼\n你是一只爱聊天的小鲸鱼。\n\n【管理员】owner：群里你好");
}

#[test]
fn at_names_quotes_and_media_are_rendered_like_the_old_bridge() {
    let mut rig = Rig::new("render", Seed::default());
    rig.start();
    // @ of a member with a card, a quote of the bot's own message, and an image.
    rig.push(group_event(
        2001,
        1001,
        "tester",
        "",
        vec![
            json!({"type": "reply", "data": {"id": "5555"}}),
            json!({"type": "at", "data": {"qq": "1002"}}),
            text_seg(" 看这个"),
            json!({"type": "image", "data": {"file": "a.png", "url": "http://x.invalid/a.png"}}),
        ],
    ));
    rig.wait_texts(GROUP_KEY, 2);
    let body = body_of(&rig.dsh.prompts()[0].text).to_string();
    assert_eq!(
        body,
        "tester：[引用 bot：之前机器人说的话]@Alice 看这个[图片]\n【图片/表情】本条消息包含 1 个图片/表情（消息ID=2001）。"
    );

    // A quote whose target belongs to another group is not resolved (plain placeholder).
    rig.push(group_event(2002, 1001, "tester", "", vec![json!({"type": "reply", "data": {"id": "5556"}}), text_seg("嗯")]));
    rig.wait_texts(GROUP_KEY, 4);
    assert_eq!(body_of(&rig.dsh.prompts()[1].text), "tester：[引用消息]嗯");

    // A bare quote of the bot's message has no text of its own but still addresses the bot.
    rig.push(group_event(2003, 1001, "tester", "", vec![json!({"type": "reply", "data": {"id": "5555"}})]));
    rig.wait_texts(GROUP_KEY, 6);
    assert_eq!(body_of(&rig.dsh.prompts()[2].text), "tester：[引用 bot：之前机器人说的话]");
}

#[test]
fn markdown_is_flattened_long_replies_split_and_silent_or_empty_replies_are_not_sent() {
    let mut rig = Rig::new("reply", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#MD#");
    let texts = rig.wait_texts(GROUP_KEY, 2);
    assert_eq!(texts[1], "加粗 和 链接 (http://example.invalid/x)\n标题\n• 条目");

    says(&rig, GROUP_KEY, 1001, "tester", "#LONG#");
    let texts = rig.wait_texts(GROUP_KEY, 5);
    assert_eq!(texts[3].chars().count(), 4000, "QQ part limit");
    assert_eq!(texts[4].chars().count(), 500);

    says(&rig, GROUP_KEY, 1001, "tester", "#SILENT#");
    says(&rig, GROUP_KEY, 1001, "tester", "#EMPTY#");
    // The ack goes out for both; the model's silence and the empty text send nothing more.
    let texts = rig.wait_texts(GROUP_KEY, 7);
    assert_eq!(&texts[5..], &[ACK.to_string(), ACK.to_string()]);
    let texts = rig.sentinel(GROUP_KEY, 7);
    assert_eq!(&texts[7..], &[ACK.to_string(), "synthetic reply".to_string()], "nothing was sent for [SILENT] or the empty reply");
}

#[test]
fn a_send_tool_that_succeeded_suppresses_the_auto_forward_a_failed_one_does_not() {
    let mut rig = Rig::new("dup", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#TOOLOK#");
    // Only the ack: the tool already sent, the model's closing sentence must not be forwarded again.
    rig.wait_texts(GROUP_KEY, 1);
    let texts = rig.sentinel(GROUP_KEY, 1);
    assert_eq!(texts, vec![ACK.to_string(), ACK.to_string(), "synthetic reply".to_string()], "{texts:?}");

    says(&rig, GROUP_KEY, 1001, "tester", "#TOOLERR#");
    let texts = rig.wait_texts(GROUP_KEY, 5);
    assert_eq!(&texts[3..], &[ACK.to_string(), "tool failed reply".to_string()], "a failed send tool does not suppress the reply");
}

#[test]
fn only_allowed_non_self_non_empty_messages_reach_the_model() {
    let mut rig = Rig::new(
        "filter",
        Seed { config: json!({"allow": {"groups": [GROUP_ID, 420043]}, "deny": {"groups": [420043]}}), ..Seed::default() },
    );
    rig.start();
    // not on the allow-list
    rig.push(json!({"time": 1, "self_id": SELF_ID, "post_type": "message", "message_type": "group", "sub_type": "normal", "message_id": 1, "group_id": 999,
                    "user_id": 1001, "sender": {"user_id": 1001, "nickname": "t", "card": ""}, "message": [text_seg("x")], "raw_message": "x"}));
    // on the allow-list but denied
    rig.push(json!({"time": 1, "self_id": SELF_ID, "post_type": "message", "message_type": "group", "sub_type": "normal", "message_id": 2, "group_id": 420043,
                    "user_id": 1001, "sender": {"user_id": 1001, "nickname": "t", "card": ""}, "message": [text_seg("x")], "raw_message": "x"}));
    // private chat of someone not allowed
    rig.push(private_event(3, 5150, "nobody", vec![text_seg("x")]));
    // the bot's own message
    rig.push(group_event(4, SELF_ID, "bot", "", vec![text_seg("我自己说的")]));
    // nothing but an empty message
    rig.push(group_event(5, 1001, "tester", "", vec![]));
    // finally a real one
    rig.push(group_event(6, 1001, "tester", "", vec![text_seg("真的消息")]));
    let texts = rig.wait_texts(GROUP_KEY, 2);
    assert_eq!(texts, vec![ACK.to_string(), "synthetic reply".to_string()]);
    let prompts = rig.dsh.prompts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert_eq!(body_of(&prompts[0].text), "tester：真的消息");
    assert_eq!(rig.sends().len(), 2, "no reply anywhere else: {:?}", rig.sends());
}

// ───────────────────────── AC2: commands, status, reset ─────────────────────────

#[test]
fn non_owners_cannot_run_commands_or_change_the_role_by_talking() {
    let mut rig = Rig::new("nonowner", Seed::default());
    rig.start();
    // A member's slash text is dropped silently: no refusal, no command, nothing reaches the model
    // (DSH would run `/model` itself). Role-control words in ordinary text are plain input.
    says(&rig, GROUP_KEY, 1001, "tester", "/reset");
    says(&rig, GROUP_KEY, 1001, "tester", "/silent");
    says(&rig, GROUP_KEY, 1001, "tester", "/model");
    says(&rig, GROUP_KEY, 1001, "tester", "请切换角色到猫娘");
    // The ordinary message's acknowledgement and reply come first; the sentinel's follow them.
    rig.wait_texts(GROUP_KEY, 2);
    let texts = rig.sentinel(GROUP_KEY, 2);
    let bodies: Vec<String> = rig.dsh.prompts().iter().map(|p| body_of(&p.text).to_string()).collect();
    assert_eq!(bodies, ["tester：请切换角色到猫娘", "tester：sentinel"], "member slash text reached DSH");
    assert!(texts.iter().all(|t| !t.contains("仅管理员") && !t.contains("当前模型")), "no refusal and no command result: {texts:?}");
    assert_ne!(rig.data_json("current-role.json")["mode"], json!("silent"), "a member cannot silence the bridge");
}

#[test]
fn a_dsh_command_result_on_a_member_turn_is_not_relayed() {
    let mut rig = Rig::new("member-dsh-command", Seed::default());
    rig.start();
    // Not slash-leading, so it is model input; the fake DSH still answers it as a command.
    says(&rig, GROUP_KEY, 1001, "tester", "请问 /model 怎么用");
    let texts = rig.sentinel(GROUP_KEY, 0);
    assert!(rig.dsh.prompts().iter().any(|p| p.text.contains("/model")), "the message never reached DSH");
    assert!(texts.iter().all(|t| !t.contains("fake-model")), "a member turn relayed a DSH command result: {texts:?}");
    assert!(rig.ledger().iter().all(|r| r["k"] != "ended" || r["outcome"] == "completed"), "the command turn ends normally");
}

#[test]
fn ordinary_owner_messages_with_command_words_reach_the_model() {
    let mut rig = Rig::new("owner-words", Seed { roles: vec![("小鲸鱼", "你是小鲸鱼。\n")], ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "游戏里怎么切换角色？");
    says(&rig, OWNER_KEY, OWNER, "owner", "/usr/bin/env 是什么");
    says(&rig, OWNER_KEY, OWNER, "owner", "/reset now");
    assert!(wait_for(LONG, || rig.dsh.prompts().len() >= 3), "{:?}", rig.dsh.prompts());
    let bodies: Vec<String> = rig.dsh.prompts().iter().map(|p| body_of(&p.text).to_string()).collect();
    for expected in ["【管理员】游戏里怎么切换角色？", "【管理员】/usr/bin/env 是什么", "【管理员】/reset now"] {
        assert!(bodies.iter().any(|b| b == expected), "{expected} missing in {bodies:?}");
    }
    let state = rig.data_json("current-role.json");
    assert!(state["role"].is_null() && state["mode"] != json!("silent"), "no command ran: {state}");
}

#[test]
fn status_role_silent_and_active_commands_have_the_old_texts_and_effects() {
    let mut rig = Rig::new("commands", Seed { roles: vec![("小鲸鱼", "你是小鲸鱼。\n")], ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "/status");
    let t = rig.wait_texts(OWNER_KEY, 1);
    assert_eq!(t[0], "会话 未创建；白名单 通过；角色 无；模式 active");

    says(&rig, OWNER_KEY, OWNER, "owner", "/role 小鲸鱼");
    says(&rig, OWNER_KEY, OWNER, "owner", "/role 不存在的");
    says(&rig, OWNER_KEY, OWNER, "owner", "/status");
    let t = rig.wait_texts(OWNER_KEY, 4);
    assert_eq!(t[1], "角色卡已停用：请用 /persona 或控制台的人设页切换人设。");
    assert_eq!(t[2], "角色卡已停用：请用 /persona 或控制台的人设页切换人设。", "an unknown name gets the same notice");
    assert_eq!(t[3], "会话 未创建；白名单 通过；角色 无；模式 active");
    assert!(rig.data_json("current-role.json")["role"].is_null(), "a retired /role writes nothing");

    says(&rig, OWNER_KEY, OWNER, "owner", "/role off");
    let t = rig.wait_texts(OWNER_KEY, 5);
    assert_eq!(t[4], "角色卡已停用：请用 /persona 或控制台的人设页切换人设。");
    assert!(rig.data_json("current-role.json")["role"].is_null());

    // /silent: members are ignored (not even a refusal), the owner still gets answers.
    says(&rig, OWNER_KEY, OWNER, "owner", "/silent");
    let t = rig.wait_texts(OWNER_KEY, 6);
    assert_eq!(t[5], "已进入静默模式：群友消息不再回复，仅管理员可对话。");
    assert_eq!(rig.data_json("current-role.json")["mode"], json!("silent"));
    says(&rig, GROUP_KEY, 1001, "tester", "silent 里说话");
    says(&rig, GROUP_KEY, 1001, "tester", "/reset");
    says(&rig, OWNER_KEY, OWNER, "owner", "管理员说话");
    let t = rig.wait_texts(OWNER_KEY, 8);
    assert_eq!(&t[6..], &[ACK.to_string(), "synthetic reply".to_string()], "the owner is answered in private");
    assert!(rig.texts_to(GROUP_KEY).is_empty(), "members get nothing, not even a refusal: {:?}", rig.sends());
    assert_eq!(rig.dsh.prompts().len(), 1);
    assert_eq!(body_of(&rig.dsh.prompts()[0].text), "【管理员】管理员说话");

    // JS quirk kept: a reply in a group is held back in silent mode even when the owner asked for it.
    says(&rig, GROUP_KEY, OWNER, "owner", "群里的管理员");
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), vec![ACK.to_string()]);
    let t = rig.sentinel(OWNER_KEY, 8);
    assert_eq!(t.len(), 10);
    assert!(wait_for(LONG, || rig.ledger().iter().filter(|r| r["k"] == "ended").count() == 3), "{:?}", rig.ledger());
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()], "ack only: the in-flight group reply is blocked");

    says(&rig, OWNER_KEY, OWNER, "owner", "/active");
    let t = rig.wait_texts(OWNER_KEY, 11);
    assert_eq!(t[10], "已退出静默模式，恢复正常回复。");
    assert_eq!(rig.data_json("current-role.json")["mode"], json!("active"));
}

#[test]
fn reset_drops_the_session_stops_dsh_work_and_the_next_message_starts_fresh() {
    let mut rig = Rig::new("reset", Seed::default());
    rig.start();
    // /reset with nothing to reset says nothing at all (as before).
    says(&rig, OWNER_KEY, OWNER, "owner", "/reset");
    says(&rig, OWNER_KEY, OWNER, "owner", "第一句");
    let t = rig.wait_texts(OWNER_KEY, 2);
    assert_eq!(t, vec![ACK.to_string(), "synthetic reply".to_string()]);
    assert_eq!(rig.data_json("sessions.json")["sessions"][OWNER_KEY], json!("fake-session-1"));

    says(&rig, OWNER_KEY, OWNER, "owner", "/new");
    let t = rig.wait_texts(OWNER_KEY, 3);
    assert_eq!(t[2], "已重置会话，下次消息将开新上下文");
    assert!(rig.data_json("sessions.json")["sessions"].get(OWNER_KEY).is_none(), "{}", rig.data_json("sessions.json"));
    assert_eq!(rig.dsh.calls("session/cancel").len(), 1);
    assert_eq!(rig.dsh.calls("workspace/archiveSession").len(), 1);

    says(&rig, OWNER_KEY, OWNER, "owner", "第二句");
    rig.wait_texts(OWNER_KEY, 5);
    assert_eq!(rig.data_json("sessions.json")["sessions"][OWNER_KEY], json!("fake-session-2"));
    assert_eq!(rig.dsh.calls("session/create").len(), 2);
}

#[test]
fn a_dsh_slash_command_result_is_relayed_without_a_model_turn() {
    let mut rig = Rig::new("slash", Seed::default());
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "/model");
    let t = rig.wait_texts(OWNER_KEY, 1);
    assert_eq!(t, vec!["当前模型：fake-model".to_string()], "command text is flattened and neither ack nor model reply follow");
    let t = rig.sentinel(OWNER_KEY, 1);
    assert_eq!(t.len(), 3, "{t:?}");
    assert_eq!(rig.dsh.prompts().len(), 2);
    assert!(rig.ledger().iter().all(|r| r["k"] != "ended" || r["outcome"] == "completed"));
}

// ───────────────────────── AC3: failure, timeout, cancel, authority ─────────────────────────

#[test]
fn model_failure_never_puts_an_internal_error_in_the_group_and_the_fallback_answers() {
    let mut rig = Rig::new(
        "failure",
        Seed {
            config: json!({"dsh": {"provider": "bad-provider", "model": "m1", "fallbacks": [{"provider": "ticket12-provider", "model": "m2"}]}}),
            ..Seed::default()
        },
    );
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "走降级");
    let t = rig.wait_texts(GROUP_KEY, 2);
    assert_eq!(t, vec![ACK.to_string(), "synthetic reply".to_string()], "one ack, the fallback model's reply, no error text");
    assert_eq!(rig.dsh.prompts().len(), 2, "the prompt was re-delivered once on the next candidate");

    // Without a fallback chain left, the group sees only the ack; the owner receives metadata.
    let mut rig2 = Rig::new("failure2", Seed::default());
    rig2.start();
    says(&rig2, GROUP_KEY, 1001, "tester", "#FAIL#");
    rig2.wait_texts(GROUP_KEY, 1);
    assert!(wait_for(LONG, || rig2.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "failed")), "{:?}", rig2.ledger());
    let t = rig2.sentinel(GROUP_KEY, 1);
    assert_eq!(t, vec![ACK.to_string(), ACK.to_string(), "synthetic reply".to_string()], "{t:?}");
    let sends = rig2.sends();
    let owner_alerts: Vec<_> = sends.iter().filter(|(key, _)| key == OWNER_KEY).collect();
    assert_eq!(owner_alerts.len(), 1, "{sends:?}");
    assert_eq!(owner_alerts[0].1, "[group:420042] 模型/后端回合失败：backend=dsh provider=ticket12-provider model=ticket12-model class=backend code=boom");
    for (key, text) in sends {
        assert!(!text.contains("exploded") && !text.contains("internal"), "raw error leaked to {key}: {text}");
        if key != OWNER_KEY {
            assert!(!text.contains("boom") && !text.contains("class=backend"), "failure metadata leaked to {key}: {text}");
        }
    }
}

#[test]
fn an_aborted_turn_says_stopped_and_a_timed_out_turn_says_nothing() {
    let mut rig = Rig::new("abort", Seed { config: json!({"socialV2": {"turnTimeoutSecs": 3}}), ..Seed::default() });
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#ABORT#");
    let t = rig.wait_texts(GROUP_KEY, 2);
    assert_eq!(t, vec![ACK.to_string(), "⏹️ 已停止".to_string()]);

    says(&rig, GROUP_KEY, 1001, "tester", "#SLOW#");
    assert!(
        wait_for(Duration::from_secs(30), || {
            let ledger = rig.ledger();
            ledger.iter().any(|r| r["k"] == "verified" && r["verdict"] == "attempt-abandoned")
                && ledger.iter().filter(|r| r["k"] == "ended").count() == 2
        }),
        "the timed-out turn must be abandoned and closed in the ledger: {:?}",
        rig.ledger()
    );
    assert_eq!(rig.dsh.calls("session/cancel").len(), 1, "the abandoned DSH turn was cancelled");
    let t = rig.sentinel(GROUP_KEY, 3);
    assert_eq!(&t[2..], &[ACK.to_string(), ACK.to_string(), "synthetic reply".to_string()], "timeout: ack only, no error text: {t:?}");
}

#[test]
fn a_reset_while_the_model_runs_cancels_the_turn_and_its_late_reply_is_not_sent() {
    let mut rig = Rig::new("resetbusy", Seed::default());
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#SLOW#");
    rig.wait_texts(OWNER_KEY, 1);
    assert!(wait_for(LONG, || rig.data_json("sessions.json")["sessions"].get(OWNER_KEY).is_some()));
    says(&rig, OWNER_KEY, OWNER, "owner", "/reset");
    let t = rig.wait_texts(OWNER_KEY, 2);
    assert_eq!(t[1], "已重置会话，下次消息将开新上下文", "the reset is handled while the turn is still running");
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "cancelled")), "{:?}", rig.ledger());
    let t = rig.sentinel(OWNER_KEY, 2);
    assert_eq!(&t[2..], &[ACK.to_string(), "synthetic reply".to_string()], "no late reply and no 已停止 from the cancelled turn: {t:?}");
}

#[test]
fn revoking_the_group_while_the_model_runs_stops_the_reply() {
    let mut rig = Rig::new("revoke", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#SLOW#");
    rig.wait_texts(GROUP_KEY, 1);
    // The operator removes the group from the allow-list while the turn is in flight, then DSH finishes it.
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["allow"]["groups"] = json!([]);
    fs::write(&path, cfg.to_string()).unwrap();
    let sid = rig.data_json("sessions.json")["sessions"][GROUP_KEY].as_str().unwrap().to_string();
    // ending the slow turn: cancel from the DSH side makes it finish "aborted" without the bridge's help
    lock(&rig.dsh.inner).sessions.get_mut(&sid).unwrap().cancelled = true;
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.texts_to(GROUP_KEY), vec![ACK.to_string()], "no reply (not even 已停止) once the group is no longer allowed");
}

#[test]
fn a_reply_with_a_local_path_or_credential_is_blocked_not_sent() {
    let mut rig = Rig::new("sentinel", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#PATH#");
    rig.wait_texts(GROUP_KEY, 1);
    let t = rig.sentinel(GROUP_KEY, 1);
    assert_eq!(t, vec![ACK.to_string(), ACK.to_string(), "synthetic reply".to_string()], "{t:?}");
}

#[test]
fn hotfix88_outbound_guard_false_sends_sensitive_reply_without_notice() {
    let mut rig = Rig::new("guard88-off", Seed {
        config: json!({"security":{"outboundGuard":false}, "socialV2":{"context":{"stripFromModelText":["STRIPCFG88"]}}}), ..Seed::default()
    });
    fs::write(rig.root.join("config/model-text-blacklist.json"), r#"{"strip":["STRIPFILE88"]}"#).unwrap();
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#GUARD88# STRIPCFG88 STRIPFILE88");
    assert_eq!(rig.wait_texts(GROUP_KEY, 2), vec![ACK.to_string(), "文件在 C:\\Users\\tester\\secret.txt，密码：qwerty123".to_string()]);
    let prompt = &rig.dsh.prompts()[0].text;
    assert!(!prompt.contains("STRIPCFG88"), "config stripping remains active with the guard off");
    assert!(!prompt.contains("STRIPFILE88"), "file blacklist remains active with the guard off");
    rig.sentinel(GROUP_KEY, 2);
    assert!(rig.texts_to(OWNER_KEY).is_empty(), "guard off sends no blocked-reply notice");
}

#[test]
fn hotfix88_outbound_guard_absent_and_true_still_block_sensitive_reply() {
    for security in [json!({}), json!({"outboundGuard":true})] {
        let mut rig = Rig::new("guard88-on", Seed { config: json!({"security":security}), ..Seed::default() });
        rig.start();
        says(&rig, GROUP_KEY, 1001, "tester", "#GUARD88#");
        assert!(rig.wait_texts(OWNER_KEY, 1)[0].contains("被安全策略拦截"));
        assert_eq!(rig.sentinel(GROUP_KEY, 1), vec![ACK.to_string(), ACK.to_string(), "synthetic reply".to_string()]);
    }
}

#[test]
fn hotfix88_outbound_guard_is_reloaded_before_an_in_flight_reply() {
    let mut rig = Rig::new("guard88-reload", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#SLOW# #GUARD88#");
    assert!(wait_for(LONG, || !rig.dsh.prompts().is_empty()));
    let (status, body) = rig.console("POST", "/api/security", Some(&json!({"outboundGuard":false})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(rig.wait_texts(GROUP_KEY, 2), vec![ACK.to_string(), "文件在 C:\\Users\\tester\\secret.txt，密码：qwerty123".to_string()]);
    rig.sentinel(GROUP_KEY, 2);
    assert!(rig.texts_to(OWNER_KEY).is_empty());
}

// ───────────────────────── AC4: console mode switch ─────────────────────────

#[test]
fn switching_to_chat_in_the_console_is_stored_reported_and_changes_the_running_bridge() {
    let mut rig = Rig::new("switch", Seed { mode: "reserved2", ..Seed::default() });
    rig.start();
    let (status, body) = rig.console("GET", "/api/status", None);
    assert_eq!((status, body["mode"].clone()), (200, json!("reserved2")), "{body}");

    let (status, body) = rig.console("POST", "/api/mode", Some(&json!({"mode": "bogus"})));
    assert_eq!(status, 400, "{body}");
    assert_eq!(rig.data_json("mode.json")["mode"], json!("reserved2"), "a refused switch changes nothing");

    let (status, body) = rig.console("POST", "/api/mode", Some(&json!({"mode": "chat"})));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["mode"], json!("chat"));
    assert_eq!(rig.data_json("mode.json")["mode"], json!("chat"), "the selected runtime's own mode file");
    let (_, status_body) = rig.console("GET", "/api/status", None);
    assert_eq!(status_body["mode"], json!("chat"));
    assert_eq!(status_body["roleMode"], json!("active"));

    // The bridge process that was started in reserved2 now serves chat: ack, plain reply, chat preset.
    says(&rig, GROUP_KEY, 1001, "tester", "切过来了吗");
    let t = rig.wait_texts(GROUP_KEY, 2);
    assert_eq!(t, vec![ACK.to_string(), "synthetic reply".to_string()]);
    assert_eq!(rig.dsh.calls("session/create")[0].pointer("/request/agentPreset"), Some(&json!("ticket12-chat")));

    // `simulation` is the canonical name of reserved2 and is stored under the internal spelling.
    let (status, body) = rig.console("POST", "/api/mode", Some(&json!({"mode": "simulation"})));
    assert_eq!((status, body["mode"].clone()), (200, json!("reserved2")), "{body}");
    assert_eq!(rig.data_json("mode.json")["mode"], json!("reserved2"));
}

#[test]
fn status_reports_the_stored_role_and_role_mode() {
    let rig = Rig::new(
        "status",
        Seed { roles: vec![("小鲸鱼", "你是小鲸鱼。\n")], current_role: Some(json!({"role": "小鲸鱼", "mode": "silent"})), ..Seed::default() },
    );
    let (status, body) = rig.console("GET", "/api/status", None);
    assert_eq!(status, 200);
    assert_eq!(body["mode"], json!("chat"));
    assert_eq!(body["role"], json!("小鲸鱼"));
    assert_eq!(body["roleMode"], json!("silent"));
}

#[test]
fn a_session_made_under_another_mode_is_replaced_not_reused_by_chat() {
    let mut rig = Rig::new(
        "policy",
        Seed {
            sessions: Some(json!({"sessions": {GROUP_KEY: "fake-session-old"},
                                  "sessionPolicies": {GROUP_KEY: "[\"reserved2\",\"ticket12-v2-preset\",null]"}})),
            ..Seed::default()
        },
    );
    lock(&rig.dsh.inner).sessions.insert("fake-session-old".into(), Sess::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "换模式后的第一句");
    rig.wait_texts(GROUP_KEY, 2);
    let prompts = rig.dsh.prompts();
    assert_ne!(prompts[0].session, "fake-session-old", "the reserved2 session must not carry chat messages");
    assert_eq!(rig.dsh.calls("session/create").len(), 1);
    assert_eq!(rig.dsh.calls("workspace/archiveSession").len(), 1, "the old session was retired");
    let sessions = rig.data_json("sessions.json");
    assert_eq!(sessions["sessionPolicies"][GROUP_KEY], json!("[\"chat\",\"ticket12-chat\",null]"));
}

#[test]
fn chat_never_creates_a_session_on_a_preset_dsh_does_not_offer() {
    let mut rig = Rig::new("nopreset", Seed { config: json!({"agentPreset": "not-offered"}), ..Seed::default() });
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "你好");
    assert!(
        wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "failed" && r["reason"].to_string().contains("not-offered"))),
        "the refusal must be recorded: {:?}",
        rig.ledger()
    );
    assert!(rig.dsh.calls("session/create").is_empty(), "no session may be created on DSH's default preset");
    assert!(rig.dsh.prompts().is_empty());
    // Ticket 25: the failure is told to the administrator privately (one alert); the group sees nothing.
    assert!(
        wait_for(LONG, || rig.texts_to(OWNER_KEY).iter().any(|t| t.starts_with("[group:420042] 回合失败：") && t.contains("not-offered"))),
        "the administrator must be told the turn failed: {:?}",
        rig.sends()
    );
    let beyond_the_owner: Vec<_> = rig.sends().into_iter().filter(|(target, _)| target != OWNER_KEY).collect();
    assert!(beyond_the_owner.is_empty(), "nothing is sent to the group: {:?}", rig.sends());
}

#[path = "fixtures/media.rs"]
mod media_fixtures;

#[test]
fn review_chat_market_sticker_reaches_dsh_as_images() {
    let mut rig = Rig::new("review-market-chat", Seed::default());
    rig.start();
    rig.push(group_event(19002,1001,"tester","",vec![text_seg("查看表情"),json!({"type":"mface","data":{"file":"ticket19.gif", "summary":"市场表情"}})]));
    rig.wait_prompts(1);
    let calls = rig.dsh.calls("session/prompt");
    let content = calls[0].pointer("/request/content").unwrap().as_array().unwrap();
    assert_eq!(content.iter().filter(|b| b["type"] == "image").count(), 8, "market sticker lost: {content:?}");
    rig.wait_texts(GROUP_KEY, 2);
}

#[test]
fn media_chat_delivers_composed_sampled_images_to_dsh_in_order() {
    use base64::Engine;
    let mut rig = Rig::new("media-chat", Seed::default());
    rig.start();
    rig.push(group_event(19001,1001,"tester","",vec![text_seg("查看动图"),json!({"type":"image","data":{"file":"ticket19.gif"}})]));
    rig.wait_prompts(1);
    let calls=rig.dsh.calls("session/prompt");
    let content=calls[0].pointer("/request/content").unwrap().as_array().unwrap();
    let images:Vec<_>=content.iter().filter(|b|b["type"]=="image").collect();
    assert_eq!(images.len(),8,"{content:?}");
    assert!(content.iter().filter_map(|b|b["text"].as_str()).any(|t|t.contains("动图，共 37 帧，已均匀抽取 8 帧")));
    for (block,expected) in images.iter().zip([0u8,5,10,15,20,25,30,36]) {
        assert_eq!(block["mediaType"],"image/jpeg");
        let bytes=base64::engine::general_purpose::STANDARD.decode(block["data"].as_str().unwrap()).unwrap();
        let img=image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!(img.dimensions(),(40,30));
        assert!(img.get_pixel(0,0)[0].abs_diff(expected)<=4);
    }
    rig.wait_texts(GROUP_KEY,2);
}

// Ticket 15 uses the same public binary, gateway and DSH seam as ordinary chat.
#[test]
fn reserved_admin_state_changes_drive_real_participation_and_flush() {
    let mut rig = Rig::new(
        "reserved-admin15",
        Seed {
            mode: "reserved",
            config: json!({"ackMessage":"", "social": {
                "triggerProbability":0, "activeCheckMinMs":0, "activeCheckMaxMs":0,
                "activeReplyDelayMinMs":0, "activeReplyDelayMaxMs":0,
                "activeDurationEnabled":false, "proactiveEnabled":false,
                "skipProbability":0, "idleWindowMs":120000,
                "burstIntervalMinMs":1, "burstIntervalMaxMs":1
            }}),
            ..Seed::default()
        },
    );
    rig.start();
    // The live clock has advanced before the companion console first reads its clock.
    thread::sleep(Duration::from_millis(150));
    says(&rig, GROUP_KEY, 1001, "tester", "watching-only15");
    assert!(
        wait_for(LONG, || rig.console("GET", "/api/social", None).1
            ["pendingSummaries"][GROUP_KEY]
            == 1),
        "watching message must enter the summary buffer"
    );
    assert!(rig.dsh.prompts().is_empty());
    let state = rig.console(
        "POST",
        "/api/social/state",
        Some(&json!({"key":GROUP_KEY,"phase":"active"})),
    );
    assert_eq!(
        state,
        (200, json!({"ok":true,"key":GROUP_KEY,"phase":"active"}))
    );
    says(&rig, GROUP_KEY, 1001, "tester", "#SPLIT15#");
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 3),
        ["第一句", "hello world", "第二句"]
    );
    let flush = rig.console("POST", "/api/social/flush", Some(&json!({"key":GROUP_KEY})));
    assert_eq!(flush, (200, json!({"ok":true})));
    let prompts = rig.wait_prompts(2);
    assert!(!prompts[0].text.contains("watching-only15"), "manual activation must exclude earlier watching input from the active batch");
    assert!(prompts[1].text.contains("【群聊摘要】"));
    assert!(prompts[1].text.contains("watching-only15"));
    assert!(wait_for(LONG, || rig.console("GET", "/api/social", None).1
        ["pendingSummaries"]
        == json!({})));
    thread::sleep(Duration::from_secs(1));
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        ["第一句", "hello world", "第二句"],
        "summary output must remain silent"
    );
}

#[test]
fn reserved_commands_and_disabled_social_use_the_public_chat_fallback() {
    let mut rig = Rig::new(
        "reserved-disabled15",
        Seed {
            mode: "reserved",
            config: json!({"social":{"enabled":false},"ackMessage":""}),
            ..Seed::default()
        },
    );
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "/status");
    assert_eq!(
        rig.wait_texts(OWNER_KEY, 1),
        ["会话 未创建；白名单 通过；角色 无；模式 active"]
    );
    // A member's slash text is ignored in reserved mode too: no refusal, no DSH command, no relay.
    says(&rig, GROUP_KEY, 1001, "tester", "/model");
    says(&rig, GROUP_KEY, 1001, "tester", "#SPLIT15#");
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 1),
        ["第一句 hello world 第二句"]
    );
    assert!(rig.dsh.prompts().iter().all(|p| !p.text.contains("/model")), "member slash text reached DSH: {:?}", rig.dsh.prompts());
    let mut social = (0, Value::Null);
    assert!(wait_for(LONG, || {
        social = rig.console("GET", "/api/social", None);
        social.0 == 200
    }), "social view never became available: {social:?}");
    assert_eq!(social.1["pendingSummaries"], json!({}), "{social:?}");
}

#[test]
fn reserved_role_commands_answer_the_retirement_notice_and_write_nothing() {
    let mut rig = Rig::new(
        "reserved-role19",
        Seed {
            mode: "reserved",
            config: json!({"social":{"enabled":false},"ackMessage":""}),
            roles: vec![("小鲸鱼", "你是小鲸鱼。\n")],
            ..Seed::default()
        },
    );
    rig.start();
    for command in ["/role 小鲸鱼", "/role 不存在的", "/role off"] {
        says(&rig, OWNER_KEY, OWNER, "owner", command);
    }
    for text in rig.wait_texts(OWNER_KEY, 3) {
        assert_eq!(text, "角色卡已停用：请用 /persona 或控制台的人设页切换人设。");
    }
    assert!(rig.data_json("current-role.json")["role"].is_null(), "a retired /role writes nothing");
    assert!(rig.dsh.prompts().is_empty(), "no /role form reaches the model");
}

fn reserved_seed(extra: Value) -> Seed {
    let mut social = json!({"triggerProbability":0,"proactiveEnabled":false,"activeDurationEnabled":false,
        "activeCheckMinMs":0,"activeCheckMaxMs":0,"activeReplyDelayMinMs":0,"activeReplyDelayMaxMs":0,
        "idleWindowMs":120000,"idleRetryProbability":0,"skipProbability":0,"longGapProbability":0,
        "burstIntervalMinMs":1,"burstIntervalMaxMs":1});
    social
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    Seed {
        mode: "reserved",
        config: json!({"ackMessage":"","social":social}),
        ..Seed::default()
    }
}
fn reserved_phase(rig: &Rig) -> String {
    rig.console("GET", "/api/social", None).1["states"][GROUP_KEY]["phase"]
        .as_str()
        .unwrap_or("")
        .to_string()
}
fn wait_phase(rig: &Rig, phase: &str) {
    assert!(
        wait_for(LONG, || reserved_phase(rig) == phase),
        "expected phase {phase}, got {}",
        reserved_phase(rig)
    );
}

#[test]
fn reserved_probe_returns_to_watching_and_inbound_revives_probing() {
    let mut rig = Rig::new(
        "reserved-probe15",
        reserved_seed(json!({"idleWindowMs":0,"idleRetryProbability":1,"idleRetryWaitMs":7000})),
    );
    rig.start();
    assert_eq!(
        rig.console(
            "POST",
            "/api/social/state",
            Some(&json!({"key":GROUP_KEY,"phase":"active"}))
        )
        .0,
        200
    );
    wait_phase(&rig, "probing");
    assert!(rig.wait_prompts(1)[0].text.contains("群里安静了一会儿"));
    says(&rig, GROUP_KEY, 1001, "tester", "new-after-probe15");
    wait_phase(&rig, "active");
    assert!(rig.wait_prompts(2)[1].text.contains("【新消息】"));
    wait_phase(&rig, "probing");
    wait_phase(&rig, "idle");
}

#[test]
fn reserved_proactive_participation_wraps_up_and_silent_farewell_exits() {
    let mut rig = Rig::new(
        "reserved-proactive15",
        reserved_seed(json!({"proactiveEnabled":true,"proactiveIdleThresholdMs":0,
        "proactiveCheckMinMs":0,"proactiveCheckMaxMs":0,"proactiveProbability":1,
        "activeDurationEnabled":true,"activeDurationMinMs":1000,"activeDurationMaxMs":1000})),
    );
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "context-for-proactive15");
    assert!(rig.wait_prompts(1)[0].text.contains("你打算开启一个新话题"));
    let p = rig.wait_prompts(2);
    assert!(p[1].text.contains("现在该自然地收尾/潜水了"));
    wait_phase(&rig, "idle");
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        ["synthetic reply", "synthetic reply"]
    );
    assert_eq!(
        rig.console(
            "POST",
            "/api/social",
            Some(&json!({"proactiveEnabled":false}))
        )
        .0,
        200
    );
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #SILENT#");
    rig.wait_prompts(3);
    rig.wait_prompts(4);
    wait_phase(&rig, "idle");
    assert_eq!(
        rig.texts_to(GROUP_KEY).len(),
        2,
        "silent normal/farewell turns must not send marker text"
    );
}

#[test]
fn reserved_model_tool_cooperation_silence_failure_and_next_turn() {
    let mut rig = Rig::new("reserved-model15", reserved_seed(json!({})));
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #TOOLOK#");
    rig.wait_prompts(1);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended")));
    assert!(
        rig.texts_to(GROUP_KEY).is_empty(),
        "tool success must suppress automatic output"
    );
    says(&rig, GROUP_KEY, 1001, "tester", "#TOOLERR#");
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["tool failed reply"]);
    says(&rig, GROUP_KEY, 1001, "tester", "#SILENT#");
    rig.wait_prompts(3);
    thread::sleep(Duration::from_secs(1));
    assert_eq!(rig.texts_to(GROUP_KEY).len(), 1);
    says(&rig, GROUP_KEY, 1001, "tester", "#FAIL#");
    rig.wait_prompts(4);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "failed")));
    assert_eq!(
        rig.texts_to(GROUP_KEY).len(),
        1,
        "model failures stay out of the group"
    );
    says(&rig, GROUP_KEY, 1001, "tester", "recovered-next-turn15");
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 2),
        ["tool failed reply", "synthetic reply"]
    );
}

#[test]
fn reserved_each_split_rechecks_mode_even_after_away_and_back() {
    let mut rig = Rig::new(
        "reserved-epoch15",
        reserved_seed(json!({"burstIntervalMinMs":2500,"burstIntervalMaxMs":2500})),
    );
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #SPLIT15#");
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["第一句"]);
    let original = rig.data_json("sessions.json");
    assert_eq!(rig.console("POST", "/api/mode", Some(&json!({"mode":"reserved"}))).0, 200);
    assert_eq!(rig.data_json("sessions.json"), original, "same-mode writes preserve the session");
    assert_eq!(
        rig.console("POST", "/api/mode", Some(&json!({"mode":"chat"})))
            .0,
        200
    );
    assert_eq!(
        rig.console("POST", "/api/mode", Some(&json!({"mode":"reserved"})))
            .0,
        200
    );
    thread::sleep(Duration::from_secs(4));
    assert_eq!(
        rig.texts_to(GROUP_KEY),
        ["第一句"],
        "old split parts have lost authority"
    );
    let sessions = rig.data_json("sessions.json");
    assert!(sessions["sessions"].get(GROUP_KEY).is_none(), "mode change retires the old session mapping");
    assert!(sessions["sessionPolicies"].get(GROUP_KEY).is_none(), "mode change retires the old session policy");
    assert!(!rig.dsh.calls("session/cancel").is_empty(), "retirement stops old DSH work");
    assert_eq!(rig.dsh.calls("workspace/archiveSession").len(), 1, "retirement archives the old session once");
    assert!(rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .any(|r| r["k"] == "cancelled"));
}

#[test]
fn reserved_mode_exit_retires_public_state_and_syncs_dsh_before_returning() {
    let mut rig = Rig::new(
        "reserved-retirement15",
        reserved_seed(json!({"activeReplyDelayMinMs":3000,"activeReplyDelayMaxMs":3000})),
    );
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "watching-summary15");
    assert!(wait_for(LONG, || rig.console("GET", "/api/social", None).1
        ["pendingSummaries"][GROUP_KEY] == 1));
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 pending-reserved15");
    wait_phase(&rig, "active");
    for mode in ["chat", "reserved"] {
        let (status, response) = rig.console("POST", "/api/mode", Some(&json!({"mode":mode})));
        assert_eq!(status, 200);
        let snapshot = rig.console("GET", "/api/social", None).1;
        assert_eq!(snapshot["states"], json!({}), "mode exit must retire states before returning");
        assert_eq!(snapshot["pendingSummaries"], json!({}), "mode exit must clear pending summaries");
        assert_eq!(response["dshSynced"], true, "successful DSH settings write must be reported");
    }
    let writes = rig.dsh.calls("settings/update");
    assert_eq!(writes, vec![json!({"ns":"qq-mode","patch":{"mode":"chat"}}),
        json!({"ns":"qq-mode","patch":{"mode":"reserved"}})]);
    thread::sleep(Duration::from_secs(4));
    assert!(rig.dsh.prompts().is_empty(), "retired delayed input must never run");
    assert!(rig.texts_to(GROUP_KEY).is_empty());
}

#[test]
fn reserved_social_post_returns_effective_config_then_http_reloads_partial() {
    let mut rig = Rig::new("reserved-config15", reserved_seed(json!({
        "burstIntervalMinMs":0,"burstIntervalMaxMs":0,"longGapMinMs":0,"longGapMaxMs":0
    })));
    rig.start();
    let initial = rig.console("GET", "/api/social", None).1;
    assert_eq!(initial["config"]["enabled"], true);
    assert_eq!(initial["config"]["contextWindow"], 20);
    assert_eq!(initial["config"]["idleRetryWaitMs"], 120000);
    let (status, posted) = rig.console("POST", "/api/social", Some(&json!({"longGapProbability":1})));
    assert_eq!(status, 200);
    let mut expected = initial["config"].clone();
    expected["longGapProbability"] = json!(1);
    assert_eq!(posted, json!({"ok":true,"config":expected}));
    let stored: Value = serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
    assert!(stored["social"].get("enabled").is_none());
    assert_eq!(rig.console("GET", "/api/social", None).1["config"], stored["social"]);
    let (status, posted) = rig.console("POST", "/api/social", Some(&json!({
        "longGapMinMs":"9","longGapMaxMs":-2,"longGapProbability":8
    })));
    assert_eq!(status, 200);
    let mut expected = stored["social"].clone();
    expected["longGapMinMs"] = json!(0);
    expected["longGapMaxMs"] = json!(9);
    assert_eq!(posted, json!({"ok":true,"config":expected}));
    assert_eq!(rig.console("GET", "/api/social", None).1["config"], expected);
}

#[test]
fn reserved_admin_idle_and_restart_invalidate_delayed_jobs() {
    let mut rig = Rig::new(
        "reserved-restart15",
        reserved_seed(json!({"activeReplyDelayMinMs":3000,"activeReplyDelayMaxMs":3000})),
    );
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 old-delayed15");
    wait_phase(&rig, "active");
    assert_eq!(
        rig.console(
            "POST",
            "/api/social/state",
            Some(&json!({"key":GROUP_KEY,"phase":"idle"}))
        )
        .0,
        200
    );
    thread::sleep(Duration::from_secs(4));
    assert!(rig.dsh.prompts().is_empty());
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 restart-delayed15");
    wait_phase(&rig, "active");
    let mut old = rig.candidate.take().unwrap();
    old.kill().unwrap();
    old.wait().unwrap();
    // A known queued event revoked by a mode change cannot replay on restart.
    assert_eq!(
        rig.console("POST", "/api/mode", Some(&json!({"mode":"chat"})))
            .0,
        200
    );
    assert_eq!(
        rig.console("POST", "/api/mode", Some(&json!({"mode":"reserved"})))
            .0,
        200
    );
    rig.start();
    thread::sleep(Duration::from_secs(4));
    assert!(rig.dsh.prompts().is_empty());
    says(
        &rig,
        GROUP_KEY,
        1001,
        "tester",
        "@9001 fresh-after-restart15",
    );
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["synthetic reply"]);
    assert!(rig.wait_prompts(1)[0]
        .text
        .contains("fresh-after-restart15"));
    assert!(!rig.dsh.prompts()[0].text.contains("restart-delayed15"));
}

#[test]
fn reserved_quote_only_self_is_a_direct_trigger_and_current_text_is_not_truncated() {
    let mut rig = Rig::new("reserved-quote15", reserved_seed(json!({})));
    rig.start();
    rig.push(group_event(
        15501,
        1001,
        "tester",
        "",
        vec![json!({"type":"reply","data":{"id":"5555"}})],
    ));
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["synthetic reply"]);
    let body = body_of(&rig.wait_prompts(1)[0].text).to_string();
    assert!(body.contains("[引用 bot：之前机器人说的话]"));
    assert!(body.contains("请回复消息。"));
    rig.console(
        "POST",
        "/api/social/state",
        Some(&json!({"key":GROUP_KEY,"phase":"idle"})),
    );
    let long = format!("@9001 {} tail-marker15", "文".repeat(250));
    says(&rig, GROUP_KEY, 1001, "tester", &long);
    let prompts = rig.wait_prompts(2);
    assert!(
        prompts[1]
            .text
            .split("【当前消息】")
            .nth(1)
            .unwrap()
            .contains("tail-marker15"),
        "current message must retain its full text; only recent context is capped"
    );
}

#[cfg(feature = "parity-clock")]
#[test]
fn reserved_selective_silence_is_seeded_and_carried_into_the_next_required_batch() {
    let mut rig = Rig::new(
        "reserved-selective15",
        reserved_seed(json!({"skipProbability":1})),
    );
    rig.start();
    assert_eq!(
        rig.console(
            "POST",
            "/api/social/state",
            Some(&json!({"key":GROUP_KEY,"phase":"active"}))
        )
        .0,
        200
    );
    says(
        &rig,
        GROUP_KEY,
        1001,
        "tester",
        "ordinary-seen-but-unanswered15",
    );
    assert!(wait_for(LONG, || rig.console("GET", "/api/social", None).1
        ["pendingSummaries"][GROUP_KEY]
        == 1));
    assert!(rig.dsh.prompts().is_empty());
    says(&rig, GROUP_KEY, 1001, "tester", "在吗 #SPLIT15#");
    let prompts = rig.wait_prompts(1);
    assert!(prompts[0].text.contains("ordinary-seen-but-unanswered15"));
    assert!(prompts[0].text.contains("在吗 #SPLIT15#"));
    assert!(
        !prompts[0].text.contains("请只输出 [SILENT]"),
        "required reply cannot opt out in its prompt"
    );
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 3),
        ["第一句", "hello world", "第二句"]
    );
}

#[test]
fn hf124_reserved_summary_is_normalized_before_burst_splitting() {
    let mut rig = Rig::new("hf124-summary-burst", reserved_seed(json!({})));
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #SUMMARY124#");
    assert!(wait_for(LONG, || rig.data_json("social-v1.json")["conversations"][GROUP_KEY]
        ["lastAiReplyAt"].as_i64().is_some_and(|t| t > 0)), "reply burst did not settle");
    assert_eq!(rig.texts_to(GROUP_KEY), ["前后"]);
    assert!(wait_for(LONG, || rig.data_lines("send-outbox.jsonl").iter()
        .filter(|r| r["k"] == "confirmed").count() == 1));
    let messages: Vec<_> = rig.data_lines("send-outbox.jsonl").into_iter()
        .filter(|r| r["k"] == "intent")
        .flat_map(|r| r["messages"].as_array().unwrap().clone()).collect();
    assert_eq!(messages, vec![json!("前后")]);
    let wire: Vec<_> = lock(&rig.gateway).calls.iter()
        .filter(|(a, b)| a == "send_group_msg" && b["group_id"] == GROUP_ID)
        .map(|(_, b)| b["message"].clone()).collect();
    assert_eq!(wire, vec![json!([{"type":"text","data":{"text":"前后"}}])]);
}

#[test]
fn hf124_automatic_face_at_utf16_boundary_matches_wire_and_outbox() {
    let mut rig = Rig::new("hf124-face-boundary", Seed {
        config: json!({"ackMessage":""}), ..Seed::default()
    });
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#FACEBOUNDARY124#");
    assert_face_reply_parts(&rig, vec!["a".repeat(3998), "[表情28]后".into()]);
}

#[test]
fn hf124_reserved_face_at_length_boundary_matches_wire_and_outbox() {
    let mut rig = Rig::new("hf124-reserved-face", reserved_seed(json!({"maxReplyChars":8})));
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #FACERESERVED124#");
    assert_face_reply_parts(&rig, vec!["abcdef".into(), "[表情28]后".into()]);
}

#[test]
fn hf124_reserved_face_longer_than_limit_stays_one_bubble() {
    let mut rig = Rig::new("hf124-small-face", reserved_seed(json!({"maxReplyChars":1})));
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #FACESMALL124#");
    assert_face_reply_parts(&rig, vec!["前".into(), "[表情28]".into(), "后".into()]);
}

#[test]
fn hf124_automatic_newline_split_records_the_text_actually_sent() {
    let mut rig = Rig::new("hf124-face-newline", Seed {
        config: json!({"ackMessage":""}), ..Seed::default()
    });
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#FACENEWLINE124#");
    assert_face_reply_parts(&rig, vec!["a".repeat(3998), "[表情28]后".into()]);
}

fn assert_face_reply_parts(rig: &Rig, expected: Vec<String>) {
    assert!(wait_for(LONG, || rig.data_lines("send-outbox.jsonl").iter()
        .filter(|r| r["k"] == "confirmed").count() >= expected.len()), "reply did not settle");
    let wire: Vec<_> = lock(&rig.gateway).calls.iter()
        .filter(|(a, b)| a == "send_group_msg" && b["group_id"] == GROUP_ID)
        .map(|(_, b)| b["message"].clone()).collect();
    assert_eq!(wire.len(), expected.len());
    assert_eq!(wire[1][0], json!({"type":"face","data":{"id":"28"}}),
        "the boundary face must reach QQ as a face segment");
    let mut actual = Vec::new();
    for segments in &wire {
        let text: String = segments.as_array().unwrap().iter().map(|segment| {
            match segment["type"].as_str().unwrap() {
                "text" => segment["data"]["text"].as_str().unwrap().to_owned(),
                "face" => format!("[表情{}]", segment["data"]["id"].as_str().unwrap()),
                other => panic!("unexpected reply segment: {other}"),
            }
        }).collect();
        actual.push(text);
    }
    assert_eq!(actual, expected);
    let messages: Vec<_> = rig.data_lines("send-outbox.jsonl").into_iter()
        .filter(|r| r["k"] == "intent")
        .flat_map(|r| r["messages"].as_array().unwrap().clone()).collect();
    assert_eq!(messages, actual.into_iter().map(Value::String).collect::<Vec<_>>(),
        "outbox parts must be exactly the canonical text sent on the wire");
}

#[test]
fn reserved_long_reply_preserves_url_atoms_and_punctuation_boundaries() {
    let mut rig = Rig::new(
        "reserved-url15",
        reserved_seed(json!({"maxReplyChars":4,"burstEnabled":false})),
    );
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #URLSPLIT15#");
    let sent = rig.wait_texts(GROUP_KEY, 4);
    assert_eq!(
        sent,
        ["甲，乙，", "https://example.invalid/long-path", "，", "尾"]
    );
}

#[test]
fn reserved_burst_definite_failure_still_attempts_the_third_sentence() {
    let mut rig = Rig::new(
        "reserved-burst-failure15",
        reserved_seed(json!({"burstIntervalMinMs":150,"burstIntervalMaxMs":150})),
    );
    lock(&rig.gateway).reject_text = Some("hello world".into());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #SPLIT15#");
    assert!(
        wait_for(LONG, || rig
            .data_lines("send-outbox.jsonl")
            .iter()
            .any(|r| r["k"] == "not_committed")),
        "second sentence must receive a definite gateway failure"
    );
    assert_eq!(
        rig.wait_texts(GROUP_KEY, 3),
        ["第一句", "hello world", "第二句"],
        "all three burst items must be attempted in order"
    );
    assert!(wait_for(LONG, || rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .filter(|r| r["k"] == "confirmed")
        .count()
        == 2));
    let state = rig.data_json("social-v1.json");
    assert_eq!(state["conversations"][GROUP_KEY]["lastAiReplyAt"], 0);
    assert_eq!(
        state["conversations"][GROUP_KEY]["lastActiveMessageAt"],
        state["conversations"][GROUP_KEY]["activeEnteredAt"],
        "a partial burst failure must not refresh activity"
    );
}

#[test]
fn reserved_burst_failure_preserves_activity_and_cooldown_timestamps() {
    let mut rig = Rig::new(
        "reserved-burst-timestamps15",
        reserved_seed(json!({"burstIntervalMinMs":150,"burstIntervalMaxMs":150})),
    );
    // A failure on the final item isolates timestamp accounting from burst continuation.
    lock(&rig.gateway).reject_text = Some("第二句".into());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #SPLIT15#");
    rig.wait_texts(GROUP_KEY, 3);
    assert!(wait_for(LONG, || rig
        .data_lines("send-outbox.jsonl")
        .iter()
        .any(|r| r["k"] == "not_committed")));
    // A private turn is a completion barrier without changing the group timestamps.
    says(&rig, OWNER_KEY, OWNER, "owner", "completion-barrier15");
    rig.wait_texts(OWNER_KEY, 1);
    let c = &rig.data_json("social-v1.json")["conversations"][GROUP_KEY];
    assert_eq!(
        c["lastAiReplyAt"], 0,
        "failed bursts must not start the selective-silence cooldown"
    );
    assert_eq!(
        c["lastActiveMessageAt"], c["activeEnteredAt"],
        "failed bursts must not refresh activity"
    );
}

#[test]
fn reserved_restart_retries_a_failed_presubmit_once() {
    restart_presubmit_failure(false);
}

#[test]
fn reserved_restart_retries_a_definitely_refused_submission_once() {
    restart_presubmit_failure(true);
}

fn restart_presubmit_failure(refused: bool) {
    let mut seed = reserved_seed(json!({}));
    if !refused {
        seed.config["agentPreset"] = json!("temporarily-missing-preset");
    }
    let mut rig = Rig::new(
        if refused {
            "reserved-refused-restart15"
        } else {
            "reserved-presubmit-restart15"
        },
        seed,
    );
    lock(&rig.dsh.inner).refuse_prompt = refused;
    rig.start();
    let input = group_event(
        150015,
        1001,
        "tester",
        "",
        vec![text_seg("@9001 recover-presubmit15")],
    );
    rig.push(input.clone());
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "failed")));
    assert!(
        rig.dsh.prompts().is_empty(),
        "failure must precede DSH submission"
    );
    assert_eq!(
        rig.ledger().iter().any(|r| r["k"] == "attempt"),
        refused,
        "exercise both no-RPC and definite-refusal recovery"
    );
    let mut old = rig.candidate.take().unwrap();
    old.kill().unwrap();
    old.wait().unwrap();
    let config_path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    cfg["agentPreset"] = json!("ticket12-chat");
    fs::write(&config_path, cfg.to_string()).unwrap();
    lock(&rig.dsh.inner).refuse_prompt = false;
    rig.start();
    rig.push(input);
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["synthetic reply"]);
    assert!(wait_for(LONG, || rig
        .ledger()
        .iter()
        .any(|r| r["k"] == "ended" && r["outcome"] == "completed")));
    let mut old = rig.candidate.take().unwrap();
    old.kill().unwrap();
    old.wait().unwrap();
    rig.start();
    thread::sleep(Duration::from_secs(3));
    assert_eq!(
        rig.dsh.prompts().len(),
        1,
        "one recovered submission across both restarts and duplicate ingress"
    );
    assert_eq!(rig.texts_to(GROUP_KEY), ["synthetic reply"]);
}

#[test]
fn reserved_restart_keeps_undispatched_batch_ingress_replayable() {
    let mut rig = Rig::new(
        "reserved-batch-restart15",
        reserved_seed(json!({"activeCheckMinMs":60000,"activeCheckMaxMs":60000})),
    );
    rig.start();
    rig.console(
        "POST",
        "/api/social/state",
        Some(&json!({"key":GROUP_KEY,"phase":"active"})),
    );
    says(&rig, GROUP_KEY, 1001, "tester", "undispatched-batch15");
    // The following private turn is an ingress barrier on the same gateway stream.
    says(&rig, OWNER_KEY, OWNER, "owner", "private-barrier15");
    rig.wait_texts(OWNER_KEY, 1);
    let mut old = rig.candidate.take().unwrap();
    old.kill().unwrap();
    old.wait().unwrap();
    rig.start();
    assert!(
        wait_for(LONG, || rig.console("GET", "/api/social", None).1
            ["pendingSummaries"][GROUP_KEY]
            == 1),
        "received active-batch input must replay into the fresh watching state"
    );
    assert!(rig.texts_to(GROUP_KEY).is_empty());
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 after-crash15");
    let prompts = rig.wait_prompts(2);
    assert!(prompts
        .last()
        .unwrap()
        .text
        .contains("undispatched-batch15"));
}

#[test]
fn reserved_admin_idle_revocation_survives_restart_before_the_next_tick() {
    let mut rig = Rig::new(
        "reserved-idle-revocation15",
        reserved_seed(json!({"activeCheckMinMs":60000,"activeCheckMaxMs":60000})),
    );
    rig.start();
    rig.console(
        "POST",
        "/api/social/state",
        Some(&json!({"key":GROUP_KEY,"phase":"active"})),
    );
    says(
        &rig,
        GROUP_KEY,
        1001,
        "tester",
        "@9001 stale-active-batch15",
    );
    says(&rig, OWNER_KEY, OWNER, "owner", "private-barrier15");
    rig.wait_texts(OWNER_KEY, 1);
    assert_eq!(
        rig.console(
            "POST",
            "/api/social/state",
            Some(&json!({"key":GROUP_KEY,"phase":"idle"}))
        )
        .0,
        200
    );
    let mut old = rig.candidate.take().unwrap();
    old.kill().unwrap();
    old.wait().unwrap();
    rig.start();
    thread::sleep(Duration::from_secs(3));
    assert!(
        rig.texts_to(GROUP_KEY).is_empty(),
        "revoked addressed ingress must not become a new turn on restart"
    );
    assert_eq!(
        rig.dsh.prompts().len(),
        1,
        "only the private ingress barrier was submitted"
    );
}

// ───────────────────────── ticket 24: access changes act on queued work immediately ─────────────────────────

/// The newest audit entry's id (the console lists newest first).
fn latest_audit_id(rig: &Rig, path: &str) -> Value {
    let (status, recent) = rig.console("GET", "/api/audit/recent?limit=20", None);
    assert_eq!(status, 200, "{recent}");
    recent["entries"].as_array().unwrap().iter().find(|e| e["path"] == path).unwrap_or_else(|| panic!("no audit entry for {path}: {recent}"))["id"].clone()
}

fn no_prompt_contains(rig: &Rig, marker: &str) -> bool {
    rig.dsh.prompts().iter().all(|p| !p.text.contains(marker))
}

#[test]
fn ticket24_chat_revoke_and_undo_drop_the_queued_job_and_the_late_reply() {
    let mut rig = Rig::new("t24-chat-revoke-undo", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#SLOW# first");
    rig.wait_texts(GROUP_KEY, 1);
    assert!(wait_for(LONG, || rig.data_json("sessions.json")["sessions"].get(GROUP_KEY).is_some()));
    // Queued behind the running turn under the current authority.
    says(&rig, GROUP_KEY, 1001, "tester", "queued-under-old-authority");
    thread::sleep(Duration::from_millis(800));
    let (status, reply) = rig.console("POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}})));
    assert_eq!(status, 200, "{reply}");
    let id = latest_audit_id(&rig, "/api/whitelist");
    let (status, reply) = rig.console("POST", "/api/audit/undo", Some(&json!({"id": id})));
    assert_eq!(status, 200, "{reply}");
    // After a config change JS shows its reloaded raw config values (numbers here).
    assert_eq!(rig.console("GET", "/api/whitelist", None).1["allow"]["groups"], json!([GROUP_ID]), "undo restored access");
    // The revocation stopped the running turn's DSH work; nothing of the old epoch reaches the group.
    assert!(wait_for(LONG, || !rig.dsh.calls("session/cancel").is_empty()), "revocation must stop DSH work");
    let t = rig.sentinel(GROUP_KEY, 1);
    assert_eq!(&t[1..], &[ACK.to_string(), "synthetic reply".to_string()], "no late reply and no queued job after undo: {t:?}");
    assert!(no_prompt_contains(&rig, "queued-under-old-authority"), "the queued job ran with the revoked authority: {:?}", rig.dsh.prompts());
    let last = rig.dsh.prompts().last().unwrap().session.clone();
    assert_ne!(last, rig.dsh.prompts()[0].session, "the re-granted conversation starts a new session");
    let entries = rig.console("GET", "/api/audit/recent?limit=20", None).1["entries"].clone();
    assert_eq!(entries[0]["path"], "/api/audit/undo");
    assert_eq!(entries[0]["ok"], true);
}

#[test]
fn ticket24_chat_mode_round_trip_drops_the_queued_job() {
    let mut rig = Rig::new("t24-chat-mode-trip", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#SLOW# first");
    rig.wait_texts(GROUP_KEY, 1);
    assert!(wait_for(LONG, || rig.data_json("sessions.json")["sessions"].get(GROUP_KEY).is_some()));
    says(&rig, GROUP_KEY, 1001, "tester", "queued-before-mode-change");
    thread::sleep(Duration::from_millis(800));
    assert_eq!(rig.console("POST", "/api/mode", Some(&json!({"mode": "reserved2"}))).0, 200);
    assert_eq!(rig.console("POST", "/api/mode", Some(&json!({"mode": "chat"}))).0, 200);
    let t = rig.sentinel(GROUP_KEY, 1);
    assert_eq!(&t[1..], &[ACK.to_string(), "synthetic reply".to_string()], "{t:?}");
    assert!(no_prompt_contains(&rig, "queued-before-mode-change"), "a job queued before the mode change ran after it: {:?}", rig.dsh.prompts());
}

#[test]
fn ticket24_social_v2_reset_drops_queued_chat_jobs() {
    let mut rig = Rig::new("t24-chat-v2-reset", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#SLOW# first");
    rig.wait_texts(GROUP_KEY, 1);
    assert!(wait_for(LONG, || rig.data_json("sessions.json")["sessions"].get(GROUP_KEY).is_some()));
    says(&rig, GROUP_KEY, 1001, "tester", "queued-before-reset");
    thread::sleep(Duration::from_millis(800));
    // JS drains every prompt queue on the second-generation reset.
    assert_eq!(rig.console("POST", "/api/socialV2/reset", Some(&json!({}))), (200, json!({"ok": true})));
    let sid = rig.dsh.prompts()[0].session.clone();
    // The product itself stops the running turn in DSH (no help from the fixture).
    assert!(
        wait_for(LONG, || rig.dsh.calls("session/cancel").iter().any(|c| c.pointer("/request/sessionId") == Some(&json!(sid)))),
        "the reset did not stop the running turn's DSH work: {:?}", rig.dsh.calls("session/cancel")
    );
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "cancelled")), "the running turn did not end cancelled: {:?}", rig.ledger());
    let t = rig.sentinel(GROUP_KEY, 1);
    assert_eq!(&t[1..], &[ACK.to_string(), "synthetic reply".to_string()], "no late reply of the reset turn and no queued job: {t:?}");
    assert!(no_prompt_contains(&rig, "queued-before-reset"), "a job queued before the reset ran after it: {:?}", rig.dsh.prompts());
}

#[test]
fn ticket24_closed_agent_owner_change_and_back_in_the_console_cancels_the_old_approval() {
    let mut rig = Rig::new("closed-t24-owner-console", Seed { mode: "closed-agent", ..Seed::default() });
    rig.start();
    says(&rig, OWNER_KEY, OWNER, "owner", "#APPROVAL#");
    assert!(wait_for(Duration::from_secs(12), || rig.console("GET", "/api/pending", None).1["pending"].as_array().is_some_and(|a| a.len() == 1)));
    // Owner A -> B -> A faster than the interaction maintenance tick.
    assert_eq!(rig.console("POST", "/api/whitelist", Some(&json!({"ownerQQ": STRANGER}))).0, 200);
    assert_eq!(rig.console("POST", "/api/whitelist", Some(&json!({"ownerQQ": OWNER}))).0, 200);
    assert!(wait_for(Duration::from_secs(12), || rig.dsh.calls("$events/result").len() == 1), "the stale approval was not cancelled");
    assert_eq!(rig.dsh.calls("$events/result")[0]["outcome"]["value"], "rejected");
    assert_eq!(rig.console("GET", "/api/pending", None).1, json!({"pending":[]}));
    // The owner's answer no longer reaches the old approval: it is a new request in a new session.
    says(&rig, OWNER_KEY, OWNER, "owner", "通过");
    assert!(wait_for(Duration::from_secs(12), || rig.dsh.prompts().len() == 2), "the answer did not start a new turn");
    assert_eq!(rig.dsh.calls("$events/result").len(), 1, "the late answer must not produce a second receipt");
    assert_ne!(rig.dsh.prompts()[1].session, rig.dsh.prompts()[0].session, "the revoked owner session must not be reused");
}

#[test]
fn ticket24_reserved_revoke_and_undo_between_splits_stop_the_remaining_parts() {
    let mut rig = Rig::new("reserved-t24-revoke", reserved_seed(json!({"burstIntervalMinMs":2500,"burstIntervalMaxMs":2500})));
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #SPLIT15#");
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["第一句"]);
    assert_eq!(rig.console("POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}}))).0, 200);
    let id = latest_audit_id(&rig, "/api/whitelist");
    let (status, reply) = rig.console("POST", "/api/audit/undo", Some(&json!({"id": id})));
    assert_eq!(status, 200, "{reply}");
    thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.texts_to(GROUP_KEY), ["第一句"], "split parts of the revoked epoch were still sent after the undo");
    assert!(rig.data_lines("send-outbox.jsonl").iter().any(|r| r["k"] == "cancelled"), "the refused parts are recorded as cancelled");
}

#[test]
fn ticket24_reserved_revoke_and_undo_drop_the_delayed_job_before_submission() {
    let mut rig = Rig::new("reserved-t24-delayed", reserved_seed(json!({"activeReplyDelayMinMs":3000,"activeReplyDelayMaxMs":3000})));
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 queued-before-revoke");
    wait_phase(&rig, "active");
    assert_eq!(rig.console("POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}}))).0, 200);
    let id = latest_audit_id(&rig, "/api/whitelist");
    assert_eq!(rig.console("POST", "/api/audit/undo", Some(&json!({"id": id}))).0, 200);
    thread::sleep(Duration::from_secs(4));
    assert!(rig.dsh.prompts().is_empty(), "the job delayed under the revoked epoch reached DSH: {:?}", rig.dsh.prompts());
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 after-undo");
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["synthetic reply"]);
    assert!(no_prompt_contains(&rig, "queued-before-revoke"), "{:?}", rig.dsh.prompts());
}

#[test]
fn ticket24_reserved_session_reset_between_splits_stops_the_remaining_parts() {
    let mut rig = Rig::new("reserved-t24-reset", reserved_seed(json!({"burstIntervalMinMs":2500,"burstIntervalMaxMs":2500})));
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #SPLIT15#");
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), ["第一句"]);
    let (status, reply) = rig.console("POST", "/api/session/reset", Some(&json!({"key": GROUP_KEY})));
    assert_eq!(status, 200, "{reply}");
    thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.texts_to(GROUP_KEY), ["第一句"], "split parts of the reset session were still sent");
}

// ───────────────────────── ticket 24 second fix round ─────────────────────────

/// Codex 4: a chat job that passed its queue check and then waits for its image must not run after
/// a revocation and undo that happened during the download.
#[test]
fn ticket24_fix2_a_chat_job_waiting_for_its_image_does_not_run_after_revoke_and_undo() {
    let mut rig = Rig::new("t24-chat-media-epoch", Seed::default());
    lock(&rig.gateway).hold_images = true;
    rig.start();
    rig.push(group_event(24101, 1001, "tester", "", vec![text_seg("image-before-revoke"), json!({"type":"image","data":{"file":"ticket24-hold.gif"}})]));
    assert!(wait_for(LONG, || rig.gateway_calls("get_image") > 0), "the image download never started");
    let (status, reply) = rig.console("POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}})));
    assert_eq!(status, 200, "{reply}");
    let id = latest_audit_id(&rig, "/api/whitelist");
    let (status, reply) = rig.console("POST", "/api/audit/undo", Some(&json!({"id": id})));
    assert_eq!(status, 200, "{reply}");
    lock(&rig.gateway).hold_images = false;
    thread::sleep(Duration::from_secs(3));
    assert!(no_prompt_contains(&rig, "image-before-revoke"), "the job of the revoked epoch reached DSH after the undo: {:?}", rig.dsh.prompts());
    says(&rig, GROUP_KEY, 1001, "tester", "after-undo");
    assert!(wait_for(LONG, || rig.dsh.prompts().iter().any(|p| p.text.contains("after-undo"))), "the re-granted conversation does not answer");
    assert!(no_prompt_contains(&rig, "image-before-revoke"));
}

/// S2 through the product path: a whole reset while a reserved2 turn runs stops that turn in DSH,
/// the turn ends through the normal path, and the next message starts a new session.
#[test]
fn ticket24_fix2_a_whole_reset_stops_the_running_reserved2_turn_in_dsh() {
    let mut rig = Rig::new("t24-v2-reset-running", Seed { mode: "reserved2", ..Seed::default() });
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "#SLOW# before-reset");
    let first = rig.wait_prompts(1);
    let sid = first[0].session.clone();
    assert_eq!(rig.console("POST", "/api/socialV2/reset", Some(&json!({}))), (200, json!({"ok": true})));
    assert!(
        wait_for(LONG, || rig.dsh.calls("session/cancel").iter().any(|c| c.pointer("/request/sessionId") == Some(&json!(sid)))),
        "the reset did not stop the running turn's DSH work: {:?}", rig.dsh.calls("session/cancel")
    );
    assert!(
        wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "cancelled")),
        "the running turn did not end through the normal path: {:?}", rig.ledger()
    );
    says(&rig, GROUP_KEY, 1001, "tester", "after-reset");
    assert!(
        wait_for(LONG, || rig.dsh.prompts().iter().any(|p| p.text.contains("after-reset"))),
        "the conversation does not wake after the reset: {:?}", rig.dsh.prompts()
    );
    let prompts = rig.dsh.prompts();
    let after = prompts.iter().find(|p| p.text.contains("after-reset")).unwrap();
    assert_ne!(after.session, sid, "the reset session was reused");
    assert_eq!(prompts.iter().filter(|p| p.text.contains("before-reset") && p.session == sid).count(), 1, "the reset turn ran again: {prompts:?}");
}

/// Gap batch small-parity item 1: the stored reserved2 message of a private chat carries the sender
/// `私聊`, and an owner message carries JS `ownerLabel` (`bridge.js:11538`, `10290`).
#[test]
fn reserved2_stored_messages_use_the_private_sender_label_and_the_owner_label() {
    let mut rig = Rig::new("v2-stored-sender", Seed { mode: "reserved2", ..Seed::default() });
    rig.start();
    rig.push(private_event(31001, OWNER, "owner-nick", vec![text_seg("owner-private-marker")]));
    rig.push(private_event(31002, STRANGER, "stranger-nick", vec![text_seg("stranger-private-marker")]));
    rig.push(group_event(31003, OWNER, "owner-nick", "owner-card", vec![text_seg("owner-group-marker")]));
    rig.push(group_event(31004, 1001, "tester-nick", "", vec![text_seg("member-group-marker")]));
    let stored = |key: &str, marker: &str| -> Option<Value> {
        rig.data_json("social-v2.json")["conversations"][key]["recentMessages"]
            .as_array()?
            .iter()
            .find(|m| m["plain"].as_str().is_some_and(|p| p.contains(marker)))
            .cloned()
    };
    assert!(
        wait_for(LONG, || stored(OWNER_KEY, "owner-private-marker").is_some()
            && stored("private:7707", "stranger-private-marker").is_some()
            && stored(GROUP_KEY, "owner-group-marker").is_some()
            && stored(GROUP_KEY, "member-group-marker").is_some()),
        "stored messages missing: {}",
        rig.data_json("social-v2.json")
    );
    let owner_private = stored(OWNER_KEY, "owner-private-marker").unwrap();
    assert_eq!(owner_private["sender"], json!("私聊"));
    assert_eq!(owner_private["isOwner"], json!(true));
    assert_eq!(owner_private["ownerLabel"], json!("管理员（ownerQQ 8808）"));
    let stranger = stored("private:7707", "stranger-private-marker").unwrap();
    assert_eq!(stranger["sender"], json!("私聊"));
    assert_eq!(stranger["ownerLabel"], json!(""));
    let owner_group = stored(GROUP_KEY, "owner-group-marker").unwrap();
    assert_eq!(owner_group["sender"], json!("owner-card"));
    assert_eq!(owner_group["ownerLabel"], json!("管理员（ownerQQ 8808）"));
    let member = stored(GROUP_KEY, "member-group-marker").unwrap();
    assert_eq!(member["sender"], json!("tester-nick"));
    assert_eq!(member["ownerLabel"], json!(""));
}

#[test]
fn chat_reports_a_failed_reset_instead_of_silently_ignoring_it() {
    let mut rig = Rig::new("failed-reset-visible", Seed::default());
    rig.start();
    fs::write(rig.root.join("data/sessions.json"), json!({"sessions": {GROUP_KEY: "fake-session"}, "sessionPolicies": []}).to_string()).unwrap();
    says(&rig, GROUP_KEY, OWNER, "owner", "/reset");
    assert_eq!(rig.wait_texts(GROUP_KEY, 1), vec!["重置失败：runtime session data is invalid".to_string()], "a real reset failure must be visible to the owner");
}



// Production hotfix #81: observe the actual reserved2 CLI delivery boundary.
#[cfg(windows)]
#[test]
fn hotfix81_cli_exit_zero_policy_refusal_is_private_and_never_relayed() {
    let mut rig = Rig::new("hotfix81-cli-refusal", Seed { mode: "reserved2", config: json!({
        "security":{"outboundGuard":false},
        "socialV2":{"wake":{"maxWakeConfigReminders":3,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}},
        "backends": {"sessions": {GROUP_KEY: "agy-test"}, "list": [{
            "id": "agy-test", "kind": "cli", "provider": "google", "model": "gemini-fixture",
            "cmd": "powershell.exe", "args": ["-NoLogo", "-NoProfile", "-NonInteractive", "-File", "fake-agy.ps1"]
        }]}
    }), ..Seed::default() });
    fs::write(rig.root.join("resources/fake-agy.ps1"), r#"
$null = [Console]::In.ReadToEnd()
[Console]::Out.Write('{"type":"result","is_error":true,"error":{"code":"SAFETY","message":"The prompt could not be submitted: violate Google Generative AI Prohibited Use Policy. CHAT_SECRET_81"}}')
exit 0
"#).unwrap();
    rig.start();
    rig.push(group_event(81001, 1001, "tester", "", vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}), text_seg("CHAT_SECRET_81")]));
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended")), "turn did not end");
    assert!(rig.texts_to(GROUP_KEY).is_empty(), "CLI refusal leaked: {:?}", rig.sends());
    rig.wait_texts(OWNER_KEY, 1);
    thread::sleep(Duration::from_secs(5)); // bounded absence: closing reminders must not retry a failed CLI
    assert_eq!(rig.data_lines("logs/opencode-trial.jsonl").len(), 1, "one failed text-only turn, no wake-reminder replay");
    assert_eq!(rig.ledger().iter().filter(|r| r["k"] == "ended").count(), 1);
    let alerts = rig.texts_to(OWNER_KEY);
    assert_eq!(alerts.len(), 1, "one failure alert: {alerts:?}");
    assert!(alerts[0].contains(GROUP_KEY) && alerts[0].contains("agy-test") && alerts[0].contains("gemini-fixture") && alerts[0].contains("refusal"), "{alerts:?}");
    assert!(!alerts[0].contains("CHAT_SECRET_81"), "no chat text: {alerts:?}");
    assert!(alerts[0].contains("blacklist search will run"));
    let records = rig.data_lines("logs/model-refusals.jsonl");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["errorClass"], "refusal");
    assert_eq!(records[0]["errorCode"], "SAFETY");
    assert_eq!(records[0]["backend"], "agy-test");
    assert!(records[0]["messages"].as_array().unwrap().iter().any(|m| m["messageId"] == "81001" && m["seq"].as_i64().unwrap_or(0) > 0), "{records:?}");
    assert!(!serde_json::to_string(&records).unwrap().contains("CHAT_SECRET_81"));
    assert!(rig.dsh.prompts().is_empty(), "a failed pinned text turn must not be replayed through DSH");
}

#[test]
fn hotfix81_dsh_quota_alert_is_content_free_and_deduped_per_conversation() {
    let mut rig = Rig::new("hotfix81-dsh-quota", Seed { mode: "reserved2", config:json!({"socialV2":{"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}}}), ..Seed::default() });
    rig.start();
    for (index, key) in [GROUP_KEY, GROUP_KEY, "private:7707"].iter().enumerate() {
        let id = 81100 + index as i64;
        if *key == GROUP_KEY {
            rig.push(group_event(id, 1001, "tester", "", vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#RATE81# CHAT_SECRET_81")]));
        } else {
            rig.push(private_event(id, STRANGER, "tester", vec![text_seg("#RATE81# CHAT_SECRET_81")]));
        }
        assert!(wait_for(LONG, || rig.ledger().iter().filter(|r| r["k"] == "ended" && r["outcome"] == "failed").count() >= index + 1), "failed turn did not end: {:?}", rig.ledger());
    }
    rig.wait_texts(OWNER_KEY, 2);
    let alerts = rig.texts_to(OWNER_KEY);
    assert_eq!(alerts.len(), 2, "one per conversation and class within the window: {alerts:?}");
    assert!(alerts.iter().any(|t| t.contains(GROUP_KEY)) && alerts.iter().any(|t| t.contains("private:7707")), "{alerts:?}");
    assert!(alerts.iter().all(|t| t.contains("quota") && t.contains("ticket12-model") && !t.contains("CHAT_SECRET_81")), "metadata only: {alerts:?}");
    assert!(rig.texts_to(GROUP_KEY).is_empty() && rig.texts_to("private:7707").is_empty(), "{:?}", rig.sends());
    assert!(rig.data_lines("logs/model-refusals.jsonl").is_empty(), "quota is not a refusal");
}

#[test]
fn hotfix83_dsh_empty_candidates_records_carried_ids_once_per_turn_without_chat() {
    // Live DSH carries recent context in its authenticated packet. Offer the nine
    // older messages this fixture previously carried in the full recent block.
    let mut rig = Rig::new("hotfix83-dsh-refusal", Seed { mode: "reserved2", config:json!({"socialV2":{"context":{"wakeRecentLimit":9},"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}}}), ..Seed::default() });
    let mut state = rig.data_json("social-v2.json");
    state["conversations"][GROUP_KEY] = json!({"recentMessages":(0..12).map(|i| json!({"messageId":(83000+i).to_string(),"seq":i+1,"sender":"tester","text":"CHAT_SECRET_83"})).collect::<Vec<_>>(), "unread":[],"lastUnreadSeq":12,
        "wakeConfig":{"mode":"diving","infinite":true,"sleepUntil":null,"triggers":{"atMention":true},"batchWindowMs":1000,"confirmedAt":0,"confirmedBy":"default"}});
    fs::write(rig.root.join("data/social-v2.json"),state.to_string()).unwrap();
    rig.start();
    for (index, id) in [83099,83100].iter().enumerate() {
        rig.push(group_event(*id, 1001, "tester", "", vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#REFUSE83# CHAT_SECRET_83")]));
        assert!(wait_for(LONG, || rig.data_lines("logs/model-refusals.jsonl").len() == index + 1),"turn index {index}, records={:?}, ledger={:?}",rig.data_lines("logs/model-refusals.jsonl"),rig.ledger());
        assert!(wait_for(LONG, || rig.ledger().iter().filter(|r| r["k"] == "ended" && r["outcome"] == "failed").count() >= index+1));
    }
    let records = rig.data_lines("logs/model-refusals.jsonl");
    assert_eq!(records.len(), 2, "alert dedupe never deduplicates refusal records");
    let first = &records[0];
    assert_eq!(first["key"],GROUP_KEY);
    assert_eq!(first["backend"],"dsh");
    assert_eq!(first["provider"],"ticket12-provider");
    assert_eq!(first["model"],"ticket12-model");
    assert_eq!(first["errorClass"],"refusal");
    assert_eq!(first["errorCode"],"EMPTY_RESPONSE");
    assert!(first["time"].as_i64().unwrap() > 0);
    let refs=first["messages"].as_array().unwrap();
    assert!(refs.iter().any(|r| r["messageId"] == "83003"), "include the older recent messages actually carried: {refs:?}");
    assert!(refs.iter().any(|r| r["messageId"] == "83099"), "the incoming snapshot message: {refs:?}");
    assert!(!refs.iter().any(|r| r["messageId"] == "83000"), "uncarried archive messages are excluded: {refs:?}");
    assert!(refs.iter().all(|r| r["seq"].is_number()), "sequence metadata is retained: {refs:?}");
    let delivered = rig.dsh.prompts()[0].text.clone();
    let packet: Value = serde_json::from_str(delivered.split("【本次消息快照】\n").nth(1).unwrap().lines().next().unwrap()).unwrap();
    let offered: Vec<Value> = packet["messages"].as_array().unwrap().iter().chain(packet["recent"].as_array().unwrap())
        .map(|m| json!({"messageId":m["messageId"],"seq":m["seq"]})).collect();
    assert_eq!(refs, &offered, "refusal references must describe the delivered packet");
    assert!(!serde_json::to_string(&records).unwrap().contains("CHAT_SECRET_83"));
    rig.wait_texts(OWNER_KEY,1);
    assert_eq!(rig.texts_to(OWNER_KEY).len(),1);
    assert!(rig.texts_to(OWNER_KEY)[0].contains("blacklist search will run"));
    assert!(rig.texts_to(GROUP_KEY).is_empty());
}

#[test]
fn hotfix81_dsh_empty_answer_alerts_owner_without_a_conversation_send() {
    let mut rig=Rig::new("hotfix81-dsh-empty",Seed{mode:"reserved2",config:json!({"socialV2":{"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}}}),..Seed::default()});
    rig.start();
    rig.push(group_event(81400,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#EMPTY81#")]));
    rig.wait_texts(OWNER_KEY,1);
    assert!(wait_for(LONG,||rig.ledger().iter().any(|r|r["k"]=="ended"&&r["outcome"]=="failed")));
    assert_eq!(rig.texts_to(OWNER_KEY).len(),1);
    assert!(rig.texts_to(OWNER_KEY)[0].contains("class=empty-response"));
    assert!(rig.texts_to(GROUP_KEY).is_empty());
    assert!(rig.data_lines("logs/model-refusals.jsonl").is_empty());
    rig.push(group_event(81401,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#ABORT#")]));
    assert!(wait_for(LONG,||rig.ledger().iter().filter(|r|r["k"]=="ended").count()==2));
    assert_eq!(rig.ledger().iter().filter(|r|r["k"]=="ended").last().unwrap()["outcome"],"completed","an aborted turn is not a model/backend failure");
    assert_eq!(rig.texts_to(OWNER_KEY).len(),1);
}

#[test]
fn hotfix81_slang_backend_refusal_only_notifies_the_owner() {
    let rig=Rig::new("hotfix81-slang-refusal",Seed{config:json!({"slang":{"enabled":true,"learnerBackend":"dsh","learnerPreset":"ticket12-chat","extractMinMessages":1,"autoResearch":false}}),..Seed::default()});
    fs::write(rig.root.join("data/slang-windows.json"),json!({"windows":{GROUP_KEY:[{"sender":"tester","text":"#REFUSE83# CHAT_SECRET_SLANG","time":1}]},"consumedAt":{}}).to_string()).unwrap();
    assert_eq!(rig.console("POST","/api/slang/extract",Some(&json!({"key":GROUP_KEY}))).0,200);
    rig.wait_texts(OWNER_KEY,1);
    let alerts=rig.texts_to(OWNER_KEY);
    assert_eq!(alerts.len(),1);
    assert!(alerts[0].contains(GROUP_KEY)&&alerts[0].contains("class=refusal")&&!alerts[0].contains("CHAT_SECRET_SLANG"),"{alerts:?}");
    assert!(rig.texts_to(GROUP_KEY).is_empty());
    assert!(rig.texts_to("private:7707").is_empty());
    assert!(rig.data_json("slang.json").as_array().is_none_or(|entries|entries.is_empty()),"failed model output never becomes slang content");
}

#[cfg(windows)]
#[test]
fn hotfix81_cli_timeout_sends_one_owner_alert_and_no_refusal_record() {
    let mut rig = Rig::new("hotfix81-cli-timeout", Seed { mode:"reserved2",config:json!({
        "socialV2":{"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}},
        "backends":{"sessions":{GROUP_KEY:"timeout-cli"},"list":[{"id":"timeout-cli","kind":"cli","model":"timeout-model","cmd":"powershell.exe",
            "args":["-NoProfile","-NonInteractive","-File","timeout.ps1"]}]}
    }),..Seed::default() });
    fs::write(rig.root.join("resources/timeout.ps1"),"$null=[Console]::In.ReadToEnd()\nStart-Sleep -Seconds 90\n").unwrap();
    rig.start();
    rig.push(group_event(81200,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("CHAT_SECRET_TIMEOUT")]));
    rig.wait_texts(OWNER_KEY,1);
    assert!(wait_for(LONG,||rig.ledger().iter().any(|r|r["k"]=="ended" && r["outcome"]=="failed")));
    let alerts=rig.texts_to(OWNER_KEY);
    assert_eq!(alerts.len(),1);
    assert!(alerts[0].contains("class=timeout")&&alerts[0].contains("timeout-cli")&&alerts[0].contains("timeout-model")&&!alerts[0].contains("CHAT_SECRET_TIMEOUT"),"{alerts:?}");
    assert!(rig.texts_to(GROUP_KEY).is_empty());
    assert!(rig.dsh.prompts().is_empty());
    assert!(rig.data_lines("logs/model-refusals.jsonl").is_empty());
}

#[cfg(windows)]
#[test]
fn hotfix81_cli_results_distinguish_answers_errors_empty_candidates_and_failed_exits() {
    let mut rig=Rig::new("hotfix81-cli-results",Seed{mode:"reserved2",config:json!({
        "socialV2":{"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}},
        "backends":{"sessions":{GROUP_KEY:"plain-fixture"},"list":[{"id":"plain-fixture","kind":"cli","model":"fixture-model","cmd":"powershell.exe",
            "args":["-NoProfile","-NonInteractive","-File","result.ps1"]}]}
    }),..Seed::default()});
    fs::write(rig.root.join("resources/result.ps1"),r#"
$null=[Console]::In.ReadToEnd()
$dir=[IO.Path]::Combine($env:HER_RUNTIME_ROOT,'data')
[Console]::Out.Write([IO.File]::ReadAllText([IO.Path]::Combine($dir,'cli-result.txt')))
exit ([int][IO.File]::ReadAllText([IO.Path]::Combine($dir,'cli-exit.txt')))
"#).unwrap();
    let cases=[
        (r#"{"response":"ordinary safety discussion"}"#,0,false),
        ("The prompt could not be submitted: Google Generative AI Prohibited Use Policy CHAT_SECRET",0,true),
        ("Error: quota exhausted CHAT_SECRET",0,true),
        ("   ",0,true),
        (r#"{"ok":false,"message":"backend failed CHAT_SECRET"}"#,0,true),
        (r#"{"candidates":[]}"#,0,true),
        (r#"{"candidates":[{"finishReason":"SAFETY","content":{"parts":[]}}]}"#,0,true),
        (r#"{"error":{"type":"SAFETY","message":"rejected CHAT_SECRET"}}"#,0,true),
        (r#"{"error":{"code":403,"message":"policy violation CHAT_SECRET"}}"#,0,true),
        (r#"{"error":{"code":"ECONNRESET","message":"connection lost while checking safety CHAT_SECRET"}}"#,0,true),
        (r#"{"response":"must not be relayed"}"#,7,true),
        (r#"{"error":{"code":"SAFETY","message":"rejected CHAT_SECRET"}}"#,7,true),
    ];
    rig.start();
    for (index,(output,exit,failed)) in cases.iter().enumerate(){
        fs::write(rig.root.join("data/cli-result.txt"),output).unwrap();
        fs::write(rig.root.join("data/cli-exit.txt"),exit.to_string()).unwrap();
        rig.push(group_event(81300+index as i64,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("CHAT_SECRET")]));
        assert!(wait_for(LONG,||rig.ledger().iter().filter(|r|r["k"]=="ended").count()>=index+1));
        if !failed {rig.wait_texts(GROUP_KEY,1);}
        assert_eq!(rig.texts_to(GROUP_KEY),vec!["ordinary safety discussion"],"case {index}: only the successful answer is relayed");
    }
    rig.wait_texts(OWNER_KEY,5);
    assert_eq!(rig.texts_to(OWNER_KEY).len(),5,"dedupe independently by refusal, quota, empty response, network and backend class");
    assert!(rig.texts_to(OWNER_KEY).iter().all(|text|!text.contains("CHAT_SECRET")));
    let refusals=rig.data_lines("logs/model-refusals.jsonl");
    assert_eq!(refusals.len(),6);
    assert_eq!(refusals.last().unwrap()["errorCode"],"SAFETY","nonzero exits retain typed refusal metadata");
    assert!(refusals.iter().any(|r|r["errorCode"]=="403"),"numeric provider error codes remain metadata: {refusals:?}");
}

#[cfg(windows)]
#[test]
fn hotfix81_exit_zero_diagnostics_are_not_answers() {
    for (name, output, failed) in [
        ("traceback", "Traceback (most recent call last):\n  File \"cli.py\", line 12, in main\nRuntimeError: CHAT_SECRET", true),
        ("http", "HTTP 503 Service Unavailable\n<html>CHAT_SECRET</html>", true),
        ("events", "{\"type\":\"start\"}\n{\"type\":\"error\",\"error\":{\"code\":\"UPSTREAM_ERROR\",\"message\":\"CHAT_SECRET\"}}\n", true),
        ("policy", "We can discuss policy and how it works.", false),
    ] {
        let mut rig = Rig::new(&format!("hotfix81-zero-{name}"), Seed { mode:"reserved2", config:json!({
            "socialV2":{"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}},
            "backends":{"sessions":{GROUP_KEY:"plain-fixture"},"list":[{"id":"plain-fixture","kind":"cli","model":"fixture-model","cmd":"powershell.exe",
                "args":["-NoProfile","-NonInteractive","-File","result.ps1"]}]}
        }), ..Seed::default() });
        fs::write(rig.root.join("resources/result.ps1"), r#"
$null=[Console]::In.ReadToEnd()
[Console]::Out.Write([IO.File]::ReadAllText([IO.Path]::Combine($env:HER_RUNTIME_ROOT,'data','cli-result.txt')))
exit 0
"#).unwrap();
        fs::write(rig.root.join("data/cli-result.txt"), output).unwrap();
        rig.start();
        rig.push(group_event(81400,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("CHAT_SECRET")]));
        assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended")));
        if failed {
            rig.wait_texts(OWNER_KEY,1);
            assert!(rig.texts_to(GROUP_KEY).is_empty(), "{name}: {:?}", rig.sends());
            let alerts = rig.texts_to(OWNER_KEY);
            assert_eq!(alerts.len(),1,"{name}");
            assert!(alerts[0].contains("plain-fixture") && !alerts[0].contains("CHAT_SECRET"));
        } else {
            rig.wait_texts(GROUP_KEY,1);
            assert_eq!(rig.texts_to(GROUP_KEY),vec![output]);
            assert!(rig.texts_to(OWNER_KEY).is_empty());
        }
    }
}

#[cfg(windows)]
#[test]
fn hotfix81_dsh_quota_then_cli_refusal_reports_both_failures() {
    let mut rig = Rig::new("hotfix81-fallback-refusal", Seed { mode:"reserved2", config:json!({
        "socialV2":{"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}},
        "backends":{"failoverFallback":true,"list":[{"id":"agy-fallback","kind":"cli","provider":"cli-provider","model":"cli-model","cmd":"powershell.exe",
            "args":["-NoProfile","-NonInteractive","-File","refusal.ps1"]}]}
    }), ..Seed::default() });
    fs::write(rig.root.join("resources/refusal.ps1"), r#"
$null=[Console]::In.ReadToEnd()
[Console]::Out.Write('{"error":{"code":"SAFETY","message":"CHAT_SECRET"}}')
exit 0
"#).unwrap();
    rig.start();
    rig.push(group_event(81500,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#RATE81# CHAT_SECRET")]));
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended" && r["outcome"] == "failed")));
    let records = rig.data_lines("logs/model-refusals.jsonl");
    assert_eq!(records.len(),1,"the fallback refusal is recorded even after DSH quota failure");
    assert_eq!(records[0]["backend"],"agy-fallback");
    assert_eq!(records[0]["provider"],"cli-provider");
    assert_eq!(records[0]["model"],"cli-model");
    assert_eq!(records[0]["errorCode"],"SAFETY");
    assert!(records[0]["messages"].as_array().unwrap().iter().any(|m| m["messageId"] == "81500"));
    assert!(!records[0].to_string().contains("CHAT_SECRET"));
    rig.wait_texts(OWNER_KEY,2);
    let alerts = rig.texts_to(OWNER_KEY);
    assert_eq!(alerts.len(),2,"{alerts:?}");
    assert!(alerts.iter().any(|s|s.contains("backend=dsh") && s.contains("class=quota")),"{alerts:?}");
    assert!(alerts.iter().any(|s|s.contains("backend=agy-fallback") && s.contains("model=cli-model") && s.contains("class=refusal") && s.contains("blacklist search will run")),"{alerts:?}");
    assert!(alerts.iter().all(|s|!s.contains("CHAT_SECRET")));
    assert!(rig.texts_to(GROUP_KEY).is_empty());

    // Refusal records belong to each backend's failed attempt, even when another
    // backend already refused this turn. Alerts still deduplicate by class.
    fs::write(rig.root.join("config/model-text-blacklist.json"), r##"{"strip":["#RATE81#"]}"##).unwrap();
    rig.push(group_event(81501,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#REFUSE83# CHAT_SECRET")]));
    assert!(wait_for(LONG, || rig.ledger().iter().filter(|r| r["k"] == "ended" && r["outcome"] == "failed").count() == 2));
    let records = rig.data_lines("logs/model-refusals.jsonl");
    assert_eq!(records.len(),3,"both DSH and CLI refusals have their own identity");
    assert_eq!(records.iter().filter(|r|r["backend"] == "agy-fallback").count(),2);
    assert_eq!(rig.texts_to(OWNER_KEY).len(),2,"same conversation/classes remain deduplicated");
    assert!(rig.texts_to(GROUP_KEY).is_empty());
}

#[cfg(windows)]
#[test]
fn hotfix83_blacklist_changes_between_cli_turns_and_bad_file_only_warns_once() {
    let mut rig = Rig::new("hotfix83-blacklist", Seed { mode: "reserved2", config: json!({
        "socialV2":{"context":{"stripFromModelText":["CFG83"]},"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}},
        "backends":{"sessions":{GROUP_KEY:"agy-test"},"list":[{"id":"agy-test","kind":"cli","cmd":"powershell.exe",
            "args":["-NoProfile","-NonInteractive","-File","fake-agy.ps1"]}]}
    }), ..Seed::default() });
    fs::write(rig.root.join("resources/fake-agy.ps1"), r#"
$text=[Console]::In.ReadToEnd()
$dir=[IO.Path]::Combine($env:HER_RUNTIME_ROOT,'data')
$count=@([IO.Directory]::GetFiles($dir,'cli-input-*.txt')).Length+1
[IO.File]::WriteAllText([IO.Path]::Combine($dir,"cli-input-$count.txt"),$text)
[IO.File]::WriteAllText([IO.Path]::Combine($dir,'cli-args.txt'),($args -join ' '))
[Console]::Out.Write('{"response":"synthetic cli reply"}')
"#).unwrap();
    rig.start();
    for turn in 1..=10 {
        if turn == 2 { fs::write(rig.root.join("config/model-text-blacklist.json"), r#"{"strip":["WORD83","83002"]}"#).unwrap(); }
        if turn == 3 { fs::write(rig.root.join("config/model-text-blacklist.json"), "invalid JSON CHAT_SECRET_83").unwrap(); }
        if turn == 5 {
            fs::remove_file(rig.root.join("config/model-text-blacklist.json")).unwrap();
            fs::create_dir(rig.root.join("config/model-text-blacklist.json")).unwrap();
        }
        if turn == 7 {
            fs::remove_dir(rig.root.join("config/model-text-blacklist.json")).unwrap();
            fs::write(rig.root.join("config/model-text-blacklist.json"), json!({"strip":["WORD83"],"padding":"x".repeat(65536)}).to_string()).unwrap();
        }
        if turn == 9 {
            fs::write(rig.root.join("config/model-text-blacklist.json"), json!({"strip":vec!["WORD83";501]}).to_string()).unwrap();
        }
        if turn == 10 {
            fs::write(rig.root.join("config/model-text-blacklist.json"), json!({"strip":["WORD83","字".repeat(65)]}).to_string()).unwrap();
        }
        rig.push(group_event(83000 + turn, 1001, "tester", "", vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("CFG83 WORD83")]));
        rig.wait_texts(GROUP_KEY, turn as usize);
        let prompt = fs::read_to_string(rig.root.join(format!("data/cli-input-{turn}.txt"))).unwrap();
        assert!(!prompt.contains("CFG83"), "the config list stays merged");
        assert_eq!(prompt.contains("WORD83"), turn != 2, "the next turn reads the file without a restart");
        if turn == 2 { assert!(prompt.contains(r#""messageId":"83002""#), "strip entries never erase transport IDs: {prompt}"); }
    }
    assert_eq!(rig.texts_to(GROUP_KEY), vec!["synthetic cli reply";10], "only the JSON response field is relayed");
    assert!(rig.texts_to(OWNER_KEY).is_empty(), "bad blacklist files never fail a turn");
    assert!(fs::read_to_string(rig.root.join("data/cli-args.txt")).unwrap().contains("--output-format json"));
    let warnings = fs::read_to_string(rig.root.join("candidate-err.log")).unwrap();
    assert_eq!(warnings.matches("[model-text-blacklist] state=invalid").count(), 2, "oversized files are invalid; repeated invalid states warn only once: {warnings}");
    assert_eq!(warnings.matches("[model-text-blacklist] state=unreadable").count(), 1, "unreadable files are empty and warn once: {warnings}");
    assert!(!warnings.contains("CHAT_SECRET_83"));
    assert!(fs::read_to_string(rig.root.join("data/social-v2.json")).unwrap().contains("CFG83 WORD83"), "stored chat stays original");
    assert!(!fs::read_to_string(rig.root.join("config/config.json")).unwrap().contains("WORD83"), "the file overlay is never persisted to config");
}

#[test]
fn queued_chat_waits_for_update_admission_without_losing_the_message() {
    let mut rig = Rig::new("update-admission", Seed::default());
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "before update fence");
    assert_eq!(rig.wait_texts(GROUP_KEY, 2), vec![ACK, "synthetic reply"]);
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(rig.root.join("data/locks/turn-update.lock"))
        .unwrap();
    assert!(
        wait_for(Duration::from_secs(10), || gate.try_lock().is_ok()),
        "the baseline pipeline must finish before holding admission"
    );
    says(&rig, GROUP_KEY, 1001, "tester", "waiting-update-fence");
    assert!(
        wait_for(Duration::from_secs(10), || rig
            .console("GET", "/api/status", None)
            .1
            .to_string()
            .contains("waiting-update-fence")),
        "the message must reach live ingress while admission is held"
    );
    thread::sleep(Duration::from_secs(3));
    assert_eq!(
        rig.dsh.prompts().len(),
        1,
        "an update fence must prevent model dispatch"
    );
    assert_eq!(rig.texts_to(GROUP_KEY).len(), 2);
    drop(gate);
    assert!(
        wait_for(Duration::from_secs(10), || rig.texts_to(GROUP_KEY).len()
            == 4),
        "releasing update admission must run the waiting message"
    );
    assert_eq!(rig.dsh.prompts().len(), 2);
    assert!(rig.dsh.prompts()[1].text.contains("waiting-update-fence"));

    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(rig.root.join("data/locks/turn-update.lock"))
        .unwrap();
    assert!(wait_for(Duration::from_secs(10), || gate
        .try_lock()
        .is_ok()));
    says(&rig, GROUP_KEY, 1001, "tester", "obsolete-update-fence");
    assert!(wait_for(Duration::from_secs(10), || rig
        .console("GET", "/api/status", None)
        .1
        .to_string()
        .contains("obsolete-update-fence")));
    thread::sleep(Duration::from_millis(500));
    says(&rig, GROUP_KEY, OWNER, "owner", "/reset");
    assert!(rig.wait_texts(GROUP_KEY, 5)[4].contains("已重置会话"));
    drop(gate);
    says(&rig, GROUP_KEY, 1001, "tester", "after update reset");
    rig.wait_texts(GROUP_KEY, 7);
    assert_eq!(rig.dsh.prompts().len(), 3);
    assert!(rig.dsh.prompts()[2].text.contains("after update reset"));
    assert!(
        rig.dsh
            .prompts()
            .iter()
            .all(|prompt| !prompt.text.contains("obsolete-update-fence")),
        "a reset while admission waits must invalidate the queued epoch"
    );
}

struct DependencyMetadata16 {
    url: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl DependencyMetadata16 {
    fn new() -> Self {
        let listener=TcpListener::bind(("127.0.0.1",0)).unwrap();
        let url=format!("http://127.0.0.1:{}/",listener.local_addr().unwrap().port());
        listener.set_nonblocking(true).unwrap();
        let stop=Arc::new(std::sync::atomic::AtomicBool::new(false));let done=stop.clone();
        let worker=thread::spawn(move||while !done.load(std::sync::atomic::Ordering::SeqCst) {
            if let Ok((mut stream,_))=listener.accept() {
                let _=read_request(&mut stream);
                respond(&mut stream,200,&json!({"dist-tags":{"latest":"0.3.0"},"versions":{"0.3.0":{}}}));
            } else {thread::sleep(Duration::from_millis(10));}
        });
        Self{url,stop,worker:Some(worker)}
    }
}
impl Drop for DependencyMetadata16 {
    fn drop(&mut self) {self.stop.store(true,std::sync::atomic::Ordering::SeqCst);self.worker.take().unwrap().join().unwrap();}
}
#[test]
fn update_deps_private_administrator_approves_only_after_a_confirmed_reply() {
    let mut confirmed_report=String::new();
    for failure in ["success","refusal","quota","server","malformed","empty","lost"] {
        let feed=DependencyMetadata16::new();
        let mut rig=Rig::new(&format!("dependency-approval-{failure}"),Seed{config:json!({"update":{"policy":"notify"},"dependencies":{"dsh":{"version":"0.2.1-rc.2","registry":feed.url}}}),..Seed::default()});
        let (code,check)=rig.console("POST","/api/dependencies/check",Some(&json!({})));
        assert_eq!(code,200,"{check}");assert_eq!(check["dependencies"]["dsh"]["available"],"0.3.0");
        rig.start();
        if failure=="success" {
            says(&rig,GROUP_KEY,OWNER,"owner","/UPDATE DEPS");
            rig.wait_texts(GROUP_KEY,1);
            says(&rig,"private:7707",STRANGER,"stranger","/update deps");
            rig.wait_texts("private:7707",1);
            assert_eq!(rig.console("GET","/api/dependencies",None).1["approval"],Value::Null);
            assert!(rig.dsh.prompts().is_empty());
        } else {
            let result=match failure {
                "refusal"=>json!({"status":"failed","retcode":1400}),
                "quota"=>json!({"status":"failed","retcode":429}),
                "server"=>json!({"status":"failed","retcode":500}),
                "empty"=>json!({}),
                _=>json!({"status":"not-a-valid-reply"}),
            };
            if failure=="lost" {
                lock(&rig.gateway).lose_text=Some(confirmed_report.clone());
            } else {
                let expected=confirmed_report.clone();
                lock(&rig.gateway).intercept=Some(Arc::new(move|action,body| {
                    let text:String=body["message"].as_array().into_iter().flatten().filter_map(|s|s["data"]["text"].as_str()).collect();
                    (action.starts_with("send_") && text==expected).then(||result.clone())
                }));
            }
        }
        says(&rig,OWNER_KEY,OWNER,"owner","/UpDaTe DePs");
        rig.wait_texts(OWNER_KEY,1);
        if failure=="success" {
            assert!(wait_for(LONG,||rig.console("GET","/api/dependencies",None).1["approval"]["version"]=="0.3.0"));
            let replies=rig.texts_to(OWNER_KEY).join("\n");assert!(replies.contains("0.2.1-rc.2") && replies.contains("0.3.0"));
            confirmed_report=rig.texts_to(OWNER_KEY)[0].clone();
        } else {
            // A second control command is an observable FIFO barrier, without a model turn.
            says(&rig,OWNER_KEY,OWNER,"owner","/update");
            rig.wait_texts(OWNER_KEY,2);
            assert_eq!(rig.console("GET","/api/dependencies",None).1["approval"],Value::Null);
        }
        assert!(rig.dsh.prompts().is_empty(),"update commands never reach the model");
    }
}

fn hf129_now_ms() -> i64 { if cfg!(feature="parity-clock") { 1_760_000_000_000 } else { now_ms() } }

// The fake model waits for an explicit release, so arrivals and tool reads are ordered by witnesses.
fn hf129_rig(label: &str) -> Rig {
    let rig = Rig::new(label, Seed { mode:"reserved2", config:json!({"socialV2":{
        "proactive":{"enabled":false},
        "wake":{"preSleepWaitMs":1000,"maxWakeConfigReminders":1,"maxWakePerMinute":100,"maxWakePerHour":1000},
        "reply":{"wakeQuietMs":1,"quietMinMs":1,"quietMedianMs":1,"quietMaxMs":1},
        "send":{"firstGapEnabled":false},
        "wait":{"minMs":1,"maxMs":1000,"maxPerTurn":1}
    }}), ..Seed::default() });
    *lock(&rig.dsh.loop_fixture) = Some((rig.root.clone(),rig.console_port,rig.gateway.clone()));
    fs::write(rig.root.join("data/social-v2.json"),json!({"conversations":{(GROUP_KEY):{
        "agentToken":"wake-test-token","bootstrapSent":true,"lastUnreadSeq":0,"lastReadThroughSeq":0,
        "unread":[],"recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,
            "confirmedAt":1,"triggers":{"atMention":true,"question":true,"probability":1.0}}
    }}}).to_string()).unwrap();
    rig
}

fn hf129_decisions(rig: &Rig) -> Vec<Value> { rig.data_lines("logs/rbridge-live.jsonl").into_iter().filter(|v|v["shouldWake"].is_boolean()).collect() }

#[test]
fn hf129_probability_and_question_during_turn_never_queue_a_followup() {
    let mut rig = hf129_rig("hf129-in-turn");
    rig.start();
    rig.push(group_event(129001,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    for (id,text) in [(129002,"ordinary busy message"),(129003,"你有 Claude 一半强吗？")] {
        rig.push(group_event(id,1001,"tester","",vec![text_seg(text)]));
    }
    assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 3));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("turn ended")));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),1,"busy unread messages must not cause another turn");
    let decisions=hf129_decisions(&rig);
    assert_eq!(decisions[1]["reason"],"in-turn");
    assert_eq!(decisions[1]["shouldWake"],false);
    assert_eq!(decisions[2]["reason"],"in-turn");
    assert_eq!(decisions[2]["shouldWake"],false);
}

fn hf129_tool(rig: &Rig, method: &str, path: &str, body: Option<&Value>) -> (u16,Value) {
    let state=rig.data_json("social-v2.json");
    let token=state["conversations"][GROUP_KEY]["agentToken"].as_str().unwrap();
    http_call(rig.console_port,method,path,&[("x-console-token",CONSOLE_TOKEN),("x-agent-token",token)],body).unwrap()
}

#[test]
fn hf129_urgent_after_last_read_queues_exactly_one_and_covered_wakes_drop() {
    for read in ["none","mark","wait"] {
        let mut rig=hf129_rig(&format!("hf129-covered-{read}"));
        rig.start();
        rig.push(group_event(129010,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
        rig.wait_prompts(1);
        // Wake snapshot has offered seq 1. Both urgent messages are after that last read.
        rig.push(group_event(129011,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("urgent after read")]));
        rig.push(group_event(129012,OWNER,"owner","",vec![text_seg("owner after read")]));
        assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 3));
        // An ordinary newer unread message must never resurrect covered urgent wakes.
        if read != "none" {
            let (status,packet)=if read=="wait" || read=="mark" {
                hf129_tool(&rig,"POST","/api/socialV2/wait",Some(&json!({"key":GROUP_KEY,"purpose":"messages","timeoutMs":1})))
            } else {
                hf129_tool(&rig,"GET",&format!("/api/socialV2/unread?key={GROUP_KEY}"),None)
            };
            assert_eq!(status,200,"{packet}");
            assert_eq!(packet["readThroughSeq"],3,"{packet}");
            if read=="mark" {
                let (status,result)=hf129_tool(&rig,"POST","/api/socialV2/mark-read",Some(&json!({"key":GROUP_KEY,"throughSeq":3})));
                // Observation rules stay binding; use the existing wait-cap escape below if needed.
                assert_eq!(status,200,"{result}");
            }
            rig.push(group_event(129013,1001,"tester","",vec![text_seg("newer unrelated unread")]));
            assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 4));
        }
        fs::write(rig.root.join("hf129-release"),"").unwrap();
        assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("turn ended")));
        if read=="none" { rig.wait_prompts(2); }
        thread::sleep(Duration::from_secs(2));
        assert_eq!(rig.dsh.prompts().len(),if read=="none" {2} else {1},"read={read}");
        if read!="none" {
            let dropped:Vec<_>=hf129_decisions(&rig).into_iter().filter(|d|d["reason"]=="covered").collect();
            assert_eq!(dropped.len(),2,"{dropped:?}");
            assert!(dropped.iter().all(|d|d["shouldWake"]==false));
        }
    }
}

#[test]
fn hf130_questions_between_other_members_do_not_wake() {
    let mut rig=hf129_rig("hf130-unaddressed");
    let mut state=rig.data_json("social-v2.json");
    state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"]=json!({"question":true});
    fs::write(rig.root.join("data/social-v2.json"),state.to_string()).unwrap();
    rig.start();
    for (id,text) in [(130001,"今天吃啥？"),(130002,"你有Claude一半强吗？"),(130003,"GPT模型谁更强？"),(130004,"说话")] {
        rig.push(group_event(id,1001,"tester","",vec![text_seg(text)]));
    }
    assert!(wait_for(LONG, || hf129_decisions(&rig).len()>=4));
    let decisions=hf129_decisions(&rig);
    assert!(decisions.iter().all(|d|d["shouldWake"]==false),"{decisions:?}");
    assert!(rig.dsh.prompts().is_empty());
}

#[test]
fn hf130_question_direction_requires_a_quote_or_a_recent_exchange() {
    for (label,age,participant,quote,other_at,text,want) in [
        ("quote",200_000,false,true,false,"今天吃啥？",true),
        ("answered",1000,true,false,false,"你还有推荐吗？",true),
        ("unrelated",1000,false,false,false,"你有推荐吗？",false),
        ("expired",100_000,true,false,false,"你还有推荐吗？",false),
        ("other-at",1000,true,false,true,"你还有推荐吗？",false),
        ("quote-other-at",1000,true,true,true,"今天吃啥？",false),
        ("nag-recent",1000,true,false,false,"说话",true),
        ("nag-expired",100_000,true,true,false,"说话",false),
    ] {
        let mut rig=hf129_rig(&format!("hf130-{label}"));
        let mut state=rig.data_json("social-v2.json");
        let conv=&mut state["conversations"][GROUP_KEY];
        conv["wakeConfig"]["triggers"]=json!({"question":true});
        // A previous confirmed targeted answer; no textual alias or AI keyword establishes direction.
        let at=hf129_now_ms()-age;
        conv["lastAiReplyAt"]=json!(at);
        conv["recentMessages"]=json!([{"isSelf":true,"text":"prior answer","time":at,"replyToUserId":if participant {"1001"}else{"1002"}}]);
        fs::write(rig.root.join("data/social-v2.json"),state.to_string()).unwrap();
        rig.start();
        let mut segments=vec![];
        if quote { segments.push(json!({"type":"reply","data":{"id":"5555"}})); }
        if other_at { segments.push(json!({"type":"at","data":{"qq":"1002"}})); }
        segments.push(text_seg(text));
        rig.push(group_event(130010,1001,"tester","",segments));
        assert!(wait_for(LONG, || !hf129_decisions(&rig).is_empty()));
        let decisions=hf129_decisions(&rig);
        assert_eq!(decisions[0]["shouldWake"],want,"{label}: {decisions:?}");
        if want { assert_eq!(decisions[0]["reason"],"question","{label}"); rig.wait_prompts(1); }
        else { assert!(rig.dsh.prompts().is_empty(),"{label}"); }
    }
}

#[test]
fn hf129_idle_probability_and_hf130_question_switch_remain_independent() {
    for probability in [0.0,1.0] {
        let mut rig=hf129_rig(&format!("hf129-idle-{probability}"));
        let mut state=rig.data_json("social-v2.json");
        state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"]=json!({"question":false,"probability":probability});
        fs::write(rig.root.join("data/social-v2.json"),state.to_string()).unwrap();
        rig.start();
        rig.push(group_event(129020,1001,"tester","",vec![json!({"type":"reply","data":{"id":"5555"}}),text_seg("你有空吗？")]));
        assert!(wait_for(LONG, || !hf129_decisions(&rig).is_empty()));
        let decision=hf129_decisions(&rig)[0].clone();
        assert_eq!(decision["shouldWake"],probability==1.0,"{decision}");
        assert_eq!(decision["reason"],if probability==1.0 {"probability"} else {"no-trigger"});
        if probability==1.0 { rig.wait_prompts(1); }
        else { assert!(rig.dsh.prompts().is_empty()); }
    }
}

#[test]
fn hf130_a_confirmed_targeted_send_establishes_the_configured_exchange() {
    for window in [90_000,1] {
        let mut rig=hf129_rig(&format!("hf130-send-{window}"));
        let mut cfg:Value=serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
        cfg["socialV2"]["wake"]["questionExchangeWindowMs"]=json!(window);
        fs::write(rig.root.join("config/config.json"),cfg.to_string()).unwrap();
        let mut state=rig.data_json("social-v2.json");
        state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"]=json!({"question":true});
        fs::write(rig.root.join("data/social-v2.json"),state.to_string()).unwrap();
        rig.start();
        rig.push(group_event(130020,1001,"tester","",vec![text_seg("ordinary exchange start")]));
        assert!(wait_for(LONG, || hf129_decisions(&rig).len()==1));
        let (status,result)=hf129_tool(&rig,"POST","/api/socialV2/send-message",Some(&json!({"key":GROUP_KEY,"messages":["answer to you"],"replyToMessageId":"1"})));
        assert_eq!(status,200,"{result}");
        assert_eq!(rig.gateway_calls("send_group_msg"),1);
        let own=rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["recentMessages"].as_array().unwrap().iter().find(|m|m["isSelf"]==true).unwrap().clone();
        assert_eq!(own["replyToUserId"],"1001","{own}");
        // Both parity processes use the same epoch but initialize their monotonic origins
        // independently. Witness live ingress past the confirmed console timestamp before
        // testing a follow-up; never make a future timestamp look like a valid exchange.
        let sent_at=own["time"].as_i64().unwrap();
        let mut synchronized=false;
        for barrier_id in 130100..130200 {
            let before=hf129_decisions(&rig).len();
            rig.push(group_event(barrier_id,1002,"other","",vec![text_seg("clock barrier")]));
            assert!(wait_for(LONG, || hf129_decisions(&rig).len()>before));
            if rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["lastIncomingAt"].as_i64().unwrap_or(0) > sent_at+100 { synchronized=true; break; }
        }
        assert!(synchronized,"live parity clock never passed the confirmed send");
        let before=hf129_decisions(&rig).len();
        rig.push(group_event(130021,1001,"tester","",vec![text_seg("你还有推荐吗？")]));
        assert!(wait_for(LONG, || hf129_decisions(&rig).len()>before));
        let d=hf129_decisions(&rig).last().unwrap().clone();
        assert_eq!(d["shouldWake"],window==90_000,"{d}");
        if window==90_000 { assert_eq!(d["reason"],"question"); rig.wait_prompts(1); }
        else { assert!(rig.dsh.prompts().is_empty()); }
    }
}

#[test]
fn hf129_directed_questions_and_pokes_during_turn_do_not_queue() {
    let mut rig=hf129_rig("hf129-question-poke");
    let mut state=rig.data_json("social-v2.json");
    state["conversations"][GROUP_KEY]["wakeConfig"]["triggers"]=json!({"atMention":true,"question":true,"poke":true});
    fs::write(rig.root.join("data/social-v2.json"),state.to_string()).unwrap();
    rig.start();
    rig.push(group_event(129030,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129031,1001,"tester","",vec![json!({"type":"reply","data":{"id":"5555"}}),text_seg("你还有推荐吗？")]));
    for user in [1001,OWNER] {
        rig.push(json!({"post_type":"notice","notice_type":"notify","sub_type":"poke","group_id":GROUP_ID,"self_id":SELF_ID,"user_id":user,"sender_id":user,"target_id":SELF_ID}));
    }
    assert!(wait_for(LONG, || hf129_decisions(&rig).len()>=4));
    let d=hf129_decisions(&rig);
    assert!(d.iter().skip(1).all(|v|v["reason"]=="in-turn" && v["shouldWake"]==false),"{d:?}");
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("turn ended")));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),1);
}

#[test]
fn hf129_a_reminder_that_reads_a_queued_urgent_message_drops_its_followup() {
    let mut rig=hf129_rig("hf129-reminder");
    let mut cfg:Value=serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
    cfg["socialV2"]["wake"]["maxWakeConfigReminders"]=json!(2);
    fs::write(rig.root.join("config/config.json"),cfg.to_string()).unwrap();
    rig.start();
    rig.push(group_event(129040,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129041,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("urgent after initial read")]));
    assert!(wait_for(LONG, || hf129_decisions(&rig).len()>=2));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    let prompts=rig.wait_prompts(2);
    assert!(prompts[1].text.contains("【提醒】"));
    assert!(wait_for(LONG, || fs::read(rig.root.join("hf129-reminder-read")).ok()
        .and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok()).is_some_and(|v|v["readThroughSeq"]==2)));
    let packet:Value=serde_json::from_slice(&fs::read(rig.root.join("hf129-reminder-read")).unwrap()).unwrap();
    assert_eq!(packet["readThroughSeq"],2,"{packet}");
    // This witnessed arrival pumps the old armed wake while the reminder is running,
    // and leaves a newer unread after the reminder's final read.
    rig.push(group_event(129042,1001,"tester","",vec![text_seg("newer unrelated unread after reminder read")]));
    assert!(wait_for(LONG, || hf129_decisions(&rig).len()>=3));
    fs::write(rig.root.join("hf129-reminder-release"),"").unwrap();
    assert!(wait_for(LONG, || rig.ledger().iter().filter(|r|r["k"]=="ended").count()>=2));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),2,"a covered urgent wake must not run after the reminder read");
    assert_eq!(hf129_decisions(&rig).iter().filter(|d|d["reason"]=="covered" && d["seq"]==2).count(),1);
}

#[test]
fn hf129_fix1_evicted_urgent_still_gets_exactly_one_followup() {
    let mut rig = hf129_rig("hf129-fix1-eviction");
    rig.start();
    rig.push(group_event(129100,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129101,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("unseen urgent")]));
    for i in 0..31 { rig.push(group_event(129102+i,1001,"tester","",vec![text_seg("ordinary busy message")])); }
    assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 33));
    assert!(!rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["unread"].as_array().unwrap().iter().any(|m|m["seq"]==2));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    rig.wait_prompts(2);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),2,"eviction cannot prove delivery");
}

#[test]
fn hf129_fix1_out_of_order_urgent_delivery_needs_no_followup() {
    let mut rig = hf129_rig("hf129-fix1-out-of-order");
    rig.start();
    rig.push(group_event(129140,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129141,1001,"tester","",vec![text_seg("older unread gap")]));
    rig.push(group_event(129142,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("urgent read out of order")]));
    assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 3));
    let (status,packet)=hf129_tool(&rig,"GET",&format!("/api/socialV2/unread?key={GROUP_KEY}&afterSeq=2"),None);
    assert_eq!(status,200,"{packet}");
    assert!(packet["messages"].as_array().unwrap().iter().any(|m|m["seq"]==3),"{packet}");
    assert!(packet["readThroughSeq"].as_i64().unwrap()<3,"gap remains: {packet}");
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("turn ended")));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),1,"the urgent seq was actually delivered");
}

#[test]
fn hf129_fix1_refused_snapshot_does_not_cover_an_unseen_mention() {
    let mut rig = hf129_rig("hf129-fix1-refused");
    let mut cfg:Value=serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
    cfg["dsh"]["fallbacks"]=json!([{"provider":"ticket12-provider","model":"fallback-model"}]);
    fs::write(rig.root.join("config/config.json"),cfg.to_string()).unwrap();
    rig.start();
    rig.push(group_event(129150,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129151,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("UNSEEN_MENTION_129")]));
    assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 2));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    assert!(wait_for(LONG, || rig.root.join("refused-prompt").exists()));
    assert!(fs::read_to_string(rig.root.join("refused-prompt")).unwrap().contains("UNSEEN_MENTION_129"));
    rig.wait_prompts(2);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),2,"one refused replacement and exactly one accepted followup");
    assert!(!hf129_decisions(&rig).iter().any(|d| d["reason"]=="covered" && d["seq"]==2));
}

#[test]
fn hf129_fix1_collector_stream_failure_preserves_one_unseen_urgent_followup() {
    let mut rig=hf129_rig("hf129-fix1-stream");
    rig.start();
    rig.push(group_event(129210,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    let (status,packet)=hf129_tool(&rig,"GET",&format!("/api/socialV2/unread?key={GROUP_KEY}"),None);
    assert_eq!(status,200,"{packet}");
    rig.push(group_event(129211,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("unseen after last read")]));
    assert!(wait_for(LONG,||hf129_decisions(&rig).len()>=2));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    fs::write(rig.root.join("hf129-stream-break"),"").unwrap();
    rig.wait_prompts(2);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),2,"exactly one followup survives collector failure");
    let log=fs::read_to_string(rig.root.join("candidate-out.log")).unwrap();
    assert!(!log.contains("turn cap"),"this exercises a collector failure before the cap: {log}");
}

#[test]
fn hf129_fix3_failed_turn_keeps_a_tool_delivered_urgent_followup() {
    let mut rig = hf129_rig("hf129-fix1-stream-failed-read");
    rig.start();
    rig.push(group_event(129520,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129521,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("delivered but the turn fails")]));
    assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 2));
    let (status, packet) = hf129_tool(&rig,"GET",&format!("/api/socialV2/unread?key={GROUP_KEY}"),None);
    assert_eq!(status,200,"{packet}");
    assert!(packet["messages"].as_array().unwrap().iter().any(|m|m["seq"]==2));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    fs::write(rig.root.join("hf129-stream-break"),"").unwrap();
    rig.wait_prompts(2);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),2,"failed turns retain the base followup despite tool evidence");
    assert!(!hf129_decisions(&rig).iter().any(|d|d["reason"]=="covered" && d["seq"]==2));
}

#[test]
fn hf129_fix1_successful_replacement_tool_read_covers_the_urgent_seq() {
    let mut rig=hf129_rig("hf129-fix1-replacement");
    let mut cfg:Value=serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
    cfg["dsh"]["fallbacks"]=json!([{"provider":"ticket12-provider","model":"fallback-model"}]);
    fs::write(rig.root.join("config/config.json"),cfg.to_string()).unwrap();
    rig.start();
    rig.push(group_event(129160,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129161,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("DELIVERED_MENTION_129")]));
    assert!(wait_for(LONG,||hf129_decisions(&rig).len()>=2));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    rig.wait_prompts(2);
    assert!(wait_for(LONG,||hf129_decisions(&rig).iter().any(|d|d["reason"]=="covered" && d["seq"]==2)));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),2,"two attempts in the same turn, no followup");
    assert_eq!(rig.data_lines("logs/rbridge-live-wakes.jsonl").len(),1);
}

#[test]
fn hf129_fix1_console_mark_without_model_delivery_keeps_the_urgent_wake() {
    let mut rig=hf129_rig("hf129-fix1-console-mark");
    let mut cfg:Value=serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
    cfg["socialV2"]["wake"]["preSleepWaitEnabled"]=json!(false);
    fs::write(rig.root.join("config/config.json"),cfg.to_string()).unwrap();
    rig.start();
    rig.push(group_event(129170,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129171,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("not delivered to the model")]));
    assert!(wait_for(LONG,||hf129_decisions(&rig).len()>=2));
    let (status,packet)=http_call(rig.console_port,"POST","/api/socialV2/mark-read",&[("x-console-token",CONSOLE_TOKEN)],Some(&json!({"key":GROUP_KEY,"throughSeq":2}))).unwrap();
    assert_eq!(status,200,"{packet}");
    assert_eq!(packet["markedCount"],2);
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    rig.wait_prompts(2);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),2,"console acknowledgement is not model delivery");
}

#[test]
fn hf129_fix2_queued_prompt_behind_unrelated_start_keeps_the_mention() {
    let mut rig=hf129_rig("hf129-fix2-queued");
    let mut cfg:Value=serde_json::from_slice(&fs::read(rig.root.join("config/config.json")).unwrap()).unwrap();
    cfg["dsh"]["fallbacks"]=json!([{"provider":"ticket12-provider","model":"fallback-model"}]);
    fs::write(rig.root.join("config/config.json"),cfg.to_string()).unwrap();
    rig.start();
    rig.push(group_event(129420,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129421,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("QUEUED_UNSEEN_MENTION_129")]));
    assert!(wait_for(LONG,||hf129_decisions(&rig).len()>=2));
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    assert!(wait_for(LONG,||rig.root.join("queued-prompt").exists()));
    assert!(fs::read_to_string(rig.root.join("queued-prompt")).unwrap().contains("QUEUED_UNSEEN_MENTION_129"));
    rig.wait_prompts(3);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),3,"exactly one follow-up after the queued attempt");
    assert!(!hf129_decisions(&rig).iter().any(|d|d["reason"]=="covered" && d["seq"]==2));
}

#[test]
fn hf129_fix2_owner_reset_discards_an_unseen_mention() {
    let mut rig=hf129_rig("hf129-fix2-owner-reset");
    rig.start();
    rig.push(group_event(129430,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("#HOLD129#")]));
    rig.wait_prompts(1);
    rig.push(group_event(129431,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("unseen before owner reset")]));
    assert!(wait_for(LONG,||hf129_decisions(&rig).len()>=2));
    let (status,reply)=rig.console("POST","/api/socialV2/reset",Some(&json!({})));
    assert_eq!(status,200,"{reply}");
    fs::write(rig.root.join("hf129-release"),"").unwrap();
    assert!(wait_for(LONG,||fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("turn ended")));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.dsh.prompts().len(),1,"owner reset discards the pending @");
    rig.push(group_event(129432,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("new generation after reset")]));
    rig.wait_prompts(2);
    assert!(!rig.dsh.prompts()[1].text.contains("unseen before owner reset"));
}

fn check_hotfix128_final_text(marker: &str, token: Option<&str>, guard: bool) {
    let mut rig = Rig::new("hotfix128", Seed { mode: "reserved2", config: json!({
        "security":{"outboundGuard":guard},
        "socialV2":{"wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100}}
    }), ..Seed::default() });
    fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{
        GROUP_KEY:{"agentToken":"known-session-token-128","unread":[],"recentMessages":[]}
    }}).to_string()).unwrap();
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", &format!("@9001 {marker}"));
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default().contains("DSH 回合结束")), "missing completed summary");
    let live = fs::read_to_string(rig.root.join("candidate-out.log")).unwrap();
    let activity = fs::read_to_string(rig.root.join("data/qq-activity.log")).unwrap();
    let summary = live.lines().find(|l| l.contains("DSH 回合结束")).unwrap();
    if let Some(token) = token {
        for (name, log) in [("live", live.as_str()), ("activity", activity.as_str())] {
            assert!(!log.contains(token) && !log.contains(&token[..10]), "{name} leaked a full or truncated token: {log}");
            assert!(log.contains("***"), "{name} must mask the token: {log}");
        }
    } else if marker == "#THOUGHT128#" {
        assert!(summary.ends_with("public final128"), "reasoning block must be removed: {summary}");
        assert!(!summary.contains("private plan") && !summary.contains("thought"));
    } else {
        assert!(summary.ends_with("synthetic reply"), "ordinary text is unchanged: {summary}");
        assert!(activity.contains("AI 内部输出：synthetic reply"));
    }
    if marker == "#REPEAT128#" {
        assert_eq!(summary.matches("***").count(), 2, "one fixed mask per repeated token");
        let internal = activity.lines().find(|line| line.contains("AI 内部输出：")).unwrap();
        assert_eq!(internal.matches("***").count(), 2, "Markdown must preserve both masks: {internal}");
    }
    assert!(rig.texts_to(OWNER_KEY).is_empty(), "local masking must not alert the owner: {:?}", rig.sends());
    assert!(rig.texts_to(GROUP_KEY).is_empty(), "reserved2 final text is internal");
}

#[test]
fn hotfix128_guard_off_masks_final_text_before_live_and_activity_previews() {
    check_hotfix128_final_text("#TOKEN128#", Some("known-session-token-128"), false);
}


#[test]
fn hotfix128_guard_off_masks_unknown_48_hex_tokens_before_previews() {
    check_hotfix128_final_text("#SHAPE128#", Some("0123456789abcdef0123456789abcdef0123456789abcdef"), false);
}

#[test]
fn hotfix128_summary_drops_leading_reasoning() {
    check_hotfix128_final_text("#THOUGHT128#", None, false);
}

#[test]
fn hotfix128_ordinary_final_text_is_unchanged() {
    check_hotfix128_final_text("#ORDINARY128#", None, false);
}

#[test]
fn hotfix128_token_masking_with_the_guard_on_is_quiet() {
    check_hotfix128_final_text("#TOKEN128#", Some("known-session-token-128"), true);
}

#[test]
fn hotfix128_repeated_tokens_keep_fixed_masks_in_activity() {
    check_hotfix128_final_text("#REPEAT128#", Some("known-session-token-128"), false);
}


#[test]
fn hotfix128_fix1_failover_previews_mask_before_each_cut() {
    for token in ["K128sessionVALUExyz", "0123456789abcdef0123456789abcdef0123456789abcdef"] {
        for (method, boundary, prefix) in [
            ("turn", 200, ""),
            ("prompt", 80, "prompt 调用失败：session/prompt 被拒：INVALID_REQUEST: "),
            ("prompt", 200, "prompt 调用失败：session/prompt 被拒：INVALID_REQUEST: "),
            ("select", 200, "所有候选模型都设置失败：preview-refuse/m：session/selectModel 被拒：fixture: "),
        ] {
            let pad = boundary - 10 - prefix.chars().count();
            let message = format!("{}{token} trailing", "x ".repeat(pad / 2) + &"x".repeat(pad % 2));
            let mut rig = Rig::new("preview-fix1", Seed { config: json!({
                "security":{"outboundGuard":false},
                "dsh":{"provider":"preview-primary","model":"m","fallbacks":[{"provider":"preview-refuse","model":"m"}]}
            }), ..Seed::default() });
            fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{
                GROUP_KEY:{"agentToken":"K128sessionVALUExyz","unread":[],"recentMessages":[]}
            }}).to_string()).unwrap();
            lock(&rig.dsh.inner).preview_fault = Some((method.into(), message));
            rig.start();
            says(&rig, GROUP_KEY, 1001, "tester", "@9001 preview-fix1");
            assert!(wait_for(LONG, || rig.ledger().iter().any(|r| r["k"] == "ended")), "turn did not finish");
            let live = fs::read_to_string(rig.root.join("candidate-out.log")).unwrap();
            let record = fs::read_to_string(rig.root.join("data/run-record.jsonl")).unwrap();
            for (name, text) in [("live", live.as_str()), ("recovery", record.as_str())] {
                assert!(!text.contains(&token[..8]), "{method}/{boundary}/{name} leaked a token fragment: {text}");
                assert!(text.contains("***"), "{method}/{boundary}/{name} lost the mask: {text}");
            }
            assert!(rig.texts_to(GROUP_KEY).iter().all(|t| t == ACK), "failures cannot reach the group");
        }
    }
}


#[test]
fn hotfix128_wrapped_fix1_lf_is_masked_in_live_and_activity() {
    check_hotfix128_final_text("#WRAPPEDLF128#", Some("known-session-token-128"), false);
}

#[test]
fn hotfix128_wrapped_fix1_crlf_is_masked_in_live_and_activity() {
    check_hotfix128_final_text("#WRAPPEDCRLF128#", Some("known-session-token-128"), false);
}


#[test]
fn hotfix128_fix2_reserved2_debug_error_masks_escaped_breaks() {
    for separator in ["\n", "\r\n"] {
        let mut rig = Rig::new("debug-error-fix2", Seed { mode: "reserved2", config: json!({
            "security":{"outboundGuard":false}
        }), ..Seed::default() });
        fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{
            GROUP_KEY:{"agentToken":"K128sessionVALUExyz","unread":[],"recentMessages":[]}
        }}).to_string()).unwrap();
        lock(&rig.dsh.inner).preview_fault = Some(("turn".into(), format!("diagnostic K128session{separator}VALUExyz end")));
        rig.start();
        says(&rig, GROUP_KEY, 1001, "tester", "@9001 debug-error-fix2");
        assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("candidate-out.log")).unwrap_or_default()
            .contains("turn ended: Err(")), "reserved2 error did not finish");
        for name in ["candidate-out.log", "candidate-err.log", "data/qq-activity.log", "data/run-record.jsonl", "data/turn-ledger.jsonl"] {
            let text = fs::read_to_string(rig.root.join(name)).unwrap();
            assert!(!text.contains("K128session") && !text.contains("VALUExyz"), "{name} leaked escaped error: {text}");
            if name != "candidate-err.log" { assert!(text.contains("***"), "{name} missing mask: {text}"); }
        }
        assert!(rig.texts_to(GROUP_KEY).is_empty(), "error is local/owner only");
    }
}

#[test]
fn hf123_fake_model_old_send_wait_mark_read_loop_ends_within_four_calls() {
    let mut rig = Rig::new("hf123-loop", Seed { mode: "reserved2", config: json!({"socialV2":{
        "wait":{"maxPerTurn":1,"minMs":100,"maxMs":1000},
        "wake":{"preSleepWaitMs":1000,"maxWakeConfigReminders":0},
        "reply":{"quietMedianMs":1,"quietMinMs":1,"quietMaxMs":1},
        "send":{"firstGapEnabled":false,"maxSendPerMinute":100,"maxSendPerHour":100}
    }}), ..Seed::default() });
    *lock(&rig.dsh.loop_fixture) = Some((rig.root.clone(), rig.console_port, rig.gateway.clone()));
    fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{(GROUP_KEY):{
        "unread":[],"recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,"triggers":{"atMention":true}}
    }}}).to_string()).unwrap();
    rig.start();
    says(&rig, GROUP_KEY, 1001, "tester", "@9001 #LOOP123#");
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r|r["k"] == "ended")), "turn never ended: {:?}", lock(&rig.dsh.loop_results));
    let calls = lock(&rig.dsh.loop_results).clone();
    assert_eq!(calls.len(), 4, "fake model must finish after initial observation + send → wait → mark_read: {calls:?}");
    assert_eq!(calls[3]["result"]["nextAction"], "end_turn");
    assert!(rig.ledger().iter().any(|r|r["k"] == "ended" && r["outcome"] == "completed"), "{:?}", rig.ledger());
    assert_eq!(rig.data_lines("send-outbox.jsonl").iter().filter(|r|r["k"] == "confirmed").count(), 1);
    assert_eq!(rig.gateway_calls("send_group_msg"), 1);
}

#[cfg(windows)]
#[test]
fn hf123_cli_relay_uses_shared_face_outbox_and_repeat_guard() {
    let mut rig = Rig::new("hf123-cli-shared", Seed { mode:"reserved2", config:json!({
        "socialV2":{
            "wake":{"maxWakeConfigReminders":1,"batchWindowMs":1000,"maxWakePerMinute":30,"maxWakePerHour":100},
            "reply":{"quietMedianMs":1,"quietMinMs":1,"quietMaxMs":1},
            "send":{"firstGapEnabled":false,"repeatLimit":1,"maxSendPerMinute":100,"maxSendPerHour":100}
        },
        "backends":{"sessions":{GROUP_KEY:"relay123"},"list":[{
            "id":"relay123","kind":"cli","cmd":"node","args":["relay123.cjs"]
        }]}
    }), ..Seed::default() });
    fs::write(rig.root.join("resources/relay123.cjs"), r#"
const fs = require('node:fs');
const path = require('node:path');
let prompt = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', chunk => prompt += chunk);
process.stdin.on('end', () => {
  const file = path.join(process.env.HER_RUNTIME_ROOT, 'data', 'cli123-count');
  const count = fs.existsSync(file) ? Number(fs.readFileSync(file, 'utf8')) : 0;
  fs.writeFileSync(file, String(count + 1));
  process.stdout.write(JSON.stringify({response:prompt.includes('[表情N]') ? '[表情28]' : 'missing-face-contract'}));
});
"#).unwrap();
    rig.start();
    rig.push(group_event(123101,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("cli-first")]));
    assert!(wait_for(LONG, || rig.ledger().iter().filter(|r|r["k"] == "ended").count() >= 1), "CLI turn did not end");
    assert_eq!(rig.data_lines("send-outbox.jsonl").iter().filter(|r|r["k"] == "confirmed").count(), 1,
        "the CLI relay must use the same outbox as model send tools");
    let messages: Vec<Value> = lock(&rig.gateway).calls.iter().filter(|(action,_)|action == "send_group_msg").map(|(_,body)|body["message"].clone()).collect();
    assert_eq!(messages, vec![json!([{"type":"face","data":{"id":"28"}}])]);
    rig.push(group_event(123102,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("cli-second")]));
    assert!(wait_for(LONG, || rig.ledger().iter().filter(|r|r["k"] == "ended").count() >= 2));
    assert_eq!(rig.gateway_calls("send_group_msg"), 1, "the second CLI answer must be refused by the shared guard");
    rig.wait_texts(OWNER_KEY,1);
    assert_eq!(rig.texts_to(OWNER_KEY).len(), 1, "one guard notice, no extra backend-failure alert");
    assert_eq!(fs::read_to_string(rig.root.join("data/cli123-count")).unwrap(), "2");
    assert!(rig.dsh.prompts().is_empty(), "a refused CLI send must not fall through to a DSH turn");
}

#[cfg(windows)]
#[test]
fn hf123_cli_relay_revises_a_held_draft_without_spending_send_cap() {
    let mut rig = Rig::new("hf123-cli-held", Seed { mode:"reserved2", config:json!({
        "socialV2":{
            "wake":{"maxWakeConfigReminders":1,"maxWakePerMinute":30,"maxWakePerHour":100},
            "reply":{"quietMedianMs":120000,"quietMinMs":120000,"quietMaxMs":120000},
            "send":{"firstGapEnabled":false,"maxPerTurn":1,"maxSendPerMinute":100,"maxSendPerHour":100}
        },
        "backends":{"sessions":{GROUP_KEY:"relay123"},"list":[{
            "id":"relay123","kind":"cli","cmd":"node","args":["relay123.cjs"]
        }]}
    }), ..Seed::default() });
    fs::write(rig.root.join("resources/relay123.cjs"), r#"
const fs = require('node:fs');
const path = require('node:path');
let prompt = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', chunk => prompt += chunk);
process.stdin.on('end', () => {
  const dir = path.join(process.env.HER_RUNTIME_ROOT, 'data');
  const file = path.join(dir, 'cli123-count');
  const count = (fs.existsSync(file) ? Number(fs.readFileSync(file, 'utf8')) : 0) + 1;
  fs.writeFileSync(file, String(count));
  if (count === 1) { process.stdout.write(JSON.stringify({response:'obsolete[表情14]'})); return; }
  const timer = setInterval(() => {
    if (!fs.existsSync(path.join(dir, 'release-cli123'))) return;
    clearInterval(timer);
    process.stdout.write(JSON.stringify({response:prompt.includes('revise-now') ? 'revised[表情28]' : 'missing-revision'}));
  }, 20);
});
"#).unwrap();
    fs::write(rig.root.join("data/social-v2.json"), json!({"conversations":{(GROUP_KEY):{
        "bootstrapSent":true,"lastIncomingAt":now_ms(),"lastReplyIncomingAt":now_ms(),
        "unread":[],"recentMessages":[],"wakeConfig":{"mode":"diving","infinite":true,"triggers":{"atMention":true}}
    }}}).to_string()).unwrap();
    rig.start();
    let (status, wake) = rig.console("POST","/api/socialV2/wake",Some(&json!({"key":GROUP_KEY})));
    assert!((200..300).contains(&status), "{wake}");
    assert!(wait_for(LONG, || rig.data_lines("send-outbox.jsonl").iter().any(|r|r["k"] == "intent") || rig.gateway_calls("send_group_msg") > 0));
    assert!(rig.data_lines("send-outbox.jsonl").iter().any(|r|r["k"] == "intent"), "CLI must reserve the common outbox before its hold");
    assert_eq!(rig.gateway_calls("send_group_msg"), 0, "the obsolete draft must still be held");
    says(&rig, GROUP_KEY,1001,"tester","revise-now");
    assert!(wait_for(LONG, || fs::read_to_string(rig.root.join("data/cli123-count")).unwrap_or_default() == "2"), "held CLI send never requested a revision");
    // Publish the new timing policy before the second fake model response is released.
    let (status, config) = rig.console("POST","/api/socialV2/config",Some(&json!({"reply":{"quietMedianMs":1,"quietMinMs":1,"quietMaxMs":1}})));
    assert_eq!(status,200,"{config}");
    fs::write(rig.root.join("data/release-cli123"),"ready").unwrap();
    assert!(wait_for(LONG, || rig.ledger().iter().any(|r|r["k"] == "ended")));
    let messages: Vec<Value> = lock(&rig.gateway).calls.iter().filter(|(action,_)|action == "send_group_msg").map(|(_,body)|body["message"].clone()).collect();
    assert_eq!(messages, vec![json!([{"type":"text","data":{"text":"revised"}},{"type":"face","data":{"id":"28"}}])]);
    let rows = rig.data_lines("send-outbox.jsonl");
    assert_eq!(rows.iter().filter(|r|r["k"] == "attempt").count(),1);
    assert_eq!(rows.iter().filter(|r|r["k"] == "confirmed").count(),1);
    assert_eq!(rows.iter().filter(|r|r["k"] == "superseded").count(),1);
    assert_eq!(rig.data_json("social-v2.json")["conversations"][GROUP_KEY]["replyRevisionBudget"]["used"],1);
    assert!(rig.texts_to(OWNER_KEY).is_empty(), "a held draft is not a send-limit failure");
}

#[cfg(windows)]
#[test]
fn hf129_fix3_cli_urgent_followup_finishes_without_replay() {
    let mut rig = hf129_rig("hf129-fix3-cli");
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["backends"] = json!({"sessions":{GROUP_KEY:"cli129"},"list":[{
        "id":"cli129","kind":"cli","cmd":"node","args":["cli129.cjs"]
    }]});
    fs::write(path, cfg.to_string()).unwrap();
    fs::write(rig.root.join("resources/cli129.cjs"), r#"
const fs = require('fs'), path = require('path');
process.stdin.resume();
process.stdin.on('end', () => {
  const dir = path.join(process.env.HER_RUNTIME_ROOT, 'data');
  const counter = path.join(dir, 'cli129-count');
  const n = Number(fs.existsSync(counter) ? fs.readFileSync(counter, 'utf8') : 0) + 1;
  fs.writeFileSync(counter, String(n));
  const timer = setInterval(() => {
    if (n === 1 && !fs.existsSync(path.join(dir, 'cli129-release'))) return;
    clearInterval(timer);
    process.stdout.write(JSON.stringify({response: 'synthetic cli answer ' + n}));
  }, 20);
});
"#).unwrap();
    rig.start();
    rig.push(group_event(129501,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("first CLI turn")]));
    let count = || fs::read_to_string(rig.root.join("data/cli129-count")).unwrap_or_default();
    assert!(wait_for(LONG, || count() == "1"), "CLI never started: {:?}", rig.data_lines("logs/opencode-trial.jsonl"));
    rig.push(group_event(129502,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("urgent during CLI turn")]));
    assert!(wait_for(LONG, || hf129_decisions(&rig).len() >= 2));
    fs::write(rig.root.join("data/cli129-release"), "").unwrap();
    assert!(wait_for(LONG, || rig.texts_to(GROUP_KEY).len() >= 2));
    assert!(wait_for(LONG, || rig.data_lines("turn-ledger.jsonl").iter().filter(|r|r["k"]=="ended").count() >= 2));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(count(), "2", "the base urgent followup must finish without a replay loop");
    assert_eq!(rig.texts_to(GROUP_KEY), vec!["synthetic cli answer 1", "synthetic cli answer 2"]);
    assert_eq!(rig.data_lines("logs/rbridge-live-wakes.jsonl").len(), 2);
}

#[test]
fn hf129_fix3_persistent_submission_failure_stops_at_base_retry_cap() {
    let mut rig = hf129_rig("hf129-fix3-submit-failure");
    let path = rig.root.join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["socialV2"]["wake"]["maxWakePerMinute"] = json!(1);
    cfg["socialV2"]["wake"]["maxWakePerHour"] = json!(1);
    fs::write(path, cfg.to_string()).unwrap();
    lock(&rig.dsh.inner).refuse_prompt = true;
    rig.start();
    rig.push(group_event(129510,1001,"tester","",vec![json!({"type":"at","data":{"qq":SELF_ID.to_string()}}),text_seg("persistently refused urgent prompt")]));
    let attempts = || rig.ledger().iter().filter(|r|r["k"]=="ended" && r["outcome"]=="failed").count();
    assert!(wait_for(Duration::from_secs(75), || attempts() >= 4), "base retries never finished: {:?}", rig.ledger());
    thread::sleep(Duration::from_secs(5));
    assert_eq!(attempts(), 4, "one initial submission plus three retries, even with refunded wake quota");
    assert_eq!(rig.dsh.calls("session/prompt").len(), 4);
    assert!(rig.dsh.prompts().is_empty());
    assert!(rig.texts_to(GROUP_KEY).is_empty());
}
