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
