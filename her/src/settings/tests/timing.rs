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
