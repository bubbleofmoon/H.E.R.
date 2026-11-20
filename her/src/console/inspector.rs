//! Read-only console view over the turn ledger and committed harness events.
use super::{authorized_in, json_result, Request};
use crate::harness::store::inspect_events;
use crate::runtime::RuntimeContext;
use crate::turn_ledger::{self, Turn};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub(super) const PAGE_PATH: &str = "/inspector";
const CONVERSATIONS: &str = "/api/inspector/conversations";
const TURN: &str = "/api/inspector/turn";
const EVENT_BATCH: usize = 50;

pub(crate) fn is_route(path: &str) -> bool {
    matches!(path, PAGE_PATH | CONVERSATIONS | TURN)
}

pub(super) fn route(context: &RuntimeContext, cfg: &Value, req: &Request) -> (u16, String, String) {
    if !authorized_in(context, req, cfg) {
        return json_result(Err((401, "console authorization required".into())));
    }
    if req.headers.contains_key("x-agent-token") || req.headers.contains_key("x-agent-call") {
        return json_result(Err((
            403,
            "the session inspector is restricted to administrators".into(),
        )));
    }
    if req.method != "GET" {
        return json_result(Err((405, "method not allowed".into())));
    }
    if !context.is_isolated() {
        return json_result(Err((
            503,
            "the session inspector requires a selected runtime".into(),
        )));
    }
    match req.path.as_str() {
        PAGE_PATH => (
            200,
            "text/html; charset=utf-8".into(),
            include_str!("inspector.html").into(),
        ),
        CONVERSATIONS => json_result(conversations(context, req)),
        TURN => json_result(timeline(context, cfg, req)),
        _ => json_result(Err((404, "inspector route not found".into()))),
    }
}

fn read_turns(context: &RuntimeContext, turn_id: Option<&str>) -> Result<Vec<Turn>, (u16, String)> {
    let mut records = turn_ledger::read_records_waiting(context)
        .map_err(|_| (503, "turn ledger is unavailable".to_string()))?;
    if let Some(id) = turn_id {
        records.retain(|record| record["turn"] == id);
    }
    Ok(turn_ledger::fold(&records).0)
}

fn summary(turn: &Turn) -> Value {
    json!({
        "turnId": turn.id,
        "code": turn.code(),
        "conversation": turn.key,
        "engine": turn.backend,
        "state": turn.state.label(),
        "beganAt": turn.begun_at,
        "endedAt": turn.ended_at,
        "outcomeReason": turn.outcome_reason,
        "pauseReason": turn.pause_reason,
    })
}

fn conversations(context: &RuntimeContext, req: &Request) -> Result<Value, (u16, String)> {
    let turns = read_turns(context, None)?;
    let keys: BTreeSet<_> = turns.iter().map(|turn| turn.key.as_str()).collect();
    let selected = req.query.get("conversation").map(String::as_str);
    let rows: Vec<_> = turns
        .iter()
        .rev()
        .filter(|turn| selected == Some(turn.key.as_str()))
        .map(summary)
        .collect();
    Ok(json!({"ok": true, "conversations": keys, "turns": rows}))
}

fn timeline(context: &RuntimeContext, cfg: &Value, req: &Request) -> Result<Value, (u16, String)> {
    let id = req
        .query
        .get("turnId")
        .filter(|id| !id.is_empty())
        .ok_or((400, "turnId is required".to_string()))?;
    let after = req.query.get("after").map_or(Ok(0), |after| {
        after.parse::<i64>().ok().filter(|seq| *seq >= 0).ok_or((
            400,
            "after must be a nonnegative event sequence".to_string(),
        ))
    })?;

    // Read the ledger first so a terminal outcome cannot hide a harness event committed before it.
    let turn = read_turns(context, Some(id))?
        .into_iter()
        .next()
        .ok_or((404, "turn not found".to_string()))?;
    let mut events = if turn.backend == "harness" {
        let price_table = crate::pricing::resolve(cfg.get("pricing"));
        inspect_events(
            context,
            &turn.id,
            &turn.key,
            after,
            EVENT_BATCH + 1,
            &price_table,
        )
        .map_err(|error| (503, error))?
    } else {
        Vec::new()
    };
    let more = events.len() > EVENT_BATCH;
    events.truncate(EVENT_BATCH);
    let next_after = events
        .last()
        .and_then(|event| event["seq"].as_i64())
        .unwrap_or(after);
    let ledger = (turn.backend == "dsh").then(|| {
        let view = turn.to_json();
        json!({"attempts": view["attempts"], "tools": view["tools"]})
    });
    Ok(json!({
        "ok": true,
        "turn": summary(&turn),
        "ledger": ledger,
        "events": events,
        "nextAfter": next_after,
        "more": more,
        "live": turn.state.is_open(),
    }))
}

pub(super) const LINK: &str = r#"<nav aria-label="HER"><a id="her-inspector-link" href="/inspector">会话检查器</a></nav>
<script>
(() => {
  const token = new URLSearchParams(location.search).get('token') || sessionStorage.getItem('consoleToken');
  if (token) document.getElementById('her-inspector-link').href = '/inspector?' + new URLSearchParams({token});
})();
</script>"#;

#[cfg(test)]
#[path = "inspector_tests.rs"]
mod tests;
