mod hf119 {
    use super::*;

    fn send_fixture(
        key: &str,
        requests: usize,
    ) -> (
        FixtureRoot,
        RuntimeContext,
        Value,
        std::thread::JoinHandle<Vec<FakeOneBotRequest>>,
    ) {
        let (root, _, mut cfg, _) = hf94_wait_fixture(json!({}));
        let (endpoint, gateway) = start_fake_onebot_with_budget(
            requests,
            None,
            Vec::new(),
            std::time::Duration::from_secs(30),
        );
        cfg["socialV2"]["reply"] =
            json!({"quietMedianMs":10000,"quietMinMs":10000,"quietMaxMs":10000});
        cfg["socialV2"]["send"] = json!({"firstGapEnabled":false,"duplicateWindowMs":0,"maxSendPerMinute":100,"maxSendPerHour":100});
        std::fs::write(root.0.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(
            root.0.join("dependencies.json"),
            json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":endpoint})
                .to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root.0).unwrap();
        let mut state = load_social_state_for(&context).unwrap();
        if key != HF94_KEY {
            state["conversations"][key] = state["conversations"][HF94_KEY].clone();
        }
        context
            .write_atomic(
                RuntimeArea::Data,
                "social-v2.json",
                state.to_string().as_bytes(),
            )
            .unwrap();
        (root, context, cfg, gateway)
    }

    fn send(context: &RuntimeContext, cfg: &Value, key: &str, texts: &[&str]) -> (u16, Value) {
        let (kind, id, _) = agent::parse_key(key).unwrap();
        let body = json!({});
        agent::execute_send(
            context,
            cfg,
            &agent::SendCall {
                key,
                kind,
                id_s: id,
                token: "hf94-agent",
                messages: texts.iter().map(|t| t.to_string()).collect(),
                reply_ref: String::new(),
                at_user: None,
                gap_mode: "fixed",
                body: &body,
                flavor: agent::SendFlavor::Message,
                policy: GuardPolicy::Reserved2,
                resolved_quote: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn hf136_held_reply_is_not_clipped_by_wait_closing_margin() {
        for fits in [true, false] {
            let (root, _, mut cfg, _) = hf94_wait_fixture(json!({}));
            let (endpoint, done, gateway) = start_scenario_fake_onebot();
            cfg["socialV2"]["reply"] = json!({"quietMedianMs":10000,"quietMinMs":10000,"quietMaxMs":10000});
            cfg["socialV2"]["send"] = json!({"firstGapEnabled":false,"duplicateWindowMs":0,"maxSendPerMinute":100,"maxSendPerHour":100});
            std::fs::write(root.0.join("config/config.json"), cfg.to_string()).unwrap();
            std::fs::write(root.0.join("dependencies.json"), json!({"dsh":"http://127.0.0.1:1",
                "model":"http://127.0.0.1:1","onebot":endpoint}).to_string()).unwrap();
            let context = RuntimeContext::from_root(&root.0).unwrap();
            test_clock::install(HF94_T0+4000, |_| {});
            let _clock = Hf94Clock;
            let epoch = crate::send_outbox::token_epoch("hf94-agent");
            let turn = crate::turn_ledger::Recorder::begin(&context, HF94_KEY, "dsh", "hf136-hold", &epoch, &[], None).unwrap();
            crate::turn_ledger::append_record(&context, &json!({"k":"budget","turn":turn.id,
                "deadline":HF94_T0 + if fits {15000} else {12000}})).unwrap();
            let (status, out) = send(&context, &cfg, HF94_KEY, &["closing reply"]);
            assert_eq!(status, 200, "{out}");
            let records = crate::send_outbox::read_records(&context).unwrap();
            assert_eq!(out["sent"], 1, "{out}");
            assert_ne!(out["turnBudgetExhausted"], true, "{out}");
            assert_eq!(now_ms(), HF94_T0+10000, "ordinary quiet timing still applies");
            assert_eq!(records.iter().filter(|r|r["k"]=="attempt").count(), 1);
            assert!(!records.iter().any(|r|r["k"]=="superseded"));
            assert!(load_social_state_for(&context).unwrap()["conversations"][HF94_KEY]["turnBudgetEnded"].is_null());
            done.send(()).unwrap();
            assert_eq!(gateway.join().unwrap().iter().filter(|r|r.path=="/send_group_msg").count(),1);
        }
    }

    #[test]
    fn hf119_send_quiet_counts_thinking_and_applies_to_later_replies() {
        let (_root, context, cfg, gateway) = send_fixture(HF94_KEY, 2);
        test_clock::install(HF94_T0 + 4000, |_| {});
        let _clock = Hf94Clock;
        assert_eq!(send(&context, &cfg, HF94_KEY, &["first"]).0, 200);
        assert_eq!(
            now_ms(),
            HF94_T0 + 10000,
            "four seconds thinking must leave six seconds of hold"
        );
        record_reserved2_incoming(
            &context,
            &cfg,
            HF94_KEY,
            "later",
            json!({"time":now_ms(),"text":"more","isSelf":false}),
        )
        .unwrap();
        assert_eq!(send(&context, &cfg, HF94_KEY, &["second"]).0, 200);
        assert_eq!(
            now_ms(),
            HF94_T0 + 20000,
            "later replies have the same timing contract"
        );
        assert_eq!(
            gateway
                .join()
                .unwrap()
                .iter()
                .filter(|r| r.path == "/send_group_msg")
                .count(),
            2
        );
    }

    #[test]
    fn hf119_neutral_media_is_delivered_without_restarting_send_quiet() {
        let (_root, context, cfg, gateway) = send_fixture(HF94_KEY, 1);
        record_reserved2_incoming(
            &context,
            &cfg,
            HF94_KEY,
            "text",
            json!({"time":HF94_T0,"text":"hi","isSelf":false}),
        )
        .unwrap();
        record_reserved2_incoming(
            &context,
            &cfg,
            HF94_KEY,
            "picture",
            json!({"time":HF94_T0+8000,"text":"[图片]","quietNeutral":true,"isSelf":false}),
        )
        .unwrap();
        test_clock::install(HF94_T0 + 9000, |_| {});
        let _clock = Hf94Clock;
        assert_eq!(send(&context, &cfg, HF94_KEY, &["reply"]).0, 200);
        assert_eq!(
            now_ms(),
            HF94_T0 + 10000,
            "the picture must not move the quiet anchor"
        );
        let state = load_social_state_for(&context).unwrap();
        assert_eq!(
            state["conversations"][HF94_KEY]["unread"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(gateway.join().unwrap().len(), 1);
    }

    #[test]
    fn hf119_arrival_during_hold_returns_messages_and_cancels_unattempted_outbox() {
        let (_root, context, cfg, gateway) = send_fixture(HF94_KEY, 0);
        let (incoming, config) = (context.clone(), cfg.clone());
        let mut arrived = false;
        test_clock::install(HF94_T0 + 4000, move |now| {
            if !arrived && now >= HF94_T0 + 5000 {
                arrived = true;
                record_reserved2_incoming(
                    &incoming,
                    &config,
                    HF94_KEY,
                    "during-hold",
                    json!({"text":"one more thing","time":now,"isSelf":false}),
                )
                .unwrap();
            }
        });
        let _clock = Hf94Clock;
        let (status, out) = send(&context, &cfg, HF94_KEY, &["obsolete", "never queued"]);
        assert_eq!(
            status, 200,
            "revision is an actionable result, not a transport error: {out}"
        );
        assert_eq!(out["reviseRequired"], true, "{out}");
        assert_eq!(out["sent"], 0);
        assert_eq!(out["newMessages"][0]["text"], "one more thing");
        assert!(out["reason"].as_str().unwrap().contains("对方又说了"));
        let records = crate::send_outbox::read_records(&context).unwrap();
        assert!(
            !records.iter().any(|r| r["k"] == "attempt"),
            "cancelled content must never reach a gateway attempt"
        );
        let decisions: Vec<_> = records.iter().filter(|r| r["k"] == "superseded").collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["indices"], json!([0, 1]));
        assert_eq!(gateway.join().unwrap().len(), 0);
    }

    #[test]
    fn hf119_private_typing_is_cleared_on_send_and_on_cancel_groups_never_type() {
        for cancel in [false, true] {
            let key = "private:8808";
            let (_root, context, cfg, gateway) = send_fixture(key, if cancel { 2 } else { 3 });
            let (incoming, config) = (context.clone(), cfg.clone());
            let mut arrived = false;
            test_clock::install(HF94_T0, move |now| {
                if cancel && !arrived {
                    arrived = true;
                    record_reserved2_incoming(
                        &incoming,
                        &config,
                        key,
                        "typing-cancel",
                        json!({"text":"stop","time":now,"isSelf":false}),
                    )
                    .unwrap();
                }
            });
            let _clock = Hf94Clock;
            let (_, out) = send(&context, &cfg, key, &["hello"]);
            assert_eq!(out["reviseRequired"] == true, cancel, "{out}");
            let requests = gateway.join().unwrap();
            let typing: Vec<_> = requests
                .iter()
                .filter(|r| r.path == "/set_input_status")
                .map(|r| r.body.clone())
                .collect();
            assert_eq!(
                typing,
                vec![
                    json!({"user_id":8808,"event_type":1}),
                    json!({"user_id":8808,"event_type":0})
                ]
            );
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r.path == "/send_private_msg")
                    .count(),
                usize::from(!cancel)
            );
            if !cancel {
                assert!((HF94_T0 + 1000..=HF94_T0 + 6000).contains(&now_ms()));
            }
        }
        let (_root, context, cfg, gateway) = send_fixture(HF94_KEY, 1);
        test_clock::install(HF94_T0, |_| {});
        let _clock = Hf94Clock;
        send(&context, &cfg, HF94_KEY, &["hello group"]);
        let requests = gateway.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/send_group_msg");
    }

    fn typing_cleanup_change(change: &'static str) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let returned = std::sync::Arc::new(AtomicBool::new(false));
        // The production cosmetic call returns after 250 ms even without a fake response.
        // A response barrier therefore needs its own test deadline and an actual return witness.
        crate::onebot::INPUT_STATUS_TEST.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some((std::time::Duration::from_secs(10), returned.clone()));
        });
        struct StatusTest;
        impl Drop for StatusTest {
            fn drop(&mut self) {
                crate::onebot::INPUT_STATUS_TEST.with(|slot| *slot.borrow_mut() = None);
            }
        }
        let _status_test = StatusTest;
        let mutation_returned = returned.clone();
        let key = "private:8808";
        let (root, _context, cfg, unused_gateway) = send_fixture(key, 0);
        assert!(unused_gateway.join().unwrap().is_empty());
        // Status 1 receives an immediate response; the status-0 response stays behind the barrier.
        let (endpoint, requests, release, gateway) =
            start_blocking_fake_onebot_with_refresh(Some(json!({})));
        std::fs::write(
            root.0.join("dependencies.json"),
            json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":endpoint})
                .to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root.0).unwrap();
        let (changed_context, config) = (context.clone(), cfg.clone());
        let observer = std::thread::spawn(move || {
            for event in [1, 0] {
                let request = requests
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                assert_eq!(request.path, "/set_input_status");
                assert_eq!(request.body, json!({"user_id":8808,"event_type":event}));
            }
            // The gateway has read status 0, but its response is still behind the barrier.
            match change {
                "pause" => mutate_social_state_for(&changed_context, |state| {
                    state["paused"] = json!(true);
                    Ok(())
                })
                .unwrap(),
                "revoke" => mutate_social_state_for(&changed_context, |state| {
                    state["conversations"][key]["agentToken"] = json!("revoked-epoch");
                    Ok(())
                })
                .unwrap(),
                "arrival" => {
                    record_reserved2_incoming(
                        &changed_context,
                        &config,
                        key,
                        "typing-cleanup-arrival",
                        json!({"text":"changed while clearing typing",
                    "time":HF94_T0+10000,"isSelf":false}),
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            if change == "arrival" {
                let persisted = load_social_state_for(&changed_context).unwrap();
                assert_eq!(persisted["conversations"][key]["lastUnreadSeq"], 1);
                assert_eq!(
                    persisted["conversations"][key]["unread"][0]["inboundKey"],
                    "typing-cleanup-arrival"
                );
            }
            assert!(
                !mutation_returned.load(Ordering::SeqCst),
                "the state mutation must be durable before status 0 actually returns"
            );
            release.send(()).unwrap();
            gateway.join().unwrap().unwrap();
        });
        test_clock::install(HF94_T0, |_| {});
        let _clock = Hf94Clock;
        let (_, out) = send(&context, &cfg, key, &["obsolete"]);
        observer.join().unwrap();
        assert!(
            returned.load(Ordering::SeqCst),
            "status 0 returned before send completed"
        );
        let records = crate::send_outbox::read_records(&context).unwrap();
        assert!(
            !records.iter().any(|r| r["k"] == "attempt"),
            "{change} while status 0 was pending must prevent submission: {out}"
        );
        if change == "arrival" {
            assert_eq!(out["reviseRequired"], true, "{out}");
            assert_eq!(
                out["replyRevisionsUsed"], 1,
                "fresh fixture must return its first revision"
            );
            assert_eq!(out["sent"], 0);
            assert_eq!(
                out["newMessages"][0]["text"],
                "changed while clearing typing"
            );
            assert_eq!(out["progress"]["superseded"], json!([0]));
        } else {
            assert_eq!(out["cancelled"], json!([0]), "{out}");
        }
    }

    #[test]
    fn hf119_typing_cleanup_pause_prevents_submission() {
        typing_cleanup_change("pause");
    }

    #[test]
    fn hf119_typing_cleanup_revocation_prevents_submission() {
        typing_cleanup_change("revoke");
    }

    #[test]
    fn hf119_typing_cleanup_arrival_requires_revision() {
        typing_cleanup_change("arrival");
    }

    #[test]
    fn hf119_typing_length_uses_utf16_and_counts_observed_thinking() {
        let (_root, context, mut cfg, gateway) = send_fixture(HF94_KEY, 2);
        cfg["socialV2"]["send"] = json!({"firstGapEnabled":true,"firstGapBaseMs":1000,"firstGapPerCharMs":100,
            "burstIntervalMinMs":100,"maxGapMs":10000,"duplicateWindowMs":0});
        cfg["socialV2"]["reply"] = json!({"quietMedianMs":1,"quietMinMs":1,"quietMaxMs":1});
        context
            .write_atomic(
                RuntimeArea::Config,
                "config.json",
                cfg.to_string().as_bytes(),
            )
            .unwrap();
        let epoch = crate::send_outbox::token_epoch("hf94-agent");
        let turn = crate::turn_ledger::Recorder::begin(
            &context,
            HF94_KEY,
            "dsh",
            "hf119",
            &epoch,
            &[],
            None,
        )
        .unwrap();
        crate::turn_ledger::append_record(
            &context,
            &json!({"k":"started","turn":turn.id,"t":HF94_T0-400}),
        )
        .unwrap();
        test_clock::install(HF94_T0, |_| {});
        let _clock = Hf94Clock;
        reply_timing::TEST_SEED.with(|s| s.set(Some(119)));
        send(&context, &cfg, HF94_KEY, &["🙂"]);
        let first = now_ms() - HF94_T0;
        assert!(
            (500..=1100).contains(&first),
            "1200 ms UTF-16 budget * [0.75,1.25] minus 400 thinking = [500,1100], got {first}"
        );
        reply_timing::TEST_SEED.with(|s| s.set(Some(119)));
        test_clock::set(HF94_T0 + 10000);
        // A later response counts time spent thinking since the previous successful send.
        send(&context, &cfg, HF94_KEY, &["🙂"]);
        assert_eq!(now_ms(), HF94_T0 + 10000);
        reply_timing::TEST_SEED.with(|s| s.set(None));
        assert_eq!(gateway.join().unwrap().len(), 2);
    }

    #[test]
    fn hf119_burst_arrival_cancels_remaining_items_without_repeating_the_sent_bubble() {
        let (_root, context, cfg, gateway) = send_fixture(HF94_KEY, 1);
        let (incoming, config) = (context.clone(), cfg.clone());
        let mut arrived = false;
        test_clock::install(HF94_T0, move |now| {
            if !arrived && now > HF94_T0 + 10000 {
                arrived = true;
                record_reserved2_incoming(
                    &incoming,
                    &config,
                    HF94_KEY,
                    "burst-interruption",
                    json!({"text":"new topic","time":now,"isSelf":false}),
                )
                .unwrap();
            }
        });
        let _clock = Hf94Clock;
        let (_, out) = send(
            &context,
            &cfg,
            HF94_KEY,
            &["first", "obsolete second", "third"],
        );
        assert_eq!(out["reviseRequired"], true, "{out}");
        assert_eq!(out["sent"], 1);
        let requests = gateway.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body["message"][0]["data"]["text"], "first");
    }

    #[test]
    fn hf119_superseded_partial_burst_is_settled_and_never_resumed_after_restart() {
        let (root, context, cfg, gateway) = send_fixture(HF94_KEY, 2);
        let epoch = crate::send_outbox::token_epoch("hf94-agent");
        let turn = crate::turn_ledger::Recorder::attach(&context, "hf119-restart", HF94_KEY);
        crate::turn_ledger::append_record(
            &context,
            &json!({"k":"begun","turn":turn.id,
            "key":HF94_KEY,"epoch":epoch,"t":HF94_T0-1,"backend":"dsh","inbound":[]}),
        )
        .unwrap();
        turn.attempt("hf119-rpc", "hf119-session").unwrap();
        let (incoming, config) = (context.clone(), cfg.clone());
        let mut arrived = false;
        test_clock::install(HF94_T0, move |now| {
            if !arrived && now > HF94_T0 + 10000 {
                arrived = true;
                record_reserved2_incoming(
                    &incoming,
                    &config,
                    HF94_KEY,
                    "revision-restart",
                    json!({"text":"new topic","time":now,"isSelf":false}),
                )
                .unwrap();
            }
        });
        let _clock = Hf94Clock;
        let (_, revised) = send(
            &context,
            &cfg,
            HF94_KEY,
            &["first", "obsolete second", "obsolete third"],
        );
        assert_eq!(revised["reviseRequired"], true, "{revised}");
        assert_eq!(revised["sent"], 1);
        assert_eq!(
            send(&context, &cfg, HF94_KEY, &["replacement"]).1["sent"],
            1
        );

        // Restart before the turn completes: use only the durable ledgers, with no report in memory.
        let restarted = RuntimeContext::from_root(&root.0).unwrap();
        let outbox = crate::send_outbox::read_records(&restarted).unwrap();
        let (turns, _) =
            crate::turn_ledger::fold(&crate::turn_ledger::read_records(&restarted).unwrap());
        let recovered = turns.iter().find(|t| t.id == turn.id).unwrap();
        let batches = crate::turn_ledger::batches_of_turn(recovered, None, &outbox);
        let sends = crate::turn_recovery::send_state(&batches);
        assert_eq!(
            sends,
            crate::turn_recovery::SendState::Settled,
            "discarded content must not create a send-unresolved liability"
        );
        let done = crate::dsh::PromptProbe::Turn(crate::dsh::TurnFacts {
            rpc_id: "hf119-rpc".into(),
            dsh_turn: 1,
            text: String::new(),
            tools: Vec::new(),
            ended: Some(json!({"reason":{"kind":"completed"}})),
        });
        assert!(matches!(
            crate::turn_recovery::decide(recovered, Some(&done), &sends, Ok(())),
            crate::turn_recovery::Verdict::Complete(_)
        ));
        let batch_id = revised["batchId"].as_str().unwrap();
        let batch = crate::send_outbox::fold_batch(&outbox, batch_id).unwrap();
        assert_eq!(batch.to_json()["pending"], json!([]));
        assert_eq!(batch.to_json()["items"][1]["state"], "superseded");
        assert_eq!(batch.to_json()["items"][2]["state"], "superseded");
        let ob = media_onebot(&restarted, &cfg).unwrap();
        let (status, resumed) = resume_send_batch(
            &restarted,
            &cfg,
            &ob,
            HF94_KEY,
            "group",
            "94001",
            "hf94-agent",
            batch_id,
        )
        .unwrap();
        assert_eq!(status, 200, "{resumed}");
        assert_eq!(resumed["pending"], json!([]));
        assert_eq!(resumed["superseded"], json!([1, 2]));
        let state = load_social_state_for(&restarted).unwrap();
        assert_eq!(
            state["conversations"][HF94_KEY]["sendTimes"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "superseded items release their quota"
        );
        let requests = gateway.join().unwrap();
        let sent: Vec<_> = requests
            .iter()
            .map(|r| r.body["message"][0]["data"]["text"].clone())
            .collect();
        assert_eq!(sent, vec![json!("first"), json!("replacement")]);
        assert_eq!(
            crate::send_outbox::read_records(&restarted)
                .unwrap()
                .iter()
                .filter(|r| r["batch"] == batch_id && r["k"] == "attempt")
                .count(),
            1
        );
    }

    // Exercise the same stop through a new batch and a sparse resume plan. The filesystem
    // hook cuts execution immediately after the first discard append, before any next item.
    fn discard_fault(resuming: bool, crash: bool, stage: &'static str) {
        use crate::send_outbox::{append_record, fold_batch, read_records, token_epoch, ItemState};
        let (root, context, cfg, gateway) = send_fixture(HF94_KEY, 1);
        let epoch = token_epoch("hf94-agent");
        let seeded = "hf119-sparse-resume";
        if resuming {
            append_record(&context, &json!({"k":"intent","batch":seeded,"key":HF94_KEY,
                "epoch":epoch,"messages":["already sent","first resumed","also sent","obsolete second","obsolete third"],
                "delays":[1000,1000,1000,1000],"t":HF94_T0})).unwrap();
            for idx in [0, 2] {
                append_record(
                    &context,
                    &json!({"k":"confirmed","batch":seeded,"idx":idx,"messageId":700+idx}),
                )
                .unwrap();
            }
        }
        let (incoming, config) = (context.clone(), cfg.clone());
        let mut arrived = false;
        test_clock::install(HF94_T0, move |now| {
            if !arrived && now > HF94_T0 + 10000 {
                arrived = true;
                record_reserved2_incoming(
                    &incoming,
                    &config,
                    HF94_KEY,
                    "fault-arrival",
                    json!({"text":"new topic","time":now,"isSelf":false}),
                )
                .unwrap();
            }
        });
        let _clock = Hf94Clock;
        struct ClearAppendHook;
        impl Drop for ClearAppendHook {
            fn drop(&mut self) {
                crate::runtime::APPEND_TEST_HOOK.with(|h| *h.borrow_mut() = None);
            }
        }
        let hook_guard = ClearAppendHook;
        let path = root.0.join("data/send-outbox.jsonl");
        crate::runtime::APPEND_TEST_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |at, bytes| {
                let record: Value = serde_json::from_slice(bytes).unwrap();
                if at == stage && record["k"] == "superseded" {
                    if crash {
                        // The complete decision is on disk; simulate loss of all process memory
                        // before subsequent per-item processing or quota settlement can happen.
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .unwrap()
                            .sync_all()
                            .unwrap();
                        panic!("injected crash after durable discard");
                    }
                    return Err("injected discard append failure".into());
                }
                Ok(())
            }))
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if resuming {
                resume_send_batch(
                    &context,
                    &cfg,
                    &media_onebot(&context, &cfg).unwrap(),
                    HF94_KEY,
                    "group",
                    "94001",
                    "hf94-agent",
                    seeded,
                )
            } else {
                let body = json!({});
                agent::execute_send(
                    &context,
                    &cfg,
                    &agent::SendCall {
                        key: HF94_KEY,
                        kind: "group",
                        id_s: "94001",
                        token: "hf94-agent",
                        messages: vec![
                            "first".into(),
                            "obsolete second".into(),
                            "obsolete third".into(),
                        ],
                        reply_ref: String::new(),
                        at_user: None,
                        gap_mode: "fixed",
                        body: &body,
                        flavor: agent::SendFlavor::Message,
                        policy: GuardPolicy::Reserved2,
                        resolved_quote: None,
                    },
                )
            }
        }));
        drop(hook_guard);
        assert_eq!(gateway.join().unwrap().len(), 1);
        let restarted = RuntimeContext::from_root(&root.0).unwrap();
        let records = read_records(&restarted).unwrap();
        let batch_id = records.iter().find(|r| r["k"] == "intent").unwrap()["batch"]
            .as_str()
            .unwrap();
        let batch = fold_batch(&records, batch_id).unwrap();
        let remaining = if resuming { vec![3, 4] } else { vec![1, 2] };
        if crash {
            assert!(result.is_err(), "the crash boundary must be reached");
            assert_eq!(
                batch.indices_where(ItemState::resendable),
                Vec::<usize>::new(),
                "one durable decision must discard the entire tail after restart"
            );
            assert_eq!(
                batch.indices_where(|s| *s == ItemState::Superseded),
                remaining
            );
            assert_eq!(
                crate::turn_recovery::send_state(&[batch]),
                crate::turn_recovery::SendState::Settled
            );
            let (status, out) = resume_send_batch(
                &restarted,
                &cfg,
                &media_onebot(&restarted, &cfg).unwrap(),
                HF94_KEY,
                "group",
                "94001",
                "hf94-agent",
                batch_id,
            )
            .unwrap();
            assert_eq!(status, 200, "{out}");
            assert_eq!(out["pending"], json!([]));
            assert_eq!(out["superseded"], json!(remaining));
        } else {
            let response = match result.unwrap() {
                Ok(response) => response,
                Err((status, error)) => (status, json!({"ok":false,"error":error})),
            };
            assert_eq!(
                response.0, 503,
                "discard persistence failure must reach the caller: {response:?}"
            );
            assert_ne!(
                response.1["reviseRequired"], true,
                "failed discard cannot report successful revision"
            );
            assert!(
                response.1["error"]
                    .as_str()
                    .unwrap()
                    .contains("injected discard append failure"),
                "{response:?}"
            );
            if stage == "locked" {
                assert_eq!(
                    batch.indices_where(ItemState::resendable),
                    remaining,
                    "failed append must not invent durable terminal states"
                );
                assert!(matches!(
                    crate::turn_recovery::send_state(&[batch]),
                    crate::turn_recovery::SendState::Unresolved(_)
                ));
            }
        }
        assert_eq!(
            read_records(&restarted)
                .unwrap()
                .iter()
                .filter(|r| r["k"] == "attempt")
                .count(),
            1,
            "neither revision nor resume may attempt obsolete content"
        );
    }

    #[test]
    fn hf119_discard_crash_fresh() {
        discard_fault(false, true, "sync");
    }
    #[test]
    fn hf119_discard_crash_resume() {
        discard_fault(true, true, "sync");
    }
    #[test]
    fn hf119_discard_append_failure_fresh() {
        for stage in ["locked", "sync"] {
            discard_fault(false, false, stage);
        }
    }
    #[test]
    fn hf119_discard_append_failure_resume() {
        for stage in ["locked", "sync"] {
            discard_fault(true, false, stage);
        }
    }

    #[test]
    fn hf119_revision_budget_is_bounded_and_scoped_to_turn() {
        let (_root, context, cfg, gateway) = send_fixture(HF94_KEY, 1);
        let (incoming, config) = (context.clone(), cfg.clone());
        test_clock::install(HF94_T0, move |now| {
            record_reserved2_incoming(
                &incoming,
                &config,
                HF94_KEY,
                &format!("busy-{now}"),
                json!({"text":"busy","time":now,"isSelf":false}),
            )
            .unwrap();
        });
        let _clock = Hf94Clock;
        for n in 1..=3 {
            let (_, out) = send(&context, &cfg, HF94_KEY, &["revise"]);
            assert_eq!(out["replyRevisionsUsed"], n, "{out}");
            assert_eq!(out["replyRevisionLimitReached"], n == 3);
        }
        let start = now_ms();
        let (_, out) = send(&context, &cfg, HF94_KEY, &["bounded"]);
        assert_eq!(out["sent"], 1, "{out}");
        assert_eq!(
            now_ms(),
            start + 10000,
            "the fourth attempt has a fixed finite deadline"
        );
        assert_eq!(gateway.join().unwrap().len(), 1);
        let state = load_social_state_for(&context).unwrap();
        assert!(
            state["conversations"][HF94_KEY]["waitBudget"].is_null(),
            "send holds do not spend #94 wait slots"
        );
        mutate_social_state_for(&context, |state| {
            state["conversations"][HF94_KEY]["waitTurnId"] = json!("next-turn");
            Ok(())
        })
        .unwrap();
        let (_, out) = send(&context, &cfg, HF94_KEY, &["new turn"]);
        assert_eq!(
            out["replyRevisionsUsed"], 1,
            "a new turn gets its own revision budget"
        );
    }

    #[test]
    fn hf119_each_bubble_gets_a_typing_budget_and_burst_gaps_count_toward_it() {
        let (_root, context, mut cfg, gateway) = send_fixture(HF94_KEY, 2);
        cfg["socialV2"]["send"] = json!({"firstGapEnabled":true,"firstGapBaseMs":2000,"firstGapPerCharMs":0,
            "burstIntervalMinMs":100,"maxGapMs":10000,"duplicateWindowMs":0});
        cfg["socialV2"]["reply"] = json!({"quietMedianMs":1,"quietMinMs":1,"quietMaxMs":1});
        context
            .write_atomic(
                RuntimeArea::Config,
                "config.json",
                cfg.to_string().as_bytes(),
            )
            .unwrap();
        let start = HF94_T0 + 20000;
        test_clock::install(start, |_| {});
        let _clock = Hf94Clock;
        struct SeedReset(Option<u64>);
        impl Drop for SeedReset {
            fn drop(&mut self) {
                reply_timing::TEST_SEED.with(|s| s.set(self.0));
            }
        }
        let _seed = SeedReset(reply_timing::TEST_SEED.with(|s| s.replace(Some(9))));
        let (_, out) = send(&context, &cfg, HF94_KEY, &["first", "second"]);
        assert_eq!(out["sent"], 2, "{out}");
        // JS Number(value) || fallback makes perChar=0 use 60 ms: the nominal
        // budgets are 2300/2360 ms. Seeds 9/10 draw 2354/2890 ms after quiet.
        // The 100 ms gap consumes part of the second budget, leaving 2790 ms.
        let attempts: Vec<_> = crate::send_outbox::read_records(&context)
            .unwrap()
            .into_iter()
            .filter(|r| r["k"] == "attempt")
            .map(|r| (r["idx"].as_u64().unwrap(), r["t"].as_i64().unwrap()))
            .collect();
        assert_eq!(
            attempts,
            vec![(0, start + 2354), (1, start + 5244)],
            "each bubble gets its own budget and the gap counts toward the second"
        );
        assert_eq!(now_ms(), start + 5244);
        assert_eq!(gateway.join().unwrap().len(), 2);
    }

    #[test]
    fn hf119_neutral_media_keeps_owner_and_mention_urgency() {
        for flag in ["isOwner", "atSelf"] {
            let (_root, context, cfg, gateway) = send_fixture(HF94_KEY, 1);
            let mut text = json!({"time":HF94_T0,"text":"urgent","isSelf":false});
            text[flag] = json!(true);
            record_reserved2_incoming(&context, &cfg, HF94_KEY, "urgent", text).unwrap();
            record_reserved2_incoming(
                &context,
                &cfg,
                HF94_KEY,
                "neutral",
                json!({"time":HF94_T0+8000,"text":"[图片]","quietNeutral":true,"isSelf":false}),
            )
            .unwrap();
            test_clock::install(HF94_T0 + 9000, |_| {});
            let _clock = Hf94Clock;
            send(&context, &cfg, HF94_KEY, &["reply"]);
            assert_eq!(
                now_ms(),
                HF94_T0 + 9000,
                "a neutral picture must not replace the earlier urgent window"
            );
            assert_eq!(gateway.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn hf119_seeded_distributions_and_invalid_overrides() {
        use reply_timing::{QuietWindow, TimingRandom};
        let defaults = QuietWindow {
            median: 10000,
            min: 4000,
            max: 25000,
        };
        for invalid in [
            json!(null),
            json!(-1),
            json!(0),
            json!(false),
            json!("1000"),
            json!(1.5),
            json!(120001),
        ] {
            let cfg = json!({"socialV2":{"reply":{"wakeQuietMs":invalid,"quietMedianMs":invalid,"quietMinMs":invalid,"quietMaxMs":invalid}}});
            assert_eq!(QuietWindow::configured(&cfg, false), defaults);
            assert_eq!(crate::wake_cfg_of(&cfg).reply_wake_quiet_ms, Some(3000));
        }
        assert_eq!(
            QuietWindow::configured(
                &json!({"socialV2":{"reply":{"quietMinMs":20000,"quietMaxMs":5000}}}),
                false
            ),
            defaults
        );
        let mut a = TimingRandom::seeded(119);
        let mut b = TimingRandom::seeded(119);
        let mut draws = Vec::new();
        for _ in 0..10001 {
            let v = a.quiet(defaults);
            assert_eq!(v, b.quiet(defaults));
            assert!((4000..=25000).contains(&v));
            draws.push(v);
        }
        draws.sort();
        assert!(
            (9700..=10300).contains(&draws[5000]),
            "sample median {}",
            draws[5000]
        );
        assert!(draws.windows(2).any(|w| w[0] != w[1]));
    }
}
