//! Queue memory consolidation in existing QQ sessions; memory_write owns database changes.
use crate::{
    dsh::{PromptProbe, Rpc},
    runtime::{RuntimeArea, RuntimeContext},
    task_runs::{Finish, LogStream, RunHandle},
    tasks::{TaskDef, TaskId},
};
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const PROMPT: &str = "【记忆整理】现在做一次内部整理，不要把这段话说给用户听。
请回顾这次会话，把**以后还会用到**的内容写进长期记忆，用 memory_write，一条一句：
- 用户/群友的偏好、要求、禁忌
- 重要决定与它的理由（例如\"为什么选 A 不选 B\"）
- 踩过的坑、报错与解法
- 这个群/这个人的长期话题、关系、称呼习惯
要求：3~8 条；只写以后还会用到的，不要流水账；每条 200 字以内；subject 填话题名或人名。
写完后**不要**发 QQ 消息，也不要回\"好的\"，直接结束。";

struct Session {
    id: String,
    title: String,
    updated: f64,
    running: bool,
    tokens: f64,
    window: f64,
}

// The JS state contains both historic numeric timestamps and ISO strings. In particular,
// Number(ISO) is NaN: preserve that interval behaviour rather than silently resetting progress.
fn number(value: &Value) -> f64 {
    crate::aux_text::js_number(value).unwrap_or(f64::NAN)
}
fn json_number(value: f64) -> Value {
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value < i64::MAX as f64
    {
        json!(value as i64)
    } else {
        json!(value)
    }
}
fn setting(auto: &Value, key: &str, default: f64) -> f64 {
    auto.get(key)
        .filter(|v| !v.is_null())
        .map(number)
        .unwrap_or(default)
}

fn iso(ms: i64) -> String {
    let date = crate::pricing::beijing_parts(ms - 8 * 3_600_000).2;
    let sec = ms.div_euclid(1000).rem_euclid(86400);
    format!(
        "{date}T{:02}:{:02}:{:02}.{:03}Z",
        sec / 3600,
        sec % 3600 / 60,
        sec % 60,
        ms.rem_euclid(1000)
    )
}

/// Dispatch intents: one per session between recording a prompt and recording its completion.
const DISPATCH: &str = "digest-dispatch.json";
/// An intent whose outcome DSH cannot settle keeps its session from another prompt this long;
/// well under the daily cadence, so the next daily run no longer depends on jitter.
const UNRESOLVED_HOLD_MS: f64 = 20.0 * 3_600_000.0;

fn write_json(context: &RuntimeContext, name: &str, value: &Value) -> bool {
    let bytes = serde_json::to_vec_pretty(value).expect("JSON state serializes");
    context
        .write_atomic(RuntimeArea::Data, name, &bytes)
        .is_ok()
}

fn valid_intent(intent: &Value) -> bool {
    intent["rpcId"].as_str().is_some_and(|id| !id.is_empty())
        && intent["at"].is_string()
        && intent["atMs"].is_number()
}

fn failed(reason: &str) -> Finish {
    Finish::Failed {
        exit_code: Some(1),
        reason: reason.into(),
    }
}

fn cancelled() -> Finish {
    Finish::Cancelled {
        reason: "digest cancelled".into(),
    }
}

fn held(context: &RuntimeContext, cancel: &AtomicBool) -> bool {
    let desired = crate::lifecycle::DesiredStore::new(context.data_dir().join("lifecycle"));
    cancel.load(Ordering::SeqCst)
        || !matches!(crate::lifecycle::effective(&desired.read(), crate::parity_clock::now_ms()), crate::lifecycle::Effective::Act { wants, .. } if wants.jobs)
}

pub fn run(
    context: &RuntimeContext,
    def: &TaskDef,
    log: &mut RunHandle,
    cancel: &AtomicBool,
) -> Finish {
    // A supervisor context can outlive endpoint changes; the short-lived JS job read
    // dependencies/config at each invocation. Pin a fresh selected context for this run.
    let current = match context
        .runtime_root()
        .and_then(|root| RuntimeContext::from_root(root).ok())
    {
        Some(context) => context,
        None => return failed("selected digest runtime unavailable"),
    };
    let context = &current;
    // Once stop is observed, it stays in force for the whole job, including 401 recovery.
    let stopped = std::cell::Cell::new(false);
    let is_cancelled = || {
        stopped.set(stopped.get() || held(context, cancel));
        stopped.get()
    };
    if is_cancelled() {
        return cancelled();
    }
    let deadline = Instant::now() + def.timeout;
    let cfg = match context.load_config() {
        Ok(v) => v,
        Err(_) => return failed("digest configuration unavailable"),
    };
    let auto = &cfg["memory"]["autoFlush"];
    if auto["enabled"] == false {
        log.log(LogStream::Meta, "digest disabled");
        return Finish::Succeeded;
    }
    // Daily and pressure checks have different task gates but share one progress file.
    // Keep this persistent OS gate across selection, every dispatch and checkpoint.
    let _gate = match context.open_locked_file(RuntimeArea::Data, "locks/memory-digest.lock") {
        Ok(gate) => gate,
        Err(error) if error == "runtime file is already locked" => {
            if is_cancelled() {
                return cancelled();
            }
            log.log(LogStream::Meta, "digest skipped: already running");
            return Finish::Succeeded;
        }
        Err(_) => return failed("digest gate unavailable"),
    };
    if is_cancelled() {
        return cancelled();
    }
    let Some(base) = context.dependency_url("dsh") else {
        return failed("digest DSH endpoint unavailable");
    };
    let rpc = match Rpc::connect_with_runtime(context, base) {
        Ok(rpc) => rpc,
        Err(_) if is_cancelled() => return cancelled(),
        Err(_) => return failed("digest DSH credentials unavailable"),
    };
    let call_deadline = || deadline.min(Instant::now() + Duration::from_secs(30));
    if is_cancelled() {
        return cancelled();
    }
    let listed =
        match rpc.call_with_cancellation("session/list", json!({}), call_deadline(), &is_cancelled)
        {
            Ok(v) => v,
            Err(_) if is_cancelled() => return cancelled(),
            Err(_) => return failed("digest session list failed"),
        };
    let mut sessions: Vec<Session> = Vec::new();
    for item in listed["items"].as_array().into_iter().flatten() {
        let Some(title) = item["projections"]["values"]["title"]
            .as_str()
            .filter(|s| s.starts_with("QQ "))
        else {
            continue;
        };
        let Some(id) = item["sessionId"].as_str() else {
            return failed("digest session list invalid");
        };
        let session = Session {
            id: id.into(),
            title: title.into(),
            updated: number(&item["updatedAt"]),
            running: crate::aux_text::js_truthy(&item["running"]),
            tokens: number(&item["projections"]["values"]["contextPressure"]["pressureTokens"]),
            window: {
                let window =
                    number(&item["projections"]["values"]["contextPressure"]["contextWindow"]);
                if window == 0.0 || window.is_nan() {
                    setting(auto, "contextLimitTokens", 1_000_000.0)
                } else {
                    window
                }
            },
        };
        match sessions.iter_mut().find(|s| s.title == title) {
            Some(previous) if session.updated > previous.updated => *previous = session,
            Some(_) => {}
            None => sessions.push(session),
        }
    }
    if sessions.is_empty() {
        log.log(LogStream::Meta, "digest has no QQ sessions");
        return Finish::Succeeded;
    }
    let mut state: Value = match context.read_optional_text(RuntimeArea::Data, "digest-state.json")
    {
        Ok(Some(text)) => match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return failed("digest state invalid"),
        },
        Ok(None) => json!({"sessions":{}}),
        Err(_) => return failed("digest state unavailable"),
    };
    if !state.is_object()
        || state
            .get("sessions")
            .is_some_and(|v| !v.is_null() && !v.is_object())
    {
        return failed("digest state invalid");
    }
    if !state["sessions"].is_object() {
        state["sessions"] = json!({});
    }
    let mut dispatch: Value = match context.read_optional_text(RuntimeArea::Data, DISPATCH) {
        Ok(Some(text)) => match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return failed("digest dispatch record invalid"),
        },
        Ok(None) => json!({"v":1,"pending":{}}),
        Err(_) => return failed("digest dispatch record unavailable"),
    };
    if !dispatch.is_object()
        || !dispatch["pending"]
            .as_object()
            .is_some_and(|pending| pending.values().all(valid_intent))
    {
        return failed("digest dispatch record invalid");
    }
    let now = crate::parity_clock::now_ms();
    let mut failures = 0;
    // A previous run may have died after DSH accepted a prompt and before its checkpoint. Ask DSH
    // about each recorded rpcId instead of sending again; such a session gets no prompt this run.
    let mut held_back: Vec<String> = Vec::new();
    let pending: Vec<(String, Value)> = dispatch["pending"]
        .as_object()
        .map(|p| p.iter().map(|(id, v)| (id.clone(), v.clone())).collect())
        .unwrap_or_default();
    let (mut accepted, mut absent, mut unresolved) = (0, 0, 0);
    for (id, intent) in &pending {
        if is_cancelled() {
            return cancelled();
        }
        if Instant::now() >= deadline {
            return failed("digest timed out");
        }
        let rpc_ids = [intent["rpcId"].as_str().unwrap_or_default().to_string()];
        let probe =
            crate::dsh::probe_prompt_bounded(&rpc, id, &rpc_ids, call_deadline(), &is_cancelled);
        if is_cancelled() {
            return cancelled();
        }
        let recent = (now as f64) - number(&intent["atMs"]) < UNRESOLVED_HOLD_MS;
        match probe {
            PromptProbe::Queued | PromptProbe::Turn(_) => {
                // Record the completion the interrupted run could not.
                accepted += 1;
                held_back.push(id.clone());
                state["sessions"][id] = json!({"at":intent["at"],"tokens":intent["tokens"],
                    "title":intent["title"],"lastUpdatedAt":intent["lastUpdatedAt"]});
                if !write_json(context, "digest-state.json", &state) {
                    failures += 1;
                    continue;
                }
            }
            PromptProbe::NotFound { proved: true }
            | PromptProbe::SessionGone
            | PromptProbe::RemovedFromQueue => absent += 1,
            // DSH cannot say whether it holds the prompt: never send it again while it is recent.
            PromptProbe::NotFound { proved: false } | PromptProbe::Unreachable(_) if recent => {
                unresolved += 1;
                held_back.push(id.clone());
                if matches!(probe, PromptProbe::Unreachable(_)) {
                    failures += 1;
                }
                continue;
            }
            PromptProbe::NotFound { proved: false } | PromptProbe::Unreachable(_) => {
                unresolved += 1
            }
        }
        if let Some(pending) = dispatch["pending"].as_object_mut() {
            pending.remove(id);
        }
        if !write_json(context, DISPATCH, &dispatch) {
            failures += 1;
        }
    }
    if !pending.is_empty() {
        log.log(
            LogStream::Meta,
            &format!(
                "digest reconciled accepted={accepted} absent={absent} unresolved={unresolved}"
            ),
        );
    }
    let timestamp = iso(now);
    let today = &timestamp[..10];
    let done_today = state["sessions"]
        .as_object()
        .map(|s| {
            s.values()
                .filter(|v| v["at"].as_str().is_some_and(|at| at.starts_with(today)))
                .count()
        })
        .unwrap_or(0);
    let mut selected = Vec::new();
    for s in sessions {
        if held_back.contains(&s.id) {
            continue;
        }
        let last = &state["sessions"][&s.id];
        let last_seen = number(&last["lastUpdatedAt"]);
        let latest = if s.updated.is_nan() || last_seen.is_nan() {
            f64::NAN
        } else {
            s.updated.max(last_seen)
        };
        if def.id == TaskId::MemoryDigestCheck {
            let pct = if s.window == 0.0 || s.window.is_nan() {
                0.0
            } else {
                s.tokens / s.window * 100.0
            };
            if pct < setting(auto, "contextPct", 70.0) || s.tokens <= number(&last["tokens"]) {
                continue;
            }
        } else if !((now as f64) - latest < setting(auto, "activeWindowHours", 24.0) * 3_600_000.0)
        {
            continue;
        }
        if (now as f64) - number(&last["at"]) < setting(auto, "minIntervalMinutes", 60.0) * 60_000.0
        {
            continue;
        }
        if s.running {
            continue;
        }
        // This is the historic pre-run cap, not a per-batch limit.
        if done_today as f64 >= setting(auto, "maxSessionsPerDay", 8.0) {
            break;
        }
        selected.push(s);
    }
    log.log(
        LogStream::Meta,
        &format!(
            "digest selected={} completed_today={done_today}",
            selected.len()
        ),
    );
    for s in selected {
        if is_cancelled() {
            return cancelled();
        }
        if Instant::now() >= deadline {
            return failed("digest timed out");
        }
        // Record the intent before sending, so a run killed before the checkpoint below leaves
        // the ids a rerun needs to ask DSH instead of queueing the same consolidation again.
        let (rpc_id, request_id) = (crate::dsh::mint_request_id(), crate::dsh::mint_request_id());
        let sent_at = crate::parity_clock::now_ms();
        dispatch["pending"][&s.id] = json!({"rpcId":rpc_id,"requestId":request_id,
            "task":def.id.as_str(),"at":iso(sent_at),"atMs":sent_at,
            "tokens":json_number(s.tokens),"title":s.title,"lastUpdatedAt":json_number(s.updated)});
        if !write_json(context, DISPATCH, &dispatch) {
            failures += 1;
            if let Some(pending) = dispatch["pending"].as_object_mut() {
                pending.remove(&s.id);
            }
            continue;
        }
        let result = rpc.call_with_cancellation_as(
            "session/prompt",
            json!({"sessionId":s.id,"mode":"queue","content":[{"type":"text","text":PROMPT}],
                "requestId":request_id}),
            &rpc_id,
            call_deadline(),
            &is_cancelled,
        );
        if is_cancelled() {
            return cancelled();
        }
        match result {
            Ok(_) => {
                state["sessions"][&s.id] = json!({"at":iso(crate::parity_clock::now_ms()),"tokens":json_number(s.tokens),"title":s.title,"lastUpdatedAt":json_number(s.updated)});
                if !write_json(context, "digest-state.json", &state) {
                    // The intent stays: the next run reconciles it with DSH.
                    failures += 1;
                    continue;
                }
            }
            // DSH refused it: nothing was queued, so the intent is void.
            Err(error) if crate::dsh::is_refused(&error) => failures += 1,
            // Unknown outcome: the intent stays for the next run to reconcile.
            Err(_) => {
                failures += 1;
                continue;
            }
        }
        if let Some(pending) = dispatch["pending"].as_object_mut() {
            pending.remove(&s.id);
        }
        if !write_json(context, DISPATCH, &dispatch) {
            failures += 1;
        }
    }
    log.log(LogStream::Meta, &format!("digest failures={failures}"));
    if is_cancelled() {
        return cancelled();
    }
    if failures == 0 {
        Finish::Succeeded
    } else {
        failed("digest dispatch or state write failed")
    }
}
