//! Read-only console view over the turn ledger and committed harness events.
use super::{front::AdminCaller, json_result, Request};
use crate::harness::store::inspect_events;
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use crate::turn_ledger::{self, Turn};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub(super) const PAGE_PATH: &str = "/inspector";
const CONVERSATIONS: &str = "/api/inspector/conversations";
const TURN: &str = "/api/inspector/turn";
const DEFAULT_TURN_LIMIT: usize = 50;
const MAX_TURN_LIMIT: usize = 100;
const MAX_CONVERSATIONS: usize = 100;

struct LedgerSnapshot {
    turns: Vec<Turn>,
    by_id: HashMap<String, usize>,
    by_key: HashMap<String, Vec<usize>>,
    keys: Vec<String>,
}

struct CachedLedger {
    path: PathBuf,
    stamp: Option<(u64, SystemTime)>,
    snapshot: Arc<LedgerSnapshot>,
}

// Keep one snapshot for the selected console runtime. Never hold this lock during ledger I/O
// or while acquiring any other lock.
static LEDGER: Mutex<Option<CachedLedger>> = Mutex::new(None);

pub(crate) fn is_route(path: &str) -> bool {
    matches!(path, PAGE_PATH | CONVERSATIONS | TURN)
}

pub(super) fn route(caller: AdminCaller<'_>, cfg: &Value, req: &Request) -> (u16, String, String) {
    let context = caller.context();
    if req.method != "GET" {
        return json_result(Err((405, "method not allowed".into())));
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

fn ledger_stamp(path: &Path) -> std::io::Result<Option<(u64, SystemTime)>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !crate::guarded_file::is_link(&metadata) => {
            Ok(Some((metadata.len(), metadata.modified()?)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid turn ledger path",
        )),
    }
}

fn read_turns(context: &RuntimeContext) -> Result<Arc<LedgerSnapshot>, (u16, String)> {
    let unavailable = || (503, "turn ledger is unavailable".to_string());
    let path = context
        .resolve_path(
            RuntimeArea::Data,
            turn_ledger::ISOLATED_LEDGER_FILE,
            PathIntent::ReadOptionalFile,
        )
        .map_err(|_| unavailable())?;
    // Stamp before the read so an append during the read invalidates this snapshot on the next poll.
    let stamp = ledger_stamp(&path).map_err(|_| unavailable())?;
    if let Some(cached) = LEDGER
        .lock()
        .map_err(|_| unavailable())?
        .as_ref()
        .filter(|cached| cached.path == path && cached.stamp == stamp)
    {
        return Ok(Arc::clone(&cached.snapshot));
    }
    let records = turn_ledger::read_records_waiting(context).map_err(|_| unavailable())?;
    let turns = turn_ledger::fold(&records).0;
    let mut by_id = HashMap::new();
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    let mut keys = Vec::new();
    for (index, turn) in turns.iter().enumerate().rev() {
        let indices = by_key.entry(turn.key.clone()).or_default();
        if indices.is_empty() && keys.len() < MAX_CONVERSATIONS {
            keys.push(turn.key.clone());
        }
        indices.push(index);
        by_id.insert(turn.id.clone(), index);
    }
    keys.sort_unstable();
    let snapshot = Arc::new(LedgerSnapshot {
        turns,
        by_id,
        by_key,
        keys,
    });
    *LEDGER.lock().map_err(|_| unavailable())? = Some(CachedLedger {
        path,
        stamp,
        snapshot: Arc::clone(&snapshot),
    });
    Ok(snapshot)
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
    let limit = req
        .query
        .get("limit")
        .map_or(Ok(DEFAULT_TURN_LIMIT), |limit| {
            limit
                .parse::<u64>()
                .ok()
                .filter(|limit| *limit > 0)
                .map(|limit| limit.min(MAX_TURN_LIMIT as u64) as usize)
                .ok_or_else(|| (400, "limit must be a positive integer".to_string()))
        })?;
    let snapshot = read_turns(context)?;
    let indices = req
        .query
        .get("conversation")
        .and_then(|key| snapshot.by_key.get(key))
        .map_or(&[][..], Vec::as_slice);
    let start = if let Some(before) = req.query.get("before") {
        let anchor = snapshot
            .by_id
            .get(before)
            .and_then(|index| indices.binary_search_by(|other| index.cmp(other)).ok())
            .ok_or_else(|| {
                (
                    400,
                    "before must identify a turn in this conversation".to_string(),
                )
            })?;
        anchor + 1
    } else {
        0
    };
    let remaining = &indices[start..];
    let rows: Vec<_> = remaining
        .iter()
        .take(limit)
        .map(|&index| summary(&snapshot.turns[index]))
        .collect();
    let next_before = (remaining.len() > limit).then(|| &snapshot.turns[remaining[limit - 1]].id);
    Ok(json!({
        "ok": true,
        "conversations": snapshot.keys,
        "turns": rows,
        "nextBefore": next_before,
    }))
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
    let snapshot = read_turns(context)?;
    let turn = snapshot
        .by_id
        .get(id)
        .map(|&index| &snapshot.turns[index])
        .ok_or((404, "turn not found".to_string()))?;
    let (events, more) = if turn.backend == "harness" {
        let price_table = crate::pricing::resolve(cfg.get("pricing"));
        inspect_events(context, &turn.id, &turn.key, after, &price_table)
            .map_err(|error| (503, error))?
    } else {
        (Vec::new(), false)
    };
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
        "turn": summary(turn),
        "ledger": ledger,
        "events": events,
        "nextAfter": next_after,
        "more": more,
        "live": turn.state.is_open(),
    }))
}

#[cfg(test)]
#[path = "inspector_tests.rs"]
mod tests;
