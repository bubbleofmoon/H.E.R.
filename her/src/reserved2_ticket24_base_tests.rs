// Ticket 24 hub tests that compile against the integration tip as well (red evidence there).
use super::*;

#[tokio::test]
async fn a_token_change_seen_on_ingress_discards_a_pending_retry_of_the_old_epoch() {
    let (root, mut hub) = queued_hub("t24-retry-epoch").await;
    let key = "group:420042";
    hub.ready.clear();
    hub.awaiting.clear();
    // A wake of the old epoch failed before submission and waits for its bounded retry.
    hub.retries.insert(key.into(), (0, "atMention".into()));
    hub.failures.insert(key.into(), 1);
    // The administrator ends the epoch (token rotated) before the retry is due.
    let mut state = hub.state().unwrap();
    state["conversations"][key]["agentToken"] = json!("rotated-token");
    hub.context.write_atomic(RuntimeArea::Data, "social-v2.json", state.to_string().as_bytes()).unwrap();
    // The next ingress observes the new token first.
    let frame = json!({"post_type":"message","message_type":"group","group_id":420042,"user_id":1001,"message_id":24001,"message":[{"type":"text","data":{"text":"no mention"}}]});
    let ev: crate::Event = serde_json::from_value(frame.clone()).unwrap();
    hub.handle(&ev, &frame.to_string(), false, 9001, "bot", &HashMap::new()).await.unwrap();
    hub.tick("bot", 9001).await.unwrap();
    let ran = hub.active.get(key).map(|a| a.reason.clone());
    for active in hub.active.values() { active.task.abort(); }
    drop(hub);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(ran, None, "a retry queued under the revoked token ran under the new one");
}
