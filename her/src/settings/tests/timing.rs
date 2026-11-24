use super::*;

#[test]
fn reply_quiet_batch_is_atomic_and_validates_reset_and_undo() {
    let (root, context) = settings_fixture("reply quiet batch");
    let page = |body| post(&context, "/api/socialV2/config", body);
    let set = |key, value| post(&context, "/api/settings/set", json!({"setting": key, "value": value}));
    let (status, reply) = page(json!({"reply": {"quietMinMs": 30_000, "quietMedianMs": 30_000, "quietMaxMs": 30_000}}));
    assert_eq!(status, 200, "{reply}");
    let cfg = context.load_config().unwrap();
    let window = reply_timing::QuietWindow::configured(&cfg, false);
    assert_eq!((window.min, window.median, window.max), (30_000, 30_000, 30_000), "the next send reads all three values");
    let before = stored_config(&root);
    let rows = audit_rows(&root);
    let (history, _) = history_page(&context, "?setting=reply.quietMaxMs");
    for (status, reply) in [
        set("reply.quietMinMs", 30_001),
        page(json!({"reply": {"quietMaxMs": 29_999}, "provideRecommendations": false})),
        post(&context, "/api/settings/reset", json!({"setting": "reply.quietMaxMs"})),
        post(&context, "/api/settings/undo", json!({"entry": history[0]["entry"]})),
    ] {
        assert_eq!(status, 400, "inconsistent quiet triple must be refused: {reply}");
        assert_eq!(reply["error"], "reply.quietMinMs <= reply.quietMedianMs <= reply.quietMaxMs is required");
        assert_eq!(stored_config(&root), before);
        assert_eq!(audit_rows(&root), rows);
    }
    let (status, reply) = page(json!({"reply": {"urgentQuietMinMs": 1, "urgentQuietMedianMs": 1, "urgentQuietMaxMs": 1}}));
    assert_eq!(status, 200, "{reply}");
    let urgent = reply_timing::QuietWindow::configured(&context.load_config().unwrap(), true);
    assert_eq!((urgent.min, urgent.median, urgent.max), (1, 1, 1), "the urgent hold is live");
}

#[test]
fn wake_budget_batch_checks_ceiling_and_wait_before_committing() {
    let (root, context) = settings_fixture("wake budget batch");
    let page = |body| post(&context, "/api/socialV2/config", body);
    let (status, reply) = page(json!({"wake": {"turnHardCapMs": 455_000, "turnCeilingMs": 455_000}}));
    assert_eq!(status, 200, "{reply}");
    let cfg = crate::wake_cfg_of(&context.load_config().unwrap());
    assert_eq!((cfg.turn_hard_cap_ms, cfg.turn_ceiling_ms), (455_000, 455_000));
    let before = stored_config(&root);
    let rows = audit_rows(&root);
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "wake.turnCeilingMs", "value": 454_999}));
    assert_eq!(status, 400, "a ceiling below the cap must be refused: {reply}");
    assert_eq!(reply["error"], "wake.turnHardCapMs must be <= wake.turnCeilingMs");
    let (status, reply) = page(json!({"wait": {"batchMaxMs": 120_000, "minQuietAfterNewMs": 120_001}}));
    assert_eq!(status, 400, "a wait-only write must preserve an explicit cap: {reply}");
    assert_eq!(
        reply["error"],
        "reserved2 turn budget is invalid: turnHardCapMs must be an integer at least the pre-sleep observation plus the wait's quiet extension plus 30000 ms"
    );
    assert_eq!(audit_rows(&root), rows);
    assert_eq!(stored_config(&root), before);

    let (status, reply) = page(json!({"wake": {"turnCeilingMs": 515_000}}));
    assert_eq!(status, 200, "{reply}");
    let (status, reply) = post(&context, "/api/settings/reset", json!({"setting": "wake.turnHardCapMs"}));
    assert_eq!(status, 200, "{reply}");
    assert!(stored_config(&root).pointer("/socialV2/wake/turnHardCapMs").is_none(), "older readers derive the cap too");
    assert_eq!(crate::wake::turn_budget(&context.load_config().unwrap()).hard_cap_ms, 515_000);
}

#[test]
fn wake_batch_chat_write_refreshes_inherited_plans() {
    let (root, context) = chat_fixture("wake batch chat");
    let mut state = context.read_social_state().unwrap();
    state["conversations"]["group:101"]["wakeConfig"] = json!({"confirmedBy": "default", "wakeCount": 7});
    state["conversations"]["group:102"]["wakeConfig"] = json!({"confirmedBy": "default", "batchWindowMs": 3000});
    state["conversations"][OWNER_CHAT]["wakeConfig"] = json!({"confirmedBy": "set_wake_config", "batchWindowMs": 3000});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();

    let reply = chat(&context, "settings-set", json!({"settingKey": "socialV2.wake.batchWindowMs", "value": 4500})).unwrap();
    assert_eq!(reply["value"], 4500);
    assert_eq!(stored_config(&root)["socialV2"]["wake"]["batchWindowMs"], 4500);
    let loaded = context.load_config().unwrap();
    assert_eq!(crate::wake_cfg_of(&loaded).batch_window_ms, 4500);
    let state = context.read_social_state().unwrap();
    assert!(state["conversations"]["group:101"]["wakeConfig"].get("batchWindowMs").is_none(), "the omitted window follows the global setting");
    assert_eq!(state["conversations"]["group:102"]["wakeConfig"]["batchWindowMs"], 4500, "the chat write refreshes an explicit inherited window");
    assert_eq!(state["conversations"]["group:101"]["wakeConfig"]["wakeCount"], 7);
    assert_eq!(state["conversations"][OWNER_CHAT]["wakeConfig"]["batchWindowMs"], 3000);

    let mut stale = state;
    stale["conversations"]["group:101"]["wakeConfig"]["batchWindowMs"] = json!(3000);
    context.write_atomic(RuntimeArea::Data, "social-v2.json", stale.to_string().as_bytes()).unwrap();
    finish_settings_write_back(&context);
    let restarted = context.read_social_state().unwrap();
    assert_eq!(restarted["conversations"]["group:101"]["wakeConfig"]["batchWindowMs"], 4500, "startup repairs an interrupted plan refresh");
}

#[test]
fn restart_preserves_inherited_wake_plan_sleep_until() {
    let (root, context) = chat_fixture("inherited wake restart");
    write_config(&root, |cfg| {
        cfg["socialV2"]["wake"]["recommendedDefaultInfinite"] = json!(false);
    });
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "wake.batchWindowMs", "value": 4500}));
    assert_eq!(status, 200, "{reply}");
    let mut plan = crate::wake::default_wake_config(&context.load_config().unwrap(), now_ms(), 0.5);
    let sleep_until = json!("2099-06-01T00:00:00.000Z");
    plan["sleepUntil"] = sleep_until.clone();
    let mut stale = plan.clone();
    stale["triggers"]["probability"] = json!(0.75);
    let mut state = context.read_social_state().unwrap();
    state["conversations"]["group:101"]["wakeConfig"] = plan.clone();
    state["conversations"][OWNER_CHAT]["wakeConfig"] = stale;
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();

    finish_settings_write_back(&context);

    let restarted = context.read_social_state().unwrap();
    let current = &restarted["conversations"]["group:101"]["wakeConfig"];
    assert_eq!(current["sleepUntil"], sleep_until, "restart must preserve the inherited sleepUntil");
    assert_eq!(current, &plan, "an up-to-date inherited plan stays unchanged");
    assert_eq!(restarted["conversations"][OWNER_CHAT]["wakeConfig"], plan, "restart repairs stale probability without changing sleepUntil");
}

#[test]
fn restart_reconciles_all_inherited_wake_defaults() {
    for registry_write in [true, false] {
        let (root, context) = chat_fixture("mixed wake settings restart");
        let original = json!({
            "mode": "diving", "infinite": true, "sleepUntil": null, "batchWindowMs": 3000,
            "triggers": {"atMention": true, "nameMention": true, "question": true, "poke": true,
                "anyMessage": false, "keywords": ["old"], "speakerIds": ["42"], "probability": 0.75, "dropNames": ["muted"]},
            "confirmedAt": 5, "confirmedBy": "default", "wakeCount": 7
        });
        if registry_write {
            let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "wake.batchWindowMs", "value": 4500}));
            assert_eq!(status, 200, "{reply}");
        }
        // The configuration was saved, but its conversation refresh was interrupted.
        write_config(&root, |cfg| cfg["socialV2"]["wake"] = json!({
            "batchWindowMs": 4500, "defaultMode": "active", "recommendedDefaultInfinite": false,
            "recommendedAtMention": false, "recommendedNameMention": false,
            "recommendedQuestion": false, "recommendedPoke": false,
            "recommendedKeywords": ["new"], "recommendedProbability": 0.25,
        }));
        let mut state = context.read_social_state().unwrap();
        state["conversations"]["group:101"]["wakeConfig"] = original.clone();
        let mut confirmed = original;
        confirmed["confirmedBy"] = json!("set_wake_config");
        state["conversations"][OWNER_CHAT]["wakeConfig"] = confirmed.clone();
        context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();

        finish_settings_write_back(&context);

        let restarted = context.read_social_state().unwrap();
        let plan = &restarted["conversations"]["group:101"]["wakeConfig"];
        assert_eq!(plan["triggers"], json!({"atMention": false, "nameMention": false, "question": false,
            "poke": false, "anyMessage": true, "keywords": ["new"], "speakerIds": [], "probability": 0.25, "dropNames": ["muted"]}));
        assert_eq!(plan["batchWindowMs"], 4500);
        assert_eq!(plan["mode"], "active");
        assert_eq!(plan["infinite"], false);
        assert!(plan["sleepUntil"].as_str().and_then(crate::wake::parse_iso_ms).is_some(), "becoming finite draws a deadline");
        assert_eq!(plan["confirmedAt"], 5);
        assert_eq!(plan["wakeCount"], 7);
        assert_eq!(restarted["conversations"][OWNER_CHAT]["wakeConfig"], confirmed);
    }
}

#[test]
fn restart_keeps_omitted_wake_values_that_already_follow_settings() {
    let (root, context) = chat_fixture("omitted current wake defaults");
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "wake.batchWindowMs", "value": 4500}));
    assert_eq!(status, 200, "{reply}");
    let mut plan = crate::wake::default_wake_config(&context.load_config().unwrap(), now_ms(), 0.5);
    plan.as_object_mut().unwrap().remove("batchWindowMs").unwrap();
    plan["triggers"].as_object_mut().unwrap().remove("probability").unwrap();
    let mut state = context.read_social_state().unwrap();
    state["conversations"]["group:101"]["wakeConfig"] = plan;
    let path = root.0.join("data/social-v2.json");
    let before = serde_json::to_vec(&state).unwrap();
    context.write_atomic(RuntimeArea::Data, "social-v2.json", &before).unwrap();

    finish_settings_write_back(&context);

    assert_eq!(std::fs::read(path).unwrap(), before, "startup does not rewrite an effectively current wake plan");
}

#[test]
fn unchanged_wake_setting_keeps_inherited_plan_and_sleep_until() {
    let (root, context) = chat_fixture("unchanged wake setting");
    write_config(&root, |cfg| cfg["socialV2"]["wake"]["recommendedDefaultInfinite"] = json!(false));
    let set = |value| post(&context, "/api/settings/set", json!({"setting": "wake.batchWindowMs", "value": value}));
    let (status, reply) = set(4500);
    assert_eq!(status, 200, "{reply}");
    let mut plan = crate::wake::default_wake_config(&context.load_config().unwrap(), now_ms(), 0.5);
    let sleep_until = json!("2099-06-01T00:00:00.000Z");
    plan["sleepUntil"] = sleep_until.clone();
    // A set that changes nothing must leave even a stale plan for the next refresh.
    plan["batchWindowMs"] = json!(3000);
    let mut state = context.read_social_state().unwrap();
    state["conversations"]["group:101"]["wakeConfig"] = plan.clone();
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    let current = || context.read_social_state().unwrap()["conversations"]["group:101"]["wakeConfig"].clone();

    let (status, reply) = set(4500);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["changed"], false);
    assert_eq!(current()["sleepUntil"], sleep_until, "an unchanged setting must preserve the inherited sleepUntil");
    assert_eq!(current(), plan, "an unchanged setting does not rebuild the inherited plan");

    let reply = chat(&context, "settings-set", json!({"settingKey": "socialV2.wake.batchWindowMs", "value": 4500})).unwrap();
    assert_eq!(reply["changed"], false);
    assert_eq!(current(), plan, "an unchanged chat setting does not rebuild the inherited plan");

    let (status, reply) = set(6000);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["changed"], true);
    assert_eq!(current()["batchWindowMs"], 6000);
    assert_eq!(current()["sleepUntil"], sleep_until, "a changed setting also preserves the inherited sleepUntil");
}

#[test]
fn proactive_and_wait_range_refusals_keep_config_and_audit() {
    let (root, context) = settings_fixture("proactive and wait ranges");
    let page = |body| post(&context, "/api/socialV2/config", body);
    let (status, reply) = page(json!({
        "proactive": {"checkIntervalMinMs": 60_000, "checkIntervalMaxMs": 60_000},
        "wait": {"minMs": 1000, "maxMs": 1000}
    }));
    assert_eq!(status, 200, "{reply}");
    let path = root.0.join("config/config.json");
    let before = std::fs::read(&path).unwrap();
    let rows = audit_rows(&root);

    for (body, error) in [
        (json!({"proactive": {"checkIntervalMinMs": 60_001}}), "proactive.checkIntervalMinMs <= proactive.checkIntervalMaxMs is required"),
        (json!({"wait": {"maxMs": 999}}), "wait.minMs <= wait.maxMs is required"),
    ] {
        let (status, reply) = page(body);
        assert_eq!(status, 400, "{reply}");
        assert_eq!(reply["error"], error);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(audit_rows(&root), rows);
    }
}

#[test]
fn legacy_wake_preemption_clamps_to_the_two_hour_limit() {
    let (root, context) = settings_fixture("legacy wake preemption");
    for (configured, expected) in [(0, 0), (7_199_999, 7_199_999), (7_200_000, 7_200_000), (7_200_001, 7_200_000)] {
        write_config(&root, |cfg| cfg["socialV2"]["wake"]["preemptAfterMs"] = json!(configured));
        let cfg = crate::wake_cfg_of(&context.load_config().unwrap());
        assert_eq!(cfg.preempt_after_ms, expected, "configured preemptAfterMs {configured}");
    }
}

#[test]
fn legacy_negative_wake_quotas_stay_disabled() {
    let (root, context) = settings_fixture("legacy wake quotas");
    for (configured, expected) in [(-1, 0), (0, 0), (1, 1), (10_000, 10_000), (10_001, 10_000)] {
        write_config(&root, |cfg| cfg["socialV2"]["wake"] = json!({
            "maxWakePerMinute": configured, "maxWakePerHour": configured,
        }));
        let cfg = crate::wake_cfg_of(&context.load_config().unwrap());
        assert_eq!((cfg.max_wake_per_minute, cfg.max_wake_per_hour), (expected, expected), "configured wake quotas {configured}");
    }
}

#[test]
fn send_settings_feed_the_next_quota_reservation_and_automatic_gap() {
    let (_root, context) = chat_fixture("live send controls");
    let key = "group:101";
    let epoch = crate::send_outbox::token_epoch(GROUP_CHAT_TOKEN);
    let (status, reply) = post(&context, "/api/socialV2/config", json!({
        "send": {
            "maxSendPerMinute": 1,
            "longGapProbability": 1.0,
            "longGapMinMs": 5467,
            "longGapMaxMs": 5467,
            "firstGapBaseMs": 4567,
            "firstGapPerCharMs": 123,
        }
    }));
    assert_eq!(status, 200, "{reply}");
    let cfg = context.load_config().unwrap();
    let now = now_ms();
    reserve_send_quota(&context, &cfg, key, 1, now, &epoch).unwrap();
    assert_eq!(reserve_send_quota(&context, &cfg, key, 1, now, &epoch).unwrap_err().0, 429, "the next reservation uses the stored minute limit");
    assert_eq!(agent::automatic_gap(&cfg, 0.5, 0.75), 5467, "automatic gaps use the stored long range");
    assert_eq!(agent::first_typing_delay(&cfg, "😂"), 4813, "the next typing budget counts UTF-16 units");
    let reply = chat(&context, "settings-set", json!({"settingKey": "sendDelayMs", "value": 543})).unwrap();
    assert_eq!(reply["value"], 543);
    assert_eq!(crate::chat::send_delay_ms(&context.load_config().unwrap()), 543);
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "send.maxSendPerMinute", "value": 2}));
    assert_eq!(status, 200, "{reply}");
    let cfg = context.load_config().unwrap();
    reserve_send_quota(&context, &cfg, key, 1, now, &epoch).unwrap();
    assert_eq!(reserve_send_quota(&context, &cfg, key, 1, now, &epoch).unwrap_err().0, 429);
}

#[test]
fn send_settings_apply_the_stored_sticker_pause_and_refuse_inverted_bounds() {
    use std::cell::Cell;
    use std::rc::Rc;

    let (root, context) = settings_fixture("sticker pause setting");
    let page = |body| post(&context, "/api/socialV2/config", body);
    let (status, reply) = page(json!({"reply": {"stickerPauseMinMs": 0, "stickerPauseMaxMs": 0}}));
    assert_eq!(status, 200, "{reply}");
    let before = stored_config(&root);
    let (status, reply) = page(json!({"reply": {"stickerPauseMinMs": 1}}));
    assert_eq!(status, 400, "{reply}");
    assert_eq!(stored_config(&root), before);
    let seen = Rc::new(Cell::new(None));
    let observed = Rc::clone(&seen);
    STICKER_PAUSE_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move |ms| observed.set(Some(ms)))));
    sticker_send_pause(&context.load_config().unwrap());
    STICKER_PAUSE_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
    assert_eq!(seen.get(), Some(0), "the next sticker uses the stored pause");
}

fn store_group_plan(context: &RuntimeContext, plan: Value) {
    let mut state = context.read_social_state().unwrap();
    state["conversations"]["group:101"]["wakeConfig"] = plan;
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
}

fn group_plan(context: &RuntimeContext) -> Value {
    context.read_social_state().unwrap()["conversations"]["group:101"]["wakeConfig"].clone()
}

#[test]
fn infinite_switch_disarms_near_sleep_timer() {
    use crate::session::{Driver, wake_config_from_json};

    let (root, context) = chat_fixture("infinite sleep switch");
    write_config(&root, |cfg| cfg["socialV2"]["wake"]["recommendedDefaultInfinite"] = json!(false));
    let now = now_ms();
    let deadline = now + 10_000;
    let mut plan = crate::wake::default_wake_config(&context.load_config().unwrap(), now, 0.5);
    plan["sleepUntil"] = json!(crate::sticker::iso_at(deadline));
    let finite = wake_config_from_json(Some(&plan));
    let mut driver = Driver::new(crate::wake::WakeScheduler::new(Default::default(), finite.clone()), 42);
    driver.adopt_wake_config(finite, now);
    assert_eq!(driver.sched.sleep_at, Some(deadline));
    store_group_plan(&context, plan);

    let (status, reply) = post(&context, "/api/socialV2/config", json!({"wake": {"recommendedDefaultInfinite": true}}));
    assert_eq!(status, 200, "{reply}");
    let infinite = group_plan(&context);
    assert_eq!(infinite["infinite"], true);
    assert!(infinite.get("sleepUntil").is_none());
    driver.adopt_wake_config(wake_config_from_json(Some(&infinite)), now);
    assert_eq!(driver.sched.sleep_at, None, "the old finite timer must be disarmed");
    assert!(driver.pump(deadline).is_empty(), "the old finite timer must not emit a timeout");
}

#[test]
fn social_save_keeps_omitted_batch_without_adoption() {
    let (root, context) = chat_fixture("unchanged social batch");
    write_config(&root, |cfg| cfg["socialV2"]["wake"] = json!({"batchWindowMs": 4500}));
    let mut plan = crate::wake::default_wake_config(&context.load_config().unwrap(), now_ms(), 0.5);
    plan.as_object_mut().unwrap().remove("batchWindowMs").unwrap();
    let seen = crate::session::WakeSync::of(&plan);
    store_group_plan(&context, plan);
    let path = root.0.join("data/social-v2.json");
    let before = std::fs::read(&path).unwrap();

    let (status, reply) = post(&context, "/api/socialV2/config", json!({"wake": {"batchWindowMs": 4500}}));
    assert_eq!(status, 200, "{reply}");
    let saved = group_plan(&context);
    assert!(saved.get("batchWindowMs").is_none(), "the inherited batch window must stay omitted");
    assert!(crate::session::WakeSync::changed(Some(&seen), &saved).is_none(), "the running driver keeps pending wakes");
    assert_eq!(std::fs::read(path).unwrap(), before, "an unchanged plan does not rewrite social state");
}

#[test]
fn finite_deadline_follows_range_changes_once() {
    let (root, context) = chat_fixture("finite range repair");
    write_config(&root, |cfg| cfg["socialV2"]["wake"] = json!({
        "recommendedDefaultInfinite": false, "recommendedSleepMinMs": 4000, "recommendedSleepMaxMs": 4000,
    }));
    let mut plan = crate::wake::default_wake_config(&context.load_config().unwrap(), now_ms(), 0.5);
    // Old releases saved the deadline alone. A live save still has the old configured range.
    plan.as_object_mut().unwrap().remove("sleepRangeMs");
    store_group_plan(&context, plan);
    let range_post = json!({"wake": {"recommendedSleepMinMs": 8000, "recommendedSleepMaxMs": 8000}});
    let before = now_ms();
    let (status, reply) = post(&context, "/api/socialV2/config", range_post);
    assert_eq!(status, 200, "{reply}");
    let saved = group_plan(&context);
    let deadline = crate::wake::parse_iso_ms(saved["sleepUntil"].as_str().unwrap()).unwrap();
    assert!((before + 8000..=now_ms() + 8000).contains(&deadline));
    finish_settings_write_back(&context);
    assert_eq!(group_plan(&context), saved, "a second reconcile keeps the sampled deadline");

    write_config(&root, |cfg| cfg["socialV2"]["wake"]["sleepMaxMs"] = json!(2000));
    let before = now_ms();
    finish_settings_write_back(&context);
    let repaired = group_plan(&context);
    let deadline = crate::wake::parse_iso_ms(repaired["sleepUntil"].as_str().unwrap()).unwrap();
    assert!((before + 2000..=now_ms() + 2000).contains(&deadline), "restart repairs a changed effective range");
}

#[test]
fn live_wake_save_with_unreadable_store_writes_nothing() {
    let (root, context) = chat_fixture("wake settings read refused");
    let (status, reply) = post(&context, "/api/settings/set", json!({"setting": "wake.batchWindowMs", "value": 4500}));
    assert_eq!(status, 200, "{reply}");
    store_group_plan(&context, crate::wake::default_wake_config(&context.load_config().unwrap(), now_ms(), 0.5));
    write_config(&root, |cfg| cfg["socialV2"]["wake"]["batchWindowMs"] = json!(8000));
    StoreProbe::at(&root.0).edit("PRAGMA user_version=4;");
    let path = root.0.join("data/social-v2.json");
    let before = std::fs::read(&path).unwrap();
    let config_path = root.0.join("config/config.json");
    let file_before = std::fs::read(&config_path).unwrap();
    let error = handle_social_config_post(&context, r#"{"wake":{"recommendedPoke":false}}"#).unwrap_err();
    assert_eq!(error.0, 503);
    assert_eq!(
        crate::config_tx::refresh_default_wake_configs(&context)
            .unwrap_err()
            .into_http()
            .0,
        503,
        "a post-commit refresh requires the store too"
    );
    assert_eq!(std::fs::read(path).unwrap(), before, "file-only defaults must never reach the plan");
    assert_eq!(std::fs::read(config_path).unwrap(), file_before, "the refused save keeps the configuration");
}

#[test]
fn audit_undo_restores_inherited_poke_and_restart_repairs_interruption() {
    let (root, context) = chat_fixture("undo inherited poke");
    write_config(&root, |cfg| cfg["socialV2"] = json!({}));
    let initial = crate::wake::default_wake_config(&context.load_config().unwrap(), now_ms(), 0.5);
    assert_eq!(initial["triggers"]["poke"], true);
    store_group_plan(&context, initial);
    let (status, reply) = post(&context, "/api/socialV2/config", json!({"wake": {"recommendedPoke": false}}));
    assert_eq!(status, 200, "{reply}");
    let disabled = group_plan(&context);
    assert_eq!(disabled["triggers"]["poke"], false);
    let (status, recent) = admin_request(&context, "GET", "/api/audit/recent?limit=10", None);
    assert_eq!(status, 200, "{recent}");
    let id = &recent["entries"].as_array().unwrap().iter().find(|row| row["path"] == "/api/socialV2/config").unwrap()["id"];
    let (status, reply) = post(&context, "/api/audit/undo", json!({"id": id}));
    assert_eq!(status, 200, "{reply}");
    assert!(stored_config(&root).pointer("/socialV2/wake").is_none());
    assert_eq!(group_plan(&context)["triggers"]["poke"], true, "undo restores the inherited trigger immediately");

    store_group_plan(&context, disabled);
    finish_settings_write_back(&context);
    assert_eq!(group_plan(&context)["triggers"]["poke"], true, "restart finishes an interrupted undo without wake history");
}

#[test]
fn restart_preserves_no_action_reset_keywords() {
    for recommended in [json!([]), json!(["global"])] {
        let (root, context) = chat_fixture("no action wake keywords");
        write_config(&root, |cfg| cfg["socialV2"]["wake"] = json!({"recommendedKeywords": recommended}));
        let cfg = context.load_config().unwrap();
        let mut plan = crate::wake::default_wake_config(&cfg, now_ms(), 0.5);
        plan["confirmedBy"] = json!("set_wake_config");
        plan["noActionCount"] = json!(2);
        plan["triggers"]["keywords"] = json!(["local"]);
        store_group_plan(&context, plan);
        crate::social_state::complete_reserved2_turn(
            &context,
            &cfg,
            "group:101",
            GROUP_CHAT_TOKEN,
            Some(1),
            &json!(0),
            0,
        )
        .unwrap();
        let reset = group_plan(&context);
        assert_eq!(reset["confirmedBy"], "default");
        assert_eq!(reset["triggers"]["keywords"], json!(["local"]));

        finish_settings_write_back(&context);
        assert_eq!(group_plan(&context), reset, "restart keeps the no-action reset's custom keywords");
        let (status, reply) = post(&context, "/api/socialV2/config", json!({"wake": {"recommendedKeywords": recommended}}));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(group_plan(&context), reset);
        let (status, reply) = post(&context, "/api/socialV2/config", json!({"wake": {"recommendedKeywords": ["replacement"]}}));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(group_plan(&context)["triggers"]["keywords"], json!(["replacement"]));
        write_config(&root, |cfg| cfg["socialV2"]["wake"]["recommendedKeywords"] = json!([]));
        finish_settings_write_back(&context);
        assert_eq!(group_plan(&context)["triggers"]["keywords"], json!([]), "clearing the recommendation clears inherited words");
    }
}

#[test]
fn legacy_unsigned_wake_quotas_clamp_to_maximum() {
    let (root, context) = settings_fixture("unsigned wake quota");
    for quota in [i64::MAX as u64, 1_u64 << 63, u64::MAX] {
        write_config(&root, |cfg| cfg["socialV2"]["wake"] = json!({"maxWakePerMinute": quota, "maxWakePerHour": quota}));
        let cfg = crate::wake_cfg_of(&context.load_config().unwrap());
        assert_eq!((cfg.max_wake_per_minute, cfg.max_wake_per_hour), (10_000, 10_000));
    }
}
