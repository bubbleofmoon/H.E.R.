//! Administrator alert dedupe and rate limit (ticket 25, AC5).
//!
//! The exact policy of the frozen JS `notifyOwnerQQ`: one private message per dedupe key per 60 s and ten per
//! hour in total, body cut to 800 characters. The limiter is a pure state machine over an injected time so the
//! windows are testable without waiting; it decides, the caller sends. Resident callers send in the background;
//! restore failure callers wait for bounded completion before exit. The notifier does not retry sends;
//! QQ login episodes explicitly own a durable, bounded retry policy above this layer.
//!
//! One process-wide limiter serves every alert the process raises (agent feedback, failover steps, turn errors,
//! crash notice, recovery notices). The console and the live bridge are separate processes, each with its own
//! limiter, as each is its own JS-style notifier.
#![allow(dead_code)]

use std::collections::HashMap;

/// Same-key window of the frozen JS.
pub const KEY_WINDOW_MS: i64 = 60_000;
/// JS global cap per sliding hour.
pub const HOURLY_CAP: usize = 10;
pub const HOUR_MS: i64 = 3_600_000;
/// JS `truncateText(text, 800)`.
pub const BODY_MAX_CHARS: i64 = 800;
/// Distinct (scope, key) slots remembered; the oldest are dropped beyond this, which only ever allows a repeat sooner.
const MAX_SLOTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertVerdict {
    /// Send it now (the slot and the hourly budget have been charged).
    Send,
    /// The same key was sent less than 60 s ago.
    SkipKeyWindow,
    /// Ten alerts already went out in the last hour.
    SkipHourlyCap,
}

impl AlertVerdict {
    pub fn is_send(&self) -> bool {
        matches!(self, Self::Send)
    }
    /// The skip reason as the JS logs it, for the local record.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::SkipKeyWindow => "key-window",
            Self::SkipHourlyCap => "hourly-cap",
        }
    }
}

#[derive(Debug, Default)]
pub struct AlertLimiter {
    last: HashMap<(String, String), i64>,
    hour: Vec<i64>,
}

impl AlertLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide for an alert with `key` in `scope` (the runtime scope; one process may host several) at `now_ms`.
    /// Only a `Send` verdict is recorded: a skipped alert neither refreshes the key window nor spends hourly budget.
    pub fn check(&mut self, scope: &str, key: &str, now_ms: i64) -> AlertVerdict {
        let slot = (scope.to_string(), key.to_string());
        if let Some(last) = self.last.get(&slot) {
            if now_ms - last < KEY_WINDOW_MS {
                return AlertVerdict::SkipKeyWindow;
            }
        }
        self.hour.retain(|t| now_ms - *t < HOUR_MS);
        if self.hour.len() >= HOURLY_CAP {
            return AlertVerdict::SkipHourlyCap;
        }
        if self.last.len() >= MAX_SLOTS && !self.last.contains_key(&slot) {
            if let Some(oldest) = self
                .last
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone())
            {
                self.last.remove(&oldest);
            }
        }
        self.last.insert(slot, now_ms);
        self.hour.push(now_ms);
        AlertVerdict::Send
    }

    /// Alerts charged to the current hour (diagnostics and tests).
    pub fn sent_in_last_hour(&self, now_ms: i64) -> usize {
        self.hour.iter().filter(|t| now_ms - **t < HOUR_MS).count()
    }
}

/// The alert body: redacted first (a secret must never be cut in half and so escape the redactor), then cut to 800.
pub fn prepare_body(text: &str, redact: impl Fn(&str) -> String) -> String {
    crate::social_mem::truncate_text(&redact(text), BODY_MAX_CHARS)
}

// ---- the process-wide notifier -------------------------------------------------------------------

use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::RuntimeContext;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};

/// Which rate budget an alert is charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// Alerts the frozen JS texts (`notifyOwnerQQ`) and the administrator messages JS intends (`notifyAdmin`, accepted
    /// deviation 21): one per kind per minute, ten per hour.
    Js,
    /// Rust-only owner notices (accepted deviation 22): the same rule on a limiter of their own, so a flood of notices
    /// can never use up the budget of the alerts JS sends.
    Notice,
    /// Memory-layer alerts (an empty Identity or Relationship, `briefing.rs`): the same rule on a limiter of their
    /// own, so they never spend the notices' budget.
    Memory,
}

/// The limiter shared by every JS-texted alert of this process.
pub fn shared_limiter() -> &'static Mutex<AlertLimiter> {
    static L: OnceLock<Mutex<AlertLimiter>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(AlertLimiter::new()))
}

/// The limiter of the Rust-only notices.
pub fn notice_limiter() -> &'static Mutex<AlertLimiter> {
    static L: OnceLock<Mutex<AlertLimiter>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(AlertLimiter::new()))
}

/// The limiter of the memory-layer alerts.
pub fn memory_limiter() -> &'static Mutex<AlertLimiter> {
    static L: OnceLock<Mutex<AlertLimiter>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(AlertLimiter::new()))
}

/// Charge `key` to `budget` (pure over the two limiters, so the policy is testable).
fn admit(js: &mut AlertLimiter, notice: &mut AlertLimiter, budget: Budget, scope: &str, key: &str, now_ms: i64) -> AlertVerdict {
    match budget {
        Budget::Js => js.check(scope, key, now_ms),
        Budget::Notice => notice.check(scope, key, now_ms),
        Budget::Memory => memory_limiter().lock().unwrap_or_else(|p| p.into_inner()).check(scope, key, now_ms),
    }
}

fn admit_global(budget: Budget, scope: &str, key: &str, now_ms: i64) -> AlertVerdict {
    let mut limiter = match budget {
        Budget::Js => shared_limiter(),
        Budget::Notice => notice_limiter(),
        Budget::Memory => memory_limiter(),
    }
    .lock()
    .unwrap_or_else(|p| p.into_inner());
    limiter.check(scope, key, now_ms)
}

/// The dedupe key of an administrator message kind (`turn-error`, `not-accepted`, `blocked-reply`). It never equals the
/// key of the failover step alert (`turn-error`), which shares the budget but is another message.
pub fn admin_key(kind: &str) -> String {
    format!("admin-{kind}")
}

fn scope_of(context: &RuntimeContext) -> String {
    context
        .runtime_root()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "legacy".to_string())
}

/// `ownerQQ` as a positive account id; `None` when it is not configured (JS: the alert is only logged).
pub fn owner_id(cfg: &Value) -> Option<i64> {
    let owner = match cfg.get("ownerQQ") {
        Some(Value::Number(n)) if n.as_f64().is_some_and(|f| f != 0.0) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    owner.trim().parse::<i64>().ok().filter(|id| *id > 0)
}

fn note(context: &RuntimeContext, key: &str, outcome: Outcome, reason: &str) {
    crate::recovery_log::record(context, Trigger::Turn, Component::Alert, &format!("owner-alert-{key}"), outcome, reason);
}

/// A bounded queue served by one long-lived thread: alerts are fire and forget, but a storm of them must not spawn a
/// thread (or a gateway connection) per message.
struct Outbox<T> {
    tx: std::sync::mpsc::SyncSender<T>,
}

impl<T: Send + 'static> Outbox<T> {
    fn start(capacity: usize, deliver: impl Fn(T) + Send + 'static) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<T>(capacity);
        std::thread::Builder::new()
            .name("admin-alert".to_string())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    deliver(job);
                }
            })
            .expect("the alert worker thread starts");
        Self { tx }
    }

    /// `false` when the queue is full (the alert is dropped, never queued without bound).
    fn submit(&self, job: T) -> bool {
        self.tx.try_send(job).is_ok()
    }
}

struct PrivateJob {
    context: RuntimeContext,
    cfg: Value,
    owner: i64,
    body: String,
    key: String,
    gap_ids: Vec<i64>,
}

fn deliver_private(job: PrivateJob) -> bool {
    let PrivateJob { context, cfg, owner, body, key, gap_ids } = job;
    // The queue admission is durable, but not a send attempt. Persist uncertainty
    // before touching the network, so a crash cannot automatically duplicate a send.
    if !gap_ids.is_empty() {
        match crate::inbound::Journal::open(&context).and_then(|j| j.begin_gap_notice(&gap_ids)) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                note(&context, &key, Outcome::Failed, &format!("gap notice checkpoint failed: {error}"));
                return false;
            }
        }
    }
    let outcome = crate::console::media_onebot(&context, &cfg)
        .map_err(crate::onebot::SendError::NotCommitted)
        .and_then(|ob| ob.send_one_outcome(&crate::onebot::Target::Private(owner), &body, None, None).map(|_| ()));
    if !gap_ids.is_empty() {
        let saved = crate::inbound::Journal::open(&context).and_then(|j| match &outcome {
            Ok(()) => j.mark_gaps_notified(&gap_ids, crate::parity_clock::now_ms()),
            Err(crate::onebot::SendError::NotCommitted(_)) => j.release_gap_notice(&gap_ids, true),
            Err(crate::onebot::SendError::Unknown(_)) => Ok(()),
        });
        if let Err(error) = saved {
            note(&context, &key, Outcome::Failed, &format!("gap notice outcome checkpoint failed: {error}"));
        }
    }
    match outcome {
        Ok(()) => {
            crate::safe_println!("[notify] 已私聊管理员：{}", crate::activity::slice(&crate::activity::redact(&context, &body), 80));
            note(&context, &key, Outcome::Ok, "sent to the administrator");
            true
        }
        Err(crate::onebot::SendError::NotCommitted(error) | crate::onebot::SendError::Unknown(error)) => {
            let error = crate::activity::redact(&context, &error);
            crate::safe_println!("[notify] 通知管理员失败：{error}");
            note(&context, &key, Outcome::Failed, &format!("sending to the administrator failed: {error}"));
            false
        }
    }
}

/// Fire and forget: one private message to `owner`, never retried (an unknown outcome of a private message must not be
/// sent twice). A failure reaches the log and the run record, never the caller.
fn send_private(context: &RuntimeContext, cfg: &Value, owner: i64, body: String, key: String) -> bool {
    send_private_with_gaps(context, cfg, owner, body, key, Vec::new())
}

fn send_private_with_gaps(context: &RuntimeContext, cfg: &Value, owner: i64, body: String, key: String, gap_ids: Vec<i64>) -> bool {
    static OUTBOX: OnceLock<Outbox<PrivateJob>> = OnceLock::new();
    let outbox = OUTBOX.get_or_init(|| Outbox::start(32, |job| { deliver_private(job); }));
    let accepted = outbox.submit(PrivateJob { context: context.clone(), cfg: cfg.clone(), owner, body, key: key.clone(), gap_ids });
    if !accepted {
        crate::safe_println!("[notify] 管理员通知队列已满，丢弃：{key}");
        note(context, &key, Outcome::Failed, "the alert queue is full; the message was dropped");
    }
    accepted
}

/// Decide, then send or record the skip. Background calls report admission; waiting calls report delivery.
fn notify_with(context: &RuntimeContext, cfg: &Value, budget: Budget, text: &str, dedupe_key: &str, wait: bool, gap_ids: Vec<i64>) -> bool {
    let Some(owner) = owner_id(cfg) else {
        crate::safe_println!("[notify] 未配置 ownerQQ，只写日志：{}", crate::activity::slice(&crate::activity::redact(context, text), 160));
        return false;
    };
    let now = crate::parity_clock::now_ms();
    match admit_global(budget, &scope_of(context), dedupe_key, now) {
        AlertVerdict::Send => {}
        AlertVerdict::SkipKeyWindow => {
            crate::safe_println!("[notify] 跳过（{dedupe_key} 60 秒内已发过）");
            note(context, dedupe_key, Outcome::Skipped, "same key already sent within 60 seconds");
            return false;
        }
        AlertVerdict::SkipHourlyCap => {
            crate::safe_println!("[notify] 跳过（每小时上限 10 条）");
            note(context, dedupe_key, Outcome::Skipped, "hourly cap of 10 reached");
            return false;
        }
    }
    let body = prepare_body(text, |t| crate::activity::redact(context, t));
    if !wait {
        return send_private_with_gaps(context, cfg, owner, body, dedupe_key.to_string(), gap_ids);
    }
    // A short-lived restore CLI must observe completion before process::exit. Bound the
    // whole attempt, including scheduling/connect/write/read, even if a peer trickles bytes.
    let job = PrivateJob { context: context.clone(), cfg: cfg.clone(), owner, body, key: dedupe_key.to_string(), gap_ids };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new().name("restore-alert".into()).spawn(move || {
        let _ = tx.send(deliver_private(job));
    }).is_err() {
        note(context, dedupe_key, Outcome::Failed, "the alert delivery worker could not start");
        return false;
    }
    match rx.recv_timeout(std::time::Duration::from_secs(35)) {
        Ok(sent) => sent,
        Err(_) => {
            note(context, dedupe_key, Outcome::Failed, "alert completion timed out; delivery is unknown");
            false
        }
    }
}

/// JS `notifyOwnerQQ`: one private message to `ownerQQ`, at most one per `dedupe_key` per minute and ten per hour for
/// the whole process. The body is redacted and cut to 800 characters.
pub fn notify_owner(context: &RuntimeContext, cfg: &Value, text: &str, dedupe_key: &str) -> bool {
    notify_with(context, cfg, Budget::Js, text, dedupe_key, false, Vec::new())
}

/// JS `notifyAdmin` as the source intends it (accepted deviation 21; JS crashes before sending): a private message to
/// `ownerQQ`, deduplicated per `kind` (one per minute) and counted in the same ten-per-hour budget as the other
/// JS-texted alerts. Without `ownerQQ` it is only logged.
pub fn notify_admin(context: &RuntimeContext, cfg: &Value, kind: &str, text: &str) -> bool {
    notify_with(context, cfg, Budget::Js, text, &admin_key(kind), false, Vec::new())
}

/// A Rust-only owner notice (accepted deviation 22: DSH recovered, failed or queued chat turn, SnowLuma degraded or
/// recovered, interrupted events, inbound gaps, a turn resumed but never handed back). Same rule as the JS-texted alerts
/// (one per key per minute, ten per hour) on its own budget.
pub fn notify_notice(context: &RuntimeContext, cfg: &Value, text: &str, dedupe_key: &str) -> bool {
    notify_with(context, cfg, Budget::Notice, text, dedupe_key, false, Vec::new())
}

/// A memory-layer alert (`briefing::alert_empty`): one per key per minute, ten per hour, on its own budget.
pub fn notify_memory(context: &RuntimeContext, cfg: &Value, text: &str, dedupe_key: &str) -> bool {
    notify_with(context, cfg, Budget::Memory, text, dedupe_key, false, Vec::new())
}

/// Queue an already claimed gap notice. The worker checkpoints uncertainty before
/// I/O and records only confirmed delivery; known refusals become pending again.
/// Admission failures are released by the caller. Unknown sends are never retried.
pub fn notify_gap_notice(context: &RuntimeContext, cfg: &Value, text: &str, dedupe_key: &str, ids: &[i64]) -> bool {
    notify_with(context, cfg, Budget::Notice, text, dedupe_key, false, ids.to_vec())
}

/// Wait at most 35 seconds for one notice delivery attempt. False includes limiter skips,
/// failures and unknown completion. The caller owns any persisted retry policy; restore
/// callers do not retry, while QQ login episodes remain pending until confirmed delivery.
pub fn notify_notice_wait(context: &RuntimeContext, cfg: &Value, text: &str, dedupe_key: &str) -> bool {
    notify_with(context, cfg, Budget::Notice, text, dedupe_key, true, Vec::new())
}

#[cfg(test)]
#[path = "release_notice_tests.rs"]
mod release_notice_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_is_sent_once_per_minute_and_other_keys_are_independent() {
        let mut l = AlertLimiter::new();
        assert_eq!(l.check("s", "turn-error", 1_000_000), AlertVerdict::Send);
        assert_eq!(
            l.check("s", "turn-error", 1_059_999),
            AlertVerdict::SkipKeyWindow
        );
        assert_eq!(
            l.check("s", "crash", 1_030_000),
            AlertVerdict::Send,
            "another key has its own window"
        );
        assert_eq!(
            l.check("s", "turn-error", 1_060_000),
            AlertVerdict::Send,
            "exactly 60 s later is allowed (JS: now - last < 60000)"
        );
    }

    #[test]
    fn a_skipped_alert_does_not_extend_the_window() {
        let mut l = AlertLimiter::new();
        assert!(l.check("s", "k", 0).is_send());
        assert_eq!(l.check("s", "k", 59_000), AlertVerdict::SkipKeyWindow);
        assert!(
            l.check("s", "k", 60_000).is_send(),
            "the skip at 59 s must not have moved the window"
        );
    }

    #[test]
    fn scopes_do_not_share_key_windows_but_share_the_hourly_budget() {
        let mut l = AlertLimiter::new();
        assert!(l.check("a", "k", 100_000).is_send());
        assert!(l.check("b", "k", 100_001).is_send());
        assert_eq!(l.sent_in_last_hour(100_002), 2);
    }

    #[test]
    fn at_most_ten_per_sliding_hour() {
        let mut l = AlertLimiter::new();
        let t0 = 10_000_000i64;
        for i in 0..10 {
            assert!(
                l.check("s", &format!("k{i}"), t0 + i * 1000).is_send(),
                "alert {i}"
            );
        }
        assert_eq!(
            l.check("s", "k-extra", t0 + 11_000),
            AlertVerdict::SkipHourlyCap
        );
        assert_eq!(
            l.sent_in_last_hour(t0 + 11_000),
            10,
            "a skipped alert is not charged"
        );
        // The first one leaves the hour window.
        assert!(l.check("s", "k-extra", t0 + HOUR_MS).is_send());
        assert_eq!(
            l.check("s", "k-extra2", t0 + HOUR_MS + 1),
            AlertVerdict::SkipHourlyCap,
            "k1..k9 are still inside the hour"
        );
    }

    #[test]
    fn a_six_hour_outage_alerts_a_bounded_number_of_times() {
        let mut l = AlertLimiter::new();
        let mut sent = 0;
        let mut t = 0i64;
        while t < 6 * HOUR_MS {
            if l.check("s", "dsh-down", t).is_send() {
                sent += 1;
            }
            t += 5_000; // the health probe fails every 5 s, like JS checkDsh
        }
        assert!(sent <= 60, "10 per hour over 6 hours: {sent}");
        assert!(sent >= 6, "still at least one per hour: {sent}");
    }

    #[test]
    fn slot_memory_is_bounded() {
        let mut l = AlertLimiter::new();
        for i in 0..1000 {
            l.check("s", &format!("k{i}"), i * HOUR_MS); // far apart, so the hourly cap never triggers
        }
        assert!(l.last.len() <= MAX_SLOTS);
    }

    #[test]
    fn rust_only_notices_never_spend_the_budget_of_the_js_texted_alerts() {
        let (mut js, mut notice) = (AlertLimiter::new(), AlertLimiter::new());
        let t0 = 50_000_000i64;
        for i in 0..HOURLY_CAP as i64 {
            assert!(admit(&mut js, &mut notice, Budget::Notice, "s", &format!("n{i}"), t0 + i).is_send());
        }
        assert_eq!(
            admit(&mut js, &mut notice, Budget::Notice, "s", "n-extra", t0 + 100),
            AlertVerdict::SkipHourlyCap,
            "the notices have their own hourly cap"
        );
        assert!(
            admit(&mut js, &mut notice, Budget::Js, "s", "turn-error", t0 + 101).is_send(),
            "a JS-texted alert is still sent after ten notices"
        );
        assert_eq!(js.sent_in_last_hour(t0 + 102), 1, "only the JS-texted alert was charged to its budget");
    }

    #[test]
    fn a_js_texted_alert_never_spends_the_notice_budget_either() {
        let (mut js, mut notice) = (AlertLimiter::new(), AlertLimiter::new());
        for i in 0..HOURLY_CAP as i64 {
            assert!(admit(&mut js, &mut notice, Budget::Js, "s", &format!("j{i}"), 1_000 + i).is_send());
        }
        assert!(admit(&mut js, &mut notice, Budget::Notice, "s", "dsh-recovered", 2_000).is_send());
    }

    #[test]
    fn administrator_messages_are_deduplicated_per_kind_and_capped_per_hour() {
        let (mut js, mut notice) = (AlertLimiter::new(), AlertLimiter::new());
        let t0 = 70_000_000i64;
        assert!(admit(&mut js, &mut notice, Budget::Js, "s", &admin_key("turn-error"), t0).is_send());
        assert_eq!(
            admit(&mut js, &mut notice, Budget::Js, "s", &admin_key("turn-error"), t0 + 59_999),
            AlertVerdict::SkipKeyWindow,
            "the second message of a kind inside a minute is not sent"
        );
        assert!(
            admit(&mut js, &mut notice, Budget::Js, "s", &admin_key("not-accepted"), t0 + 1).is_send(),
            "another kind has its own window"
        );
        assert!(
            admit(&mut js, &mut notice, Budget::Js, "s", &admin_key("turn-error"), t0 + 60_000).is_send(),
            "a minute later the kind may be sent again"
        );
        assert_ne!(admin_key("turn-error"), "turn-error", "an administrator kind never shares a slot with the failover step alert");
    }

    #[test]
    fn one_worker_thread_delivers_every_alert_in_order() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(usize, std::thread::ThreadId)>>> = Arc::default();
        let sink = seen.clone();
        let outbox = Outbox::start(64, move |n: usize| sink.lock().unwrap().push((n, std::thread::current().id())));
        for n in 0..40 {
            assert!(outbox.submit(n));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seen.lock().unwrap().len() < 40 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().map(|(n, _)| *n).collect::<Vec<_>>(), (0..40).collect::<Vec<_>>());
        assert!(seen.iter().all(|(_, id)| *id == seen[0].1), "one thread served every message, not one per message");
        assert_ne!(seen[0].1, std::thread::current().id());
    }

    #[test]
    fn a_full_outbox_refuses_instead_of_growing() {
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let gate_rx = std::sync::Mutex::new(gate_rx);
        let outbox = Outbox::start(2, move |_: usize| {
            let _ = gate_rx.lock().unwrap().recv();
        });
        let accepted = (0..50).filter(|n| outbox.submit(*n)).count();
        assert!(accepted <= 4, "a bounded queue (+ the one in flight) accepted {accepted}");
        drop(gate_tx);
    }

    #[test]
    fn body_is_redacted_before_truncation_and_cut_to_800() {
        let long = format!("token=aaaaaaaaaaaa {}", "字".repeat(2000));
        let body = prepare_body(&long, |t| t.replace("token=aaaaaaaaaaaa", "***"));
        assert!(!body.contains("aaaaaaaaaaaa"));
        assert!(body.chars().count() <= 801, "{}", body.chars().count());
        assert!(body.starts_with("***"));
    }
}
