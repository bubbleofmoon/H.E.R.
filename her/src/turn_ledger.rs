//! Durable turn ledger (ticket 10).
//!
//! One append-only JSONL file correlates the four things a restart must reconcile:
//! the inbound events a turn serves (`inbound`), the DSH prompt it submitted (`attempt` with the
//! JSON-RPC id, which DSH stores as `source.rpcId` of the durable user message), the tool calls
//! DSH ran for it (`tools`), and, through the agent-token epoch, the send batches of
//! `send_outbox.rs`. Records never hold tokens, only their SHA-256 epoch.
//!
//! Write rule: `begun` is appended before any DSH request of the turn and `attempt` is appended
//! before the `session/prompt` call. A turn with no `attempt` therefore provably never reached
//! DSH; a turn with an `attempt` but no proof either way is `unknown` and is verified against DSH
//! history, never blindly resubmitted.
//!
//! Folding the records gives each turn one state; later state-changing records override earlier
//! ones, so a paused turn that is later verified simply ends.

use serde_json::{json, Value};

use crate::runtime::{RuntimeArea, RuntimeContext};

/// Rust-owned legacy ledger (same pattern as the send outbox).
pub const LEGACY_LEDGER_FILE: &str = "C:/qqbot/state/turn-ledger-rust.jsonl";
/// Relative name inside the selected runtime data area.
pub const ISOLATED_LEDGER_FILE: &str = "turn-ledger.jsonl";

pub fn now_ms() -> i64 {
    crate::parity_clock::now_ms() as i64
}

/// Read every ledger record in append order. A truncated record (crash mid-append) is skipped with a
/// warning; any other damaged line is an error: the ledger is evidence (see `runtime::parse_evidence_jsonl`).
pub fn read_records(context: &RuntimeContext) -> Result<Vec<Value>, String> {
    // Fixture seam for this one-shot full read (its own stage: the tail read's `turn-read` counters
    // and conflict policy belong to the turn worker). Production never calls the hook.
    #[cfg(test)]
    crate::runtime::append_io_test_hook("turn-read-all")
        .map_err(|error| format!("读取回合账本失败：{error}"))?;
    let text = if context.is_isolated() {
        context
            .read_optional_text(RuntimeArea::Data, ISOLATED_LEDGER_FILE)
            .map_err(str::to_string)?
            .unwrap_or_default()
    } else {
        match std::fs::read_to_string(LEGACY_LEDGER_FILE) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("读取回合账本失败：{error}")),
        }
    };
    crate::runtime::parse_evidence_jsonl(&text, ISOLATED_LEDGER_FILE, "回合账本行损坏")
}

/// The ledger's length in bytes (0 before it exists). A turn captures it before writing `begun`,
/// so its end can fold only the records appended since then (`fold_turn_since`).
pub fn ledger_len(context: &RuntimeContext) -> Result<u64, String> {
    if context.is_isolated() {
        return context.optional_file_len(RuntimeArea::Data, ISOLATED_LEDGER_FILE).map_err(str::to_string);
    }
    match std::fs::metadata(LEGACY_LEDGER_FILE) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(format!("读取回合账本失败：{error}")),
    }
}

fn read_legacy_tail(from: u64) -> std::io::Result<Option<(u64, Vec<u8>)>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(LEGACY_LEDGER_FILE) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let start = if len < from { 0 } else { from };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some((start, bytes)))
}

/// The records appended at or after byte `offset`, parsed like `read_records`. The cost follows
/// the records since `offset`, not the ledger's history. An offset inside a line (taken while
/// another writer appended) skips to the next whole line; a ledger now shorter than `offset`
/// (replaced) is read from the start.
pub fn read_records_since(context: &RuntimeContext, offset: u64) -> Result<Vec<Value>, String> {
    read_records_since_until(context, offset, None)
}

/// As `read_records_since`; a worker's terminal decision passes its conflict deadline so Windows
/// sharing/lock errors (32/33) on the tail read retry like its append does.
fn read_records_since_until(
    context: &RuntimeContext,
    offset: u64,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<Value>, String> {
    // One byte before the offset tells whether it falls on a line start.
    let from = offset.saturating_sub(1);
    let tail = crate::runtime::append_io(deadline, || {
        #[cfg(test)]
        crate::runtime::append_io_test_hook("turn-read")?;
        if context.is_isolated() {
            context.read_optional_tail_io(RuntimeArea::Data, ISOLATED_LEDGER_FILE, from)
        } else {
            read_legacy_tail(from)
        }
    })
    .map_err(|error| crate::runtime::runtime_io_error("turn/read", &error))?;
    let Some((start, bytes)) = tail else { return Ok(Vec::new()) };
    let body = if offset > 0 && start == from {
        match bytes.iter().position(|b| *b == b'\n') {
            Some(newline) => &bytes[newline + 1..],
            None => &[][..],
        }
    } else {
        &bytes[..]
    };
    let text = std::str::from_utf8(body).map_err(|_| "回合账本行损坏：invalid UTF-8".to_string())?;
    crate::runtime::parse_evidence_jsonl(text, ISOLATED_LEDGER_FILE, "回合账本行损坏")
}

/// Fold one turn from the records appended since `offset` (captured before its `begun`). The
/// result equals that turn in the whole-ledger `fold`; `None` when its `begun` is not in the tail.
pub fn fold_turn_since(context: &RuntimeContext, offset: u64, id: &str) -> Result<Option<Turn>, String> {
    fold_turn_since_until(context, offset, id, None)
}

fn fold_turn_since_until(
    context: &RuntimeContext,
    offset: u64,
    id: &str,
    deadline: Option<std::time::Instant>,
) -> Result<Option<Turn>, String> {
    let records: Vec<Value> = read_records_since_until(context, offset, deadline)?
        .into_iter()
        .filter(|record| record.get("turn").and_then(Value::as_str) == Some(id))
        .collect();
    Ok(fold(&records).0.into_iter().find(|turn| turn.id == id))
}

/// Append one record. A failure means the caller must not perform the action it guards.
pub fn append_record(context: &RuntimeContext, record: &Value) -> Result<(), String> {
    append_record_inner(context, record, true)
}

/// Administrative restore runs under stopped-writer gates and must not wait on sharing retries.
pub(crate) fn append_record_without_retry(context: &RuntimeContext, record: &Value) -> Result<(), String> {
    append_record_inner(context, record, false)
}

fn append_record_inner(context: &RuntimeContext, record: &Value, retry: bool) -> Result<(), String> {
    append_record_until(context, record, retry.then(|| std::time::Instant::now() + crate::runtime::APPEND_CONFLICT_WAIT))
}

fn append_record_until(context: &RuntimeContext, record: &Value, deadline: Option<std::time::Instant>) -> Result<(), String> {
    let durable = matches!(record.get("k").and_then(Value::as_str), Some("ended" | "pause_notified" | "resolution_notified"));
    let durable = durable || (record["k"] == "verified" && matches!(record["verdict"].as_str(), Some("completion-observed" | "completion-rejected" | "cancel-requested" | "cancellation-ready")));
    if context.is_isolated() {
        let mut line = serde_json::to_vec(record).map_err(|e| format!("序列化回合账本失败：{e}"))?;
        line.push(b'\n');
        context.append_turn_ledger_bytes(&line, durable, deadline)
    } else {
        crate::ledger::append_turn_record_to(LEGACY_LEDGER_FILE, record, durable, deadline)
    }
}

/// Record that the administrator reset a conversation. Work queued before the reset must not run
/// in the replacement session (JS clears its prompt queue); `fold` ignores this record kind.
pub fn record_reset(context: &RuntimeContext, key: &str) -> Result<(), String> {
    append_record_inner(context, &serde_json::json!({"k": "reset", "t": now_ms(), "key": key}), false)
}

/// The same record for an administrator change that revoked `key` (`why` names it). The key `*`
/// stands for every conversation (the second-generation whole reset drains every prompt queue).
pub fn record_reset_why(context: &RuntimeContext, key: &str, why: &str) -> Result<(), String> {
    append_record_inner(context, &serde_json::json!({"k": "reset", "t": now_ms(), "key": key, "why": why}), false)
}

/// An access-policy narrowing that may concern conversations no list names (allow-all): it ends
/// the epoch of every conversation `policy` does not admit in `mode`.
pub fn record_policy_reset(context: &RuntimeContext, policy: &Value, mode: &str, why: &str) -> Result<(), String> {
    append_record_inner(
        context,
        &serde_json::json!({"k": "reset", "t": now_ms(), "key": POLICY_RESET, "why": why, "policy": policy, "mode": mode}),
        false,
    )
}

const POLICY_RESET: &str = "*policy";

/// The record ends `key`'s epoch: a reset of `key`, of every conversation (`*`), or a policy
/// narrowing that no longer admits `key`.
pub fn is_reset_of(record: &Value, key: &str) -> bool {
    if record.get("k").and_then(Value::as_str) != Some("reset") {
        return false;
    }
    match record.get("key").and_then(Value::as_str) {
        Some(k) if k == key || k == "*" => true,
        Some(POLICY_RESET) => {
            let mode = record.get("mode").and_then(Value::as_str).unwrap_or("");
            !crate::console::access_allowed(record.get("policy").unwrap_or(&Value::Null), mode, key)
        }
        _ => false,
    }
}

/// How many resets of `key` (or of every conversation) the ledger holds; a queued job compares
/// the count it saw at dispatch.
pub fn reset_count(context: &RuntimeContext, key: &str) -> Result<usize, String> {
    Ok(read_records(context)?.iter().filter(|r| is_reset_of(r, key)).count())
}

/// A fresh identifier for a turn or a JSON-RPC call (UUID-shaped, unique per process and time).
pub fn mint_id(prefix: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = Vec::new();
    for _ in 0..2 {
        // RandomState is seeded by the operating system on every call.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        bytes.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes.extend_from_slice(&(now_ms() as u64).to_le_bytes());
    bytes.extend_from_slice(&std::process::id().to_le_bytes());
    let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{}", &hex[..24])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Begun; no DSH request was ever written for it.
    Begun,
    /// `session/prompt` was about to be (or was) sent; acceptance not proven.
    Attempted,
    /// DSH accepted the prompt; no end seen.
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Stopped by the bridge (timeout, replaced session) so that late results are refused.
    Abandoned,
    /// Provably not executed; its inbound events were handed back for a fresh turn.
    Resumed,
    /// State cannot be proven: held for the administrator.
    Paused,
    /// The administrator decided (`abandon` or `retry`).
    Resolved,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Begun => "begun",
            State::Attempted => "attempted",
            State::Running => "running",
            State::Completed => "completed",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
            State::Abandoned => "abandoned",
            State::Resumed => "resumed",
            State::Paused => "paused",
            State::Resolved => "resolved",
        }
    }

    /// Not finished and not waiting for an administrator.
    pub fn is_open(self) -> bool {
        matches!(self, State::Begun | State::Attempted | State::Running)
    }

    /// Nothing more can happen to this turn; later records about it are ignored.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            State::Completed | State::Failed | State::Cancelled | State::Abandoned | State::Resumed | State::Resolved
        )
    }
}

#[derive(Debug, Clone)]
pub struct Attempt {
    pub rpc_id: String,
    pub session_id: String,
    pub at: i64,
    pub accepted: bool,
    pub dsh_turn: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub id: String,
    pub key: String,
    pub backend: String,
    pub reason: String,
    pub wake_claim: Option<Value>,
    pub epoch: String,
    /// Every agent-token epoch the turn ran under (it changes when an attempt is abandoned).
    pub epochs: Vec<String>,
    pub session_id: String,
    pub inbound: Vec<String>,
    pub begun_at: i64,
    pub resumed_from: Option<String>,
    pub attempts: Vec<Attempt>,
    pub tools: Vec<Value>,
    pub state: State,
    pub outcome_reason: String,
    pub pause_reason: String,
    pub pause_detail: String,
    pub paused_at: Option<i64>,
    pub last_pause_reason: String,
    pub pause_notified: bool,
    pub resolution_notified: bool,
    /// New pause/resolution records or an already-claimed alert require notification.
    pub notification_intent: bool,
    pub resolution: Option<String>,
    pub resolution_by: String,
    /// The recovery task already acted on the administrator's decision.
    pub resolution_applied: bool,
    pub ended_at: Option<i64>,
    /// A recovery task is waiting for DSH to finish this turn.
    pub reattaching: bool,
    pub notes: Vec<Value>,
}

impl Turn {
    /// First unreleased claim. Scoped completion protects only its own attempt until accepted.
    fn finalization_note(&self) -> Option<&Value> {
        self.notes.iter().find(|note| match note["verdict"].as_str() {
            Some("cancel-requested") => true,
            Some("completion-observed") => match note["rpcId"].as_str() {
                None => true, // Older ledgers contain turn-wide completion claims.
                Some(rpc) => self.last_rpc_id() == Some(rpc) && !self.notes.iter().any(|n|
                    n["verdict"] == "completion-rejected" && n["rpcId"] == rpc),
            },
            _ => false,
        })
    }

    pub fn finalization_claim(&self) -> Option<(&str, &str)> {
        self.finalization_note().map(|n| (n["verdict"].as_str().unwrap(), n["detail"].as_str().unwrap_or("")))
    }

    /// Stable across restarts, including turns written by older binaries.
    pub fn code(&self) -> String {
        let hash = <sha2::Sha256 as sha2::Digest>::digest(self.id.as_bytes());
        hash[..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn last_rpc_id(&self) -> Option<&str> {
        self.attempts.last().map(|a| a.rpc_id.as_str())
    }

    pub fn to_json(&self) -> Value {
        json!({
            "turnId": self.id,
            "code": self.code(),
            "key": self.key,
            "backend": self.backend,
            "reason": self.reason,
            "state": self.state.label(),
            "sessionId": self.session_id,
            "epoch": self.epoch,
            "inbound": self.inbound,
            "beganAt": self.begun_at,
            "endedAt": self.ended_at,
            "resumedFrom": self.resumed_from,
            "attempts": self.attempts.iter().map(|a| json!({
                "rpcId": a.rpc_id,
                "sessionId": a.session_id,
                "at": a.at,
                "accepted": a.accepted,
                "dshTurn": a.dsh_turn,
            })).collect::<Vec<_>>(),
            "tools": self.tools,
            "outcomeReason": self.outcome_reason,
            "pauseReason": self.pause_reason,
            "pauseDetail": self.pause_detail,
            "pausedAt": self.paused_at,
            "resolution": self.resolution,
            "resolutionBy": self.resolution_by,
            "reattaching": self.reattaching,
            "notes": self.notes,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Replacement {
    pub key: String,
    pub old_session: String,
    pub new_session: String,
    pub reason: String,
    pub at: i64,
}

fn text(record: &Value, field: &str) -> String {
    record.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn outcome_state(outcome: &str) -> State {
    match outcome {
        "completed" => State::Completed,
        "cancelled" => State::Cancelled,
        "abandoned" => State::Abandoned,
        "resumed" => State::Resumed,
        _ => State::Failed,
    }
}

/// Fold the whole ledger into per-turn state (in begin order) and session replacements.
pub fn fold(records: &[Value]) -> (Vec<Turn>, Vec<Replacement>) {
    let mut turns: Vec<Turn> = Vec::new();
    let mut replacements = Vec::new();
    for record in records {
        let kind = record.get("k").and_then(Value::as_str).unwrap_or_default();
        let at = record.get("t").and_then(Value::as_i64).unwrap_or(0);
        if kind == "replaced" {
            replacements.push(Replacement {
                key: text(record, "key"),
                old_session: text(record, "oldSession"),
                new_session: text(record, "newSession"),
                reason: text(record, "reason"),
                at,
            });
            continue;
        }
        let id = text(record, "turn");
        if kind == "begun" {
            turns.push(Turn {
                id,
                key: text(record, "key"),
                backend: record.get("backend").and_then(Value::as_str).unwrap_or("dsh").to_string(),
                reason: text(record, "reason"),
                wake_claim: None,
                epoch: text(record, "epoch"),
                epochs: vec![text(record, "epoch")],
                session_id: text(record, "sessionId"),
                inbound: record
                    .get("inbound")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
                begun_at: at,
                resumed_from: record.get("resumedFrom").and_then(Value::as_str).map(str::to_string),
                attempts: Vec::new(),
                tools: Vec::new(),
                state: State::Begun,
                outcome_reason: String::new(),
                pause_reason: String::new(),
                pause_detail: String::new(),
                paused_at: None,
                last_pause_reason: String::new(),
                pause_notified: false,
                resolution_notified: false,
                notification_intent: false,
                resolution: None,
                resolution_by: String::new(),
                resolution_applied: false,
                ended_at: None,
                reattaching: false,
                notes: Vec::new(),
            });
            continue;
        }
        let Some(turn) = turns.iter_mut().rev().find(|t| t.id == id) else { continue };
        match kind {
            "wake_claim" => turn.wake_claim = record.get("claim").cloned(),
            "session" => turn.session_id = text(record, "sessionId"),
            "epoch" => {
                turn.epoch = text(record, "epoch");
                turn.epochs.push(turn.epoch.clone());
            }
            "attempt" => {
                turn.attempts.push(Attempt {
                    rpc_id: text(record, "rpcId"),
                    session_id: text(record, "sessionId"),
                    at,
                    accepted: false,
                    dsh_turn: None,
                });
                if !text(record, "sessionId").is_empty() {
                    turn.session_id = text(record, "sessionId");
                }
                if turn.state.is_open() {
                    turn.state = State::Attempted;
                }
            }
            "accepted" => {
                let rpc = text(record, "rpcId");
                if let Some(attempt) = turn.attempts.iter_mut().rev().find(|a| a.rpc_id == rpc) {
                    attempt.accepted = true;
                }
                if turn.state.is_open() || turn.state == State::Paused {
                    turn.state = State::Running;
                }
            }
            "dsh_turn" => {
                let rpc = text(record, "rpcId");
                if let Some(attempt) = turn.attempts.iter_mut().rev().find(|a| a.rpc_id == rpc) {
                    attempt.dsh_turn = record.get("dshTurn").and_then(Value::as_i64);
                }
            }
            "tools" => {
                turn.tools = record.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
            }
            "reattach" => turn.reattaching = true,
            "applied" => turn.resolution_applied = true,
            "pause_notified" => {
                turn.pause_notified = true;
                turn.notification_intent = true;
            }
            "resolution_notified" => turn.resolution_notified = true,
            "ended" if turn.state.is_final() => {}
            "unknown" | "resolved" if turn.state.is_final() => {}
            "ended" => {
                turn.state = outcome_state(record.get("outcome").and_then(Value::as_str).unwrap_or("failed"));
                turn.outcome_reason = text(record, "reason");
                turn.ended_at = Some(at);
                turn.reattaching = false;
                turn.pause_reason.clear();
                turn.pause_detail.clear();
            }
            "unknown" => {
                turn.state = State::Paused;
                turn.paused_at.get_or_insert(at);
                turn.notification_intent |= record.get("notify") == Some(&json!(true));
                turn.pause_reason = text(record, "reason");
                turn.last_pause_reason = turn.pause_reason.clone();
                turn.pause_detail = text(record, "detail");
                turn.reattaching = false;
            }
            "resolved" => {
                turn.state = State::Resolved;
                turn.resolution = Some(text(record, "action"));
                turn.resolution_by = record.get("by").and_then(Value::as_str).unwrap_or("console").to_string();
                turn.notification_intent |= record.get("by").and_then(Value::as_str).is_some();
                turn.ended_at = Some(at);
                turn.reattaching = false;
                turn.pause_reason.clear();
                turn.pause_detail.clear();
            }
            "verified" => {
                let mut note = json!({"at": at, "verdict": text(record, "verdict"), "detail": text(record, "detail")});
                for field in ["rpcId", "outcome"] {
                    if let Some(value) = record.get(field) { note[field] = value.clone(); }
                }
                turn.notes.push(note);
            }
            _ => {}
        }
    }
    (turns, replacements)
}

/// The turn record writer used by the live turn path.
#[derive(Clone)]
pub struct Recorder {
    context: RuntimeContext,
    retry: bool,
    /// Captured before begun so the live finalization check reads only this turn's tail.
    offset: u64,
    pub id: String,
    pub key: String,
}

impl Recorder {
    /// Write `begun`. Fails closed: without a durable record the turn must not start.
    pub fn begin(
        context: &RuntimeContext,
        key: &str,
        backend: &str,
        reason: &str,
        epoch: &str,
        inbound: &[String],
        resumed_from: Option<&str>,
    ) -> Result<Self, String> {
        let offset = ledger_len(context)?;
        let id = mint_id("turn-");
        append_record(
            context,
            &json!({
                "k": "begun", "turn": id, "t": now_ms(), "key": key, "backend": backend,
                "reason": reason, "epoch": epoch, "inbound": inbound, "resumedFrom": resumed_from,
            }),
        )?;
        Ok(Self { context: context.clone(), retry: true, offset, id, key: key.to_string() })
    }

    pub fn attach(context: &RuntimeContext, id: &str, key: &str) -> Self {
        Self { context: context.clone(), retry: true, offset: 0, id: id.to_string(), key: key.to_string() }
    }

    /// Inline scheduler/hub and administrative cancellation must retain base append behavior.
    pub fn without_retry(mut self) -> Self {
        self.retry = false;
        self
    }

    fn put(&self, mut record: Value) -> Result<(), String> {
        record["turn"] = json!(self.id);
        record["t"] = json!(now_ms());
        append_record_inner(&self.context, &record, self.retry)
    }

    pub fn session(&self, session_id: &str) -> Result<(), String> {
        self.put(json!({"k": "session", "sessionId": session_id}))
    }

    pub fn started(&self) -> Result<(),String> { self.put(json!({"k":"started"})) }

    pub fn wake_claim(&self, claim: &Value) -> Result<(), String> {
        self.put(json!({"k":"wake_claim", "claim":claim}))
    }

    pub fn epoch(&self, epoch: &str) -> Result<(), String> {
        self.put(json!({"k": "epoch", "epoch": epoch}))
    }

    /// Must succeed before `session/prompt` is called.
    pub fn attempt(&self, rpc_id: &str, session_id: &str) -> Result<(), String> {
        self.put(json!({"k": "attempt", "rpcId": rpc_id, "sessionId": session_id}))
    }

    pub fn accepted(&self, rpc_id: &str) -> Result<(), String> {
        self.put(json!({"k": "accepted", "rpcId": rpc_id}))
    }

    pub fn dsh_turn(&self, rpc_id: &str, dsh_turn: i64) -> Result<(), String> {
        self.put(json!({"k": "dsh_turn", "rpcId": rpc_id, "dshTurn": dsh_turn}))
    }

    pub fn tools(&self, tools: Vec<Value>) -> Result<(), String> {
        self.put(json!({"k": "tools", "tools": tools}))
    }

    pub fn reattach(&self) -> Result<(), String> {
        self.put(json!({"k": "reattach"}))
    }

    /// Serialize completion observation, cancellation intent and terminal settlement. Never
    /// hold this gate across RPC, usage/tool recording or token publication.
    fn finalization_gate(&self) -> Result<Option<std::fs::File>, String> {
        if !self.context.is_isolated() { return Ok(None); }
        let deadline = std::time::Instant::now() + crate::runtime::APPEND_CONFLICT_WAIT;
        loop {
            match self.context.open_locked_file(RuntimeArea::Data, "locks/turn-ledger-finalize.lock") {
                Ok(gate) => return Ok(Some(gate)),
                Err(error) if self.retry && error == "runtime file is already locked"
                    && std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(5)),
                Err(error) => return Err(error),
            }
        }
    }

    fn conflict_deadline(&self) -> Option<std::time::Instant> {
        self.retry.then(|| std::time::Instant::now() + crate::runtime::APPEND_CONFLICT_WAIT)
    }

    fn put_until(&self, mut record: Value, deadline: Option<std::time::Instant>) -> Result<(), String> {
        record["turn"] = json!(self.id);
        record["t"] = json!(now_ms());
        append_record_until(&self.context, &record, deadline)
    }

    pub fn end(&self, outcome: &str, reason: &str) -> Result<(), String> {
        self.end_inner(outcome, reason, if outcome == "cancelled" { "cancel-requested" } else { "completion-observed" }, None)
    }

    /// Includes clean observation sleep, which is completed but owned by cancellation.
    pub fn end_cancellation(&self, outcome: &str, reason: &str) -> Result<(), String> {
        self.end_inner(outcome, reason, "cancel-requested", None)
    }

    /// Recovery may settle a crashed owner's claim only with its owned outcome. A stale
    /// verdict or terminal race is an error, so its caller cannot replay input after a no-op.
    pub fn end_reconciled(&self, outcome: &str, reason: &str) -> Result<(), String> {
        self.end_inner(outcome, reason, "reconciled", None)
    }

    /// Accept a classified result and commit it as this attempt's terminal outcome atomically.
    pub fn end_attempt(&self, rpc_id: &str, outcome: &str, reason: &str) -> Result<(), String> {
        self.end_inner(outcome, reason, "completion-observed", Some(rpc_id))
    }

    fn end_inner(&self, outcome: &str, reason: &str, owner: &str, rpc_id: Option<&str>) -> Result<(), String> {
        let _gate = self.finalization_gate()?;
        let deadline = self.conflict_deadline();
        if let Some(turn) = fold_turn_since_until(&self.context, self.offset, &self.id, deadline)?.as_ref() {
            if turn.state.is_final() {
                return if owner == "reconciled" { Err("turn already finalized; recovery must re-read".into()) } else { Ok(()) };
            }
            if let Some(rpc_id) = rpc_id {
                if turn.last_rpc_id() != Some(rpc_id) || turn.notes.iter().any(|n|
                    n["verdict"] == "completion-rejected" && n["rpcId"] == rpc_id) {
                    return Err("completion belongs to a rejected or different attempt".into());
                }
            }
            if let Some((claim, detail)) = turn.finalization_claim() {
                let note = turn.finalization_note().unwrap();
                // A cap that lost to a provisional completion has already stopped/revoked.
                // Retain that decision durably in case classification releases the completion.
                if owner == "cancel-requested" && claim == "completion-observed" && note["rpcId"].is_string() {
                    if let Some(intent) = turn.notes.iter().find(|n| n["verdict"] == "cancel-requested") {
                        if intent["detail"] != reason || outcome != if reason == "observationSleep" { "completed" } else { "cancelled" } { return Ok(()); }
                    }
                    if !turn.notes.iter().any(|n| n["verdict"] == "cancellation-ready" && n["rpcId"] == note["rpcId"]) {
                        return self.put_until(json!({"k":"verified", "verdict":"cancellation-ready",
                            "rpcId":note["rpcId"], "outcome":outcome, "detail":reason}), deadline);
                    }
                    return Ok(());
                }
                if owner == "completion-observed" && claim == owner && note["rpcId"].is_string()
                    && note["rpcId"].as_str() != rpc_id {
                    return Err("completion acceptance requires its attempt id".into());
                }
                let compatible = if claim == "completion-observed" { matches!(outcome, "completed" | "failed") }
                    else { outcome == if detail == "observationSleep" { "completed" } else { "cancelled" } && reason == detail };
                if owner == "reconciled" {
                    if !compatible { return Err("finalization claim changed; recovery must re-read".into()); }
                } else if claim != owner || !compatible { return Ok(()); }
            }
        }
        self.put_until(json!({"k": "ended", "outcome": outcome, "reason": reason}), deadline)
    }

    pub fn unknown(&self, reason: &str, detail: &str) -> Result<(), String> {
        self.put(json!({"k": "unknown", "reason": reason, "detail": detail, "notify": true}))
    }

    /// Reserve a genuine completion before collector bookkeeping; classification may release it.
    pub fn observe_completion(&self, rpc_id: &str) -> Result<(), String> {
        self.claim("completion-observed", "DSH completed", Some(rpc_id))
    }

    /// Rejection/failover invalidates only this attempt's claim. A cap whose confirmed-stop
    /// decision was deferred by that claim can now settle without another scheduler tick.
    pub fn release_completion(&self, rpc_id: &str) -> Result<(), String> {
        let _gate = self.finalization_gate()?;
        let deadline = self.conflict_deadline();
        let Some(turn) = fold_turn_since_until(&self.context, self.offset, &self.id, deadline)? else { return Ok(()) };
        if turn.state.is_final() || turn.last_rpc_id() != Some(rpc_id) || !turn.notes.iter().any(|n| n["verdict"] == "completion-observed" && n["rpcId"] == rpc_id) {
            return Ok(());
        }
        if !turn.notes.iter().any(|n| n["verdict"] == "completion-rejected" && n["rpcId"] == rpc_id) {
            self.put_until(json!({"k":"verified", "verdict":"completion-rejected", "rpcId":rpc_id, "detail":"result rejected"}), deadline)?;
        }
        if let Some(ready) = turn.notes.iter().find(|n| n["verdict"] == "cancellation-ready" && n["rpcId"] == rpc_id) {
            self.put_until(json!({"k":"ended", "outcome":ready["outcome"], "reason":ready["detail"]}), deadline)?;
        }
        Ok(())
    }

    pub fn verified(&self, verdict: &str, detail: &str) -> Result<(), String> {
        if !matches!(verdict, "completion-observed" | "cancel-requested") {
            return self.put(json!({"k": "verified", "verdict": verdict, "detail": detail}));
        }
        self.claim(verdict, detail, None)
    }

    fn claim(&self, verdict: &str, detail: &str, rpc_id: Option<&str>) -> Result<(), String> {
        let _gate = self.finalization_gate()?;
        let deadline = self.conflict_deadline();
        if let Some(turn) = fold_turn_since_until(&self.context, self.offset, &self.id, deadline)?.as_ref() {
            if turn.state.is_final() || rpc_id.is_some() && rpc_id != turn.last_rpc_id() { return Ok(()); }
            if let Some(note) = turn.finalization_note() {
                // Preserve cancellation intent behind a provisional completion, but genuine
                // acceptance still wins unless that exact completion is released.
                if verdict != "cancel-requested" || note["verdict"] != "completion-observed" || !note["rpcId"].is_string()
                    || turn.notes.iter().any(|n| n["verdict"] == "cancel-requested") { return Ok(()); }
            }
        }
        let mut record = json!({"k":"verified", "verdict":verdict, "detail":detail});
        if let Some(rpc_id) = rpc_id { record["rpcId"] = json!(rpc_id); }
        self.put_until(record, deadline)
    }

    pub fn applied(&self) -> Result<(), String> {
        self.put(json!({"k": "applied"}))
    }

    pub fn resolved(&self, action: &str) -> Result<(), String> {
        self.resolved_by(action, "console")
    }

    pub fn resolved_by(&self, action: &str, by: &str) -> Result<(), String> {
        self.put(json!({"k": "resolved", "action": action, "by": by}))
    }

    pub fn notified(&self, resolved: bool) -> Result<(), String> {
        self.put(json!({"k": if resolved { "resolution_notified" } else { "pause_notified" }}))
    }
}

/// Record that a conversation's DSH session was replaced.
pub fn record_replacement(
    context: &RuntimeContext,
    key: &str,
    old_session: &str,
    new_session: &str,
    reason: &str,
) -> Result<(), String> {
    append_record(
        context,
        &json!({
            "k": "replaced", "t": now_ms(), "key": key,
            "oldSession": old_session, "newSession": new_session, "reason": reason,
        }),
    )
}

/// Inbound events whose turn was refused because the conversation had a paused or recovering
/// turn (`held`), and that nobody has handed back yet (`requeued`).
pub fn held_events(records: &[Value], key: &str) -> Vec<String> {
    let mut held: Vec<String> = Vec::new();
    for record in records {
        let matches_key = record.get("key").and_then(Value::as_str) == Some(key);
        let events = || -> Vec<String> {
            record
                .get("inbound")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        match record.get("k").and_then(Value::as_str) {
            Some("held") if matches_key => held.extend(events()),
            Some("requeued") if matches_key => {
                let done = events();
                held.retain(|e| !done.contains(e));
            }
            // Wakes held under an epoch that a revocation or reset ended are never handed back.
            Some("reset") if is_reset_of(record, key) => held.clear(),
            _ => {}
        }
    }
    held
}

/// The turn that keeps a conversation from starting another one: a paused turn, or a turn that is
/// still open in the ledger when a new one is about to begin. Turns of one process finish before
/// the next begins, so an open one here is left over from a crash and is not reconciled yet.
pub fn blocking_turn<'a>(turns: &'a [Turn], key: &str) -> Option<&'a Turn> {
    turns.iter().find(|t| t.key == key && (t.state == State::Paused || t.state.is_open()))
}

/// Send batches (from the outbox) that belong to a turn: same conversation and token epoch,
/// created while the turn was alive. Each conversation runs one turn at a time and the token is
/// rotated whenever a turn is cancelled or replaced, so this window is unambiguous.
pub fn batches_of_turn(
    turn: &Turn,
    next_begin: Option<i64>,
    outbox: &[Value],
) -> Vec<crate::send_outbox::Batch> {
    let until = turn.ended_at.or(next_begin).unwrap_or(i64::MAX);
    outbox
        .iter()
        .filter(|r| r.get("k").and_then(Value::as_str) == Some("intent"))
        .filter(|r| r.get("key").and_then(Value::as_str) == Some(turn.key.as_str()))
        .filter(|r| r.get("epoch").and_then(Value::as_str).is_some_and(|e| turn.epochs.iter().any(|x| x == e)))
        .filter(|r| {
            let t = r.get("t").and_then(Value::as_i64).unwrap_or(0);
            t >= turn.begun_at && t <= until
        })
        .filter_map(|r| r.get("batch").and_then(Value::as_str))
        .filter_map(|id| crate::send_outbox::fold_batch(outbox, id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, turn: &str, t: i64, extra: Value) -> Value {
        let mut value = json!({"k": kind, "turn": turn, "t": t});
        if let (Some(o), Some(e)) = (value.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        value
    }

    fn begun(turn: &str, t: i64) -> Value {
        rec("begun", turn, t, json!({"key": "group:1", "epoch": "e1", "inbound": ["group:1|message:7"], "reason": "atMention"}))
    }

    fn ledger_root(tag: &str) -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(mint_id(tag)));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    #[test]
    fn a_crash_torn_prefix_is_skipped_at_the_tail_and_after_a_later_append() {
        let (root, context) = ledger_root("her-ledger-torn-");
        append_record(&context, &begun("t1", 10)).unwrap();
        let path = root.join("data").join(ISOLATED_LEDGER_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(br#"{"k":"attempt","turn":"t1","t":1"#);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_records(&context).expect("a torn tail is not a read error").len(), 1);
        append_record(&context, &rec("attempt", "t1", 11, json!({"rpcId": "r1", "sessionId": "s1"}))).unwrap();
        let records = read_records(&context).expect("the torn prefix, now a middle line, is still skipped");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["k"], "attempt");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_complete_record_missing_only_its_newline_is_kept() {
        let (root, context) = ledger_root("her-ledger-nonl-");
        let path = root.join("data").join(ISOLATED_LEDGER_FILE);
        std::fs::write(&path, format!("{}\n{}", begun("t1", 10), rec("attempt", "t1", 11, json!({"rpcId": "r1"})))).unwrap();
        assert_eq!(read_records(&context).unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_malformed_line_that_is_not_a_truncated_prefix_is_still_an_error() {
        let (root, context) = ledger_root("her-ledger-mid-");
        let path = root.join("data").join(ISOLATED_LEDGER_FILE);
        std::fs::write(&path, format!("{}\nnot json\n{}\n", begun("t1", 10), begun("t2", 12))).unwrap();
        assert!(read_records(&context).is_err());
        std::fs::write(&path, format!("{}\n{{\"a\":1}}}}\n", begun("t1", 10))).unwrap();
        assert!(read_records(&context).is_err(), "a damaged line that is not a truncated prefix is evidence, not a torn append");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_cancellation_request_defers_completion_until_authority_is_revoked() {
        let (root, context) = ledger_root("her-ledger-cancel-pending-");
        let turn = Recorder::begin(&context, "group:1", "dsh", "atMention", "e1", &[], None).unwrap();
        let control = Recorder::attach(&context, &turn.id, &turn.key);
        control.verified("cancel-requested", "turnTimeout").unwrap();
        turn.end("completed", "ok").unwrap();
        turn.end("failed", "collector timeout").unwrap();
        let before = read_records(&context).unwrap();
        assert!(!before.iter().any(|row| row["k"] == "ended"), "{before:?}");
        control.end("cancelled", "turnTimeout").unwrap();
        turn.end("completed", "late end").unwrap();
        let records = read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|row| row["k"] == "ended").count(), 1, "{records:?}");
        let turns = fold(&records).0;
        assert_eq!(turns[0].state, State::Cancelled);
        assert_eq!(turns[0].outcome_reason, "turnTimeout");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_observation_and_cancellation_intent_keep_the_first_decision() {
        for completion_first in [true, false] {
            let (root, context) = ledger_root("her-ledger-completed-observation-");
            let turn = Recorder::begin(&context, "group:1", "dsh", "atMention", "e1", &[], None).unwrap();
            let control = Recorder::attach(&context, &turn.id, &turn.key);
            if completion_first {
                turn.verified("completion-observed", "DSH completed").unwrap();
                control.verified("cancel-requested", "turnTimeout").unwrap();
            } else {
                control.verified("cancel-requested", "turnTimeout").unwrap();
                turn.verified("completion-observed", "DSH completed").unwrap();
            }
            control.end("cancelled", "turnTimeout").unwrap();
            let held = read_records(&context).unwrap();
            if completion_first {
                assert!(!held.iter().any(|row| row["k"] == "ended"), "completion bookkeeping is pending: {held:?}");
            }
            turn.end("completed", "ok").unwrap();
            let records = read_records(&context).unwrap();
            assert_eq!(records.iter().filter(|row| row["k"] == "ended").count(), 1, "{records:?}");
            assert_eq!(fold(&records).0[0].state, if completion_first { State::Completed } else { State::Cancelled });
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn hf136_fix1_stale_recovery_cannot_override_a_new_cancellation_claim() {
        let (root, context) = ledger_root("hf136-fix1-recovery-race-");
        let turn = Recorder::begin(&context, "group:136", "dsh", "test", "epoch", &[], None).unwrap();
        let recovery = Recorder::attach(&context, &turn.id, &turn.key);
        turn.clone().without_retry().verified("cancel-requested", "turnTimeout").unwrap();
        let result = recovery.end_reconciled("resumed", "old verdict");
        let records = read_records(&context).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.is_err(), "an old recovery verdict must retry under the current claim before replay");
        assert!(!records.iter().any(|r| r["k"] == "ended"));
    }

    #[test]
    fn hf136_fix1_first_observation_owns_terminal_in_both_race_orders() {
        for completion_first in [true, false] {
            for sleep in [true, false] {
                let (root, context) = ledger_root("hf136-fix1-race-");
                let collector = Recorder::begin(&context, "group:136", "dsh", "test", "epoch", &[], None).unwrap();
                let control = Recorder::attach(&context, &collector.id, &collector.key).without_retry();
                let reason = if sleep { "observationSleep" } else { "turnTimeout" };
                if completion_first {
                    collector.verified("completion-observed", "DSH completed").unwrap();
                    control.verified("cancel-requested", reason).unwrap();
                } else {
                    control.verified("cancel-requested", reason).unwrap();
                    collector.verified("completion-observed", "DSH completed").unwrap();
                }
                // The losing handler attempts its terminal write first in either order.
                if completion_first {
                    control.end_cancellation(if sleep { "completed" } else { "cancelled" }, reason).unwrap();
                } else {
                    collector.end("completed", "ok").unwrap();
                }
                collector.tools(vec![]).unwrap();
                if completion_first { collector.end("completed", "ok").unwrap(); }
                else { control.end_cancellation(if sleep { "completed" } else { "cancelled" }, reason).unwrap(); }
                let records = read_records(&context).unwrap();
                assert_eq!(records.iter().filter(|r| r["k"] == "ended").count(), 1);
                let terminal = fold(&records).0.remove(0);
                assert_eq!(terminal.outcome_reason, if completion_first { "ok" } else { reason });
                assert_eq!(terminal.state, if completion_first || sleep { State::Completed } else { State::Cancelled });
                assert_eq!(terminal.notes.iter().filter(|n| matches!(n["verdict"].as_str(), Some("completion-observed" | "cancel-requested"))).count(), 1);
                assert!(!terminal.state.is_open());
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn hf136_fix2_scoped_completion_keeps_both_round1_race_orders() {
        for completion_first in [true, false] {
            for sleep in [true, false] {
                let (root, context) = ledger_root("hf136-fix2-scoped-race-");
                let collector = Recorder::begin(&context, "group:136", "dsh", "test", "epoch", &[], None).unwrap();
                collector.attempt("rpc-1", "sid").unwrap();
                let control = Recorder::attach(&context, &collector.id, &collector.key).without_retry();
                let reason = if sleep { "observationSleep" } else { "turnTimeout" };
                let outcome = if sleep { "completed" } else { "cancelled" };
                if completion_first {
                    collector.observe_completion("rpc-1").unwrap();
                    control.verified("cancel-requested", reason).unwrap();
                    control.end_cancellation(outcome, reason).unwrap();
                    assert!(!read_records(&context).unwrap().iter().any(|r| r["k"] == "ended"));
                    collector.end_attempt("rpc-1", "completed", "ok").unwrap();
                } else {
                    control.verified("cancel-requested", reason).unwrap();
                    collector.observe_completion("rpc-1").unwrap();
                    collector.end_attempt("rpc-1", "completed", "ok").unwrap();
                    control.end_cancellation(outcome, reason).unwrap();
                }
                let records = read_records(&context).unwrap();
                let ended: Vec<_> = records.iter().filter(|r| r["k"] == "ended").collect();
                assert_eq!(ended.len(), 1, "every ended turn has exactly one terminal");
                assert_eq!(ended[0]["outcome"], if completion_first { "completed" } else { outcome });
                assert_eq!(ended[0]["reason"], if completion_first { "ok" } else { reason });
                assert!(blocking_turn(&fold(&records).0, &collector.key).is_none());
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn concurrent_attached_recorders_write_only_one_end() {
        let (root, context) = ledger_root("her-ledger-one-end-");
        let turn = Recorder::begin(&context, "group:1", "dsh", "atMention", "e1", &[], None).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8).map(|_| {
            let writer = Recorder::attach(&context, &turn.id, &turn.key);
            let barrier = barrier.clone();
            std::thread::spawn(move || { barrier.wait(); writer.end("completed", "ok").unwrap(); })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        // Reopening the writer must also respect the durable terminal record.
        Recorder::attach(&context, &turn.id, &turn.key).end("cancelled", "late cancel").unwrap();
        let records = read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|row| row["k"] == "ended").count(), 1, "{records:?}");
        assert_eq!(fold(&records).0[0].state, State::Completed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hf136_fix2_rejection_and_cap_settlement_commute_including_sleep() {
        for release_first in [true, false] {
            for sleep in [true, false] {
                let (root, context) = ledger_root("hf136-fix2-rejection-race-");
                let collector = Recorder::begin(&context, "group:136", "dsh", "test", "epoch", &[], None).unwrap();
                collector.attempt("rpc-1", "sid").unwrap();
                collector.observe_completion("rpc-1").unwrap();
                let control = Recorder::attach(&context, &collector.id, &collector.key).without_retry();
                let reason = if sleep { "observationSleep" } else { "turnTimeout" };
                let outcome = if sleep { "completed" } else { "cancelled" };
                control.verified("cancel-requested", reason).unwrap();
                if release_first { collector.release_completion("rpc-1").unwrap(); }
                control.end_cancellation(outcome, reason).unwrap();
                if !release_first { collector.release_completion("rpc-1").unwrap(); }
                collector.release_completion("rpc-1").unwrap(); // A repeated cleanup cannot duplicate the terminal.
                collector.end("failed", "rejected result").unwrap();
                let records = read_records(&context).unwrap();
                let ended: Vec<_> = records.iter().filter(|r| r["k"] == "ended").collect();
                assert_eq!(ended.len(), 1, "every ended turn has exactly one terminal");
                assert_eq!(ended[0]["outcome"], outcome);
                assert_eq!(ended[0]["reason"], reason);
                assert!(blocking_turn(&fold(&records).0, &collector.key).is_none());
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn hf136_fix2_a_stale_attempt_cannot_release_or_accept_its_successor() {
        let (root, context) = ledger_root("hf136-fix2-attempt-id-");
        let turn = Recorder::begin(&context, "group:136", "dsh", "test", "epoch", &[], None).unwrap();
        turn.attempt("rpc-1", "sid").unwrap();
        turn.observe_completion("rpc-1").unwrap();
        turn.release_completion("rpc-1").unwrap();
        assert_eq!(fold(&read_records(&context).unwrap()).0[0].finalization_claim(), None);
        assert!(turn.end_attempt("rpc-1", "completed", "rejected result").is_err());
        turn.attempt("rpc-2", "sid").unwrap();
        turn.observe_completion("rpc-2").unwrap();
        turn.release_completion("rpc-1").unwrap();
        assert!(turn.end_attempt("rpc-1", "completed", "stale result").is_err());
        assert!(turn.end("completed", "unscoped result").is_err());
        turn.end_attempt("rpc-2", "completed", "ok").unwrap();
        let records = read_records(&context).unwrap();
        let ended: Vec<_> = records.iter().filter(|r| r["k"] == "ended").collect();
        assert_eq!(ended.len(), 1, "every ended turn has exactly one terminal");
        assert_eq!(ended[0]["reason"], "ok");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hf136_independent_writers_emit_one_terminal_record() {
        let (root, context) = ledger_root("hf136-terminal-");
        let turn = Recorder::begin(&context, "group:136", "dsh", "test", "epoch", &[], None).unwrap();
        let collector = Recorder::attach(&context, &turn.id, &turn.key);
        turn.end("cancelled", "turnTimeout").unwrap();
        collector.end("completed", "ok").unwrap();
        collector.end("failed", "late failure").unwrap();
        let records = read_records(&context).unwrap();
        assert_eq!(records.iter().filter(|r| r["turn"] == turn.id && r["k"] == "ended").count(), 1);
        let turns = fold(&records).0;
        assert_eq!(turns[0].state, State::Cancelled);
        assert!(blocking_turn(&turns, &turn.key).is_none(), "recovery must treat the turn as finished");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_turn_without_an_attempt_is_begun_and_open() {
        let (turns, _) = fold(&[begun("t1", 10)]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, State::Begun);
        assert!(turns[0].attempts.is_empty());
        assert!(turns[0].state.is_open());
    }

    #[test]
    fn attempt_then_accept_then_end_walks_the_states() {
        let mut log = vec![begun("t1", 10)];
        log.push(rec("attempt", "t1", 11, json!({"rpcId": "r1", "sessionId": "s1"})));
        assert_eq!(fold(&log).0[0].state, State::Attempted);
        log.push(rec("accepted", "t1", 12, json!({"rpcId": "r1"})));
        let (turns, _) = fold(&log);
        assert_eq!(turns[0].state, State::Running);
        assert!(turns[0].attempts[0].accepted);
        log.push(rec("ended", "t1", 20, json!({"outcome": "completed", "reason": "ok"})));
        let (turns, _) = fold(&log);
        assert_eq!(turns[0].state, State::Completed);
        assert_eq!(turns[0].ended_at, Some(20));
        assert_eq!(turns[0].session_id, "s1");
    }

    #[test]
    fn pause_is_overridden_by_a_later_verification_end() {
        let log = vec![
            begun("t1", 10),
            rec("attempt", "t1", 11, json!({"rpcId": "r1", "sessionId": "s1"})),
            rec("unknown", "t1", 12, json!({"reason": "dsh-unreachable", "detail": "down"})),
        ];
        let (turns, _) = fold(&log);
        assert_eq!(turns[0].state, State::Paused);
        assert_eq!(turns[0].pause_reason, "dsh-unreachable");
        let mut log = log;
        log.push(rec("ended", "t1", 30, json!({"outcome": "completed", "reason": "verified"})));
        let (turns, _) = fold(&log);
        assert_eq!(turns[0].state, State::Completed);
        assert!(turns[0].pause_reason.is_empty());
    }

    #[test]
    fn replacements_are_listed_separately() {
        let log = vec![json!({"k": "replaced", "t": 5, "key": "group:1", "oldSession": "a", "newSession": "b", "reason": "dsh-not-found"})];
        let (turns, replacements) = fold(&log);
        assert!(turns.is_empty());
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].new_session, "b");
    }

    #[test]
    fn batches_of_a_turn_use_key_epoch_and_time_window() {
        let outbox = vec![
            json!({"k": "intent", "batch": "b1", "key": "group:1", "epoch": "e1", "t": 15, "messages": ["x"]}),
            json!({"k": "intent", "batch": "b2", "key": "group:1", "epoch": "e2", "t": 16, "messages": ["y"]}),
            json!({"k": "intent", "batch": "b3", "key": "group:1", "epoch": "e1", "t": 5, "messages": ["z"]}),
        ];
        let (turns, _) = fold(&[begun("t1", 10)]);
        let found = batches_of_turn(&turns[0], None, &outbox);
        assert_eq!(found.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), vec!["b1"]);
    }

    /// Review 10: the turn-end telemetry folds one turn from the ledger offset captured when the
    /// turn began. It parses only the records appended since then, so its cost does not grow with
    /// history, and it folds the turn exactly as the whole-ledger fold does.
    #[test]
    fn a_turn_folded_from_its_begin_offset_reads_only_records_since_then() {
        let mut tails = Vec::new();
        for history in [10usize, 20_000] {
            let (root, context) = ledger_root("her-ledger-since-");
            let path = root.join("data").join(ISOLATED_LEDGER_FILE);
            let mut old = String::new();
            for i in 0..history {
                let id = format!("old-{i}");
                old.push_str(&format!("{}\n{}\n", begun(&id, i as i64), rec("ended", &id, i as i64, json!({"outcome": "completed", "reason": "ok"}))));
            }
            std::fs::write(&path, old).unwrap();
            let offset = ledger_len(&context).unwrap();
            let turn = Recorder::begin(&context, "group:1", "dsh", "atMention", "e1", &[], None).unwrap();
            append_record(&context, &begun("other", 1)).unwrap();
            turn.attempt("r1", "s1").unwrap();
            turn.verified("cancel-requested", "turnTimeout").unwrap();
            append_record(&context, &rec("ended", "other", 2, json!({"outcome": "completed", "reason": "ok"}))).unwrap();
            turn.end("cancelled", "turnTimeout").unwrap();
            let tail = read_records_since(&context, offset).unwrap();
            assert_eq!(tail.first().map(|r| r["turn"].clone()), Some(json!(turn.id)), "the tail starts at the turn's begin record");
            tails.push(tail.len());
            let folded = fold_turn_since(&context, offset, &turn.id).unwrap().expect("turn found in its tail");
            let (all, _) = fold(&read_records(&context).unwrap());
            let full = all.into_iter().find(|t| t.id == turn.id).unwrap();
            assert_eq!(folded.state, full.state);
            assert_eq!(folded.state, State::Cancelled);
            assert_eq!(folded.outcome_reason, full.outcome_reason);
            assert_eq!(folded.notes, full.notes);
            assert_eq!(folded.attempts.len(), full.attempts.len());
            std::fs::remove_dir_all(root).unwrap();
        }
        assert_eq!(tails, vec![6, 6], "records parsed at turn end must not grow with history");
    }

    /// An offset taken while another writer was mid-line, or beyond a replaced shorter file, still
    /// yields whole records: the partial line is skipped, a shorter file is read from the start.
    #[test]
    fn a_tail_read_skips_a_partial_first_line_and_restarts_on_a_shorter_file() {
        let (root, context) = ledger_root("her-ledger-since-align-");
        let path = root.join("data").join(ISOLATED_LEDGER_FILE);
        std::fs::write(&path, format!("{}\n{}\n", begun("t1", 10), begun("t2", 11))).unwrap();
        let inside_second = ledger_len(&context).unwrap() - 5;
        append_record(&context, &begun("t3", 12)).unwrap();
        let tail = read_records_since(&context, inside_second).unwrap();
        assert_eq!(tail.iter().map(|r| r["turn"].clone()).collect::<Vec<_>>(), vec![json!("t3")]);
        let at_line_start = std::fs::read(&path).unwrap().len() as u64;
        append_record(&context, &begun("t4", 13)).unwrap();
        assert_eq!(read_records_since(&context, at_line_start).unwrap().len(), 1);
        std::fs::write(&path, format!("{}\n", begun("t5", 14))).unwrap();
        assert_eq!(read_records_since(&context, at_line_start).unwrap()[0]["turn"], "t5");
        assert!(fold_turn_since(&context, 0, "missing").unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn minted_ids_are_unique() {
        assert_ne!(mint_id("turn-"), mint_id("turn-"));
        assert!(mint_id("rpc-").starts_with("rpc-"));
    }
}
