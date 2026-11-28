//! The harness model route module (#7, #383): the candidate chain (`chain`), the next step after a model request that
//! brought no usable answer (`decide`), the wait a rate limit asks for (`retry_after`), each runtime's failover memory
//! (`RouteMemory`, `with_memory`), credential redaction (`ModelRoute`), a shared `genai::Client` (`shared_client`) and
//! shared Tokio runtime (`shared_runtime` / `block_on_shared`), and the async route-chain executor (`ChainExecutor`)
//! owning failover memory, cancellation-aware retry backoff, route health observation, usage recording and run-record
//! emission behind the narrow `RouteStep` adapter seam (`TurnStep`, `OneShotStep`, and scripted test adapters).
use super::route_health::{self, Seen};
use super::usage;
use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::RuntimeContext;
use genai::{
    adapter::AdapterKind,
    chat::{ChatMessage, ChatOptions, ChatRequest, ReasoningEffort, StopReason, Usage},
    resolver::{AuthData, Endpoint},
    Client, ModelIden, ModelSpec, ServiceTarget,
};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};
use tokio::sync::watch;

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

/// The refusal of a chain when the runtime configures no harness model at all (route health then reports nothing).
pub(crate) const MODEL_NOT_CONFIGURED: &str = "harness model is not configured";

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

    /// The settings store's spelling of a route, with defaults materialized and only the fields the reader uses.
    pub(crate) fn setting_value(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "model": self.model,
            "reasoningEffort": self.effort,
            "maxOutputTokens": self.max_output_tokens,
            "retry429": self.retry_429,
        })
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
    // Read the registry's member without substituting the default for invalid file data.
    match crate::settings::HARNESS_ROUTES.found_in(cfg) {
        None | Some(Value::Null) => {
            let model = crate::settings::HARNESS_MODEL.text(cfg).ok_or(MODEL_NOT_CONFIGURED)?;
            Ok(vec![RouteSpec::implicit(model)])
        }
        Some(value) => parse_list(value),
    }
}

/// Validates and reads an explicit route list for both the runtime and the settings store.
pub(crate) fn parse_list(value: &Value) -> Result<Vec<RouteSpec>, String> {
    let entries = value
        .as_array()
        .filter(|entries| (1..=MAX_ROUTES).contains(&entries.len()))
        .ok_or_else(|| format!("harness routes must be a list of 1 to {MAX_ROUTES} routes"))?;
    let mut routes: Vec<RouteSpec> = Vec::with_capacity(entries.len());
    for (at, entry) in entries.iter().enumerate() {
        let route = route_spec(entry).map_err(|field| format!("harness route {} is invalid ({field})", at + 1))?;
        if routes.iter().any(|known| known.id == route.id) {
            return Err(format!("harness route {} is invalid (id)", at + 1));
        }
        routes.push(route);
    }
    Ok(routes)
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

/// The route conversation `conversation`'s turns ask first (#162), read when each turn starts: the conversation's own
/// model route in the settings store (`settings::CONVERSATION_ROUTE`, through `settings::conversation_value`; a reset
/// stored none), else the global setting `harness.firstRoute` on `cfg`, the loader's document. `None` for none. Whether
/// it names a route of the chain is `conversation_order`'s to decide.
pub(crate) fn selected(context: &RuntimeContext, cfg: &Value, conversation: &str) -> Option<String> {
    let stored = crate::settings::conversation_value(context, conversation, crate::settings::CONVERSATION_ROUTE.key);
    let value = stored.unwrap_or_else(|| crate::settings::HARNESS_FIRST_ROUTE.read(cfg));
    value.as_str().map(str::to_string)
}

/// `chain` in the order a conversation's turns walk it (#162): the route whose id is `selected` first, then the other
/// routes in configured order. A selection that names no route of `chain` (none, or a route since removed or renamed)
/// leaves the configured order. Ids compare as stored strings on both sides. The result holds every route of `chain`
/// once, so another order is another fingerprint (`chain_signature`) with its own failover memory.
pub(crate) fn conversation_order(chain: &[RouteSpec], selected: Option<&str>) -> Vec<RouteSpec> {
    let mut order = chain.to_vec();
    if let Some(at) = selected.and_then(|id| order.iter().position(|route| route.id == id)) {
        let route = order.remove(at);
        order.insert(0, route);
    }
    order
}

/// A route id: 1 to `ROUTE_ID_MAX_CHARS` ASCII letters, digits, `.`, `_` or `-`, and never `default`, the fallback key
/// of `harness.rotation.caps`. The settings kind of a route id (`settings::kinds::RouteId`) uses the same rule.
pub(crate) fn valid_id(id: &str) -> bool {
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

impl Miss {
    /// The content-free failure class of this miss (`model_failure::Failure::class`): a 429 is `quota`, an empty
    /// answer `empty-response` and a dropped stream `network`.
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Miss::RateLimited { .. } => "quota",
            Miss::Empty => "empty-response",
            Miss::Dropped => "network",
            Miss::Failed { class } => class,
        }
    }
}

/// What the turn knows when one of its requests misses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Situation {
    /// The route's `retry429`.
    pub retry_429: bool,
    /// Whether empty answers retry on the same route before failing over.
    pub retry_empty: bool,
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
            if situation.retry_empty
                && !situation.sent
                && situation.empty_answers < EMPTY_RETRIES
                && fits(wait)
            {
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
    context.runtime_root().to_string_lossy().to_string()
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

/// Process-wide shared `genai::Client` (HTTP connection pool) reused by streaming turns and one-shot jobs.
pub(crate) fn shared_client() -> Result<Client, &'static str> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let web_client = genai::WebConfig::default()
        .apply_to_builder(reqwest::Client::builder())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "harness model client is unavailable")?;
    let client = Client::builder()
        .with_reqwest(web_client)
        .build()
        .map_err(|_| "harness model client is unavailable")?;
    let _ = CLIENT.set(client.clone());
    Ok(CLIENT.get().cloned().unwrap_or(client))
}

/// Process-wide shared Tokio runtime used by the sync bridge (`block_on_shared`) for one-shot jobs, rather than
/// spawning a fresh OS thread and Tokio runtime per request.
pub(crate) fn shared_runtime() -> Result<&'static tokio::runtime::Runtime, &'static str> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let built = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("her-route-chain")
        .enable_all()
        .build()
        .map_err(|_| "harness async runtime is unavailable")?;
    let _ = RUNTIME.set(built);
    RUNTIME.get().ok_or("harness async runtime is unavailable")
}

/// Runs `future` to completion on the shared Tokio runtime (`shared_runtime`), safely handling callers on plain OS
/// threads, `MultiThread` Tokio workers / `spawn_blocking` threads, and `CurrentThread` test runtimes.
pub(crate) fn block_on_shared<F, T>(future: F) -> Result<T, &'static str>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let runtime = shared_runtime()?;
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(runtime.block_on(future));
    }
    std::thread::Builder::new()
        .name("her-oneshot-bridge".into())
        .spawn(move || runtime.block_on(future))
        .map_err(|_| "harness route bridge thread failed")?
        .join()
        .map_err(|_| "harness route worker panicked")
}

/// `text` with the harness credential `key` replaced by `[REDACTED]`: the form every record and capture keeps.
pub(crate) fn redact_credential(text: &str, key: &str) -> String {
    text.replace(key, "[REDACTED]")
}

/// Redact decoded strings, so JSON escaping and split stream chunks cannot hide a key.
pub(crate) fn redact_credentials(text: &str, credentials: &[String]) -> String {
    credentials
        .iter()
        .fold(text.to_string(), |text, key| redact_credential(&text, key))
}

/// Recursively redacts `credentials` across all string keys and values of a JSON `Value`.
pub(crate) fn redact(value: &mut Value, credentials: &[String]) {
    match value {
        Value::String(text) => *text = redact_credentials(text, credentials),
        Value::Array(values) => values.iter_mut().for_each(|v| redact(v, credentials)),
        Value::Object(fields) => {
            *fields = std::mem::take(fields)
                .into_iter()
                .map(|(name, mut value)| {
                    redact(&mut value, credentials);
                    (redact_credentials(&name, credentials), value)
                })
                .collect();
        }
        _ => {}
    }
}

/// One route of a turn's or walk's chain with its relay target. Credentials are opaque: never derive Debug or serialize
/// this value.
pub(crate) struct Route {
    pub(crate) spec: RouteSpec,
    /// The route id and model id as records keep them, with credentials redacted.
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) target: ServiceTarget,
}

/// A candidate chain on the selected runtime's relay, authenticated with the harness credential: the configured routes
/// (`chain`) in the order a turn or walk traverses them (`for_conversation`: the conversation's selected route first),
/// tried in that order from `start`. Credentials are opaque: never derive Debug or serialize this value.
#[derive(Clone)]
pub struct ModelRoute {
    pub(crate) credentials: Vec<String>,
    pub(crate) endpoint: String,
    /// The routes in the order this execution walks them.
    pub(crate) routes: Vec<Route>,
    /// The same routes in configured order: route health keys on it whatever order a turn walks (`observe`).
    pub(crate) configured: Vec<RouteSpec>,
    /// The route the execution starts on: the conversation's sticky degraded route.
    pub(crate) start: usize,
}

impl Clone for Route {
    fn clone(&self) -> Self {
        Self {
            spec: self.spec.clone(),
            id: self.id.clone(),
            model: self.model.clone(),
            target: self.target.clone(),
        }
    }
}

impl ModelRoute {
    /// The first route's model id (tests).
    #[cfg(test)]
    pub fn model_name(&self) -> &str {
        &self.routes[0].model
    }

    /// The selected runtime's chain (`chain`) in configured order, whatever a conversation selected (tests; turns use
    /// `for_conversation`); without a valid one the turn fails as not configured.
    #[cfg(test)]
    pub fn from_runtime(context: &RuntimeContext, cfg: &Value) -> Result<Self, String> {
        let chain = chain(cfg)?;
        Self::for_specs(context, chain.clone(), chain)
    }

    /// #162: the chain conversation `conversation`'s turn walks: its selected route first (`selected`: its own model
    /// route, else `harness.firstRoute`), then the other routes in configured order (`conversation_order`); the
    /// configured order when the selection names no route of the chain.
    pub fn for_conversation(context: &RuntimeContext, cfg: &Value, conversation: &str) -> Result<Self, String> {
        let chain = chain(cfg)?;
        let order = conversation_order(&chain, selected(context, cfg, conversation).as_deref());
        Self::for_specs(context, order, chain)
    }

    /// The selected runtime's relay route for `model` alone, authenticated with the harness credential.
    #[cfg(test)]
    pub fn for_model(context: &RuntimeContext, model: &str) -> Result<Self, String> {
        let chain = vec![RouteSpec::implicit(model.to_string())];
        Self::for_specs(context, chain.clone(), chain)
    }

    /// Builds a resolved route chain for `order` (walk order) and `configured` (configured order for route health).
    pub(crate) fn for_specs(
        context: &RuntimeContext,
        order: Vec<RouteSpec>,
        configured: Vec<RouteSpec>,
    ) -> Result<Self, String> {
        if order.is_empty() {
            return Err(MODEL_NOT_CONFIGURED.into());
        }
        let snapshot = crate::settings::resolve_model_endpoint_and_key(context)
            .map_err(|_| "stored model credentials are unavailable")?;
        let origin = snapshot
            .effective_endpoint
            .as_deref()
            .ok_or("harness model endpoint is unavailable")?;
        let key = snapshot
            .harness_api_key()
            .ok_or("Set harness.apiKey in Settings for the current model endpoint.")?
            .to_string();
        let endpoint = crate::runtime::model_openai_endpoint(origin);
        let credentials = snapshot.all_secrets;
        if order.iter().any(|spec| {
            credentials
                .iter()
                .any(|secret| !secret.is_empty() && spec.model.contains(secret))
        }) {
            return Err("harness route model contains a credential".into());
        }
        let routes = order
            .into_iter()
            .map(|spec| Route {
                id: redact_credentials(&spec.id, &credentials),
                model: redact_credentials(&spec.model, &credentials),
                target: ServiceTarget {
                    model: ModelIden::new(AdapterKind::OpenAI, spec.model.clone()),
                    endpoint: Endpoint::from_owned(endpoint.clone()),
                    auth: AuthData::Key(key.to_string()),
                },
                spec,
            })
            .collect();
        Ok(Self {
            endpoint: redact_credentials(&endpoint, &credentials),
            credentials,
            routes,
            configured,
            start: 0,
        })
    }

    /// `text` with this route's harness credential redacted (`redact_credentials`).
    pub(crate) fn redact(&self, text: &str) -> String {
        redact_credentials(text, &self.credentials)
    }

    /// Mask a copy of the delivered request using this chain's credential snapshot.
    #[cfg(test)]
    pub(crate) fn capture_request(&self, model: &str, max_tokens: u32, request: &ChatRequest) -> Value {
        let mut capture = serde_json::json!({
            "model": model,
            "maxTokens": max_tokens,
            "request": serde_json::to_value(request).unwrap_or(Value::Null),
        });
        redact(&mut capture, &self.credentials);
        capture
    }

    /// The chain's fingerprint (`chain_signature`).
    pub(crate) fn signature(&self) -> String {
        chain_signature(&self.specs())
    }

    /// The chain's routes in the order this execution walks them (its failover memory's fingerprint, `signature`).
    pub(crate) fn specs(&self) -> Vec<RouteSpec> {
        self.routes.iter().map(|route| route.spec.clone()).collect()
    }

    /// #162: records route health (`route_health::observe`) for route `at` of this walk at its place in the configured
    /// chain, so the health state and its all-routes-failed episode stay one per configured chain whatever order a
    /// conversation walks.
    pub(crate) fn observe(&self, context: &RuntimeContext, at: usize, seen: Seen) {
        let Some(id) = self.routes.get(at).map(|route| route.spec.id.as_str()) else { return };
        let Some(place) = self.configured.iter().position(|spec| spec.id == id) else { return };
        route_health::observe(context, &self.configured, place, seen);
    }
}

/// Resolves `route.start` from `RouteMemory` for `conversation` and `generation` in `context`'s runtime.
pub(crate) fn select_start(
    context: &RuntimeContext,
    route: &mut ModelRoute,
    conversation: &str,
    generation: &str,
) {
    let key = ChainKey {
        conversation: conversation.to_string(),
        generation: generation.to_string(),
        signature: route.signature(),
    };
    let len = route.routes.len();
    route.start = with_memory(&scope_of(context), |memory| memory.start(&key, len, Instant::now()));
}

/// The wait a 429 response asks for (`retry_after`), through the streaming wrapper.
pub(crate) fn rate_limit_wait(error: &genai::Error) -> Option<Duration> {
    if let genai::Error::WebStream { error: inner, .. } = error {
        if let Some(error) = inner.downcast_ref::<genai::Error>() {
            return rate_limit_wait(error);
        }
    }
    let headers = error.headers()?;
    let milliseconds = headers.get("retry-after-ms").and_then(|value| value.to_str().ok());
    let seconds = headers.get("retry-after").and_then(|value| value.to_str().ok());
    retry_after(milliseconds, seconds)
}

/// Fixed-vocabulary failure of a provider error; never carries the provider's text.
pub(crate) fn model_failure(error: &genai::Error) -> crate::model_failure::Failure {
    use crate::model_failure::Failure;
    // The streaming adapter wraps HTTP failures without exposing their status on the outer error.
    if let genai::Error::WebStream { error: inner, .. } = error {
        if let Some(error) = inner.downcast_ref::<genai::Error>() {
            return model_failure(error);
        }
    }
    if error.status().is_some_and(|s| s.as_u16() == 429) {
        return Failure::new("quota", "429");
    }
    let body = match &error {
        genai::Error::ChatResponse { body, .. } => Some(serde_json::json!({"error": body})),
        genai::Error::HttpError { body, .. } => serde_json::from_str(body).ok(),
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus { body, .. } => serde_json::from_str(body).ok(),
            genai::webc::Error::Reqwest(error) if error.is_timeout() => {
                return Failure::new("timeout", "TIMEOUT");
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(body) = &body {
        if let Some(failure) = crate::backend::json_output_failure(body) {
            return failure;
        }
    }
    if let Some(status) = error.status() {
        return Failure::new("backend", &status.as_u16().to_string());
    }
    Failure::new("network", "MODEL_CONNECTION_ERROR")
}

/// Classifies a `genai::Error` into a `Miss` and its redacted content-free summary (`Failure::summary`).
pub(crate) fn classify_genai_error(error: &genai::Error, credentials: &[String]) -> (Miss, String) {
    let failure = model_failure(error);
    let summary = redact_credentials(&failure.summary(), credentials);
    if failure.class == "quota" && failure.code == "429" {
        return (Miss::RateLimited { wait: rate_limit_wait(error) }, summary);
    }
    match error {
        genai::Error::WebStream { error: inner, .. }
            if inner.downcast_ref::<genai::Error>().is_none()
                && inner.downcast_ref::<genai::webc::StreamTimeout>().is_none() =>
        {
            (Miss::Dropped, summary)
        }
        _ => (Miss::Failed { class: failure.class }, summary),
    }
}

/// One route request that failed over or ended its turn, as the pipeline reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteFailure {
    pub route: String,
    pub model: String,
    /// The content-free failure summary (`model_failure::Failure::summary`).
    pub summary: String,
    pub model_call: usize,
    pub message_refs: Vec<Value>,
}

/// Async cancellation signal for `ChainExecutor`: supports event-driven `watch::Receiver<bool>` (used by turns and
/// handover summaries) and shared `Arc<AtomicBool>` abandonment flags (used by one-shot sync callers).
#[derive(Clone, Default)]
pub(crate) struct Cancellation {
    watch: Option<watch::Receiver<bool>>,
    abandoned: Option<Arc<AtomicBool>>,
}

impl Cancellation {
    pub(crate) fn none() -> Self {
        Self::default()
    }

    pub(crate) fn from_watch(rx: watch::Receiver<bool>) -> Self {
        Self { watch: Some(rx), abandoned: None }
    }

    #[cfg(test)]
    pub(crate) fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self { watch: None, abandoned: Some(flag) }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.watch.as_ref().is_some_and(|w| *w.borrow())
            || self.abandoned.as_ref().is_some_and(|f| f.load(Ordering::SeqCst))
    }

    /// Resolves once cancellation is observed. Never blocks an OS thread.
    pub(crate) async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        match (&mut self.watch, &self.abandoned) {
            (Some(rx), None) => loop {
                if *rx.borrow() {
                    return;
                }
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            },
            (None, Some(flag)) => loop {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            },
            (Some(rx), Some(flag)) => loop {
                if *rx.borrow() || flag.load(Ordering::SeqCst) {
                    return;
                }
                tokio::select! {
                    res = rx.changed() => {
                        if res.is_err() {
                            loop {
                                if flag.load(Ordering::SeqCst) {
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            },
            (None, None) => std::future::pending::<()>().await,
        }
    }
}

/// Parameters passed to a `RouteStep` adapter for one attempt.
pub(crate) struct StepContext<'a> {
    pub route_idx: usize,
    pub route: &'a Route,
    pub credentials: &'a [String],
    pub attempt_index: usize,
    pub repeats: i64,
    pub remaining: Duration,
    pub max_tokens: u32,
}

impl StepContext<'_> {
    /// Captures a redacted JSON copy of `request` for `upstream_capture::offer`.
    pub(crate) fn capture_request(&self, request: &ChatRequest, max_tokens: u32) -> Value {
        let mut capture = serde_json::json!({
            "model": self.route.model,
            "maxTokens": max_tokens,
            "request": serde_json::to_value(request).unwrap_or(Value::Null),
        });
        redact(&mut capture, self.credentials);
        capture
    }
}

/// Fatal non-route interruption returned by a `RouteStep` adapter.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StepAbort {
    Cancelled,
    Failed(String),
}

/// Outcome of a single `RouteStep::step` call.
pub(crate) enum StepResult<T> {
    /// A usable answer; `response_usage` is `Some(...)` when a response end arrived (`Some(None)` records `b: null`).
    Answered {
        value: T,
        response_usage: Option<Option<Usage>>,
        tokens_charged: u32,
    },
    /// A miss (`RateLimited`, `Empty`, `Dropped`, or `Failed`); `response_usage` is `Some(...)` when a response end
    /// arrived (e.g. refusal, empty answer, length stop) so usage is recorded before deciding retry/failover.
    Missed {
        miss: Miss,
        summary: String,
        response_usage: Option<Option<Usage>>,
        tokens_charged: u32,
        request_bytes: usize,
        message_refs: Vec<Value>,
        observe_health: bool,
    },
    /// Non-route abort (cancellation, authority revocation, storage/config failure).
    Abort {
        abort: StepAbort,
        response_usage: Option<Option<Usage>>,
        tokens_charged: u32,
    },
}

/// Narrow adapter seam between `ChainExecutor` and a concrete execution mode (streaming turn, non-streaming one-shot,
/// or scripted test adapter).
pub(crate) trait RouteStep: Send {
    type Output: Send;

    fn step<'a>(
        &'a mut self,
        ctx: StepContext<'a>,
    ) -> Pin<Box<dyn Future<Output = StepResult<Self::Output>> + Send + 'a>>;

    /// The active `conversations.db` `model_attempts.id` for this step, when backed by durable
    /// turn storage.
    fn attempt_id(&self) -> Option<&str> {
        None
    }

    /// Invoked after `decide` selects `Next::Retry` or `Next::FailOver` (before any backoff sleep) so stateful turn
    /// adapters can check bounds and close the failed attempt in storage.
    fn on_miss_transition(&mut self, _next: Next) -> Result<(), StepAbort> {
        Ok(())
    }

    /// Invoked when a route request fails over or ends the chain (`Next::FailOver` / `Next::Fail`).
    fn on_route_failure(&mut self, _failure: RouteFailure) {}
}

/// Non-streaming one-shot `RouteStep` adapter using the shared `genai::Client`, forwarding `reasoningEffort` and
/// `maxOutputTokens`, offering armed upstream capture, redacting credentials, and classifying provider failures.
pub(crate) struct OneShotStep {
    pub context: RuntimeContext,
    pub system: String,
    pub input: String,
    /// True when the caller's `within` bound is shorter than the route's own timeout (shortened
    /// timeouts do not mark `route_health` as failed).
    pub shortened_by_bound: bool,
    /// Redacted model id of the last route failure recorded by `ChainExecutor`.
    pub last_model: Option<String>,
}

impl RouteStep for OneShotStep {
    type Output = String;

    fn step<'a>(
        &'a mut self,
        ctx: StepContext<'a>,
    ) -> Pin<Box<dyn Future<Output = StepResult<String>> + Send + 'a>> {
        Box::pin(async move {
            let mut messages = Vec::with_capacity(2);
            if !self.system.is_empty() {
                messages.push(ChatMessage::system(redact_credentials(&self.system, ctx.credentials)));
            }
            messages.push(ChatMessage::user(redact_credentials(&self.input, ctx.credentials)));
            let request = ChatRequest::new(messages);
            let request_bytes = self.system.len() + self.input.len();
            crate::upstream_capture::offer(&self.context, crate::upstream_capture::Route::Harness, || {
                ctx.capture_request(&request, ctx.max_tokens)
            });
            let client = match shared_client() {
                Ok(client) => client,
                Err(_) => {
                    return StepResult::Missed {
                        miss: Miss::Failed { class: "network" },
                        summary: "network".into(),
                        response_usage: None,
                        tokens_charged: 0,
                        request_bytes,
                        message_refs: Vec::new(),
                        observe_health: true,
                    };
                }
            };
            let mut options = ChatOptions::default().with_max_tokens(ctx.max_tokens);
            if let Some(effort) = ctx.route.spec.effort.as_deref().and_then(ReasoningEffort::from_keyword) {
                options = options.with_reasoning_effort(effort);
            }
            let call = client.exec_chat(ModelSpec::Target(ctx.route.target.clone()), request, Some(&options));
            let response = match tokio::time::timeout(ctx.remaining, call).await {
                Err(_elapsed) => {
                    let cut = self.shortened_by_bound || ctx.repeats > 0;
                    return StepResult::Missed {
                        miss: Miss::Failed { class: "timeout" },
                        summary: "timeout".into(),
                        response_usage: None,
                        tokens_charged: 0,
                        request_bytes,
                        message_refs: Vec::new(),
                        observe_health: !cut,
                    };
                }
                Ok(Err(error)) => {
                    let (miss, _) = classify_genai_error(&error, ctx.credentials);
                    let class = miss.class();
                    return StepResult::Missed {
                        miss,
                        summary: class.into(),
                        response_usage: None,
                        tokens_charged: 0,
                        request_bytes,
                        message_refs: Vec::new(),
                        observe_health: true,
                    };
                }
                Ok(Ok(response)) => response,
            };
            let mut response = response;
            let usage = std::mem::take(&mut response.usage);
            if matches!(response.stop_reason, Some(StopReason::ContentFilter(_))) {
                return StepResult::Missed {
                    miss: Miss::Failed { class: "refusal" },
                    summary: crate::model_failure::Failure::new("refusal", "MODEL_REFUSAL").summary(),
                    response_usage: Some(Some(usage)),
                    tokens_charged: 0,
                    request_bytes,
                    message_refs: Vec::new(),
                    observe_health: true,
                };
            }
            let text = redact_credentials(
                &response.into_first_text().unwrap_or_default(),
                ctx.credentials,
            );
            let text = match super::one_shot::normalize_response_text(&text) {
                Ok(text) => text,
                Err(failure) => {
                    let miss = if failure.class == "empty-response" {
                        Miss::Empty
                    } else {
                        Miss::Failed {
                            class: failure.class,
                        }
                    };
                    return StepResult::Missed {
                        miss,
                        summary: failure.summary(),
                        response_usage: Some(Some(usage)),
                        tokens_charged: 0,
                        request_bytes,
                        message_refs: Vec::new(),
                        observe_health: true,
                    };
                }
            };
            StepResult::Answered {
                value: text,
                response_usage: Some(Some(usage)),
                tokens_charged: 0,
            }
        })
    }

    fn on_route_failure(&mut self, failure: RouteFailure) {
        self.last_model = Some(failure.model);
    }
}

/// Configuration for one `ChainExecutor` run (a multi-step turn or a one-shot job).
pub(crate) struct ChainConfig {
    pub context: RuntimeContext,
    pub route: ModelRoute,
    pub key: String,
    pub generation: String,
    pub usage_job: &'static str,
    pub track_steps: bool,
    pub observe_health: bool,
    pub use_failover_memory: bool,
    pub record_events: bool,
    pub record_usage: bool,
    pub allow_retries: bool,
    pub retry_empty: bool,
    pub allow_stream_retry: bool,
    pub complete_on_answer: bool,
    pub fail_action: &'static str,
    pub deadline_error: &'static str,
    pub retry_unit: Duration,
    pub max_steps: usize,
    pub max_tokens: Option<u32>,
    pub job_max_tokens: u32,
    pub deadline: tokio::time::Instant,
}

/// Result of a successful `ChainExecutor::next_answer` call.
#[derive(Debug)]
pub(crate) struct StepSuccess<T> {
    pub value: T,
    pub spent_tokens: u32,
}

/// Terminal error returned by `ChainExecutor::next_answer`.
#[derive(Debug, PartialEq)]
pub(crate) enum ChainError {
    Cancelled,
    Aborted(String),
    Missed { class: &'static str, summary: String },
}

impl From<StepAbort> for ChainError {
    fn from(abort: StepAbort) -> Self {
        match abort {
            StepAbort::Cancelled => ChainError::Cancelled,
            StepAbort::Failed(reason) => ChainError::Aborted(reason),
        }
    }
}

/// Async route-chain executor owning failover memory, cancellation-aware retry backoff, route health observation,
/// usage recording, and `model-chain` run-record emission.
pub(crate) struct ChainExecutor {
    cfg: ChainConfig,
    scope: String,
    memory_key: ChainKey,
    route_idx: usize,
    step: usize,
    repeats: i64,
    rate_limited: bool,
    empty_answers: u32,
    stream_retried: bool,
    dispatched: bool,
    sent: bool,
    route_chain_exhausted: bool,
    spent_tokens: u32,
    usage_turn: i64,
}

impl ChainExecutor {
    pub(crate) fn new(cfg: ChainConfig) -> Self {
        let scope = scope_of(&cfg.context);
        let memory_key = ChainKey {
            conversation: cfg.key.clone(),
            generation: cfg.generation.clone(),
            signature: cfg.route.signature(),
        };
        let route_idx = cfg.route.start.min(cfg.route.routes.len().saturating_sub(1));
        let stream_retried = !cfg.allow_stream_retry;
        Self {
            cfg,
            scope,
            memory_key,
            route_idx,
            step: 0,
            repeats: 0,
            rate_limited: false,
            empty_answers: 0,
            stream_retried,
            dispatched: false,
            sent: false,
            route_chain_exhausted: false,
            spent_tokens: 0,
            usage_turn: usage::next_number(),
        }
    }

    pub(crate) fn steps_used(&self) -> usize {
        self.step
    }

    pub(crate) fn route_chain_exhausted(&self) -> bool {
        self.route_chain_exhausted
    }

    pub(crate) fn mark_dispatched(&mut self) {
        self.dispatched = true;
    }

    pub(crate) fn mark_sent(&mut self) {
        self.sent = true;
    }

    pub(crate) fn mark_completed(&self) {
        if self.cfg.use_failover_memory {
            with_memory(&self.scope, |memory| memory.on_completed(&self.memory_key));
        }
    }

    fn record_usage(&self, route: &Route, usage: Option<&Usage>, attempt_id: Option<&str>) {
        if !self.cfg.record_usage {
            return;
        }
        let place = usage::Place {
            key: self.cfg.key.clone(),
            turn: self.usage_turn,
            step: if self.cfg.track_steps { self.step as i64 } else { 0 },
            retry: self.repeats,
        };
        usage::record_attempt(
            &self.cfg.context,
            &place,
            &route.id,
            &route.model,
            self.cfg.usage_job,
            usage,
            attempt_id,
        );
    }

    fn record_event(&self, route: &Route, summary: &str, bytes: usize, event: &str, outcome: Outcome, action: &str) {
        if !self.cfg.record_events {
            return;
        }
        let trigger = if self.cfg.usage_job == usage::DIGEST {
            Trigger::Digest
        } else {
            Trigger::Turn
        };
        let reason = format!(
            "{}: route {} ({summary}) on a request of {bytes} bytes; {action}",
            self.cfg.key, route.id
        );
        crate::recovery_log::record(
            &self.cfg.context,
            trigger,
            Component::ModelChain,
            event,
            outcome,
            &reason,
        );
    }

    fn record_route_miss<S: RouteStep>(
        &self,
        adapter: &mut S,
        class: &'static str,
        observe_health: bool,
        failure: RouteFailure,
    ) {
        if self.cfg.observe_health && observe_health {
            self.cfg
                .route
                .observe(&self.cfg.context, self.route_idx, Seen::Failed { class });
        }
        adapter.on_route_failure(failure);
    }

    /// Runs one logical request across retries and failovers until a route answers, the chain fails, or cancellation
    /// / deadline / abort interrupts it.
    pub(crate) async fn next_answer<S: RouteStep>(
        &mut self,
        adapter: &mut S,
        cancel: &mut Cancellation,
    ) -> Result<StepSuccess<S::Output>, ChainError> {
        self.repeats = 0;
        self.rate_limited = false;
        self.empty_answers = 0;

        loop {
            if cancel.is_cancelled() {
                return Err(ChainError::Cancelled);
            }
            let remaining = self
                .cfg
                .deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ChainError::Aborted(self.cfg.deadline_error.to_string()));
            }
            let route = &self.cfg.route.routes[self.route_idx];
            let max_tokens = match self.cfg.max_tokens {
                Some(budget) => budget
                    .saturating_sub(self.spent_tokens)
                    .min(route.spec.max_output_tokens),
                None => self.cfg.job_max_tokens.min(route.spec.max_output_tokens),
            };
            let ctx = StepContext {
                route_idx: self.route_idx,
                route,
                credentials: &self.cfg.route.credentials,
                attempt_index: self.step,
                repeats: self.repeats,
                remaining,
                max_tokens,
            };
            let step_result = tokio::select! { biased;
                result = adapter.step(ctx) => result,
                _ = cancel.cancelled() => return Err(ChainError::Cancelled),
            };
            let attempt_id = adapter.attempt_id().map(str::to_string);
            let route = &self.cfg.route.routes[self.route_idx];
            match step_result {
                StepResult::Abort {
                    abort,
                    response_usage,
                    tokens_charged,
                } => {
                    if let Some(usage) = response_usage {
                        self.record_usage(route, usage.as_ref(), attempt_id.as_deref());
                        self.spent_tokens = self.spent_tokens.saturating_add(tokens_charged);
                    }
                    return Err(abort.into());
                }
                StepResult::Answered {
                    value,
                    response_usage,
                    tokens_charged,
                    ..
                } => {
                    if let Some(usage) = response_usage {
                        self.record_usage(route, usage.as_ref(), attempt_id.as_deref());
                        self.spent_tokens = self.spent_tokens.saturating_add(tokens_charged);
                    }
                    if cancel.is_cancelled() {
                        return Err(ChainError::Cancelled);
                    }
                    self.step += 1;
                    if self.cfg.observe_health {
                        self.cfg.route.observe(&self.cfg.context, self.route_idx, Seen::Answered);
                    }
                    if self.cfg.complete_on_answer {
                        self.mark_completed();
                    }
                    return Ok(StepSuccess {
                        value,
                        spent_tokens: self.spent_tokens,
                    });
                }
                StepResult::Missed {
                    miss,
                    summary,
                    response_usage,
                    tokens_charged,
                    request_bytes,
                    message_refs,
                    observe_health,
                } => {
                    if let Some(usage) = response_usage {
                        self.record_usage(route, usage.as_ref(), attempt_id.as_deref());
                        self.spent_tokens = self.spent_tokens.saturating_add(tokens_charged);
                    }
                    let now = Instant::now();
                    let remaining = self
                        .cfg
                        .deadline
                        .saturating_duration_since(tokio::time::Instant::now());
                    let next_route = self.cfg.allow_retries
                        && self.route_idx + 1 < self.cfg.route.routes.len()
                        && (!self.cfg.use_failover_memory
                            || with_memory(&self.scope, |memory| memory.can_fail_over(&self.memory_key, now)));
                    let steps_left = if !self.cfg.allow_retries || remaining.is_zero() {
                        false
                    } else if self.cfg.track_steps {
                        self.step + 1 < self.cfg.max_steps
                            && self.cfg.max_tokens.is_none_or(|max| self.spent_tokens < max)
                    } else {
                        true
                    };
                    let situation = Situation {
                        retry_429: route.spec.retry_429,
                        retry_empty: self.cfg.retry_empty,
                        rate_limited_before: self.rate_limited,
                        empty_answers: self.empty_answers,
                        stream_retried: self.stream_retried,
                        dispatched: self.dispatched,
                        sent: self.sent,
                        next_route,
                        steps_left,
                        unit: self.cfg.retry_unit,
                        remaining,
                    };
                    let next = decide(&miss, &situation);
                    let class = miss.class();
                    let event_tag = if self.cfg.track_steps { summary.as_str() } else { class };
                    match next {
                        Next::Fail => {
                            if cancel.is_cancelled() {
                                return Err(ChainError::Cancelled);
                            }
                            let unsafe_miss = self.dispatched
                                && (matches!(miss, Miss::Dropped) || matches!(class, "timeout" | "network"));
                            if !next_route
                                && !self.sent
                                && !unsafe_miss
                                && class != "refusal"
                                && tokio::time::Instant::now() < self.cfg.deadline
                            {
                                self.route_chain_exhausted = true;
                            }
                            self.record_route_miss(
                                adapter,
                                class,
                                observe_health,
                                RouteFailure {
                                    route: route.id.clone(),
                                    model: route.model.clone(),
                                    summary: summary.clone(),
                                    model_call: self.step + 1,
                                    message_refs,
                                },
                            );
                            self.record_event(
                                route,
                                event_tag,
                                request_bytes,
                                "route-failed",
                                Outcome::Failed,
                                self.cfg.fail_action,
                            );
                            return Err(ChainError::Missed { class, summary });
                        }
                        Next::FailOver => {
                            if cancel.is_cancelled() {
                                return Err(ChainError::Cancelled);
                            }
                            if tokio::time::Instant::now() >= self.cfg.deadline {
                                return Err(ChainError::Aborted(self.cfg.deadline_error.to_string()));
                            }
                            adapter.on_miss_transition(next)?;
                            let next_idx = self.route_idx + 1;
                            let action = format!("next route {}", self.cfg.route.routes[next_idx].id);
                            self.record_route_miss(
                                adapter,
                                class,
                                observe_health,
                                RouteFailure {
                                    route: route.id.clone(),
                                    model: route.model.clone(),
                                    summary: summary.clone(),
                                    model_call: self.step + 1,
                                    message_refs,
                                },
                            );
                            self.record_event(
                                route,
                                event_tag,
                                request_bytes,
                                "failover-step",
                                Outcome::Ok,
                                &action,
                            );
                            if self.cfg.use_failover_memory {
                                with_memory(&self.scope, |memory| {
                                    memory.on_failover(&self.memory_key, next_idx, now)
                                });
                            }
                            self.route_idx = next_idx;
                            self.rate_limited = false;
                            self.empty_answers = 0;
                            self.repeats += 1;
                            self.step += 1;
                        }
                        Next::Retry { wait } => {
                            if cancel.is_cancelled() {
                                return Err(ChainError::Cancelled);
                            }
                            if tokio::time::Instant::now() >= self.cfg.deadline {
                                return Err(ChainError::Aborted(self.cfg.deadline_error.to_string()));
                            }
                            adapter.on_miss_transition(next)?;
                            let action = format!("retrying the same route after {} ms", wait.as_millis());
                            self.record_event(route, event_tag, request_bytes, "route-retry", Outcome::Ok, &action);
                            match miss {
                                Miss::RateLimited { .. } => self.rate_limited = true,
                                Miss::Empty => self.empty_answers += 1,
                                Miss::Dropped => self.stream_retried = true,
                                Miss::Failed { .. } => {}
                            }
                            self.repeats += 1;
                            self.step += 1;
                            if !wait.is_zero() {
                                tokio::select! { biased;
                                    _ = cancel.cancelled() => return Err(ChainError::Cancelled),
                                    _ = tokio::time::sleep(wait) => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
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
        assert_eq!(crate::harness::one_shot::digest_model_id(&cfg).as_deref(), Some("model-a"));
        let mut own = cfg.clone();
        own["memory"] = json!({"digest": {"model": "d"}});
        assert_eq!(crate::harness::one_shot::digest_model_id(&own).as_deref(), Some("d"));
        let invalid = json!({"harness": {"model": "h", "routes": [{"id": "a"}]}});
        assert_eq!(
            crate::harness::one_shot::digest_model_id(&invalid),
            None,
            "an invalid chain is not configured"
        );
    }

    #[test]
    fn model_route_order_puts_the_selected_route_first_then_the_rest_in_configured_order() {
        let base = chain(&routes(vec![route("a"), route("b"), route("c")])).unwrap();
        let ids = |order: Vec<RouteSpec>| order.into_iter().map(|route| route.id).collect::<Vec<_>>();
        assert_eq!(ids(conversation_order(&base, Some("c"))), ["c", "a", "b"], "the selected route first, then the rest in order");
        assert_eq!(ids(conversation_order(&base, Some("b"))), ["b", "a", "c"]);
        assert_eq!(ids(conversation_order(&base, Some("a"))), ["a", "b", "c"], "the first route selected is the configured order");
        for unknown in [None, Some("z"), Some("B"), Some(" b"), Some("")] {
            assert_eq!(ids(conversation_order(&base, unknown)), ["a", "b", "c"], "{unknown:?} names no route of the chain");
        }
        assert_ne!(
            chain_signature(&conversation_order(&base, Some("c"))),
            chain_signature(&base),
            "another order is another fingerprint, with its own failover memory"
        );
        let single = chain(&json!({"harness": {"model": "m1"}})).unwrap();
        assert_eq!(ids(conversation_order(&single, Some("m1"))), ["m1"], "a chain of one route stays as it is");
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
            retry_empty: true,
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

    struct TestFixture {
        root: std::path::PathBuf,
        context: RuntimeContext,
        cfg: Value,
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl TestFixture {
        fn new(tag: &str, routes_cfg: Value, secret: &str) -> Self {
            let root = crate::install_root::development_root(
                std::env::temp_dir().join(format!("her-383-{tag}-{}", crate::turn_ledger::mint_id(""))),
            );
            for dir in ["config", "data", "resources"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            let cfg = json!({"consoleToken": "synthetic-console-token-383", "harness": routes_cfg});
            std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
            std::fs::write(root.join("config/harness-api-key"), secret).unwrap();
            std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            std::fs::write(
                root.join("dependencies.json"),
                r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
            )
            .unwrap();
            let context = RuntimeContext::from_root(&root).unwrap();
            Self { root, context, cfg }
        }

        fn health(&self) -> (Vec<String>, bool) {
            let report = route_health::report(&self.context, &self.cfg).unwrap();
            let states = report["routes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| format!("{}={}", r["id"].as_str().unwrap(), r["state"].as_str().unwrap()))
                .collect();
            (states, report["allFailed"] == true)
        }

        fn usage_rows(&self) -> Vec<Value> {
            std::fs::read_to_string(self.root.join("data/token-usage-rust.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        fn records(&self, event: &str) -> Vec<String> {
            crate::recovery_log::page(&self.context, &crate::run_record::Query::default())
                .unwrap()
                .records
                .into_iter()
                .filter(|r| r.event == event)
                .map(|r| r.reason)
                .collect()
        }
    }

    fn sample_usage() -> Usage {
        serde_json::from_value(json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15})).unwrap()
    }

    #[derive(Clone, Debug)]
    struct RecordedCall {
        route_id: String,
        model: String,
        effort: Option<String>,
        max_tokens: u32,
        attempt_index: usize,
        repeats: i64,
    }

    enum ScriptAction {
        Answer {
            text: String,
            response_usage: Option<Option<Usage>>,
            tokens_charged: u32,
        },
        Missed {
            miss: Miss,
            summary: String,
            response_usage: Option<Option<Usage>>,
            tokens_charged: u32,
            observe_health: bool,
        },
    }

    struct ScriptedStep {
        actions: std::collections::VecDeque<ScriptAction>,
        calls: Vec<RecordedCall>,
        failures: Vec<RouteFailure>,
        transitions: Vec<Next>,
        on_call: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    }

    impl ScriptedStep {
        fn new(actions: Vec<ScriptAction>) -> Self {
            Self {
                actions: actions.into(),
                calls: Vec::new(),
                failures: Vec::new(),
                transitions: Vec::new(),
                on_call: None,
            }
        }
    }

    impl RouteStep for ScriptedStep {
        type Output = String;

        fn step<'a>(
            &'a mut self,
            ctx: StepContext<'a>,
        ) -> Pin<Box<dyn Future<Output = StepResult<String>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.push(RecordedCall {
                    route_id: ctx.route.id.clone(),
                    model: ctx.route.model.clone(),
                    effort: ctx.route.spec.effort.clone(),
                    max_tokens: ctx.max_tokens,
                    attempt_index: ctx.attempt_index,
                    repeats: ctx.repeats,
                });
                if let Some(tx) = &self.on_call {
                    let _ = tx.send(self.calls.len());
                }
                match self.actions.pop_front().unwrap() {
                    ScriptAction::Answer { text, response_usage, tokens_charged } => StepResult::Answered {
                        value: text,
                        response_usage,
                        tokens_charged,
                    },
                    ScriptAction::Missed { miss, summary, response_usage, tokens_charged, observe_health } => StepResult::Missed {
                        miss,
                        summary,
                        response_usage,
                        tokens_charged,
                        request_bytes: 128,
                        message_refs: Vec::new(),
                        observe_health,
                    },
                }
            })
        }

        fn on_miss_transition(&mut self, next: Next) -> Result<(), StepAbort> {
            self.transitions.push(next);
            Ok(())
        }

        fn on_route_failure(&mut self, failure: RouteFailure) {
            self.failures.push(failure);
        }
    }

    fn turn_chain_cfg(context: &RuntimeContext, route: ModelRoute, key: &str) -> ChainConfig {
        ChainConfig {
            context: context.clone(),
            route,
            key: key.to_string(),
            generation: "gen-1".to_string(),
            usage_job: usage::TURN,
            track_steps: true,
            observe_health: true,
            use_failover_memory: true,
            record_events: true,
            record_usage: true,
            allow_retries: true,
            retry_empty: true,
            allow_stream_retry: true,
            complete_on_answer: false,
            fail_action: "the turn ends",
            deadline_error: "deadline exceeded",
            retry_unit: Duration::from_millis(1),
            max_steps: 16,
            max_tokens: Some(10_000),
            job_max_tokens: 0,
            deadline: tokio::time::Instant::now() + Duration::from_secs(10),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_executor_failover_sticky_memory_retry_cap_and_run_records() {
        let secret = "synthetic-secret-383";
        let cfg_routes = json!({"routes": [
            {"id": format!("a-{secret}"), "model": "model-a"},
            {"id": "b", "model": "model-b", "reasoningEffort": "high", "maxOutputTokens": 900},
            {"id": "c", "model": "model-c"},
            {"id": "d", "model": "model-d"},
            {"id": "e", "model": "model-e"}
        ]});
        let fixture = TestFixture::new("failover-sticky", cfg_routes, secret);
        let mut route = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        select_start(&fixture.context, &mut route, "group:100", "gen-1");

        let mut exec = ChainExecutor::new(turn_chain_cfg(&fixture.context, route.clone(), "group:100"));
        let mut step = ScriptedStep::new(vec![
            ScriptAction::Missed {
                miss: Miss::Failed { class: "backend" },
                summary: "model/backend backend (500)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: true,
            },
            ScriptAction::Answer {
                text: "ok-b".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
            },
        ]);
        let mut cancel = Cancellation::none();
        let res = exec.next_answer(&mut step, &mut cancel).await.unwrap();
        assert_eq!(res.value, "ok-b");
        assert_eq!(step.calls.len(), 2);
        assert_eq!((step.calls[0].route_id.as_str(), step.calls[0].model.as_str(), step.calls[0].attempt_index, step.calls[0].repeats), ("a-[REDACTED]", "model-a", 0, 0));
        assert_eq!((step.calls[1].route_id.as_str(), step.calls[1].effort.as_deref(), step.calls[1].max_tokens, step.calls[1].attempt_index, step.calls[1].repeats), ("b", Some("high"), 900, 1, 1));
        assert_eq!(step.transitions, vec![Next::FailOver]);
        assert_eq!(step.failures.len(), 1);
        exec.mark_completed();

        let failovers = fixture.records("failover-step");
        assert_eq!(failovers.len(), 1);
        assert!(failovers[0].contains("a-[REDACTED]") && failovers[0].contains("next route b"), "{failovers:?}");
        assert!(!failovers[0].contains(secret), "credentials never reach run records: {failovers:?}");

        // Next turn starts on sticky degraded route `b` with a fresh RETRY_CAP budget (2 failovers: b -> c -> d).
        let mut route2 = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        select_start(&fixture.context, &mut route2, "group:100", "gen-1");
        assert_eq!(route2.start, 1, "sticky degraded route b starts the next turn");
        let mut exec2 = ChainExecutor::new(turn_chain_cfg(&fixture.context, route2, "group:100"));
        let mut step2 = ScriptedStep::new(vec![
            ScriptAction::Missed {
                miss: Miss::Failed { class: "backend" },
                summary: "model/backend backend (502)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: true,
            },
            ScriptAction::Missed {
                miss: Miss::Failed { class: "backend" },
                summary: "model/backend backend (503)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: true,
            },
            ScriptAction::Missed {
                miss: Miss::Failed { class: "backend" },
                summary: "model/backend backend (504)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: true,
            },
        ]);
        let err = exec2.next_answer(&mut step2, &mut cancel).await.err().unwrap();
        assert_eq!(err, ChainError::Missed { class: "backend", summary: "model/backend backend (504)".into() });
        assert_eq!(
            step2.calls.iter().map(|c| c.route_id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "d"],
            "RETRY_CAP stops at d without reaching e"
        );
        assert_eq!(fixture.records("route-failed").len(), 1);

        // Uncompleted turn holds its spent RETRY_CAP budget into the next turn on sticky route `d`.
        let mut route3 = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        select_start(&fixture.context, &mut route3, "group:100", "gen-1");
        assert_eq!(route3.start, 3, "sticky degraded route d starts the next turn");
        let mut exec3 = ChainExecutor::new(turn_chain_cfg(&fixture.context, route3, "group:100"));
        let mut step3 = ScriptedStep::new(vec![ScriptAction::Missed {
            miss: Miss::Failed { class: "backend" },
            summary: "model/backend backend (505)".into(),
            response_usage: None,
            tokens_charged: 0,
            observe_health: true,
        }]);
        let err3 = exec3.next_answer(&mut step3, &mut cancel).await.err().unwrap();
        assert_eq!(err3, ChainError::Missed { class: "backend", summary: "model/backend backend (505)".into() });
        assert_eq!(step3.calls.iter().map(|c| c.route_id.as_str()).collect::<Vec<_>>(), ["d"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_executor_429_empty_and_5xx_retries_and_route_health() {
        let cfg_routes = json!({"routes": [
            {"id": "a", "model": "model-a"},
            {"id": "b", "model": "model-b"}
        ]});
        let fixture = TestFixture::new("retries-health", cfg_routes, "secret-383-retries");
        let route = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        let mut exec = ChainExecutor::new(turn_chain_cfg(&fixture.context, route, "group:200"));
        let mut step = ScriptedStep::new(vec![
            ScriptAction::Missed {
                miss: Miss::RateLimited { wait: Some(Duration::from_millis(1)) },
                summary: "model/backend quota (429)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: true,
            },
            ScriptAction::Missed {
                miss: Miss::Empty,
                summary: "model/backend empty-response (EMPTY_ANSWER)".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
                observe_health: true,
            },
            ScriptAction::Missed {
                miss: Miss::Empty,
                summary: "model/backend empty-response (EMPTY_ANSWER)".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
                observe_health: true,
            },
            ScriptAction::Missed {
                miss: Miss::Empty,
                summary: "model/backend empty-response (EMPTY_ANSWER)".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
                observe_health: true,
            },
            ScriptAction::Missed {
                miss: Miss::Empty,
                summary: "model/backend empty-response (EMPTY_ANSWER)".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
                observe_health: true,
            },
            ScriptAction::Answer {
                text: "from-b".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
            },
        ]);
        let mut cancel = Cancellation::none();
        let res = exec.next_answer(&mut step, &mut cancel).await.unwrap();
        assert_eq!(res.value, "from-b");
        assert_eq!(
            step.calls.iter().map(|c| c.route_id.as_str()).collect::<Vec<_>>(),
            ["a", "a", "a", "a", "a", "b"],
            "1 rate-limit retry + 3 empty retries on a, then 4th empty fails over to b"
        );
        assert_eq!(
            step.transitions,
            vec![
                Next::Retry { wait: Duration::from_millis(1) },
                Next::Retry { wait: Duration::from_millis(1) },
                Next::Retry { wait: Duration::from_millis(2) },
                Next::Retry { wait: Duration::from_millis(4) },
                Next::FailOver,
            ]
        );
        let retry_records: Vec<_> = crate::recovery_log::page(&fixture.context, &crate::run_record::Query::default())
            .unwrap()
            .records
            .into_iter()
            .filter(|r| r.event == "route-retry")
            .collect();
        // `run_record` collapses identical-shape `EMPTY_ANSWER` retries inside the window and flushes `repeat: 2` on `failover-step`.
        assert_eq!(retry_records.len(), 3);
        assert_eq!(retry_records.iter().map(|r| r.repeat.unwrap_or(1)).sum::<u32>(), 4);
        assert_eq!(fixture.health(), (vec!["a=failed".into(), "b=ok".into()], false));
        let usage_rows = fixture.usage_rows();
        assert_eq!(usage_rows.len(), 5, "429 has no response end; 4 empties + 1 answer record 5 rows");
        let places: Vec<(i64, i64, &str)> = usage_rows
            .iter()
            .map(|r| (r["step"].as_i64().unwrap(), r["retry"].as_i64().unwrap(), r["route"].as_str().unwrap()))
            .collect();
        assert_eq!(places, [(1, 1, "a"), (2, 2, "a"), (3, 3, "a"), (4, 4, "a"), (5, 5, "b")]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_executor_timeout_post_dispatch_and_refusal_route_health() {
        let cfg_routes = json!({"routes": [
            {"id": "a", "model": "model-a"},
            {"id": "b", "model": "model-b"}
        ]});
        let fixture = TestFixture::new("refusal-dispatch", cfg_routes, "secret-383-refusal");
        let route = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();

        // Refusal on a fails over to b, records refusal usage, and leaves a=unknown in route_health.
        let mut exec = ChainExecutor::new(turn_chain_cfg(&fixture.context, route.clone(), "group:300"));
        let mut step = ScriptedStep::new(vec![
            ScriptAction::Missed {
                miss: Miss::Failed { class: "refusal" },
                summary: "model/backend refusal (MODEL_REFUSAL)".into(),
                response_usage: Some(None),
                tokens_charged: 10,
                observe_health: true,
            },
            ScriptAction::Answer {
                text: "ok-b".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
            },
        ]);
        let mut cancel = Cancellation::none();
        assert_eq!(exec.next_answer(&mut step, &mut cancel).await.unwrap().value, "ok-b");
        assert_eq!(fixture.health(), (vec!["a=unknown".into(), "b=ok".into()], false), "refusal is no route failure");
        let rows = fixture.usage_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0]["route"].as_str(), rows[0]["b"].clone()), (Some("a"), Value::Null));

        // Shortened timeout with observe_health: false does not mark route_health as failed.
        let mut exec_cut = ChainExecutor::new(turn_chain_cfg(&fixture.context, route.clone(), "group:301"));
        let mut step_cut = ScriptedStep::new(vec![
            ScriptAction::Missed {
                miss: Miss::Failed { class: "timeout" },
                summary: "model/backend timeout (TIMEOUT)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: false,
            },
            ScriptAction::Answer {
                text: "ok-b".into(),
                response_usage: None,
                tokens_charged: 0,
            },
        ]);
        assert_eq!(exec_cut.next_answer(&mut step_cut, &mut cancel).await.unwrap().value, "ok-b");
        assert_eq!(fixture.health(), (vec!["a=unknown".into(), "b=ok".into()], false));

        // Timeout after mark_dispatched() ends the turn without failing over.
        let mut exec_disp = ChainExecutor::new(turn_chain_cfg(&fixture.context, route, "group:302"));
        exec_disp.mark_dispatched();
        let mut step_disp = ScriptedStep::new(vec![ScriptAction::Missed {
            miss: Miss::Failed { class: "timeout" },
            summary: "model/backend timeout (TIMEOUT)".into(),
            response_usage: None,
            tokens_charged: 0,
            observe_health: true,
        }]);
        let err = exec_disp.next_answer(&mut step_disp, &mut cancel).await.err().unwrap();
        assert_eq!(err, ChainError::Missed { class: "timeout", summary: "model/backend timeout (TIMEOUT)".into() });
        assert_eq!(step_disp.calls.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_executor_cancellation_aware_backoff_aborts_sleep_immediately() {
        let cfg_routes = json!({"routes": [{"id": "a", "model": "model-a"}, {"id": "b", "model": "model-b"}]});
        let fixture = TestFixture::new("cancel-backoff", cfg_routes, "secret-383-cancel");
        let route = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();

        // Watch cancellation during a 10 s 429 retry backoff.
        let mut cfg = turn_chain_cfg(&fixture.context, route.clone(), "group:400");
        cfg.deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut exec = ChainExecutor::new(cfg);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut step = ScriptedStep::new(vec![ScriptAction::Missed {
            miss: Miss::RateLimited { wait: Some(Duration::from_secs(10)) },
            summary: "model/backend quota (429)".into(),
            response_usage: None,
            tokens_charged: 0,
            observe_health: true,
        }]);
        step.on_call = Some(tx);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut cancel = Cancellation::from_watch(cancel_rx);
        tokio::spawn(async move {
            let _ = rx.recv().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = cancel_tx.send(true);
        });
        let begun = Instant::now();
        let outcome = exec.next_answer(&mut step, &mut cancel).await;
        assert!(matches!(outcome, Err(ChainError::Cancelled)));
        assert!(begun.elapsed() < Duration::from_secs(2), "watch cancellation must abort 10 s backoff immediately");
        assert_eq!(step.calls.len(), 1);

        // AtomicBool abandonment flag during a 10 s empty-answer backoff.
        let mut cfg2 = turn_chain_cfg(&fixture.context, route, "group:401");
        cfg2.retry_unit = Duration::from_secs(10);
        cfg2.deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut exec2 = ChainExecutor::new(cfg2);
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        let mut step2 = ScriptedStep::new(vec![ScriptAction::Missed {
            miss: Miss::Empty,
            summary: "model/backend empty-response (EMPTY_ANSWER)".into(),
            response_usage: None,
            tokens_charged: 0,
            observe_health: true,
        }]);
        step2.on_call = Some(tx2);
        let flag = Arc::new(AtomicBool::new(false));
        let flag_setter = flag.clone();
        let mut cancel2 = Cancellation::from_flag(flag);
        tokio::spawn(async move {
            let _ = rx2.recv().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag_setter.store(true, Ordering::SeqCst);
        });
        let begun = Instant::now();
        let outcome2 = exec2.next_answer(&mut step2, &mut cancel2).await;
        assert!(matches!(outcome2, Err(ChainError::Cancelled)));
        assert!(begun.elapsed() < Duration::from_secs(2), "abandoned flag must abort 10 s backoff immediately");
        assert_eq!(step2.calls.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_executor_cancellation_after_miss_skips_failover_memory_health_and_route_failure() {
        struct CancelAfterMissStep {
            flag: Arc<AtomicBool>,
            calls: Vec<String>,
            failures: Vec<RouteFailure>,
            transitions: Vec<Next>,
        }
        impl RouteStep for CancelAfterMissStep {
            type Output = String;
            fn step<'a>(&'a mut self, ctx: StepContext<'a>) -> Pin<Box<dyn Future<Output = StepResult<String>> + Send + 'a>> {
                Box::pin(async move {
                    self.calls.push(ctx.route.id.clone());
                    self.flag.store(true, Ordering::SeqCst);
                    StepResult::Missed {
                        miss: Miss::Failed { class: "backend" },
                        summary: "model/backend backend (500)".into(),
                        response_usage: None,
                        tokens_charged: 0,
                        request_bytes: 64,
                        message_refs: Vec::new(),
                        observe_health: true,
                    }
                })
            }
            fn on_miss_transition(&mut self, next: Next) -> Result<(), StepAbort> {
                self.transitions.push(next);
                Ok(())
            }
            fn on_route_failure(&mut self, failure: RouteFailure) {
                self.failures.push(failure);
            }
        }

        let cfg_routes = json!({"routes": [{"id": "a", "model": "model-a"}, {"id": "b", "model": "model-b"}]});
        let fixture = TestFixture::new("cancel-after-miss", cfg_routes, "secret-383-cancel-miss");
        let route = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        let mut exec = ChainExecutor::new(turn_chain_cfg(&fixture.context, route, "group:501"));
        let flag = Arc::new(AtomicBool::new(false));
        let mut step = CancelAfterMissStep {
            flag: flag.clone(),
            calls: Vec::new(),
            failures: Vec::new(),
            transitions: Vec::new(),
        };
        let mut cancel = Cancellation::from_flag(flag);
        let res = exec.next_answer(&mut step, &mut cancel).await;
        assert!(matches!(res, Err(ChainError::Cancelled)), "{res:?}");
        assert_eq!(step.calls, ["a"]);
        assert!(step.failures.is_empty(), "cancelled step must not invoke on_route_failure");
        assert!(step.transitions.is_empty(), "cancelled step must not transition to failover");
        assert!(fixture.records("failover-step").is_empty());
        assert!(fixture.records("route-failed").is_empty());
        assert_eq!(fixture.health(), (vec!["a=unknown".into(), "b=unknown".into()], false));
        let mut route_next = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        select_start(&fixture.context, &mut route_next, "group:501", "gen-1");
        assert_eq!(route_next.start, 0, "cancelled step must not mutate failover memory");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_executor_expired_deadline_with_track_steps_prevents_failover() {
        struct SlowMissStep {
            calls: Vec<String>,
            transitions: Vec<Next>,
        }
        impl RouteStep for SlowMissStep {
            type Output = String;
            fn step<'a>(&'a mut self, ctx: StepContext<'a>) -> Pin<Box<dyn Future<Output = StepResult<String>> + Send + 'a>> {
                Box::pin(async move {
                    self.calls.push(ctx.route.id.clone());
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    StepResult::Missed {
                        miss: Miss::Failed { class: "backend" },
                        summary: "model/backend backend (500)".into(),
                        response_usage: None,
                        tokens_charged: 0,
                        request_bytes: 64,
                        message_refs: Vec::new(),
                        observe_health: true,
                    }
                })
            }
            fn on_miss_transition(&mut self, next: Next) -> Result<(), StepAbort> {
                self.transitions.push(next);
                Ok(())
            }
        }

        let cfg_routes = json!({"routes": [{"id": "a", "model": "model-a"}, {"id": "b", "model": "model-b"}]});
        let fixture = TestFixture::new("expired-deadline-track-steps", cfg_routes, "secret-383-deadline");
        let route = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        let mut cfg = turn_chain_cfg(&fixture.context, route, "group:502");
        cfg.deadline = tokio::time::Instant::now() + Duration::from_millis(15);
        let mut exec = ChainExecutor::new(cfg);
        let mut step = SlowMissStep {
            calls: Vec::new(),
            transitions: Vec::new(),
        };
        let mut cancel = Cancellation::none();
        let res = exec.next_answer(&mut step, &mut cancel).await;
        assert!(matches!(res, Err(ChainError::Missed { class: "backend", .. })), "{res:?}");
        assert_eq!(step.calls, ["a"], "expired deadline with track_steps must not fail over to b");
        assert!(step.transitions.is_empty());
        assert!(fixture.records("failover-step").is_empty());
        assert_eq!(fixture.records("route-failed").len(), 1);
        let mut route_next = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        select_start(&fixture.context, &mut route_next, "group:502", "gen-1");
        assert_eq!(route_next.start, 0, "expired deadline must not mutate failover memory");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_executor_dropped_stream_post_send_and_step_usage_tracking() {
        let cfg_routes = json!({"routes": [{"id": "a", "model": "model-a"}, {"id": "b", "model": "model-b"}]});
        let fixture = TestFixture::new("drop-send-usage", cfg_routes, "secret-383-drop");
        let route = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();

        // Turn mode: first Dropped retries once immediately; second Dropped fails over to b; next_answer after tool
        // round resets retry=0 while keeping step advancing.
        let mut exec = ChainExecutor::new(turn_chain_cfg(&fixture.context, route.clone(), "group:500"));
        let mut step = ScriptedStep::new(vec![
            ScriptAction::Missed {
                miss: Miss::Dropped,
                summary: "model/backend network (INCOMPLETE_STREAM)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: true,
            },
            ScriptAction::Missed {
                miss: Miss::Dropped,
                summary: "model/backend network (INCOMPLETE_STREAM)".into(),
                response_usage: None,
                tokens_charged: 0,
                observe_health: true,
            },
            ScriptAction::Answer {
                text: "round-1".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 15,
            },
        ]);
        let mut cancel = Cancellation::none();
        assert_eq!(exec.next_answer(&mut step, &mut cancel).await.unwrap().value, "round-1");
        assert_eq!(step.transitions, vec![Next::Retry { wait: Duration::ZERO }, Next::FailOver]);
        assert_eq!(
            step.calls.iter().map(|c| c.route_id.as_str()).collect::<Vec<_>>(),
            ["a", "a", "b"]
        );

        let mut step2 = ScriptedStep::new(vec![ScriptAction::Answer {
            text: "round-2".into(),
            response_usage: Some(Some(sample_usage())),
            tokens_charged: 15,
        }]);
        assert_eq!(exec.next_answer(&mut step2, &mut cancel).await.unwrap().value, "round-2");
        assert_eq!(exec.steps_used(), 4);

        // Post-send miss ends the turn without failing over.
        exec.mark_sent();
        let mut step3 = ScriptedStep::new(vec![ScriptAction::Missed {
            miss: Miss::Failed { class: "backend" },
            summary: "model/backend backend (500)".into(),
            response_usage: None,
            tokens_charged: 0,
            observe_health: true,
        }]);
        let err = exec.next_answer(&mut step3, &mut cancel).await.err().unwrap();
        assert_eq!(err, ChainError::Missed { class: "backend", summary: "model/backend backend (500)".into() });

        // One-shot walk mode (`track_steps: false`): step stays 0 across failovers while retry increments.
        let mut one_shot_cfg = turn_chain_cfg(&fixture.context, route, usage::DIGEST_KEY);
        one_shot_cfg.usage_job = usage::DIGEST;
        one_shot_cfg.track_steps = false;
        one_shot_cfg.allow_stream_retry = false;
        one_shot_cfg.complete_on_answer = true;
        let mut walk_exec = ChainExecutor::new(one_shot_cfg);
        let mut walk_step = ScriptedStep::new(vec![
            ScriptAction::Missed {
                miss: Miss::Empty,
                summary: "empty-response".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 0,
                observe_health: true,
            },
            ScriptAction::Answer {
                text: "[]".into(),
                response_usage: Some(Some(sample_usage())),
                tokens_charged: 0,
            },
        ]);
        assert_eq!(walk_exec.next_answer(&mut walk_step, &mut cancel).await.unwrap().value, "[]");

        let rows = fixture.usage_rows();
        let summary_rows: Vec<(&str, i64, i64, &str)> = rows
            .iter()
            .map(|r| (r["job"].as_str().unwrap(), r["step"].as_i64().unwrap(), r["retry"].as_i64().unwrap(), r["route"].as_str().unwrap()))
            .collect();
        assert_eq!(
            summary_rows,
            [("turn", 2, 2, "b"), ("turn", 3, 0, "b"), ("digest", 0, 0, "a"), ("digest", 0, 1, "a")]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chain_executor_records_ready_usage_when_cancel_races_step_completion() {
        use tokio::sync::oneshot;

        struct CancelRaceStep {
            entered: Option<oneshot::Sender<()>>,
            ready: Option<oneshot::Receiver<()>>,
        }

        impl RouteStep for CancelRaceStep {
            type Output = String;

            fn step<'a>(
                &'a mut self,
                _ctx: StepContext<'a>,
            ) -> Pin<Box<dyn Future<Output = StepResult<String>> + Send + 'a>> {
                let entered = self.entered.take();
                let ready = self.ready.take();
                Box::pin(async move {
                    if let Some(tx) = entered {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = ready {
                        let _ = rx.await;
                    }
                    StepResult::Answered {
                        value: "late-answer".into(),
                        response_usage: Some(Some(sample_usage())),
                        tokens_charged: 15,
                    }
                })
            }
        }

        let cfg_routes = json!({"routes": [route("a")]});
        let fixture = TestFixture::new("cancel-race", cfg_routes, "secret-259-cancel-race");
        let model = ModelRoute::from_runtime(&fixture.context, &fixture.cfg).unwrap();
        let mut exec = ChainExecutor::new(turn_chain_cfg(&fixture.context, model, "group:9"));
        let (entered_tx, entered_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let mut step = CancelRaceStep {
            entered: Some(entered_tx),
            ready: Some(ready_rx),
        };
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut cancel = Cancellation::from_watch(cancel_rx);
        let racer = tokio::spawn(async move {
            let _ = entered_rx.await;
            let _ = cancel_tx.send(true);
            let _ = ready_tx.send(());
        });
        let err = exec.next_answer(&mut step, &mut cancel).await.unwrap_err();
        racer.await.unwrap();
        assert_eq!(err, ChainError::Cancelled);
        let rows = fixture.usage_rows();
        assert_eq!(
            rows.len(),
            1,
            "ready usage must still be recorded before returning Cancelled"
        );
        assert_eq!(rows[0]["b"]["cacheMiss"], 10);
        assert_eq!(rows[0]["b"]["output"], 5);
    }
}
