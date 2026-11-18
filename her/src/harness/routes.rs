//! The harness model route module (#7): the candidate chain (`chain`), the next step after a model request that brought
//! no usable answer (`decide`: a retry on the same route, a failover or the end of the turn, with the two kinds of 429
//! and empty-answer retries), the wait a rate limit asks for (`retry_after`) and each runtime's memory of a
//! conversation's sticky degraded route and spent failovers (`RouteMemory`, `with_memory`). `harness.rs` runs every
//! harness turn on it and the digest reads the chain's first route (`digest::model`). Usage facts (#173) and route
//! health (#174) build on it.
use crate::runtime::RuntimeContext;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The most routes one chain holds.
pub(crate) const MAX_ROUTES: usize = 8;
/// The longest route id, in characters.
pub(crate) const ROUTE_ID_MAX_CHARS: usize = 64;
/// The output-token cap of one request on a route that sets none.
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;
/// The highest output-token cap a route may set.
pub(crate) const MAX_OUTPUT_TOKENS: u32 = 32_768;
/// Failovers one conversation may make until one of its turns completes.
pub(crate) const RETRY_CAP: usize = 2;
/// Retries on the same route of an empty answer to one unchanged request.
pub(crate) const EMPTY_RETRIES: u32 = 3;
/// The longest rate-limit wait spent on the same route; a longer hint moves to the next route.
pub(crate) const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
/// How long a conversation's failover memory (its sticky route and its spent failovers) lasts after its last failover.
pub(crate) const STICKY_FOR: Duration = Duration::from_secs(30 * 60);

const MODEL_NOT_CONFIGURED: &str = "harness model is not configured";

/// One route of the chain as configured; it holds no credential.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RouteSpec {
    pub id: String,
    pub model: String,
    /// The `reasoningEffort` keyword (`genai::chat::ReasoningEffort::from_keyword`); `None` sends no effort.
    pub effort: Option<String>,
    /// The output-token cap of one request on this route.
    pub max_output_tokens: u32,
    /// `retry429`: a first 429 on a request is retried on this route.
    pub retry_429: bool,
}

impl RouteSpec {
    /// The one route of a chain without `harness.routes`: its id is its model id, so tables keyed by the model id
    /// before routes existed (`harness/rotation.rs`) keep naming it.
    pub(crate) fn implicit(model: String) -> Self {
        Self {
            id: model.clone(),
            model,
            effort: None,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            retry_429: true,
        }
    }

    fn signature(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.id,
            self.model,
            self.effort.as_deref().unwrap_or(""),
            self.max_output_tokens,
            self.retry_429
        )
    }
}

/// The fingerprint of a chain's configuration: a changed route, order or setting changes it.
pub(crate) fn chain_signature(chain: &[RouteSpec]) -> String {
    chain.iter().map(RouteSpec::signature).collect::<Vec<_>>().join("\u{1e}")
}

/// The configured chain, read when each turn starts: `harness.routes`, a list of 1 to `MAX_ROUTES` routes tried in
/// order, or, without it (absent or null), the one route of the setting `harness.model`. Any invalid route refuses the
/// whole list, and the turn fails as not configured: it never falls back to `harness.model`. A refusal names the
/// route's position and field, never a configured value. Unknown route fields are ignored.
pub(crate) fn chain(cfg: &Value) -> Result<Vec<RouteSpec>, String> {
    match cfg.pointer("/harness/routes") {
        None | Some(Value::Null) => {
            let model = crate::settings::HARNESS_MODEL.text(cfg).ok_or(MODEL_NOT_CONFIGURED)?;
            Ok(vec![RouteSpec::implicit(model)])
        }
        Some(Value::Array(entries)) if (1..=MAX_ROUTES).contains(&entries.len()) => {
            let mut routes: Vec<RouteSpec> = Vec::new();
            for (at, entry) in entries.iter().enumerate() {
                let route = route_spec(entry).map_err(|field| format!("harness route {} is invalid ({field})", at + 1))?;
                if routes.iter().any(|known| known.id == route.id) {
                    return Err(format!("harness route {} is invalid (id)", at + 1));
                }
                routes.push(route);
            }
            Ok(routes)
        }
        Some(_) => Err(format!("harness routes must be a list of 1 to {MAX_ROUTES} routes")),
    }
}

/// One route, or the name of its first invalid field.
fn route_spec(entry: &Value) -> Result<RouteSpec, &'static str> {
    let entry = entry.as_object().ok_or("route")?;
    let id = entry.get("id").and_then(Value::as_str).filter(|id| valid_id(id)).ok_or("id")?;
    // The registry's model id rule (`settings::kinds::ModelId`): trimmed, 1 to 200 characters, no control character.
    let model = entry
        .get("model")
        .and_then(|model| crate::settings::HARNESS_MODEL.normalize(model))
        .and_then(|model| model.as_str().map(str::to_string))
        .ok_or("model")?;
    let effort = match entry.get("reasoningEffort") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|keyword| genai::chat::ReasoningEffort::from_keyword(keyword).is_some())
                .ok_or("reasoningEffort")?
                .to_string(),
        ),
    };
    let max_output_tokens = match entry.get("maxOutputTokens") {
        None | Some(Value::Null) => DEFAULT_MAX_OUTPUT_TOKENS,
        Some(value) => value
            .as_u64()
            .filter(|tokens| (1..=u64::from(MAX_OUTPUT_TOKENS)).contains(tokens))
            .ok_or("maxOutputTokens")? as u32,
    };
    let retry_429 = match entry.get("retry429") {
        None | Some(Value::Null) => true,
        Some(value) => value.as_bool().ok_or("retry429")?,
    };
    Ok(RouteSpec {
        id: id.to_string(),
        model,
        effort,
        max_output_tokens,
        retry_429,
    })
}

/// A route id: 1 to `ROUTE_ID_MAX_CHARS` ASCII letters, digits, `.`, `_` or `-`, and never `default`, the fallback key
/// of `harness.rotation.caps`.
fn valid_id(id: &str) -> bool {
    (1..=ROUTE_ID_MAX_CHARS).contains(&id.len())
        && id != "default"
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// How one model request ended without a usable answer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Miss {
    /// HTTP 429; `wait` is the response's hint (`retry_after`).
    RateLimited { wait: Option<Duration> },
    /// An answer with no text and no tool call.
    Empty,
    /// The stream ended before its end event.
    Dropped,
    /// Any other failure, by its class (`model_failure::Failure::class`).
    Failed { class: &'static str },
}

/// What the turn knows when one of its requests misses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Situation {
    /// The route's `retry429`.
    pub retry_429: bool,
    /// This unchanged request already met a 429 on this route.
    pub rate_limited_before: bool,
    /// Empty answers this unchanged request already got on this route.
    pub empty_answers: u32,
    /// The turn's one dropped-stream retry is spent.
    pub stream_retried: bool,
    /// A validated tool handler was dispatched in this turn.
    pub dispatched: bool,
    /// A QQ send of this turn succeeded.
    pub sent: bool,
    /// A next route exists and the conversation's failover budget allows it.
    pub next_route: bool,
    /// Another request fits the turn's step allowance and token budget.
    pub steps_left: bool,
    /// The base wait (one second; tests shorten it).
    pub unit: Duration,
    /// The time left before the turn's deadline.
    pub remaining: Duration,
}

/// The next step after a miss.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Next {
    /// Send the same request to the same route after `wait`.
    Retry { wait: Duration },
    /// Send the same request to the next route.
    FailOver,
    /// End the turn with the miss's failure.
    Fail,
}

/// The next step after a miss. No request is made without a step left. A first 429 on an unchanged request is retried
/// on the same route after its hint, or two base waits without one, when the route's `retry429` is on and the wait is
/// at most `MAX_RATE_LIMIT_WAIT` and ends before the deadline; a repeat 429 moves to the next route. An empty answer is
/// retried on the same route up to `EMPTY_RETRIES` times, after 1, 2 and 4 base waits, while nothing was sent. The
/// first dropped stream of a turn is retried at once. Any other miss moves to the next route, except that a drop, a
/// timeout or a network failure after a validated dispatch ends the turn (a request after a possible tool effect is
/// never resent), and nothing moves to another route once a send succeeded.
pub(crate) fn decide(miss: &Miss, situation: &Situation) -> Next {
    if !situation.steps_left {
        return Next::Fail;
    }
    let fail_over = if situation.next_route && !situation.sent {
        Next::FailOver
    } else {
        Next::Fail
    };
    let fits = |wait: Duration| wait < situation.remaining;
    match miss {
        Miss::RateLimited { wait } => {
            let wait = wait.unwrap_or(situation.unit * 2);
            if situation.retry_429 && !situation.rate_limited_before && wait <= MAX_RATE_LIMIT_WAIT && fits(wait) {
                return Next::Retry { wait };
            }
            fail_over
        }
        Miss::Empty => {
            let wait = situation.unit * 2u32.pow(situation.empty_answers.min(EMPTY_RETRIES));
            if !situation.sent && situation.empty_answers < EMPTY_RETRIES && fits(wait) {
                return Next::Retry { wait };
            }
            fail_over
        }
        Miss::Dropped if situation.dispatched => Next::Fail,
        Miss::Dropped if !situation.stream_retried => Next::Retry { wait: Duration::ZERO },
        Miss::Dropped => fail_over,
        Miss::Failed { class } if situation.dispatched && matches!(*class, "timeout" | "network") => Next::Fail,
        Miss::Failed { .. } => fail_over,
    }
}

/// The wait a failed response asks for: `retry-after-ms` in milliseconds, else `retry-after` in whole seconds. An HTTP
/// date or any other form is no hint.
pub(crate) fn retry_after(milliseconds: Option<&str>, seconds: Option<&str>) -> Option<Duration> {
    if let Some(wait) = milliseconds.and_then(|value| value.trim().parse::<u64>().ok()) {
        return Some(Duration::from_millis(wait));
    }
    seconds
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Which conversation's memory a turn reads and writes: its conversation, generation and chain fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChainKey {
    pub conversation: String,
    pub generation: String,
    pub signature: String,
}

#[derive(Debug)]
struct Entry {
    generation: String,
    signature: String,
    /// The sticky route: where the conversation's next turn starts.
    route: usize,
    /// When the last failover happened.
    since: Instant,
    /// Failovers since the conversation's last completed turn.
    failovers: usize,
}

/// One runtime's failover memory: per conversation, its sticky degraded route and its spent failovers. An entry counts
/// only for the same generation and chain and until `STICKY_FOR` after its last failover; only conversations with such
/// an entry are kept.
#[derive(Debug, Default)]
pub(crate) struct RouteMemory {
    entries: HashMap<String, Entry>,
}

impl RouteMemory {
    fn current(&self, key: &ChainKey, now: Instant) -> Option<&Entry> {
        self.entries.get(&key.conversation).filter(|entry| {
            entry.generation == key.generation
                && entry.signature == key.signature
                && now.saturating_duration_since(entry.since) < STICKY_FOR
        })
    }

    /// The route a turn starts on: the conversation's sticky degraded route (clamped to the chain), else the first. An
    /// entry that no longer counts is forgotten.
    pub(crate) fn start(&mut self, key: &ChainKey, len: usize, now: Instant) -> usize {
        match self.current(key, now).map(|entry| entry.route) {
            Some(route) => route.min(len.saturating_sub(1)),
            None => {
                self.entries.remove(&key.conversation);
                0
            }
        }
    }

    /// Whether the conversation may fail over once more (`RETRY_CAP`).
    pub(crate) fn can_fail_over(&self, key: &ChainKey, now: Instant) -> bool {
        self.current(key, now).map_or(0, |entry| entry.failovers) < RETRY_CAP
    }

    /// The conversation failed over to route `next`: one more failover spent, and `next` is where its turns start.
    /// Entries that no longer count are dropped first.
    pub(crate) fn on_failover(&mut self, key: &ChainKey, next: usize, now: Instant) {
        let failovers = self.current(key, now).map_or(0, |entry| entry.failovers) + 1;
        self.entries
            .retain(|_, entry| now.saturating_duration_since(entry.since) < STICKY_FOR);
        self.entries.insert(
            key.conversation.clone(),
            Entry {
                generation: key.generation.clone(),
                signature: key.signature.clone(),
                route: next,
                since: now,
                failovers,
            },
        );
    }

    /// A turn of the conversation completed: its failover budget starts over and its sticky route stays.
    pub(crate) fn on_completed(&mut self, key: &ChainKey) {
        let same = self
            .entries
            .get(&key.conversation)
            .map(|entry| entry.generation == key.generation && entry.signature == key.signature);
        match same {
            Some(true) => {
                if let Some(entry) = self.entries.get_mut(&key.conversation) {
                    entry.failovers = 0;
                }
            }
            Some(false) => {
                self.entries.remove(&key.conversation);
            }
            None => {}
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The runtime a memory belongs to: the selected root, as the DSH chain keeps its failover state (`pipeline.rs`).
pub(crate) fn scope_of(context: &RuntimeContext) -> String {
    context
        .runtime_root()
        .map(|root| root.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Runs `with` on the route memory of runtime `scope`, shared by every turn of this process. Nothing is durable: a
/// restart starts every conversation on its first route.
pub(crate) fn with_memory<T>(scope: &str, with: impl FnOnce(&mut RouteMemory) -> T) -> T {
    static MEMORIES: OnceLock<Mutex<HashMap<String, RouteMemory>>> = OnceLock::new();
    let mut memories = MEMORIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    with(memories.entry(scope.to_string()).or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn route(id: &str) -> Value {
        json!({"id": id, "model": format!("model-{id}")})
    }

    fn routes(list: Vec<Value>) -> Value {
        json!({"harness": {"routes": list}})
    }

    #[test]
    fn harness_routes_chain_without_routes_is_the_one_route_of_harness_model() {
        let implicit = RouteSpec {
            id: "m1".into(),
            model: "m1".into(),
            effort: None,
            max_output_tokens: 4096,
            retry_429: true,
        };
        assert_eq!(chain(&json!({"harness": {"model": " m1 "}})), Ok(vec![implicit.clone()]));
        assert_eq!(chain(&json!({"harness": {"model": "m1", "routes": null}})), Ok(vec![implicit]));
        assert_eq!(chain(&json!({})), Err("harness model is not configured".to_string()));
        assert_eq!(chain(&json!({"harness": {"model": "  "}})), Err("harness model is not configured".to_string()));
    }

    #[test]
    fn harness_routes_chain_reads_each_route_in_order_with_its_defaults() {
        let cfg = json!({"harness": {"model": "ignored", "routes": [
            {"id": "flash", "model": " model-flash ", "reasoningEffort": "high", "maxOutputTokens": 1000, "retry429": false},
            {"id": "sonnet", "model": "model-sonnet", "reasoningEffort": null, "maxOutputTokens": null, "retry429": null, "note": "ignored"},
        ]}});
        assert_eq!(
            chain(&cfg),
            Ok(vec![
                RouteSpec {
                    id: "flash".into(),
                    model: "model-flash".into(),
                    effort: Some("high".into()),
                    max_output_tokens: 1000,
                    retry_429: false,
                },
                RouteSpec {
                    id: "sonnet".into(),
                    model: "model-sonnet".into(),
                    effort: None,
                    max_output_tokens: 4096,
                    retry_429: true,
                },
            ])
        );
    }

    #[test]
    fn harness_routes_chain_refuses_the_whole_list_for_any_invalid_route() {
        let mut eight: Vec<Value> = (1..=8).map(|n| route(&format!("r{n}"))).collect();
        assert_eq!(chain(&routes(eight.clone())).map(|found| found.len()), Ok(8), "eight routes are accepted");
        eight.push(route("r9"));
        assert_eq!(
            chain(&routes(eight)),
            Err("harness routes must be a list of 1 to 8 routes".to_string()),
            "nine routes are refused"
        );
        let list = "harness routes must be a list of 1 to 8 routes";
        for (cfg, expected) in [
            (json!({"harness": {"model": "m", "routes": []}}), list),
            (json!({"harness": {"model": "m", "routes": {"id": "a", "model": "m"}}}), list),
            (json!({"harness": {"model": "m", "routes": [{"id": "a"}]}}), "harness route 1 is invalid (model)"),
            (routes(vec![json!("a")]), "harness route 1 is invalid (route)"),
            (routes(vec![route("a"), json!({"model": "m"})]), "harness route 2 is invalid (id)"),
            (routes(vec![json!({"id": "default", "model": "m"})]), "harness route 1 is invalid (id)"),
            (routes(vec![json!({"id": "a b", "model": "m"})]), "harness route 1 is invalid (id)"),
            (routes(vec![json!({"id": "a/b", "model": "m"})]), "harness route 1 is invalid (id)"),
            (routes(vec![json!({"id": "é", "model": "m"})]), "harness route 1 is invalid (id)"),
            (routes(vec![json!({"id": "", "model": "m"})]), "harness route 1 is invalid (id)"),
            (routes(vec![json!({"id": "x".repeat(65), "model": "m"})]), "harness route 1 is invalid (id)"),
            (routes(vec![route("a"), route("a")]), "harness route 2 is invalid (id)"),
            (routes(vec![json!({"id": "a", "model": "  "})]), "harness route 1 is invalid (model)"),
            (routes(vec![json!({"id": "a", "model": "a\u{1}b"})]), "harness route 1 is invalid (model)"),
            (routes(vec![json!({"id": "a", "model": "x".repeat(201)})]), "harness route 1 is invalid (model)"),
            (routes(vec![json!({"id": "a", "model": "m", "reasoningEffort": "turbo"})]), "harness route 1 is invalid (reasoningEffort)"),
            (routes(vec![json!({"id": "a", "model": "m", "reasoningEffort": 3})]), "harness route 1 is invalid (reasoningEffort)"),
            (routes(vec![json!({"id": "a", "model": "m", "maxOutputTokens": 0})]), "harness route 1 is invalid (maxOutputTokens)"),
            (routes(vec![json!({"id": "a", "model": "m", "maxOutputTokens": 32769})]), "harness route 1 is invalid (maxOutputTokens)"),
            (routes(vec![json!({"id": "a", "model": "m", "maxOutputTokens": 1.5})]), "harness route 1 is invalid (maxOutputTokens)"),
            (routes(vec![json!({"id": "a", "model": "m", "retry429": "yes"})]), "harness route 1 is invalid (retry429)"),
        ] {
            assert_eq!(chain(&cfg), Err(expected.to_string()), "{cfg}");
        }
        let edges = routes(vec![
            json!({"id": "x".repeat(64), "model": "m", "maxOutputTokens": 32768}),
            json!({"id": "A.b_c-9", "model": "n", "maxOutputTokens": 1, "reasoningEffort": "none"}),
        ]);
        let accepted = chain(&edges).unwrap();
        assert_eq!((accepted[0].max_output_tokens, accepted[1].max_output_tokens), (32768, 1));
        assert_eq!(accepted[1].effort.as_deref(), Some("none"));
    }

    #[test]
    fn harness_routes_digest_model_falls_back_to_the_first_route() {
        let cfg = routes(vec![route("a"), route("b")]);
        assert_eq!(crate::digest::digest_model_id(&cfg).as_deref(), Some("model-a"));
        let mut own = cfg.clone();
        own["memory"] = json!({"digest": {"model": "d"}});
        assert_eq!(crate::digest::digest_model_id(&own).as_deref(), Some("d"));
        let invalid = json!({"harness": {"model": "h", "routes": [{"id": "a"}]}});
        assert_eq!(crate::digest::digest_model_id(&invalid), None, "an invalid chain is not configured");
    }

    #[test]
    fn harness_routes_chain_signature_changes_with_any_route_setting() {
        let base = chain(&routes(vec![route("a"), route("b")])).unwrap();
        let mut changed = base.clone();
        changed[1].max_output_tokens = 1000;
        assert_ne!(chain_signature(&base), chain_signature(&changed));
        let reordered = vec![base[1].clone(), base[0].clone()];
        assert_ne!(chain_signature(&base), chain_signature(&reordered));
        assert_eq!(chain_signature(&base), chain_signature(&base.clone()));
    }

    fn situation() -> Situation {
        Situation {
            retry_429: true,
            rate_limited_before: false,
            empty_answers: 0,
            stream_retried: false,
            dispatched: false,
            sent: false,
            next_route: true,
            steps_left: true,
            unit: Duration::from_secs(1),
            remaining: Duration::from_secs(60),
        }
    }

    #[test]
    fn harness_routes_a_plain_429_retries_the_same_route_and_honours_its_wait_hint() {
        let s = situation();
        assert_eq!(decide(&Miss::RateLimited { wait: None }, &s), Next::Retry { wait: Duration::from_secs(2) });
        let hint = Duration::from_millis(1500);
        assert_eq!(decide(&Miss::RateLimited { wait: Some(hint) }, &s), Next::Retry { wait: hint });
        assert_eq!(
            decide(&Miss::RateLimited { wait: Some(MAX_RATE_LIMIT_WAIT) }, &s),
            Next::Retry { wait: MAX_RATE_LIMIT_WAIT },
            "a hint at the limit is honoured on the same route"
        );
        assert_eq!(
            decide(&Miss::RateLimited { wait: Some(MAX_RATE_LIMIT_WAIT + Duration::from_millis(1)) }, &s),
            Next::FailOver,
            "a longer hint moves to the next route"
        );
        let short = Situation { remaining: Duration::from_secs(2), ..s };
        assert_eq!(decide(&Miss::RateLimited { wait: None }, &short), Next::FailOver, "a wait that reaches the deadline");
        let roomy = Situation { remaining: Duration::from_millis(2001), ..s };
        assert_eq!(decide(&Miss::RateLimited { wait: None }, &roomy), Next::Retry { wait: Duration::from_secs(2) });
    }

    #[test]
    fn harness_routes_a_repeat_429_or_retry429_off_moves_to_the_next_route() {
        let s = situation();
        let repeat = Situation { rate_limited_before: true, ..s };
        assert_eq!(decide(&Miss::RateLimited { wait: None }, &repeat), Next::FailOver);
        assert_eq!(decide(&Miss::RateLimited { wait: None }, &Situation { next_route: false, ..repeat }), Next::Fail);
        let off = Situation { retry_429: false, ..s };
        assert_eq!(decide(&Miss::RateLimited { wait: Some(Duration::ZERO) }, &off), Next::FailOver);
    }

    #[test]
    fn harness_routes_empty_answers_retry_three_times_with_growing_waits_then_fail_over() {
        let s = situation();
        for (seen, wait) in [(0, 1), (1, 2), (2, 4)] {
            assert_eq!(
                decide(&Miss::Empty, &Situation { empty_answers: seen, ..s }),
                Next::Retry { wait: Duration::from_secs(wait) },
                "{seen} empty answers so far"
            );
        }
        assert_eq!(decide(&Miss::Empty, &Situation { empty_answers: 3, ..s }), Next::FailOver, "the fourth moves on");
        assert_eq!(decide(&Miss::Empty, &Situation { empty_answers: 3, next_route: false, ..s }), Next::Fail);
        assert_eq!(decide(&Miss::Empty, &Situation { sent: true, ..s }), Next::Fail, "nothing is retried after a send");
        assert_eq!(
            decide(&Miss::Empty, &Situation { remaining: Duration::from_secs(1), ..s }),
            Next::FailOver,
            "a wait that reaches the deadline"
        );
    }

    #[test]
    fn harness_routes_a_dropped_stream_retries_once_before_dispatch_and_never_after() {
        let s = situation();
        assert_eq!(decide(&Miss::Dropped, &s), Next::Retry { wait: Duration::ZERO });
        assert_eq!(decide(&Miss::Dropped, &Situation { stream_retried: true, ..s }), Next::FailOver);
        assert_eq!(decide(&Miss::Dropped, &Situation { dispatched: true, ..s }), Next::Fail);
    }

    #[test]
    fn harness_routes_explicit_errors_fail_over_but_timeouts_after_a_tool_effect_never_resend() {
        let s = situation();
        for class in ["refusal", "quota", "backend", "timeout", "network"] {
            assert_eq!(decide(&Miss::Failed { class }, &s), Next::FailOver, "{class}");
        }
        let dispatched = Situation { dispatched: true, ..s };
        for class in ["refusal", "quota", "backend"] {
            assert_eq!(decide(&Miss::Failed { class }, &dispatched), Next::FailOver, "{class} after a tool");
        }
        for class in ["timeout", "network"] {
            assert_eq!(decide(&Miss::Failed { class }, &dispatched), Next::Fail, "{class} after a tool");
        }
        let sent = Situation { sent: true, dispatched: true, ..s };
        assert_eq!(decide(&Miss::Failed { class: "backend" }, &sent), Next::Fail, "no other route after a send");
        assert_eq!(decide(&Miss::Failed { class: "backend" }, &Situation { next_route: false, ..s }), Next::Fail);
    }

    #[test]
    fn harness_routes_no_request_is_made_without_a_step_left() {
        let s = Situation { steps_left: false, ..situation() };
        for miss in [
            Miss::RateLimited { wait: None },
            Miss::Empty,
            Miss::Dropped,
            Miss::Failed { class: "backend" },
        ] {
            assert_eq!(decide(&miss, &s), Next::Fail, "{miss:?}");
        }
    }

    #[test]
    fn harness_routes_retry_after_reads_milliseconds_then_whole_seconds() {
        assert_eq!(retry_after(Some("250"), Some("9")), Some(Duration::from_millis(250)));
        assert_eq!(retry_after(None, Some(" 7 ")), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(Some("soon"), Some("3")), Some(Duration::from_secs(3)));
        assert_eq!(retry_after(None, Some("Wed, 21 Oct 2015 07:28:00 GMT")), None, "an HTTP date is no hint");
        assert_eq!(retry_after(None, Some("-1")), None);
        assert_eq!(retry_after(None, Some("1.5")), None);
        assert_eq!(retry_after(None, None), None);
        assert_eq!(
            retry_after(None, Some(&u64::MAX.to_string())),
            Some(Duration::from_secs(u64::MAX)),
            "a huge hint is read, and decide moves on instead of waiting"
        );
    }

    fn key(signature: &str) -> ChainKey {
        ChainKey {
            conversation: "group:1".into(),
            generation: "gen-1".into(),
            signature: signature.into(),
        }
    }

    #[test]
    fn harness_routes_a_degraded_route_starts_the_next_turns_until_its_memory_lapses() {
        let mut memory = RouteMemory::default();
        let at = Instant::now();
        let k = key("chain-1");
        assert_eq!(memory.start(&k, 3, at), 0, "a fresh conversation starts on the first route");
        memory.on_failover(&k, 1, at);
        assert_eq!(memory.start(&k, 3, at + STICKY_FOR - Duration::from_millis(1)), 1, "the degraded route is sticky");
        memory.on_completed(&k);
        assert_eq!(memory.start(&k, 3, at + Duration::from_secs(1)), 1, "a completed turn keeps the sticky route");
        assert_eq!(memory.start(&k, 3, at + STICKY_FOR), 0, "the memory lapses after STICKY_FOR");
        assert_eq!(memory.len(), 0, "a lapsed memory is forgotten");
    }

    #[test]
    fn harness_routes_a_new_generation_or_chain_starts_on_the_first_route() {
        let mut memory = RouteMemory::default();
        let at = Instant::now();
        memory.on_failover(&key("chain-1"), 2, at);
        assert_eq!(memory.start(&key("chain-2"), 3, at), 0, "a changed chain");
        memory.on_failover(&key("chain-1"), 2, at);
        let reset = ChainKey { generation: "gen-2".into(), ..key("chain-1") };
        assert_eq!(memory.start(&reset, 3, at), 0, "a reset conversation");
        memory.on_failover(&key("chain-1"), 2, at);
        let other = ChainKey { conversation: "group:2".into(), ..key("chain-1") };
        assert_eq!(memory.start(&other, 3, at), 0, "another conversation has its own memory");
        assert_eq!(memory.start(&key("chain-1"), 2, at), 1, "a sticky route past the chain is clamped");
    }

    #[test]
    fn harness_routes_two_failovers_spend_the_conversation_budget_until_a_turn_completes() {
        let mut memory = RouteMemory::default();
        let at = Instant::now();
        let k = key("chain-1");
        assert!(memory.can_fail_over(&k, at));
        memory.on_failover(&k, 1, at);
        assert!(memory.can_fail_over(&k, at), "one failover spent");
        memory.on_failover(&k, 2, at);
        assert!(!memory.can_fail_over(&k, at), "the cap of two holds");
        let other = ChainKey { conversation: "group:2".into(), ..k.clone() };
        assert!(memory.can_fail_over(&other, at), "each conversation has its own budget");
        memory.on_completed(&k);
        assert!(memory.can_fail_over(&k, at), "a completed turn gives a fresh budget");
        memory.on_failover(&k, 1, at);
        memory.on_failover(&k, 2, at);
        assert!(memory.can_fail_over(&k, at + STICKY_FOR), "the budget lapses with the memory");
    }

    #[test]
    fn harness_routes_memory_keeps_only_conversations_with_a_recent_failover() {
        let mut memory = RouteMemory::default();
        let at = Instant::now();
        let k = key("chain-1");
        memory.on_failover(&k, 1, at);
        memory.on_completed(&ChainKey { conversation: "group:9".into(), ..k.clone() });
        assert_eq!(memory.len(), 1, "a completed turn without a failover remembers nothing");
        let later = ChainKey { conversation: "group:2".into(), ..k.clone() };
        memory.on_failover(&later, 1, at + STICKY_FOR);
        assert_eq!(memory.len(), 1, "a lapsed memory is dropped when another is written");
    }

    #[test]
    fn harness_routes_each_runtime_has_its_own_memory() {
        let k = key("chain-1");
        let at = Instant::now();
        with_memory("harness-routes-test-runtime-a", |memory| memory.on_failover(&k, 1, at));
        assert_eq!(with_memory("harness-routes-test-runtime-a", |memory| memory.start(&k, 2, at)), 1);
        assert_eq!(
            with_memory("harness-routes-test-runtime-b", |memory| memory.start(&k, 2, at)),
            0,
            "another runtime, or this one after a restart, starts on the first route"
        );
    }
}
