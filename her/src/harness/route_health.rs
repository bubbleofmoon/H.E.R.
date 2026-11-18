//! Route health (#174): what each route of the configured chain did on its last observed request, kept per runtime in
//! `data/route-health/state.json` (outside backups: a live observation, never restored), and the all-routes-failed
//! alert. Harness turns observe every answer and every route failure (`harness.rs`, `tool_loop` and
//! `ChainState::missed`); the pipeline offers the alert after a failed turn's own route reports (`pipeline.rs`,
//! `ask_harness`); the health check reports each route's state beside its parts (`health_check.rs`), never as a part,
//! so a provider outage never fails HER's health or rolls back a release.
#![allow(dead_code)]
// #174 step 2 wires this module into harness.rs, the pipeline and the health check; remove both lines then.
use super::routes::{self, RouteSpec};
use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::{RuntimeArea, RuntimeContext};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The state file, under the selected runtime's data folder.
pub(crate) const FILE: &str = "route-health/state.json";
/// The cross-process gate of every change to the state file. Every change opens it anew, so it also orders the turns of
/// one process.
const GATE: &str = "locks/route-health.lock";
/// How long a change waits for the gate before it gives up (recorded; the request goes on).
const GATE_WAIT: Duration = Duration::from_secs(2);
/// The largest state file read; a larger one is unreadable and starts over. A chain has at most `routes::MAX_ROUTES`
/// routes, so a state HER wrote stays far below it.
const MAX_STATE_BYTES: u64 = 64 * 1024;
/// The notice key of the all-routes-failed alert (`admin_alert::notify_notice`: one per runtime per minute).
pub(crate) const ALERT_KEY: &str = "model-routes-all-failed";
/// The failure classes a state may carry (`model_failure::Failure::class`, without `refusal`: see `apply`); anything
/// else reads as `backend`.
const CLASSES: [&str; 5] = ["quota", "timeout", "network", "empty-response", "backend"];

/// What one request on a route ended with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Seen {
    /// A usable answer: text or tool calls.
    Answered,
    /// A miss that failed over or ended the turn (`routes::Next::FailOver`, `routes::Next::Fail`), by its class. A miss
    /// retried on the same route is not a route failure, and a refusal is content, not route health (`apply`).
    Failed { class: &'static str },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct State {
    v: u8,
    /// The SHA-256 of the chain's fingerprint (`routes::chain_signature`): any change to the chain starts over.
    chain: String,
    routes: BTreeMap<String, Entry>,
    #[serde(rename = "allFailed", default, skip_serializing_if = "Option::is_none")]
    all_failed: Option<Episode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    /// `ok` or `failed`.
    state: String,
    /// When the route entered this state, in ms.
    since: i64,
    /// The class of its last failure, while failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    class: Option<String>,
}

/// Every route of the chain has failed since `since`, the last time with `class`; `alerted` once the owner's alert was
/// accepted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Episode {
    since: i64,
    class: String,
    alerted: bool,
}

fn chain_hash(chain: &[RouteSpec]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(routes::chain_signature(chain).as_bytes()))
}

fn known_class(class: &str) -> &'static str {
    CLASSES.into_iter().find(|known| *known == class).unwrap_or("backend")
}

/// One observation of route `at` of `chain`, applied to `state` at `now` ms. A state of another chain or format starts
/// over. A route's `since` moves only when its state changes, so an answer after an answer changes nothing. When every
/// route of the chain is failed, an episode starts, owed (not alerted); a later failure only updates its class, and any
/// answer ends it. A refusal (class `refusal`) is no route failure: the route answered, with content the model chose to
/// withhold, so it changes nothing.
fn apply(state: &mut State, chain: &[RouteSpec], at: usize, seen: Seen, now: i64) {
    if matches!(seen, Seen::Failed { class: "refusal" }) {
        return;
    }
    let hash = chain_hash(chain);
    if state.v != 1 || state.chain != hash {
        *state = State {
            v: 1,
            chain: hash,
            ..State::default()
        };
    }
    let Some(route) = chain.get(at) else {
        return;
    };
    let (name, class) = match seen {
        Seen::Answered => ("ok", None),
        Seen::Failed { class } => ("failed", Some(known_class(class).to_string())),
    };
    let same = state.routes.get(&route.id).is_some_and(|entry| entry.state == name);
    if same {
        if let Some(entry) = state.routes.get_mut(&route.id) {
            entry.class = class.clone();
        }
    } else {
        state.routes.insert(
            route.id.clone(),
            Entry {
                state: name.to_string(),
                since: now,
                class: class.clone(),
            },
        );
    }
    let every = chain
        .iter()
        .all(|route| state.routes.get(&route.id).is_some_and(|entry| entry.state == "failed"));
    if !every {
        state.all_failed = None;
    } else if let Some(class) = class {
        match state.all_failed.as_mut() {
            Some(episode) => episode.class = class,
            None => {
                state.all_failed = Some(Episode {
                    since: now,
                    class,
                    alerted: false,
                })
            }
        }
    }
}

/// The stored state, or `None` when it is missing, larger than `MAX_STATE_BYTES` or unreadable.
fn read(context: &RuntimeContext) -> Option<State> {
    if context.optional_file_len(RuntimeArea::Data, FILE).ok()? > MAX_STATE_BYTES {
        return None;
    }
    let text = context.read_optional_text(RuntimeArea::Data, FILE).ok()??;
    serde_json::from_str(&text).ok()
}

/// The gate, waiting at most `GATE_WAIT` while another change holds it.
fn gate(context: &RuntimeContext) -> Result<std::fs::File, String> {
    let deadline = Instant::now() + GATE_WAIT;
    loop {
        match context.open_locked_file(RuntimeArea::Data, GATE) {
            Ok(file) => return Ok(file),
            Err(error) if error == "runtime file is already locked" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Runs `change` on the stored state under the gate and stores the result when it changed. A missing or unreadable
/// state starts as the empty one.
fn with_state<T>(context: &RuntimeContext, change: impl FnOnce(&mut State) -> T) -> Result<T, String> {
    let _gate = gate(context)?;
    let before = read(context);
    let mut state = before.clone().unwrap_or_default();
    let out = change(&mut state);
    let changed = match &before {
        Some(before) => *before != state,
        None => state != State::default(),
    };
    if changed {
        let mut bytes = serde_json::to_vec(&state).map_err(|_| "route health could not be serialized".to_string())?;
        bytes.push(b'\n');
        context.write_atomic(RuntimeArea::Data, FILE, &bytes)?;
    }
    Ok(out)
}

fn unrecorded(context: &RuntimeContext, what: &str, error: &str) {
    crate::recovery_log::record(
        context,
        Trigger::Turn,
        Component::ModelChain,
        "route-health-unrecorded",
        Outcome::Failed,
        &format!("{what} was not written ({error})"),
    );
}

/// Records what a request on route `at` of `chain` ended with (`apply`). A selected runtime only; a failed write goes
/// to the run record (component `model-chain`, event `route-health-unrecorded`) and never fails the request.
pub(crate) fn observe(context: &RuntimeContext, chain: &[RouteSpec], at: usize, seen: Seen) {
    if !context.is_isolated() {
        return;
    }
    let now = crate::parity_clock::now_ms();
    if let Err(error) = with_state(context, |state| apply(state, chain, at, seen, now)) {
        let route = chain.get(at).map_or("", |route| route.id.as_str());
        unrecorded(context, &format!("the health of route {route}"), &error);
    }
}

/// The all-routes-failed alert. Model-visible text (the owner's chat can be read back to the model): it names the
/// conversation whose turn failed last, the number of routes and the last failure class, never a route id, a model, a
/// maker, provider text or a secret.
pub(crate) fn alert_text(key: &str, routes: usize, class: &str) -> String {
    format!(
        "[{key}] 所有回复线路都失败了：{routes} 条线路最近一次请求都没有成功（最后一次：{}）。有线路恢复之前不再提醒。",
        known_class(class)
    )
}

/// After a harness turn of conversation `key` with a route failure: sends the all-routes-failed alert when an episode of
/// the configured chain is owed, and marks it alerted once the notice queue accepted it (`admin_alert::notify_notice`).
/// A refused notice (no `ownerQQ`, the per-minute key window, the hourly cap or a full queue) stays owed for the next
/// failed turn; an episode is alerted once, until an answer ends it. Returns whether the alert was accepted now.
pub(crate) fn alert_if_owed(context: &RuntimeContext, cfg: &Value, key: &str) -> bool {
    offer(context, cfg, key, |text| crate::admin_alert::notify_notice(context, cfg, text, ALERT_KEY))
}

/// `alert_if_owed` with `notify` as the notice queue: it is offered the alert text and answers whether it accepted it
/// (tests pass their own).
fn offer(context: &RuntimeContext, cfg: &Value, key: &str, notify: impl FnOnce(&str) -> bool) -> bool {
    if !context.is_isolated() {
        return false;
    }
    let Ok(chain) = routes::chain(cfg) else {
        return false;
    };
    let hash = chain_hash(&chain);
    let result = with_state(context, |state| {
        if state.v != 1 || state.chain != hash {
            return None;
        }
        let episode = state.all_failed.as_mut().filter(|episode| !episode.alerted)?;
        let sent = notify(&alert_text(key, chain.len(), &episode.class));
        episode.alerted = sent;
        Some(sent)
    });
    match result {
        Ok(Some(sent)) => {
            let (outcome, reason) = if sent {
                (Outcome::Ok, "every route failed; the owner's alert was accepted")
            } else {
                (Outcome::Skipped, "every route failed; the owner's alert was refused and stays owed")
            };
            crate::recovery_log::record(context, Trigger::Turn, Component::ModelChain, "routes-all-failed", outcome, &format!("{key}: {reason}"));
            sent
        }
        Ok(None) => false,
        Err(error) => {
            unrecorded(context, "the all-routes-failed alert state", &error);
            false
        }
    }
}

/// The health check's view of the configured chain (`health_check::check`, `modelRoutes`): each route in chain order,
/// `ok` (its last observed request answered), `failed` with its class (its last observed request failed over or ended a
/// turn) or `unknown` (nothing observed for this chain), each known state with `since`, and `allFailed`. `None` when no
/// harness model is configured; an invalid chain gives its `error` and no routes; an unreadable state gives an `error`
/// and reads as unknown. It reads only the state file: never a model.
pub(crate) fn report(context: &RuntimeContext, cfg: &Value) -> Option<Value> {
    let chain = match routes::chain(cfg) {
        Ok(chain) => chain,
        Err(error) if error == routes::MODEL_NOT_CONFIGURED => return None,
        Err(error) => return Some(json!({"allFailed": false, "routes": [], "error": error})),
    };
    let present = context.optional_file_len(RuntimeArea::Data, FILE).map(|len| len > 0);
    let state = read(context);
    let error = match (&present, &state) {
        (Ok(false), _) | (Ok(true), Some(_)) => None,
        _ => Some("route health is unreadable"),
    };
    let hash = chain_hash(&chain);
    let state = state.filter(|state| state.v == 1 && state.chain == hash);
    let routes: Vec<Value> = chain
        .iter()
        .map(|route| match state.as_ref().and_then(|state| state.routes.get(&route.id)) {
            Some(entry) if entry.state == "failed" => json!({"id": route.id, "state": "failed", "since": entry.since,
                "class": known_class(entry.class.as_deref().unwrap_or(""))}),
            Some(entry) if entry.state == "ok" => json!({"id": route.id, "state": "ok", "since": entry.since}),
            _ => json!({"id": route.id, "state": "unknown"}),
        })
        .collect();
    let mut out = json!({
        "allFailed": state.as_ref().is_some_and(|state| state.all_failed.is_some()),
        "routes": routes,
    });
    if let Some(error) = error {
        out["error"] = json!(error);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain_of(ids: &[&str]) -> Vec<RouteSpec> {
        let routes: Vec<Value> = ids.iter().map(|id| json!({"id": id, "model": format!("fixture-gpt-{id}")})).collect();
        routes::chain(&json!({"harness": {"routes": routes}})).unwrap()
    }

    fn failed(class: &'static str) -> Seen {
        Seen::Failed { class }
    }

    fn entry(state: &State, id: &str) -> Option<(String, i64, Option<String>)> {
        state.routes.get(id).map(|entry| (entry.state.clone(), entry.since, entry.class.clone()))
    }

    struct Fixture {
        root: std::path::PathBuf,
        context: RuntimeContext,
        cfg: Value,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        /// A selected runtime with `cfg` as its configuration; every dependency points at a closed port, so an
        /// accepted alert fails to send in the background without reaching anything.
        fn new(tag: &str, cfg: Value) -> Self {
            let root = crate::install_root::development_root(
                std::env::temp_dir().join(format!("her-174-{tag}-{}", crate::turn_ledger::mint_id(""))),
            );
            for dir in ["config", "data", "resources"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
            std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            std::fs::write(
                root.join("dependencies.json"),
                r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
            )
            .unwrap();
            let context = RuntimeContext::from_root(&root).unwrap();
            Self { root, context, cfg }
        }

        fn stored(&self) -> State {
            serde_json::from_str(&std::fs::read_to_string(self.root.join("data").join(FILE)).unwrap()).unwrap()
        }

        fn records(&self, event: &str) -> Vec<String> {
            crate::recovery_log::page(&self.context, &crate::run_record::Query::default())
                .unwrap()
                .records
                .into_iter()
                .filter(|record| record.event == event)
                .map(|record| record.reason)
                .collect()
        }
    }

    fn routes_cfg(ids: &[&str], owner: bool) -> Value {
        let routes: Vec<Value> = ids.iter().map(|id| json!({"id": id, "model": format!("fixture-gpt-{id}")})).collect();
        let mut cfg = json!({"consoleToken": "synthetic-console-token-174", "harness": {"routes": routes}});
        if owner {
            cfg["ownerQQ"] = json!(8808);
        }
        cfg
    }

    #[test]
    fn route_health_each_route_keeps_its_last_outcome_and_since_moves_only_on_a_change() {
        let chain = chain_of(&["primary-route", "backup-route"]);
        let mut state = State::default();
        apply(&mut state, &chain, 0, Seen::Answered, 100);
        assert_eq!(entry(&state, "primary-route"), Some(("ok".into(), 100, None)));
        assert_eq!(entry(&state, "backup-route"), None, "a route never tried has no state");
        let unchanged = state.clone();
        apply(&mut state, &chain, 0, Seen::Answered, 200);
        assert_eq!(state, unchanged, "an answer after an answer changes nothing, so nothing is written");
        apply(&mut state, &chain, 0, failed("quota"), 300);
        assert_eq!(entry(&state, "primary-route"), Some(("failed".into(), 300, Some("quota".into()))));
        apply(&mut state, &chain, 0, failed("backend"), 400);
        assert_eq!(entry(&state, "primary-route"), Some(("failed".into(), 300, Some("backend".into()))), "the class follows the last failure");
        apply(&mut state, &chain, 0, Seen::Answered, 500);
        assert_eq!(entry(&state, "primary-route"), Some(("ok".into(), 500, None)));
        apply(&mut state, &chain, 7, Seen::Answered, 600);
        assert_eq!(state.routes.len(), 1, "a route past the chain is ignored");
        apply(&mut state, &chain, 1, failed("no-such-class"), 700);
        assert_eq!(entry(&state, "backup-route"), Some(("failed".into(), 700, Some("backend".into()))), "an unknown class reads as backend");
    }

    #[test]
    fn route_health_every_route_failed_starts_one_owed_episode_that_an_answer_ends() {
        let chain = chain_of(&["primary-route", "backup-route"]);
        let mut state = State::default();
        apply(&mut state, &chain, 0, failed("quota"), 100);
        assert_eq!(state.all_failed, None, "one of two routes failed");
        apply(&mut state, &chain, 1, failed("timeout"), 200);
        assert_eq!(state.all_failed, Some(Episode { since: 200, class: "timeout".into(), alerted: false }));
        state.all_failed.as_mut().unwrap().alerted = true;
        apply(&mut state, &chain, 0, failed("backend"), 300);
        assert_eq!(
            state.all_failed,
            Some(Episode { since: 200, class: "backend".into(), alerted: true }),
            "a later failure keeps the episode and its alert"
        );
        apply(&mut state, &chain, 1, Seen::Answered, 400);
        assert_eq!(state.all_failed, None, "an answer ends the episode");
        apply(&mut state, &chain, 1, failed("quota"), 500);
        assert_eq!(
            state.all_failed,
            Some(Episode { since: 500, class: "quota".into(), alerted: false }),
            "a new episode is owed again"
        );
        let single = chain_of(&["only-route"]);
        let mut state = State::default();
        apply(&mut state, &single, 0, failed("backend"), 100);
        assert!(state.all_failed.is_some(), "the one route of a one-route chain failing is every route failing");
    }

    #[test]
    fn route_health_a_refusal_is_no_route_failure() {
        let chain = chain_of(&["primary-route", "backup-route"]);
        let mut state = State::default();
        apply(&mut state, &chain, 0, failed("refusal"), 100);
        assert_eq!(state, State::default(), "a refusal on a fresh state records nothing, so nothing is written");
        apply(&mut state, &chain, 0, Seen::Answered, 200);
        let answered = state.clone();
        apply(&mut state, &chain, 0, failed("refusal"), 300);
        assert_eq!(state, answered, "a route that answered stays ok after a refusal");
        apply(&mut state, &chain, 0, failed("quota"), 400);
        apply(&mut state, &chain, 1, failed("timeout"), 500);
        state.all_failed.as_mut().unwrap().alerted = true;
        let outage = state.clone();
        apply(&mut state, &chain, 1, failed("refusal"), 600);
        apply(&mut state, &chain, 0, failed("refusal"), 700);
        assert_eq!(state, outage, "a refusal leaves a failed route and its episode as they were");
    }

    #[test]
    fn route_health_a_changed_chain_or_format_starts_over() {
        let chain = chain_of(&["primary-route", "backup-route"]);
        let mut state = State::default();
        apply(&mut state, &chain, 0, failed("quota"), 100);
        apply(&mut state, &chain, 1, failed("quota"), 100);
        let reordered = chain_of(&["backup-route", "primary-route"]);
        apply(&mut state, &reordered, 0, Seen::Answered, 200);
        assert_eq!(state.chain, chain_hash(&reordered));
        assert_eq!((state.routes.len(), state.all_failed.clone()), (1, None), "another chain's routes and episode are dropped");
        let mut changed = chain.clone();
        changed[0].max_output_tokens = 1000;
        assert_ne!(chain_hash(&chain), chain_hash(&changed), "any route setting is part of the chain");
        state.v = 2;
        apply(&mut state, &reordered, 1, Seen::Answered, 300);
        assert_eq!((state.v, state.routes.len()), (1, 1), "a state of another format starts over");
    }

    #[test]
    fn route_health_the_alert_names_no_route_model_maker_or_secret() {
        let text = alert_text("group:101", 2, "quota");
        assert_eq!(text, "[group:101] 所有回复线路都失败了：2 条线路最近一次请求都没有成功（最后一次：quota）。有线路恢复之前不再提醒。");
        assert!(crate::upstream_capture::denied_terms(&text).is_empty(), "{text}");
        for hidden in ["primary-route", "fixture-gpt", "harness", "openai", "synthetic"] {
            assert!(!text.contains(hidden), "{hidden}: {text}");
        }
        assert!(alert_text("group:101", 1, "provider said: secret-value").contains("最后一次：backend"), "only a known class is named");
    }

    #[test]
    fn route_health_the_alert_is_owed_until_accepted_and_sent_once_per_episode() {
        let ids = ["primary-route", "backup-route"];
        let chain = chain_of(&ids);
        let fixture = Fixture::new("alert", routes_cfg(&ids, false));
        let sent = std::cell::RefCell::new(Vec::new());
        let accept = |text: &str| {
            sent.borrow_mut().push(text.to_string());
            true
        };
        let refuse = |_: &str| false;
        assert!(!offer(&fixture.context, &fixture.cfg, "group:101", accept), "nothing is owed without an episode");
        assert!(!fixture.root.join("data").join(FILE).exists(), "nothing owed writes nothing");
        observe(&fixture.context, &chain, 0, failed("quota"));
        observe(&fixture.context, &chain, 1, failed("quota"));
        assert!(!alert_if_owed(&fixture.context, &fixture.cfg, "group:101"), "without ownerQQ the notice queue refuses it");
        assert!(!offer(&fixture.context, &fixture.cfg, "group:101", refuse), "a refused alert is not sent");
        assert_eq!(fixture.stored().all_failed.map(|episode| episode.alerted), Some(false), "a refused alert stays owed");
        assert!(!fixture.records("routes-all-failed").is_empty());
        assert!(offer(&fixture.context, &fixture.cfg, "group:101", accept), "the owed alert is sent");
        assert_eq!(*sent.borrow(), [alert_text("group:101", 2, "quota")]);
        assert_eq!(fixture.stored().all_failed.map(|episode| episode.alerted), Some(true));
        observe(&fixture.context, &chain, 0, failed("backend"));
        assert!(
            !offer(&fixture.context, &fixture.cfg, "group:101", accept),
            "an episode is alerted once, also after a restart: the state is on disk"
        );
        observe(&fixture.context, &chain, 1, Seen::Answered);
        assert_eq!(fixture.stored().all_failed, None, "an answer ends the episode");
        observe(&fixture.context, &chain, 1, failed("timeout"));
        assert!(offer(&fixture.context, &fixture.cfg, "group:102", accept), "a new episode is alerted again");
        assert_eq!(sent.borrow().len(), 2);
        assert_eq!(sent.borrow()[1], alert_text("group:102", 2, "timeout"));
        observe(&fixture.context, &chain, 1, Seen::Answered);
        observe(&fixture.context, &chain, 1, failed("quota"));
        let other = routes_cfg(&["backup-route", "primary-route"], false);
        assert!(!offer(&fixture.context, &other, "group:101", accept), "an episode of another chain is never alerted");
        assert_eq!(sent.borrow().len(), 2);
    }

    #[test]
    fn route_health_concurrent_observations_lose_no_route() {
        let ids = ["r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8"];
        let fixture = Fixture::new("concurrent", routes_cfg(&ids, false));
        let chain = chain_of(&ids);
        let threads: Vec<_> = (0..ids.len())
            .map(|at| {
                let (context, chain) = (fixture.context.clone(), chain.clone());
                std::thread::spawn(move || observe(&context, &chain, at, Seen::Failed { class: "quota" }))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let stored = fixture.stored();
        assert_eq!(stored.routes.len(), 8, "every read-modify-write ran under the gate");
        assert!(stored.all_failed.is_some());
        assert!(fixture.records("route-health-unrecorded").is_empty());
    }

    #[test]
    fn route_health_a_held_gate_never_fails_the_request() {
        let ids = ["primary-route"];
        let fixture = Fixture::new("held", routes_cfg(&ids, false));
        let held = fixture.context.open_locked_file(RuntimeArea::Data, GATE).unwrap();
        let begun = Instant::now();
        observe(&fixture.context, &chain_of(&ids), 0, Seen::Answered);
        assert!(begun.elapsed() >= GATE_WAIT, "it waited for the gate");
        assert!(!fixture.root.join("data").join(FILE).exists(), "nothing was written without the gate");
        let unrecorded = fixture.records("route-health-unrecorded");
        assert!(unrecorded.len() == 1 && unrecorded[0].contains("route primary-route"), "{unrecorded:?}");
        drop(held);
        observe(&fixture.context, &chain_of(&ids), 0, Seen::Answered);
        assert_eq!(fixture.stored().routes.len(), 1, "a free gate is taken again");
    }

    #[test]
    fn route_health_report_lists_the_chain_in_order_and_never_trusts_a_damaged_state() {
        let ids = ["primary-route", "backup-route", "spare-route"];
        let fixture = Fixture::new("report", routes_cfg(&ids, false));
        let chain = chain_of(&ids);
        let states = |report: &Value| -> Vec<String> {
            report["routes"].as_array().unwrap().iter().map(|route| format!("{}={}", route["id"].as_str().unwrap(), route["state"].as_str().unwrap())).collect()
        };
        let fresh = report(&fixture.context, &fixture.cfg).unwrap();
        assert_eq!(states(&fresh), ["primary-route=unknown", "backup-route=unknown", "spare-route=unknown"]);
        assert_eq!((fresh["allFailed"].as_bool(), fresh.get("error")), (Some(false), None));
        observe(&fixture.context, &chain, 0, failed("quota"));
        observe(&fixture.context, &chain, 1, Seen::Answered);
        let seen = report(&fixture.context, &fixture.cfg).unwrap();
        assert_eq!(states(&seen), ["primary-route=failed", "backup-route=ok", "spare-route=unknown"]);
        assert_eq!(seen["routes"][0]["class"], "quota");
        assert!(seen["routes"][0]["since"].as_i64().is_some() && seen["routes"][2].get("since").is_none());
        let file = fixture.root.join("data").join(FILE);
        let mut padded = std::fs::read_to_string(&file).unwrap();
        padded.push_str(&" ".repeat(64 * 1024 - padded.len()));
        std::fs::write(&file, &padded).unwrap();
        assert_eq!(states(&report(&fixture.context, &fixture.cfg).unwrap())[0], "primary-route=failed", "a state of exactly 64 KiB is read");
        let reordered = routes_cfg(&["backup-route", "primary-route", "spare-route"], false);
        assert!(states(&report(&fixture.context, &reordered).unwrap()).iter().all(|state| state.ends_with("=unknown")), "a changed chain reads unknown");
        assert_eq!(report(&fixture.context, &json!({})), None, "no harness model, no route report");
        let invalid = report(&fixture.context, &json!({"harness": {"routes": [{"id": "a"}]}})).unwrap();
        assert_eq!((invalid["error"].as_str(), invalid["routes"].as_array().map(Vec::len)), (Some("harness route 1 is invalid (model)"), Some(0)));
        std::fs::write(&file, "not json").unwrap();
        let damaged = report(&fixture.context, &fixture.cfg).unwrap();
        assert_eq!(damaged["error"], "route health is unreadable");
        assert!(states(&damaged).iter().all(|state| state.ends_with("=unknown")));
        std::fs::write(&file, " ".repeat(64 * 1024 + 1)).unwrap();
        assert_eq!(report(&fixture.context, &fixture.cfg).unwrap()["error"], "route health is unreadable", "an oversized state is never parsed");
        observe(&fixture.context, &chain, 2, Seen::Answered);
        assert_eq!(states(&report(&fixture.context, &fixture.cfg).unwrap())[2], "spare-route=ok", "a damaged state starts over");
    }
}
