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
    state["conversations"][OWNER_CHAT]["wakeConfig"] = json!({"confirmedBy": "set_wake_config", "batchWindowMs": 3000});
    context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();

    let reply = chat(&context, "settings-set", json!({"settingKey": "socialV2.wake.batchWindowMs", "value": 4500})).unwrap();
    assert_eq!(reply["value"], 4500);
    assert_eq!(stored_config(&root)["socialV2"]["wake"]["batchWindowMs"], 4500);
    let loaded = context.load_config().unwrap();
    assert_eq!(crate::wake_cfg_of(&loaded).batch_window_ms, 4500);
    let state = context.read_social_state().unwrap();
    assert_eq!(state["conversations"]["group:101"]["wakeConfig"]["batchWindowMs"], 4500, "the inherited wake plan refreshes");
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
