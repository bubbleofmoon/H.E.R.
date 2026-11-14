//! The Digest module: reads stored conversation evidence after its cursor, asks a model through a port and writes inferences with bridge-set provenance. The nightly job, the backlog check and reserved2 idle rotation all call it.
mod conversation;
mod evidence;
mod model;
#[cfg(test)]
mod tests;

pub(crate) use conversation::{Digest, DigestError, Limits, Outcome};
pub(crate) use model::{model_for, DigestModel, ModelError};
use conversation::Hold;
#[cfg(test)]
pub(crate) use model::install_test_model;

use crate::{runtime::{RuntimeArea, RuntimeContext}, task_runs::{Finish, LogStream, RunHandle}, tasks::{TaskDef, TaskId}};
use serde_json::Value;
use std::{sync::atomic::{AtomicBool, Ordering}, time::Instant};

// The JS state contains both historic numeric timestamps and ISO strings. In particular,
// Number(ISO) is NaN: preserve that interval behaviour rather than silently resetting progress.
fn number(value: &Value) -> f64 {
    crate::aux_text::js_number(value).unwrap_or(f64::NAN)
}
fn setting(auto: &Value, key: &str, default: f64) -> f64 {
    auto.get(key)
        .filter(|v| !v.is_null())
        .map(number)
        .unwrap_or(default)
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

/// The job's hold inside the Digest: its cancel flag, or a desired state without jobs (stop, maintenance).
struct JobHold<'a> {
    context: &'a RuntimeContext,
    cancel: &'a AtomicBool,
}

impl Hold for JobHold<'_> {
    fn held(&self) -> bool {
        held(self.context, self.cancel)
    }
}

/// Backlog the check task waits for: `contextPct` percent of the social evidence buffer.
fn check_min_messages(cfg: &Value) -> usize {
    let limit = cfg.pointer("/socialV2/context/recentLimit").and_then(Value::as_u64).filter(|n| *n > 0).unwrap_or(100) as f64;
    let pct = setting(&cfg["memory"]["autoFlush"], "contextPct", 70.0);
    let pct = if pct.is_finite() { pct.clamp(1.0, 100.0) } else { 70.0 };
    ((limit * pct / 100.0).ceil() as usize).max(1)
}

pub fn run(context: &RuntimeContext, def: &TaskDef, log: &mut RunHandle, cancel: &AtomicBool) -> Finish {
    let current = match context.runtime_root().and_then(|root| RuntimeContext::from_root(root).ok()) {
        Some(context) => context,
        None => return failed("selected digest runtime unavailable"),
    };
    let context = &current;
    // Once stop is observed, it stays in force for the whole job.
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
    // Daily and backlog checks have different task gates but share one progress cursor.
    // Keep this persistent OS gate for the whole run.
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
    let model = match model_for(context, &cfg) {
        Ok(model) => model,
        Err(error) => return failed(&format!("digest model unavailable: {}", error.0)),
    };
    let digest = Digest::new(context, &cfg, model.as_ref());
    let keys = match digest.conversations() {
        Ok(keys) => keys,
        Err(_) => return failed("digest evidence unavailable"),
    };
    let limits = Limits {
        min_messages: if def.id == TaskId::MemoryDigestCheck { check_min_messages(&cfg) } else { 1 },
        min_interval_ms: (setting(auto, "minIntervalMinutes", 60.0).max(0.0) * 60_000.0) as i64,
    };
    let max = setting(auto, "maxSessionsPerDay", 8.0).max(0.0) as usize;
    let (mut written, mut memories, mut quiet, mut failures) = (0usize, 0usize, 0usize, 0usize);
    let hold = JobHold { context, cancel };
    for key in keys {
        if is_cancelled() {
            return cancelled();
        }
        if Instant::now() >= deadline {
            return failed("digest timed out");
        }
        if written >= max {
            break;
        }
        match digest.run_conversation(&key, &limits, &hold) {
            Ok(Outcome::Quiet) => quiet += 1,
            Ok(Outcome::Written { memories: count, .. }) => {
                written += 1;
                memories += count;
            }
            Err(DigestError::Cancelled) => return cancelled(),
            Err(error) => {
                failures += 1;
                log.log(LogStream::Meta, &format!("digest failed step={} kind={}", error.step(), error.kind()));
            }
        }
    }
    log.log(LogStream::Meta, &format!("digest written={written} memories={memories} quiet={quiet} failures={failures}"));
    if is_cancelled() {
        return cancelled();
    }
    if failures == 0 {
        Finish::Succeeded
    } else {
        failed("digest conversation failed")
    }
}
