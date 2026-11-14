//! Cache-aware idle session rotation. The hub owns the conversation slot; the Digest module digests the conversation's stored messages before retirement.
use crate::{
    runtime::RuntimeContext,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct Plan {
    pub key: String,
    pub sid: String,
    pub epoch: String,
    pub guard: Value,
    pub turn_seq: i64,
    pub input: u64,
    pub recovering: bool,
    pub policy: Value,
}

pub(crate) fn candidate(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    now: i64,
) -> Result<Option<Plan>, String> {
    let state = context.read_social_state()?;
    let conv = &state["conversations"][key];
    let epoch = conv["agentToken"].as_str().unwrap_or("");
    let sid = match crate::session_map::lookup(context, key)? {
        Some(sid) => sid,
        None => match crate::session_map::unrevoked_rotation(context, key, epoch)? {
            Some(sid) => sid,
            None => return Ok(None),
        },
    };
    let r = crate::session_map::context_record(context, key, &sid, epoch)?;
    let pending = r["rotation"]["phase"] == "pending"
        || (r["rotation"]["phase"] == "rotate" && r["rotation"]["turnSeq"] == r["lastTurnSeq"]);
    let turn_seq = if pending {
        r["rotation"]["turnSeq"].as_i64()
    } else {
        None
    }
    .unwrap_or_else(|| r["lastTurnSeq"].as_i64().unwrap_or(0));
    if !pending {
        if turn_seq == 0
            || r["turnOpen"] == true
            || r["compacting"].is_string()
            || !r["installed"].is_object()
        {
            return Ok(None);
        }
        if r["rotation"]["attemptedTurn"] == turn_seq {
            return Ok(None);
        }
        let closed = r["closedAtEnd"] == true;
        if !closed || conv["adminWakes"].as_array().is_some_and(|v| !v.is_empty()) {
            return Ok(None);
        }
        let model = r["lastCall"]["model"]
            .as_str()
            .or_else(|| cfg.pointer("/dsh/model").and_then(Value::as_str))
            .unwrap_or("");
        let provider = r["lastCall"]["provider"]
            .as_str()
            .or_else(|| cfg.pointer("/dsh/provider").and_then(Value::as_str))
            .unwrap_or("");
        let caps = cfg.pointer("/dsh/rotation/caps");
        let cap = caps
            .and_then(|v| v.get(model))
            .or_else(|| caps.and_then(|v| v.get("default")))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .unwrap_or(500_000);
        if r["lastCall"]["input"].as_f64().unwrap_or(0.0) < cap as f64 {
            return Ok(None);
        }
        let ttl = cfg
            .pointer("/dsh/cacheTtlMinutes")
            .and_then(|v| v.get(provider))
            .and_then(Value::as_f64)
            .or_else(|| match provider {
                "local-relay" => Some(5.0),
                "opencode-go" => Some(120.0),
                _ => None,
            });
        let Some(ttl) = ttl.filter(|n| n.is_finite() && *n > 0.0) else {
            return Ok(None);
        };
        let idle_from = conv["lastIncomingAt"]
            .as_i64()
            .unwrap_or(now)
            .max(r["lastEnded"].as_i64().unwrap_or(now));
        if ((now - idle_from) as f64) < (ttl - 1.0).max(0.0) * 60_000.0 {
            return Ok(None);
        }
    }
    let (turns, _) = crate::turn_ledger::fold(&crate::turn_ledger::read_records(context)?);
    if crate::turn_ledger::blocking_turn(&turns, key).is_some() {
        return Ok(None);
    }
    Ok(Some(Plan {
        key: key.into(),
        sid,
        epoch: epoch.into(),
        guard: conv.clone(),
        turn_seq,
        input: r["lastCall"]["input"].as_f64().unwrap_or(0.0) as u64,
        recovering: pending,
        policy: json!({"rotation":cfg.pointer("/dsh/rotation"),"cacheTtlMinutes":cfg.pointer("/dsh/cacheTtlMinutes")}),
    }))
}

pub(crate) enum DigestResult {
    Done { ran: bool, retire_unknown: bool },
    Pending,
}


pub(crate) fn failure(context: &RuntimeContext, cfg: &Value, plan: &Plan, phase: &str) {
    rotation_notice(context, cfg, &plan.key, phase);
}

fn rotation_notice(context: &RuntimeContext, cfg: &Value, key: &str, phase: &str) {
    // Claim storage even after an in-memory fallback, so repair makes that day's
    // suppression durable without sending again.
    static SENT: OnceLock<Mutex<HashMap<ReadKey, i64>>> = OnceLock::new();
    let day = crate::session_map::rotation_alert_day();
    let sent_state = SENT.get_or_init(Default::default);
    let mut sent = sent_state.lock().unwrap_or_else(|p| p.into_inner());
    let claim = crate::session_map::mark_rotation_alert_day(context, key, day);
    #[cfg(test)]
    tests::after_notice_claim(sent_state);
    if matches!(claim, Ok(false)) {
        return;
    }
    let last = sent.entry(read_key(context, key)).or_insert(i64::MIN);
    let alert = *last < day;
    *last = (*last).max(day);
    drop(sent);
    if alert {
        crate::admin_alert::notify_notice(
            context,
            cfg,
            &format!(
                "{} session rotation: {phase} failed; check the session before retrying",
                key
            ),
            &format!("session-rotation:{key}"),
        );
    }
}

type ReadKey = (PathBuf, String);

#[derive(Default)]
struct ReadFailure {
    step: &'static str,
    count: u32,
    retry_at: i64,
}

fn read_failures() -> &'static Mutex<HashMap<ReadKey, ReadFailure>> {
    static READS: OnceLock<Mutex<HashMap<ReadKey, ReadFailure>>> = OnceLock::new();
    READS.get_or_init(Default::default)
}

fn read_key(context: &RuntimeContext, key: &str) -> ReadKey {
    (
        context
            .runtime_root()
            .unwrap_or_else(|| std::path::Path::new("legacy"))
            .into(),
        key.into(),
    )
}

/// Recovery stops at its first failed read. Count failed passes across boundaries;
/// successful reads cannot establish that recovery completed or authority was revoked.
pub(crate) fn rotation_read<T>(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    step: &'static str,
    now: i64,
    result: Result<T, String>,
) -> Result<T, String> {
    let mut reads = read_failures().lock().unwrap_or_else(|p| p.into_inner());
    let slot = read_key(context, key);
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            let r = reads.entry(slot).or_default();
            r.step = step;
            r.count = r.count.saturating_add(1);
            r.retry_at = now + if r.count >= 12 { 60_000 } else { 5_000 };
            let escalate = r.count == 12;
            drop(reads);
            if escalate {
                let phase = format!("step={step} kind=storage");
                crate::activity::append(
                    context,
                    &format!(
                        "{key} session rotation {phase}; operator attention required; retry in 60s"
                    ),
                );
                rotation_notice(
                    context,
                    cfg,
                    key,
                    &format!("{phase}; operator attention required; retry in 60s"),
                );
            }
            Err(error)
        }
    }
}

/// Called only after revocation is confirmed and the maintenance slot is released.
pub(crate) fn recovery_complete(context: &RuntimeContext, key: &str) {
    read_failures()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&read_key(context, key));
}

pub(crate) fn read_retry_at(context: &RuntimeContext, key: &str, step: &'static str) -> i64 {
    read_failures()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&read_key(context, key))
        .filter(|r| r.step == step && r.count >= 12)
        .map_or(0, |r| r.retry_at)
}

pub(crate) fn read_retry_ms(context: &RuntimeContext, key: &str) -> u64 {
    read_failures()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&read_key(context, key))
        .map_or(5_000, |r| if r.count >= 12 { 60_000 } else { 5_000 })
}

fn finish(
    context: &RuntimeContext,
    plan: &Plan,
    success: bool,
    retire_unknown: bool,
) -> Result<(), String> {
    crate::session_map::update_context(context, &plan.key, &plan.sid, &plan.epoch, |r| {
        r["rotation"]["phase"] = json!("rotate");
        r["rotation"]["turnSeq"] = json!(plan.turn_seq);
        r["rotation"]["retireUnknown"] = json!(retire_unknown);
        if !success {
            r["rotation"]["attemptedTurn"] = json!(plan.turn_seq);
        }
        if success {
            r["digestedThrough"] = json!(plan.turn_seq);
        }
    })?;
    Ok(())
}

pub(crate) async fn digest(
    context: RuntimeContext,
    cfg: Value,
    plan: Plan,
    cancel: Arc<AtomicBool>,
) -> DigestResult {
    // Only a positively observed persisted replacement token proves revocation.
    // Mapping removal can precede a failed token write, including across restart.
    match rotation_read(
        &context,
        &cfg,
        &plan.key,
        "token-read",
        crate::parity_clock::now_ms(),
        crate::console::conversation_token(&context, &plan.key),
    ) {
        Ok(Some(epoch)) if epoch != plan.epoch => {
            return DigestResult::Done {
                ran: false,
                retire_unknown: false,
            }
        }
        Ok(Some(_)) => {}
        _ => return DigestResult::Pending,
    }
    let records = (|| {
        let mapped = crate::session_map::lookup(&context, &plan.key)?;
        let record =
            crate::session_map::context_record(&context, &plan.key, &plan.sid, &plan.epoch)?;
        Ok((mapped, record))
    })();
    let (mapped, r) = match rotation_read(
        &context,
        &cfg,
        &plan.key,
        "context-read",
        crate::parity_clock::now_ms(),
        records,
    ) {
        Ok(records) => records,
        Err(_) => return DigestResult::Pending,
    };
    if mapped.as_deref().is_some_and(|sid| sid != plan.sid) {
        return DigestResult::Pending;
    }
    if r["rotation"]["phase"] == "rotate" && r["rotation"]["turnSeq"] == plan.turn_seq {
        return DigestResult::Done {
            ran: true,
            retire_unknown: r["rotation"]["retireUnknown"] == true || mapped.is_none(),
        };
    }
    if mapped.is_none() {
        return DigestResult::Pending;
    }
    match digest_inner(&context, &cfg, &plan, &cancel).await {
        Ok(Some(success)) => {
            if finish(&context, &plan, success, false).is_err() {
                let _ = report_failure(
                    &context,
                    &cfg,
                    &plan,
                    &DigestFailure::new("checkpoint", "storage", false),
                    true,
                );
                return DigestResult::Pending;
            }
            DigestResult::Done {
                ran: true,
                retire_unknown: false,
            }
        }
        Ok(None) => DigestResult::Done {
            ran: false,
            retire_unknown: false,
        },
        Err(error) => {
            let count = match report_failure(
                &context,
                &cfg,
                &plan,
                &error,
                !cancel.load(Ordering::SeqCst) || !error.settled,
            ) {
                Ok(count) => count,
                Err(()) => return DigestResult::Pending,
            };
            // One verification retry, then use the existing failed-digest retirement policy.
            // Archiving retains the source history; the cursor never claims an unknown write.
            // Unknown model work is NOT replayed: memory_write is not batch-idempotent.
            if !error.settled && count < 2 && !cancel.load(Ordering::SeqCst) {
                return DigestResult::Pending;
            }
            if finish(&context, &plan, false, !error.settled).is_err() {
                return DigestResult::Pending;
            }
            DigestResult::Done {
                ran: true,
                retire_unknown: !error.settled,
            }
        }
    }
}

/// Only fixed vocabulary crosses the diagnostic boundary. Never retain DSH/model error text.
#[derive(Debug)]
struct DigestFailure {
    step: &'static str,
    kind: &'static str,
    settled: bool,
}

impl DigestFailure {
    fn new(step: &'static str, kind: &'static str, settled: bool) -> Self {
        Self {
            step,
            kind,
            settled,
        }
    }
}

fn report_failure(
    context: &RuntimeContext,
    cfg: &Value,
    plan: &Plan,
    error: &DigestFailure,
    notify: bool,
) -> Result<u64, ()> {
    eprintln!(
        "{} session rotation step={} kind={}",
        plan.key, error.step, error.kind
    );
    let mut count = 0;
    crate::session_map::update_context(context, &plan.key, &plan.sid, &plan.epoch, |r| {
        count = r["rotation"]["failureCount"].as_u64().unwrap_or(0) + 1;
        r["rotation"]["failureCount"] = json!(count);
        r["rotation"]["lastFailure"] = json!({"step":error.step,"kind":error.kind});
    })
    .map_err(|_| ())?;
    if count == 0 {
        return Err(());
    }
    let r = crate::session_map::context_record(context, &plan.key, &plan.sid, &plan.epoch)
        .map_err(|_| ())?;
    crate::session_map::record_rotation_failure(context, &plan.key, json!({
        "sid":plan.sid,"rpcId":r["rotation"]["rpcId"],"turnSeq":plan.turn_seq,
        "digestedThrough":r["digestedThrough"],"step":error.step,"kind":error.kind,
        "at":crate::parity_clock::now_ms(),"count":count,"outcome":if error.settled {"failed"} else {"unverified"}
    })).map_err(|_| ())?;
    if notify {
        failure(
            context,
            cfg,
            plan,
            &format!("step={} kind={}", error.step, error.kind),
        );
    }
    Ok(count)
}

/// Digest the conversation's stored messages. Memories and the digest cursor commit together, so every
/// outcome is settled; cancellation returns at once and a late result writes nothing.
async fn digest_inner(context: &RuntimeContext, cfg: &Value, plan: &Plan, cancel: &Arc<AtomicBool>) -> Result<Option<bool>, DigestFailure> {
    let r = crate::session_map::context_record(context, &plan.key, &plan.sid, &plan.epoch)
        .map_err(|_| DigestFailure::new("context-read", "storage", false))?;
    if cancel.load(Ordering::SeqCst) || r["digestedThrough"].as_i64().unwrap_or(0) >= plan.turn_seq {
        return Ok(None);
    }
    let (context, cfg, key, flag) = (context.clone(), cfg.clone(), plan.key.clone(), cancel.clone());
    let mut task = tokio::task::spawn_blocking(move || {
        let model = crate::digest::model_for(&context, &cfg).map_err(|error| crate::digest::DigestError::Model(error.0))?;
        crate::digest::Digest::new(&context, &cfg, model.as_ref())
            .run_conversation(&key, &crate::digest::Limits { min_messages: 1, min_interval_ms: 0 }, &flag)
    });
    let outcome = loop {
        tokio::select! {
            joined = &mut task => break joined.map_err(|_| DigestFailure::new("digest", "worker", true))?,
            _ = tokio::time::sleep(Duration::from_millis(100)) => if cancel.load(Ordering::SeqCst) { return Ok(None); },
        }
    };
    match outcome {
        Ok(_) => Ok(Some(true)),
        Err(crate::digest::DigestError::Cancelled) => Ok(None),
        Err(error) => Err(DigestFailure::new(error.step(), error.kind(), true)),
    }
}

#[cfg(test)]
mod tests;
