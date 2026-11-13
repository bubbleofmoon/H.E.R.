// Included in console's test module so the same harness can also run on the immutable hotfix base.
mod hf119_measure {
    use super::*;
    use crate::session::{Driver, Input, WakeEvent};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn hf119_measure_busy_group_and_private_burst() {
        for (name, key, arrivals) in [
            (
                "busy_group",
                HF94_KEY,
                (0..30).map(|n| n * 2000).collect::<Vec<i64>>(),
            ),
            (
                "private_three_message_burst",
                "private:7707",
                vec![0, 2000, 4000],
            ),
        ] {
            let (root, _, mut cfg, _) = hf94_wait_fixture(json!({}));
            let (endpoint, scenario_done, gateway) = start_scenario_fake_onebot();
            cfg["allow"]["private"] = json!([8808, 7707]);
            cfg["socialV2"]["send"] = json!({"firstGapEnabled":true,"duplicateWindowMs":0,"maxSendPerMinute":100,"maxSendPerHour":100});
            cfg["socialV2"]["reply"] =
                json!({"quietMedianMs":10000,"quietMinMs":10000,"quietMaxMs":10000});
            cfg["socialV2"]["wake"] = json!({"batchWindowMs":8000,"maxWakePerMinute":0,"maxWakePerHour":0,"preSleepWaitMs":10000});
            cfg["socialV2"]["proactive"] = json!({"enabled":false});
            cfg["socialV2"]["autoReplyCheckMs"] = json!(3000000);
            std::fs::write(root.0.join("config/config.json"), cfg.to_string()).unwrap();
            std::fs::write(
                root.0.join("dependencies.json"),
                json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":endpoint})
                    .to_string(),
            )
            .unwrap();
            let context = RuntimeContext::from_root(&root.0).unwrap();
            let mut state = load_social_state_for(&context).unwrap();
            state["conversations"][key] = state["conversations"][HF94_KEY].clone();
            context
                .write_atomic(
                    RuntimeArea::Data,
                    "social-v2.json",
                    state.to_string().as_bytes(),
                )
                .unwrap();
            let driver = Rc::new(RefCell::new(Driver::new(
                crate::session::scheduler_from_state(
                    crate::wake_cfg_of(&cfg),
                    &state["conversations"][key],
                ),
                119,
            )));
            let queue = Rc::new(RefCell::new(std::collections::VecDeque::<WakeEvent>::new()));
            let (d, q, incoming, config) =
                (driver.clone(), queue.clone(), context.clone(), cfg.clone());
            let final_arrival = *arrivals.last().unwrap();
            let mut arrivals = arrivals.into_iter().enumerate().peekable();
            test_clock::install(HF94_T0 - 100, move |now| {
                while arrivals.peek().is_some_and(|(_, at)| now >= HF94_T0 + at) {
                    let (index, at) = arrivals.next().unwrap();
                    let seq = record_reserved2_incoming(
                        &incoming,
                        &config,
                        key,
                        &format!("measure-{index}"),
                        json!({"time":HF94_T0+at,"text":format!("message {index}"),"isSelf":false}),
                    )
                    .unwrap();
                    q.borrow_mut().extend(d.borrow_mut().apply(Input::Message {
                        reason: if key.starts_with("private:") {
                            "private".into()
                        } else {
                            "keyword:topic".into()
                        },
                        seq,
                        at_ms: HF94_T0 + at,
                    }));
                }
            });
            let _clock = Hf94Clock;
            let (mut wakes, mut model_calls, mut revisions) = (0, 0, 0);
            let (mut wake_times, mut send_times, mut reply_delays) =
                (Vec::new(), Vec::new(), Vec::new());
            // Fake DSH's policy is identical on base and candidate: inspect unread, think 2 s,
            // call the send tool, and revise only when that tool returns new input.
            while now_ms() < HF94_T0 + 120000 {
                queue
                    .borrow_mut()
                    .extend(driver.borrow_mut().pump(now_ms()));
                let event = queue.borrow_mut().pop_front();
                if let Some(event) = event {
                    assert_ne!(event.reason, "turnTimeout");
                    wakes += 1;
                    wake_times.push(now_ms() - HF94_T0);
                    driver
                        .borrow_mut()
                        .apply(Input::TurnStart { at_ms: now_ms() });
                    let epoch = crate::send_outbox::token_epoch("hf94-agent");
                    let turn = crate::turn_ledger::Recorder::begin(
                        &context,
                        key,
                        "dsh",
                        "synthetic-measure",
                        &epoch,
                        &[],
                        None,
                    )
                    .unwrap();
                    crate::turn_ledger::append_record(
                        &context,
                        &json!({"k":"started","turn":turn.id,"t":now_ms()}),
                    )
                    .unwrap();
                    let mut seen;
                    loop {
                        model_calls += 1;
                        seen = load_social_state_for(&context).unwrap()["conversations"][key]
                            ["lastUnreadSeq"]
                            .as_i64()
                            .unwrap_or(0);
                        wait_sleep(2000);
                        let (kind, id, _) = agent::parse_key(key).unwrap();
                        let body = json!({});
                        let (status, out) = agent::execute_send(
                            &context,
                            &cfg,
                            &agent::SendCall {
                                key,
                                kind,
                                id_s: id,
                                token: "hf94-agent",
                                messages: vec!["synthetic reply".into()],
                                reply_ref: String::new(),
                                at_user: None,
                                gap_mode: "fixed",
                                body: &body,
                                flavor: agent::SendFlavor::Message,
                                policy: GuardPolicy::Reserved2,
                                resolved_quote: None,
                            },
                        )
                        .unwrap();
                        assert_eq!(status, 200, "{out}");
                        if out["reviseRequired"] == true {
                            revisions += 1;
                            continue;
                        }
                        assert_eq!(out["sent"], 1, "{out}");
                        let state = load_social_state_for(&context).unwrap();
                        send_times.push(now_ms() - HF94_T0);
                        reply_delays.push(
                            now_ms()
                                - state["conversations"][key]["lastIncomingAt"]
                                    .as_i64()
                                    .unwrap(),
                        );
                        break;
                    }
                    mutate_social_state_for(&context, |state| {
                        let conv = &mut state["conversations"][key];
                        conv["unread"]
                            .as_array_mut()
                            .unwrap()
                            .retain(|m| m["seq"].as_i64().unwrap() > seen);
                        conv["lastReadThroughSeq"] = json!(seen);
                        Ok(())
                    })
                    .unwrap();
                    let state = load_social_state_for(&context).unwrap();
                    driver
                        .borrow_mut()
                        .refresh_observed_state(&state["conversations"][key], now_ms());
                    driver.borrow_mut().apply(Input::UnreadChanged {
                        at_ms: now_ms(),
                        seqs: state["conversations"][key]["unread"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|m| m["seq"].as_i64().unwrap())
                            .collect(),
                    });
                    let events = driver
                        .borrow_mut()
                        .apply(Input::TurnEnd { at_ms: now_ms() });
                    queue.borrow_mut().extend(events);
                    turn.end("completed", "synthetic-measure").unwrap();
                } else {
                    if now_ms() > HF94_T0 + final_arrival
                        && driver.borrow().unread_len() == 0
                        && driver.borrow().sched.pending_wake_at.is_none()
                    {
                        break;
                    }
                    wait_sleep(100);
                }
            }
            assert!(now_ms() < HF94_T0 + 120000, "scenario did not settle");
            scenario_done.send(()).unwrap();
            let requests = gateway.join().unwrap();
            let sends = requests
                .iter()
                .filter(|r| r.path == "/send_group_msg" || r.path == "/send_private_msg")
                .count();
            assert_eq!(sends, send_times.len());
            println!(
                "HF119_MEASURE {}",
                json!({"scenario":name,"arrivals":if name=="busy_group" {30} else {3},"wakes":wakes,
                "modelCalls":model_calls,"revisions":revisions,"wakeTimesMs":wake_times,"sendTimesMs":send_times,
                "replyDelaySinceLastIncomingMs":reply_delays,"gatewaySends":sends})
            );
        }
    }
}
