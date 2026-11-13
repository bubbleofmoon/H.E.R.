//! Deterministic busy-group replay, using the production decision and scheduler seams.
use super::*;

#[test]
fn hf129_busy_group_replay_measures_awake_time_and_wakes_with_rules_on_and_off() {
    fn replay(enabled: bool) -> Value {
        let cfg = json!({"socialV2":{"proactive":{"enabled":false},"wake":{"maxWakePerMinute":0,"maxWakePerHour":0}}});
        let mut driver = session::Driver::new(
            wake::WakeScheduler::new(wake_cfg_of(&cfg), wake::WakeConfig::default()),
            129,
        );
        let triggers: Triggers =
            serde_json::from_value(json!({"atMention":true,"probability":0.05})).unwrap();
        let cards = HashMap::new();
        let origin = 1_000_000;
        let horizon = 3_600_000;
        let duration = wake::PRE_SLEEP_WAIT_DEFAULT_MS;
        let mut end = None;
        let mut seq = 0;
        let mut delivered_urgent = Vec::new();
        let mut urgent_seqs = Vec::new();
        let mut wakes = 0;
        let mut awake_ms = 0;
        let mut suppressed = 0;
        let mut covered = 0;
        let mut probability_wakes = 0;
        for elapsed in (0..=horizon).step_by(1000) {
            let now = origin + elapsed;
            let mut events = Vec::new();
            if end.is_some_and(|at| now >= at) {
                if enabled {
                    driver.sched.pending.retain(|w| {
                        if !wake::is_timer_reason(&w.reason) && delivered_urgent.contains(&w.seq) {
                            covered += 1;
                            false
                        } else {
                            true
                        }
                    });
                }
                delivered_urgent.clear();
                driver.sched.prompt_running = false;
                events.extend(driver.apply(session::Input::TurnEnd { at_ms: now }));
                end = None;
            }
            events.extend(driver.pump(now));
            for event in events {
                assert!(
                    end.is_none(),
                    "the scheduler dispatched a second running turn: {event:?}"
                );
                wakes += 1;
                awake_ms += duration.min((origin + horizon - now).max(0));
                driver.apply(session::Input::TurnStart { at_ms: now });
                driver.sched.prompt_running = true;
                end = Some(now + duration);
            }
            if elapsed == horizon {
                break;
            }
            // The model uses waits every 10 s, leaving messages unread until closure.
            if end.is_some() && elapsed % 10_000 == 0 {
                delivered_urgent.extend(urgent_seqs.iter().copied().filter(|s| *s <= seq));
            }
            if elapsed % 5000 != 0 {
                continue;
            }
            seq += 1;
            let text = format!("synthetic busy-group message {seq}");
            let mention = elapsed % 1_800_000 == 0;
            if mention {
                urgent_seqs.push(seq);
            }
            let ev: Event = serde_json::from_value(json!({"post_type":"message","message_type":"group","group_id":420042,"self_id":9001,"user_id":1001,"message":[]})).unwrap();
            let (wake, reason) =
                reserved2::in_turn_decision(enabled && end.is_some(), mention, false, "")
                    .unwrap_or_else(|| decide(&ev, &text, mention, "", &cards, &triggers, false));
            if reason == "in-turn" {
                suppressed += 1;
            }
            if wake && reason == "probability" {
                probability_wakes += 1;
            }
            for event in driver.apply(session::Input::Incoming {
                reason: wake.then_some(reason),
                seq,
                at_ms: now,
                neutral: false,
                urgent: mention,
            }) {
                assert!(
                    end.is_none(),
                    "the scheduler dispatched while a turn was running: {event:?}"
                );
                wakes += 1;
                awake_ms += duration.min((origin + horizon - now).max(0));
                driver.apply(session::Input::TurnStart { at_ms: now });
                driver.sched.prompt_running = true;
                end = Some(now + duration);
            }
        }
        json!({"wakes":wakes,"awakeMs":awake_ms,"awakePercent":awake_ms as f64/horizon as f64*100.0,
            "suppressedInTurn":suppressed,"covered":covered,"probabilityWakes":probability_wakes})
    }
    let off = replay(false);
    let on = replay(true);
    assert!(
        on["wakes"].as_i64().unwrap() < off["wakes"].as_i64().unwrap(),
        "off={off} on={on}"
    );
    assert!(
        on["awakeMs"].as_i64().unwrap() < off["awakeMs"].as_i64().unwrap(),
        "off={off} on={on}"
    );
    let report = json!({"kind":"synthetic; no model/send latency forecast","messages":720,"durationMs":3_600_000,
        "arrivalEveryMs":5000,"probability":0.05,"turnDurationMs":300000,"waitReadEveryMs":10000,"mentionEveryMs":1800000,
        "rulesOff":off,"rulesOn":on});
    println!("HF129_REPLAY {report}");
    if let Ok(dir) = std::env::var("HER129_EVIDENCE") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join("wake-replay.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}
