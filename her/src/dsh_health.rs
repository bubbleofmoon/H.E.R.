//! DSH dependency health and the bounded queue of work that waits for it (ticket 25, AC2/AC5/AC6).
//!
//! The frozen JS `checkDsh` asks `settings.describe` every 5 s, keeps `dshReady`, queues messages (at most 50, the
//! oldest dropped) while it is false and flushes them when it turns true. This is the same thing as a small state
//! machine plus a per-runtime registry the live bridge feeds from a probe loop and from the failures its own calls
//! see:
//!
//! * a transition is an edge: one run-record entry per down and per up, one administrator alert on recovery, no
//!   line per probe (a six-hour outage is two edges);
//! * the work that could not reach DSH waits in a [`BoundedQueue`] and is replayed only after recovery, through
//!   the same validity and revocation checks as any queued work (the caller re-checks; nothing is replayed from here);
//! * the live bridge publishes `dependency-health.json` (atomic, beside the heartbeat) for the console process, whose
//!   administrator status reports it.

use crate::dsh_credential::{Login, LoginCheck, LoginEdge, LoginWatch};
use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::{RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

/// JS `QUEUE_MAX`.
pub const QUEUE_MAX: usize = 50;
/// JS `setInterval(checkDsh, 5000)`.
pub const PROBE_INTERVAL_MS: u64 = 5000;
/// The published file is older than this when the live bridge is not running; the console then reports it unknown.
pub const SNAPSHOT_FRESH_MS: i64 = 60_000;
pub const HEALTH_FILE: &str = "dependency-health.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edge {
    /// No change.
    Steady,
    /// First observation of the process (the startup check).
    First { ready: bool },
    WentDown,
    /// Back up after `downtime_ms`.
    CameUp { downtime_ms: i64 },
}

/// `dshReady` with its history. `ready == None` until the first observation.
#[derive(Debug, Clone, Default)]
pub struct Health {
    ready: Option<bool>,
    since_ms: i64,
    down_since_ms: i64,
    last_error: String,
    failures: u32,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one observation (a probe or a call result). Returns the transition it caused.
    pub fn observe(&mut self, ok: bool, now_ms: i64, error: &str) -> Edge {
        let edge = match (self.ready, ok) {
            (None, ok) => Edge::First { ready: ok },
            (Some(true), false) => Edge::WentDown,
            (Some(false), true) => Edge::CameUp { downtime_ms: (now_ms - self.down_since_ms).max(0) },
            _ => Edge::Steady,
        };
        if ok {
            self.failures = 0;
            self.last_error.clear();
        } else {
            self.failures = self.failures.saturating_add(1);
            self.last_error = crate::activity::preview_chars(error, 200);
        }
        if edge != Edge::Steady {
            self.since_ms = now_ms;
            if !ok {
                self.down_since_ms = now_ms;
            }
        }
        self.ready = Some(ok);
        edge
    }

    /// Known down. Before the first observation DSH is presumed usable (work is not held on a guess).
    pub fn is_down(&self) -> bool {
        self.ready == Some(false)
    }

    pub fn ready(&self) -> Option<bool> {
        self.ready
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ready": self.ready,
            "sinceMs": if self.ready.is_some() { json!(self.since_ms) } else { Value::Null },
            "lastError": if self.last_error.is_empty() { Value::Null } else { json!(self.last_error) },
        })
    }
}

/// Work waiting for DSH: at most `cap` entries, the oldest dropped when full (JS `queue.shift()`).
#[derive(Debug)]
pub struct BoundedQueue<T> {
    cap: usize,
    items: VecDeque<T>,
}

impl<T> BoundedQueue<T> {
    pub fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), items: VecDeque::new() }
    }
    /// Append `item`; returns the oldest entry when the queue was full and had to drop it.
    pub fn push_back(&mut self, item: T) -> Option<T> {
        let dropped = if self.items.len() >= self.cap { self.items.pop_front() } else { None };
        self.items.push_back(item);
        dropped
    }
    /// Put `item` back at the head (a replay that failed again keeps its place); drops the newest when over the cap.
    pub fn push_front(&mut self, item: T) -> Option<T> {
        self.items.push_front(item);
        if self.items.len() > self.cap {
            self.items.pop_back()
        } else {
            None
        }
    }
    pub fn pop_front(&mut self) -> Option<T> {
        self.items.pop_front()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.items.retain(keep);
    }
}

// ---- the per-runtime registry --------------------------------------------------------------------

fn registry() -> &'static Mutex<HashMap<String, Health>> {
    static R: OnceLock<Mutex<HashMap<String, Health>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn scope_of(context: &RuntimeContext) -> String {
    context.runtime_root().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "legacy".to_string())
}

/// The DSH base URL of a selected runtime.
pub fn dsh_base(context: &RuntimeContext) -> Option<String> {
    context.dependency_url("dsh").map(str::to_string)
}

pub fn is_down(context: &RuntimeContext) -> bool {
    registry().lock().unwrap_or_else(|p| p.into_inner()).get(&scope_of(context)).is_some_and(Health::is_down)
}

/// The last observed state: `Some(true)` ready, `Some(false)` down, `None` not observed yet.
pub fn ready(context: &RuntimeContext) -> Option<bool> {
    registry().lock().unwrap_or_else(|p| p.into_inner()).get(&scope_of(context)).and_then(Health::ready)
}

/// Feed an observation into the runtime's health and act on the edge: run-record entry, one recovery alert.
pub fn observe(context: &RuntimeContext, cfg: &Value, ok: bool, error: &str, from_probe: bool) -> Edge {
    let now = crate::parity_clock::now_ms();
    let edge = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(scope_of(context))
        .or_default()
        .observe(ok, now, error);
    match &edge {
        Edge::Steady => {}
        Edge::First { ready } => crate::recovery_log::record(
            context,
            if from_probe { Trigger::Startup } else { Trigger::Turn },
            Component::Dsh,
            "startup-check",
            if *ready { Outcome::Ok } else { Outcome::Failed },
            if *ready { "settings/describe answered" } else { error },
        ),
        Edge::WentDown => {
            crate::safe_println!("[health] ⚠️ DSH 不可用（重启中？），要提交的消息将排队等待：{}", crate::activity::redact(context, error));
            crate::recovery_log::record(
                context,
                if from_probe { Trigger::Timer } else { Trigger::Turn },
                Component::Dsh,
                "down",
                Outcome::Failed,
                error,
            );
        }
        Edge::CameUp { downtime_ms } => {
            crate::safe_println!("[health] DSH 已恢复（中断约 {} 秒）", downtime_ms / 1000);
            crate::recovery_log::record(
                context,
                Trigger::Recovery,
                Component::Dsh,
                "up",
                Outcome::Ok,
                &format!("settings/describe answered again after about {} s", downtime_ms / 1000),
            );
            crate::admin_alert::notify_notice(
                context,
                cfg,
                &format!("DSH 已恢复（中断约 {} 秒），排队的消息将重新核对权限后补投", downtime_ms / 1000),
                "dsh-recovered",
            );
        }
    }
    edge
}

/// A DSH call of the bridge failed; `error` is the call's error text. Only a failure to reach DSH counts as down: a
/// refusal or a model error proves DSH answered.
pub fn note_call_failure(context: &RuntimeContext, cfg: &Value, error: &str) {
    if is_unreachable(error) {
        observe(context, cfg, false, error, false);
    }
}

/// A DSH call succeeded.
pub fn note_call_success(context: &RuntimeContext, cfg: &Value) {
    if is_down(context) {
        observe(context, cfg, true, "", false);
    }
}

/// The error text of a call that never got an answer from DSH (as opposed to one DSH refused).
pub fn is_unreachable(error: &str) -> bool {
    // A proxy or a restarting DSH answers with something that is not a DSH response: it did not answer as DSH.
    error.contains("响应不是 JSON")
        || error.contains("响应类型不对")
        || error.contains("HTTP 连接失败")
        || error.contains("连接被拒")
        || error.contains("WebSocket 连接失败")
        || error.contains("FollowStream 握手超时")
}

/// One probe (`settings/describe`, the call the frozen JS uses; blocking) and what it proved about the DSH login: a freshly minted cookie, refreshed once after a rejection
/// (`dsh_credential::check_login`). `deadline` bounds both requests.
pub fn probe_login(context: &RuntimeContext, deadline: Option<std::time::Instant>) -> LoginCheck<()> {
    let unchecked = |result| LoginCheck { result, login: Login::Unchecked };
    let Some(base) = dsh_base(context) else {
        return unchecked(Err("no DSH dependency configured".to_string()));
    };
    let rpc = match crate::dsh::Rpc::connect_with_runtime(context, &base) {
        Ok(rpc) => rpc,
        Err(error) => return unchecked(Err(error)),
    };
    let check = crate::dsh_credential::check_login(rpc.credential(), crate::dsh_credential::repeat_pause(deadline), || match deadline {
        Some(deadline) => rpc.call_with_deadline("settings/describe", json!({}), deadline),
        None => rpc.call_without_refresh("settings/describe", json!({})),
    });
    LoginCheck { result: probe_verdict(check.result), login: check.login }
}

fn login_registry() -> &'static Mutex<HashMap<String, LoginWatch>> {
    static R: OnceLock<Mutex<HashMap<String, LoginWatch>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Feed one probe's login observation into the runtime's watch: the second rejected fresh cookie in a row is one
/// run-record entry and one administrator alert naming the likely causes; nothing more until a cookie is accepted.
/// The alert is owed until the notice budget accepts it: it is offered on the failing edge, then once a minute.
pub fn observe_login(context: &RuntimeContext, cfg: &Value, login: &Login) -> LoginEdge {
    observe_login_via(context, login, crate::parity_clock::now_ms(), &mut |text| {
        crate::admin_alert::notify_notice(context, cfg, text, "dsh-login-rejected")
    })
}

/// `notify` returns whether the alert was handed to the sender; `now_ms` paces the offers.
fn observe_login_via(context: &RuntimeContext, login: &Login, now_ms: i64, notify: &mut dyn FnMut(&str) -> bool) -> LoginEdge {
    let scope = scope_of(context);
    let (edge, offer) = {
        let mut registry = login_registry().lock().unwrap_or_else(|p| p.into_inner());
        let watch = registry.entry(scope.clone()).or_default();
        let edge = watch.observe(login);
        (edge, watch.alert_due(now_ms))
    };
    match edge {
        LoginEdge::Steady => {}
        LoginEdge::Failing => {
            crate::safe_println!("[health] ⚠️ {}", crate::dsh_credential::REJECTED_TWICE_ALERT);
            crate::recovery_log::record(
                context,
                Trigger::Timer,
                Component::Dsh,
                "login-rejected",
                Outcome::Failed,
                crate::dsh_credential::REJECTED_TWICE_REASON,
            );
        }
        LoginEdge::Recovered => {
            crate::safe_println!("[health] DSH 登录已恢复：新铸的 Cookie 被接受");
            crate::recovery_log::record(
                context,
                Trigger::Recovery,
                Component::Dsh,
                "login-accepted",
                Outcome::Ok,
                "DSH accepted a freshly minted login cookie again",
            );
        }
    }
    if offer && notify(crate::dsh_credential::REJECTED_TWICE_ALERT) {
        if let Some(watch) = login_registry().lock().unwrap_or_else(|p| p.into_inner()).get_mut(&scope) {
            watch.alert_delivered();
        }
    }
    edge
}

/// JS `checkDsh` only treats a call that throws as "DSH not ready"; its SDK returns a refusal as a result, so a DSH that
/// answers `settings/describe` with an error is up (it answered). Only a call that got no answer is down.
pub fn probe_verdict(result: Result<Value, String>) -> Result<(), String> {
    match result {
        Ok(_) => Ok(()),
        Err(error) if error.contains(" 被拒：") => Ok(()),
        Err(error) => Err(error),
    }
}

/// The published health file content: DSH and the gateway.
/// Host wall clock for file freshness: two processes (the bridge and the console) must agree on it even when a parity
/// build gives each its own fixed-epoch clock.
fn wall_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn snapshot(context: &RuntimeContext) -> Value {
    let dsh = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&scope_of(context))
        .map(Health::to_json)
        .unwrap_or_else(|| Health::new().to_json());
    json!({
        "ts": wall_ms(),
        "pid": std::process::id(),
        "dsh": dsh,
        "snowluma": crate::gateway_health::snapshot_json(crate::parity_clock::now_ms()),
    })
}

/// Publish the snapshot for the console process (best effort).
pub fn publish(context: &RuntimeContext) {
    if !crate::recovery_log::enabled(context) {
        return;
    }
    if let Err(error) = context.write_atomic(RuntimeArea::Data, HEALTH_FILE, snapshot(context).to_string().as_bytes()) {
        eprintln!("[health] 写健康快照失败：{error}");
    }
}

/// What the administrator status reports from the live bridge: `(dshReady, snowluma snapshot)` read from the file the
/// bridge publishes. `None` when no fresh snapshot exists (no live bridge running): the caller then probes DSH itself and
/// reports the gateway as unknown.
pub fn published_status(context: &RuntimeContext) -> Option<(bool, Value)> {
    let now = wall_ms();
    let v: Value = context
        .read_optional_text(RuntimeArea::Data, HEALTH_FILE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())?;
    if now - v.get("ts").and_then(Value::as_i64).unwrap_or(0) > SNAPSHOT_FRESH_MS {
        return None;
    }
    Some((
        v.pointer("/dsh/ready").and_then(Value::as_bool).unwrap_or(false),
        v.get("snowluma").cloned().unwrap_or_else(crate::gateway_health::unknown_json),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotfix128_fix1_health_error_masks_before_the_200_character_cut() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let text = format!("{}{token} trailing", "x ".repeat(95));
        let mut health = Health::new();
            health.observe(false, 1, &text);
            let snapshot = health.to_json();
            let preview = snapshot["lastError"].as_str().unwrap();
        assert_eq!(preview, format!("{}*** traili", "x ".repeat(95)));
    }


    #[test]
    fn a_long_outage_is_two_edges_and_recovery_reports_the_downtime() {
        let mut h = Health::new();
        assert_eq!(h.observe(true, 1_000, ""), Edge::First { ready: true });
        assert_eq!(h.observe(false, 6_000, "connection refused"), Edge::WentDown);
        for t in 1..2000 {
            assert_eq!(h.observe(false, 6_000 + t * 5_000, "connection refused"), Edge::Steady, "probe {t}");
        }
        assert!(h.is_down());
        assert_eq!(h.failures(), 2000);
        let up_at = 6_000 + 2000 * 5_000;
        assert_eq!(h.observe(true, up_at, ""), Edge::CameUp { downtime_ms: 2000 * 5_000 });
        assert!(!h.is_down());
        assert_eq!(h.failures(), 0);
        assert_eq!(h.observe(true, up_at + 5_000, ""), Edge::Steady);
    }

    #[test]
    fn dsh_is_presumed_usable_until_it_has_been_observed_down() {
        let mut h = Health::new();
        assert!(!h.is_down());
        assert_eq!(h.ready(), None);
        assert_eq!(h.observe(false, 10, "refused"), Edge::First { ready: false });
        assert!(h.is_down(), "a failed first check is a known outage");
        assert_eq!(h.observe(true, 20, ""), Edge::CameUp { downtime_ms: 10 });
    }

    #[test]
    fn the_queue_holds_fifty_and_drops_the_oldest() {
        let mut q = BoundedQueue::new(QUEUE_MAX);
        for i in 0..QUEUE_MAX {
            assert_eq!(q.push_back(i), None);
        }
        assert_eq!(q.push_back(QUEUE_MAX), Some(0), "the oldest is dropped");
        assert_eq!(q.len(), QUEUE_MAX);
        assert_eq!(q.pop_front(), Some(1));
        assert_eq!(q.push_front(1), None, "a replay that failed again keeps its place");
        assert_eq!(q.pop_front(), Some(1));
    }

    #[test]
    fn only_an_unanswered_call_marks_dsh_down() {
        assert!(is_unreachable("HTTP 连接失败：session/prompt"));
        assert!(is_unreachable("响应不是 JSON（session/create）：expected value；原文 <html>503"));
        assert!(!is_unreachable("session/prompt 被拒：not-found: gone"));
        assert!(!is_unreachable("401 未授权：session/create"));
    }

    /// Review (minor): the first observation of the process is a startup check only when the probe made it; a failed turn
    /// call that happens to be first is a turn record.
    #[test]
    fn a_first_observation_from_a_failed_call_is_not_recorded_as_the_startup_check() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-health-trigger-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let edge = observe(&context, &json!({}), false, "HTTP 连接失败：session/create", false);
        assert_eq!(edge, Edge::First { ready: false });
        let page = crate::recovery_log::page(&context, &crate::run_record::Query::default()).unwrap();
        let first = page.records.iter().find(|r| r.event == "startup-check").expect("the first observation is recorded");
        assert_eq!(format!("{:?}", first.trigger), "Turn", "a failed call made it, not the probe");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_dsh_that_refuses_the_probe_is_up_and_only_silence_is_down() {
        assert_eq!(probe_verdict(Ok(json!({}))), Ok(()));
        assert_eq!(probe_verdict(Err("settings/describe 被拒：unknown-method: no such method".into())), Ok(()), "DSH answered");
        assert!(probe_verdict(Err("HTTP 连接失败：settings/describe".into())).is_err());
        assert!(probe_verdict(Err("响应不是 JSON（settings/describe）：eof".into())).is_err());
        assert!(probe_verdict(Err("401 未授权：settings/describe".into())).is_err());
    }

    fn login_fixture(tag: &str) -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-health-{tag}-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    /// Review S1: a notice the alert budget refused (no ownerQQ, hourly cap, per-key window) is not the streak's alert.
    #[test]
    fn a_streak_that_starts_while_the_notice_budget_is_exhausted_alerts_once_when_it_frees() {
        let (root, context) = login_fixture("owed");
        let rejected = Login::Rejected { in_a_row: 2 };
        let sent = std::cell::RefCell::new(Vec::<String>::new());
        let mut budget_free = false;
        let now = std::cell::Cell::new(5_000_000_i64);
        let probe = |login: &Login, budget_free: bool| {
            observe_login_via(&context, login, now.get(), &mut |text| {
                sent.borrow_mut().push(text.to_string());
                budget_free
            })
        };
        assert_eq!(probe(&rejected, budget_free), LoginEdge::Failing);
        assert_eq!(sent.borrow().len(), 1, "the first offer on the failing edge is immediate");
        // Probes run every 5 s: a refused alert is not offered again within a minute.
        for _ in 0..11 {
            now.set(now.get() + 5_000);
            assert_eq!(probe(&rejected, budget_free), LoginEdge::Steady);
        }
        assert_eq!(sent.borrow().len(), 1, "refused offers are not repeated every probe");
        now.set(now.get() + 5_000);
        assert_eq!(probe(&rejected, budget_free), LoginEdge::Steady);
        assert_eq!(sent.borrow().len(), 2, "owed: offered again 60 s after the last offer");
        budget_free = true;
        for _ in 0..11 {
            now.set(now.get() + 5_000);
            probe(&rejected, budget_free);
        }
        assert_eq!(sent.borrow().len(), 2, "still within the interval");
        now.set(now.get() + 5_000);
        assert_eq!(probe(&rejected, budget_free), LoginEdge::Steady);
        assert_eq!(sent.borrow().len(), 3, "accepted at the next offer");
        for _ in 0..30 {
            now.set(now.get() + 5_000);
            probe(&rejected, budget_free);
            probe(&Login::Unchecked, budget_free);
        }
        assert_eq!(sent.borrow().len(), 3, "delivered once: never a second alert in the streak");
        assert!(sent.borrow().iter().all(|text| text == crate::dsh_credential::REJECTED_TWICE_ALERT));

        // The next streak owes its own alert; a recovery sends none.
        now.set(now.get() + 5_000);
        assert_eq!(probe(&Login::Accepted, budget_free), LoginEdge::Recovered);
        assert_eq!(sent.borrow().len(), 3);
        now.set(now.get() + 5_000);
        assert_eq!(probe(&rejected, budget_free), LoginEdge::Failing);
        assert_eq!(sent.borrow().len(), 4, "a new streak is offered at once");

        // The failing edge is one run-record entry however often the alert is offered.
        let page = crate::recovery_log::page(&context, &crate::run_record::Query::default()).unwrap();
        assert_eq!(page.records.iter().filter(|r| r.event == "login-rejected").count(), 2, "one per streak");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
