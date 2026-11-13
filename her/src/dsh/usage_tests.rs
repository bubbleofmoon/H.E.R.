//! 账本采样折叠（`UsageFold` / `harvest_of_snapshot`）的离线测试，口径照抄 JS `token-ledger.js` 与 bridge.js 的记账接线。

use super::*;
use serde_json::{json, Value};

fn ev(seq: i64, etype: &str, time: i64, data: Value) -> Value {
    json!({ "seq": seq, "type": etype, "time": time, "data": data })
}

#[test]
fn usage_model_prefers_usage_model_then_session_selection_then_event_model() {
    let mut fold = UsageFold::new(Some("sel-flash".into()), Some("sel-provider".into()));
    let with_usage_model = fold
        .observe(&ev(5, "assistant/message", 100, json!({"turn": 1, "step": 1, "usage": {"model": "usage-pro", "cacheMiss": 10, "output": 1}})), 0)
        .expect("usage sample");
    assert_eq!(with_usage_model.model.as_deref(), Some("usage-pro"));

    let from_selection = fold
        .observe(&ev(6, "assistant/message", 101, json!({"turn": 1, "step": 2, "usage": {"cacheMiss": 10}})), 0)
        .expect("usage sample");
    assert_eq!(from_selection.model.as_deref(), Some("sel-flash"));
    assert_eq!(from_selection.provider.as_deref(), Some("sel-provider"));

    let mut bare = UsageFold::new(None, None);
    let from_event = bare
        .observe(&ev(7, "assistant/message", 102, json!({"turn": 1, "step": 1, "model": "event-model", "usage": {"output": 1}})), 0)
        .expect("usage sample");
    assert_eq!(from_event.model.as_deref(), Some("event-model"));
}

#[test]
fn model_selection_updates_later_samples_and_its_provider() {
    let mut fold = UsageFold::new(None, None);
    assert!(fold.observe(&ev(1, "model/selection", 0, json!({"model": "m2", "provider": "opencode-go"})), 0).is_none());
    let sample = fold
        .observe(&ev(2, "assistant/attempt", 1, json!({"turn": 3, "step": 1, "stream": [{"usage": {"outputTokens": 4}}]})), 0)
        .expect("attempt usage");
    assert_eq!(sample.model.as_deref(), Some("m2"));
    assert_eq!(sample.provider.as_deref(), Some("opencode-go"));
    assert_eq!(sample.usage["output"].as_f64(), Some(4.0));
    assert_eq!(sample.seq, 2);
}

#[test]
fn retry_marks_only_the_step_it_belongs_to() {
    let mut fold = UsageFold::new(None, None);
    assert!(fold.observe(&ev(1, "llm/retry-started", 0, json!({"turn": 1, "step": 1, "retry": 1})), 0).is_none());
    let retried = fold.observe(&ev(2, "assistant/message", 0, json!({"turn": 1, "step": 1, "usage": {"output": 2}})), 0).unwrap();
    let first_try = fold.observe(&ev(3, "assistant/message", 0, json!({"turn": 1, "step": 2, "usage": {"output": 2}})), 0).unwrap();
    assert_eq!(retried.retry, 1);
    assert_eq!(first_try.retry, 0);
}

#[test]
fn zero_usage_null_usage_and_non_usage_events_produce_no_sample() {
    let mut fold = UsageFold::new(None, None);
    assert!(fold.observe(&ev(1, "assistant/message", 0, json!({"turn": 1, "step": 1, "usage": {"cacheMiss": 0}})), 0).is_none());
    // data.usage 存在（即使是 null）就不回落到 stream：与 JS usageOfEvent 的 `!== undefined` 判断一致
    assert!(fold
        .observe(&ev(2, "assistant/message", 0, json!({"turn": 1, "step": 1, "usage": null, "stream": [{"usage": {"output": 9}}]})), 0)
        .is_none());
    assert!(fold.observe(&ev(3, "tool/call", 0, json!({"turn": 1})), 0).is_none());
}

#[test]
fn snapshot_harvest_carries_history_samples_selection_and_the_token_usage_base() {
    let snapshot = json!({
        "projections": {
            "asOfSeq": 40,
            "values": {
                "tokenUsage": {"cacheMiss": 9, "cacheHit": 1, "output": 7},
                "modelSelection": {"lastUsed": {"model": "m-sel", "provider": "p-sel"}}
            }
        },
        "records": [
            {"type": "event", "event": ev(30, "assistant/message", 500, json!({"turn": 8, "step": 1, "usage": {"cacheMiss": 90}}))},
            {"type": "event", "event": ev(31, "turn/end", 501, json!({"turn": 8}))}
        ]
    });
    let harvest = harvest_of_snapshot(&snapshot, 999);
    assert_eq!(harvest.samples.len(), 1);
    assert_eq!(harvest.samples[0].seq, 30);
    assert_eq!(harvest.samples[0].turn, 8);
    assert_eq!(harvest.samples[0].model.as_deref(), Some("m-sel"));
    let base = harvest.base.expect("tokenUsage projection is a base");
    assert_eq!(base.as_of_seq, 40);
    assert_eq!(base.usage["cacheMiss"].as_f64(), Some(9.0));
    assert_eq!(base.usage["output"].as_f64(), Some(7.0));
    assert_eq!(base.model.as_deref(), Some("m-sel"));
    assert_eq!(base.provider.as_deref(), Some("p-sel"));
}

#[test]
fn snapshot_without_token_usage_projection_has_no_base() {
    let harvest = harvest_of_snapshot(&json!({"projections": {"asOfSeq": 3, "values": {}}, "records": []}), 0);
    assert!(harvest.base.is_none());
    assert!(harvest.samples.is_empty());
}
