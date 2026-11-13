//! Durable observed-message, compaction and last-call facts, scoped by the session map and epoch.
use crate::runtime::RuntimeContext;
use serde_json::{json, Value};

pub(crate) fn observe(
    context: &RuntimeContext,
    key: &str,
    sid: &str,
    epoch: &str,
    event: &Value,
) -> Result<(), String> {
    let kind = event["type"].as_str().unwrap_or("");
    if !matches!(
        kind,
        "user/message"
            | "compaction/start"
            | "compaction/summary"
            | "compaction/end"
            | "turn/start"
            | "turn/end"
            | "assistant/message"
            | "assistant/attempt"
            | "model/selection"
    ) {
        return Ok(());
    }
    let seq = event["seq"].as_i64().unwrap_or(0);
    let data = &event["data"];
    let usage = crate::dsh::usage_of_event(event);
    crate::session_map::update_context(context, key, sid, epoch, |r| {
        if seq <= r["observedSeq"].as_i64().unwrap_or(0) {
            return;
        }
        r["observedSeq"] = json!(seq);
        let source = data
            .get("source")
            .or_else(|| data.pointer("/message/source"))
            .unwrap_or(&Value::Null);
        match kind {
            "user/message" => {
                if source["rpcId"].is_string()
                    && r["pending"]["rpcId"] == source["rpcId"]
                    && r["pending"]["generation"] == r["generation"]
                {
                    r["installed"] = r["pending"].take();
                    r["needsBriefing"] = json!(false);
                    r["installed"]["seq"] = json!(seq);
                }
                if r["summary"]["seq"].as_i64() == Some(seq - 1)
                    && ((event.pointer("/surfaceOp/op").and_then(Value::as_str) == Some("replace")
                        && event["sourceEventSeqs"]
                            .as_array()
                            .is_some_and(|seqs| seqs.contains(&r["summary"]["seq"])))
                        || (data["surfaceOp"] == "replace"
                            && source["kind"] == "compact-checkpoint"))
                {
                    let id = r["summary"]["id"].clone();
                    invalidate(r, &id, seq);
                }
            }
            "compaction/start" => {
                r["compacting"] = data["compactionId"].clone();
            }
            "compaction/summary" => {
                r["summary"] = json!({"id":data["compactionId"],"seq":seq});
            }
            "compaction/end" => {
                let id = &data["compactionId"];
                if data.get("error").is_none_or(Value::is_null) && id.is_string() {
                    invalidate(r, id, seq);
                }
                r["compacting"] = Value::Null;
            }
            "model/selection" => {
                r["model"] = data["model"].clone();
                r["provider"] = data["provider"].clone();
            }
            "assistant/message" | "assistant/attempt"
                if usage.is_some() && r["rotation"]["phase"] != "pending" =>
            {
                let usage = usage.as_ref().unwrap();
                let b = crate::pricing::normalize_usage(Some(usage));
                let model = usage
                    .get("model")
                    .or_else(|| data.get("model"))
                    .unwrap_or(&r["model"])
                    .clone();
                let provider = data.get("provider").unwrap_or(&r["provider"]).clone();
                r["lastCall"] = json!({"seq":seq,"input":b.cache_miss+b.cache_hit+b.cache_write,"model":model,"provider":provider});
            }
            "turn/start" if r["rotation"]["phase"] != "pending" => {
                r["turnStart"] = event["time"].clone();
                r["turnOpen"] = json!(true);
                r["closedAtEnd"] = json!(false);
            }
            "turn/end" if r["rotation"]["phase"] != "pending" => {
                r["turnOpen"] = json!(false);
                if data.pointer("/reason/kind").and_then(Value::as_str) == Some("completed") {
                    let closed = context.read_social_state().ok().is_some_and(|state| {
                        let wc = &state["conversations"][key]["wakeConfig"];
                        wc["confirmedAt"].as_i64().unwrap_or(0)
                            >= r["turnStart"].as_i64().unwrap_or(i64::MAX)
                            && matches!(
                                wc["confirmedBy"].as_str(),
                                Some("mark_read" | "set_wake_config")
                            )
                    });
                    r["closedAtEnd"] = json!(closed);
                    r["lastEnded"] = event["time"].clone();
                    r["lastTurnSeq"] = json!(seq);
                }
            }
            _ => {}
        }
    })?;
    Ok(())
}

fn invalidate(r: &mut Value, id: &Value, seq: i64) {
    if !id.is_string() || r["compactionId"] == *id {
        return;
    }
    r["compactionId"] = id.clone();
    r["compactionSeq"] = json!(seq);
    r["generation"] = json!(r["generation"].as_u64().unwrap_or(0) + 1);
    r["pending"] = Value::Null;
}
