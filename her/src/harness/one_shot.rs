//! Tool-less one-shot model jobs: non-streaming completions on model routes of the selected
//! runtime's relay, authenticated with the harness credential, with no tools and no history.
//! Both the digest (`Job::Digest`, #238) and slang learning (`Job::SlangLearner`, #258) walk
//! their routes through `harness::routes::ChainExecutor` + `OneShotStep` (`RouteWalk`), sharing
//! `genai::Client`, `routes::block_on_shared`, and centralized refusal/timeout/empty-response
//! normalization. Tests install per-context fakes via `install_test_model`.
use crate::harness::routes::{
    self, Cancellation, ChainConfig, ChainError, ChainExecutor, ModelRoute, OneShotStep,
    RouteSpec,
};
use crate::harness::usage;
use crate::model_failure::Failure;
use crate::runtime::RuntimeContext;
use serde_json::Value;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[cfg(test)]
mod walk_tests;

/// The output cap of one digest request.
pub(crate) const DIGEST_MAX_TOKENS: u32 = 2048;

/// A fixed failure class (`quota`, `timeout`, `backend`, `network`, `refusal`, `empty-response`,
/// `unconfigured`, ...); never provider text.
#[derive(Debug, PartialEq)]
pub(crate) struct ModelError(pub &'static str);

/// Classified one-shot failure carrying the redacted model id that answered or failed last.
#[derive(Debug)]
pub(crate) struct JobFailure {
    pub failure: Failure,
    pub model: String,
}

/// The job that asks. Tests install their fake per context and job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    Digest,
    SlangLearner,
}

impl Job {
    /// The `job` value of this job's usage facts (`harness::usage`).
    fn usage_job(self) -> &'static str {
        match self {
            Job::Digest => usage::DIGEST,
            Job::SlangLearner => usage::SLANG,
        }
    }
}

pub(crate) trait OneShot: Send + Sync {
    /// One completion: the `system` text (left out when empty), then the user `input`.
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError>;

    /// One completion returning the classified `Failure` (preserving `errorCode`) on miss.
    fn complete_failure(&self, system: &str, input: &str) -> Result<String, Failure> {
        self.complete(system, input)
            .map_err(|error| normalize_failure(Failure::new(error.0, error.0)))
    }

    /// One completion taking no longer than `within` and sending nothing once `abandoned` is set.
    fn complete_within(
        &self,
        system: &str,
        input: &str,
        within: Duration,
        abandoned: &Arc<AtomicBool>,
    ) -> Result<String, ModelError> {
        let _ = (within, abandoned);
        self.complete(system, input)
    }
}

/// Centralized output normalization across live `OneShotStep` and test `OneShot` mocks: classifies
/// whitespace-only responses as `empty-response`, diagnostic/error envelopes via
/// `backend::plain_output_failure`, and refusal prose (unless carrying a JSON array `[`) as
/// `refusal`.
pub(crate) fn normalize_response_text(text: &str) -> Result<String, Failure> {
    if text.trim().is_empty() {
        return Err(Failure::new("empty-response", "EMPTY_ANSWER"));
    }
    if let Some(failure) = crate::backend::plain_output_failure(text) {
        return Err(failure);
    }
    if !text.contains('[') && Failure::is_refusal(text) {
        return Err(Failure::new("refusal", "MODEL_REFUSAL"));
    }
    Ok(text.to_string())
}

/// Normalizes a `Failure` whose `code` only echoes its `class` (e.g. from `ModelError` shims) into
/// canonical error codes without losing existing structured codes.
pub(crate) fn normalize_failure(failure: Failure) -> Failure {
    if failure.code != failure.class {
        return failure;
    }
    match failure.class {
        "timeout" => Failure::new("timeout", "TIMEOUT"),
        "quota" => Failure::new("quota", "QUOTA_EXCEEDED"),
        "refusal" => Failure::new("refusal", "MODEL_REFUSAL"),
        "empty-response" => Failure::new("empty-response", "EMPTY_ANSWER"),
        "network" => Failure::new("network", "MODEL_CONNECTION_ERROR"),
        "cancelled" | "unconfigured" | "backend" => failure,
        other => Failure::classify("", other),
    }
}

#[cfg(test)]
struct NormalizedTestModel(Arc<dyn OneShot>);

#[cfg(test)]
impl OneShot for NormalizedTestModel {
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError> {
        // Keep raw `complete` passthrough for direct `Digest::run_conversation` unit fixtures
        // (`session_rotation/tests.rs` distinguishes `DigestError::Output("refusal-or-error")`).
        self.0.complete(system, input)
    }

    fn complete_failure(&self, system: &str, input: &str) -> Result<String, Failure> {
        self.0
            .complete_failure(system, input)
            .map_err(normalize_failure)
            .and_then(|text| normalize_response_text(&text))
    }

    fn complete_within(
        &self,
        system: &str,
        input: &str,
        within: Duration,
        abandoned: &Arc<AtomicBool>,
    ) -> Result<String, ModelError> {
        self.0.complete_within(system, input, within, abandoned)
    }
}

/// Tests: every `job` request on `context` uses `model`.
#[cfg(test)]
pub(crate) fn install_test_model(context: &RuntimeContext, job: Job, model: Arc<dyn OneShot>) {
    context.set_test_one_shot(job, model);
}

/// The routes a one-shot walk tries (`RouteWalk`), in order, and whether they are the configured
/// harness chain (`harness::routes::chain`), whose route health the walk observes (`route_health`).
#[derive(Clone, Debug)]
pub(crate) struct Walk {
    pub routes: Vec<RouteSpec>,
    pub chain: bool,
}

/// Retry and failover policy for a `RouteWalk`.
#[derive(Clone, Copy, Debug)]
struct WalkPolicy {
    allow_retries: bool,
    retry_empty: bool,
    use_failover_memory: bool,
    record_events: bool,
}

impl WalkPolicy {
    fn for_job(job: Job) -> Self {
        Self {
            allow_retries: true,
            retry_empty: matches!(job, Job::Digest),
            use_failover_memory: true,
            record_events: true,
        }
    }

    #[cfg(test)]
    fn single_route() -> Self {
        Self {
            allow_retries: false,
            retry_empty: false,
            use_failover_memory: false,
            record_events: false,
        }
    }
}

/// The routes a digest request walks: `memory.digest.model` alone (no failover, no route health),
/// else the harness chain (`harness::routes::chain`: `harness.routes`, else `harness.model`);
/// `None` when neither is configured or the chain is invalid.
pub(crate) fn digest_walk_of(cfg: &Value) -> Option<Walk> {
    if let Some(model) = crate::settings::DIGEST_MODEL.text(cfg) {
        return Some(Walk {
            routes: vec![RouteSpec::implicit(model)],
            chain: false,
        });
    }
    routes::chain(cfg)
        .ok()
        .map(|routes| Walk { routes, chain: true })
}

/// The digest's first route model id; `None` when unconfigured or invalid.
#[cfg(test)]
pub(crate) fn digest_model_id(cfg: &Value) -> Option<String> {
    digest_walk_of(cfg)
        .and_then(|walk| walk.routes.into_iter().next())
        .map(|route| route.model)
}

/// The bound on one digest request, every route its walk tries included:
/// `memory.digest.timeoutSecs`.
pub(crate) fn digest_timeout(cfg: &Value) -> Duration {
    Duration::from_secs(
        crate::settings::DIGEST_TIMEOUT_SECS
            .integer(cfg)
            .unsigned_abs(),
    )
}

/// The digest's `OneShot` model on `context`.
pub(crate) fn digest_model(
    context: &RuntimeContext,
    cfg: &Value,
) -> Result<Arc<dyn OneShot>, ModelError> {
    walk_for(
        context,
        Job::Digest,
        usage::DIGEST_KEY,
        digest_walk_of(cfg),
        digest_timeout(cfg),
        DIGEST_MAX_TOKENS,
    )
}

/// Sets its flag when dropped so a cancelled summary's test fake stops waiting.
#[cfg(test)]
struct AbandonOnExit(Arc<AtomicBool>);

#[cfg(test)]
impl Drop for AbandonOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Async digest route walk (or installed test fake) bounded by `within` and
/// `memory.digest.timeoutSecs`.
pub(crate) async fn digest_summarise_async(
    context: &RuntimeContext,
    cfg: &Value,
    system: &str,
    input: &str,
    within: Duration,
    cancel: &mut Cancellation,
) -> Result<String, ModelError> {
    #[cfg(test)]
    if let Some(model) = context.test_one_shot(Job::Digest) {
        if within.is_zero() {
            return Err(ModelError("timeout"));
        }
        let system = system.to_string();
        let input = input.to_string();
        let abandoned = Arc::new(AtomicBool::new(false));
        let _abandon = AbandonOnExit(abandoned.clone());
        let flag = abandoned.clone();
        let task = tokio::task::spawn_blocking(move || {
            model.complete_within(&system, &input, within, &flag)
        });
        let joined = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(ModelError("cancelled")),
            joined = tokio::time::timeout(within, task) => joined,
        };
        return match joined {
            Err(_elapsed) => Err(ModelError("timeout")),
            Ok(Err(_join)) => Err(ModelError("worker")),
            Ok(Ok(res)) => res,
        };
    }
    let walk = digest_walk_of(cfg).ok_or(ModelError("unconfigured"))?;
    let route = ModelRoute::for_specs(context, walk.routes.clone(), walk.routes)
        .map_err(|_| ModelError("unconfigured"))?;
    if within.is_zero() {
        return Err(ModelError("timeout"));
    }
    run_walk_async(
        WalkRequest {
            context: context.clone(),
            route,
            chain: walk.chain,
            key: usage::DIGEST_KEY.to_string(),
            usage_job: usage::DIGEST,
            timeout: digest_timeout(cfg),
            max_tokens: DIGEST_MAX_TOKENS,
            unit: Duration::from_secs(1),
            system: system.to_string(),
            input: input.to_string(),
            within,
            policy: WalkPolicy::for_job(Job::Digest),
        },
        cancel,
    )
    .await
}

/// Runs one slang-learning completion on `context` across the configured harness route chain (or
/// installed `Job::SlangLearner` test fake), normalizing failures and preserving the failing model.
pub(crate) fn slang_complete(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    prompt: &str,
    timeout: Duration,
) -> Result<String, JobFailure> {
    let chain = routes::chain(cfg).ok();
    let fallback_model = chain
        .as_ref()
        .and_then(|routes| routes.first())
        .map_or_else(|| "unspecified".to_string(), |route| route.model.clone());
    #[cfg(test)]
    if let Some(model) = context.test_one_shot(Job::SlangLearner) {
        return NormalizedTestModel(model)
            .complete_failure("", prompt)
            .map_err(|failure| JobFailure {
                failure,
                model: fallback_model,
            });
    }
    let routes = chain.ok_or_else(|| JobFailure {
        failure: Failure::new("unconfigured", "unconfigured"),
        model: "unspecified".to_string(),
    })?;
    let walk = Walk {
        routes,
        chain: true,
    };
    let walker = RouteWalk::new(
        context,
        Job::SlangLearner,
        key,
        walk,
        timeout,
        routes::MAX_OUTPUT_TOKENS,
    )
    .map_err(|error| JobFailure {
        failure: Failure::new(error.0, error.0),
        model: fallback_model,
    })?;
    walker.run_within_failure("", prompt, timeout, Cancellation::none())
}

/// The model `job` asks on `context` through a route walk (`RouteWalk`) over `walk` (`None`: not
/// configured), bounded as a whole by `timeout`, each request answering at most `max_tokens`
/// tokens. Each response end is a usage fact (`harness::usage`) under the ledger key `key`.
pub(crate) fn walk_for(
    context: &RuntimeContext,
    job: Job,
    key: &str,
    walk: Option<Walk>,
    timeout: Duration,
    max_tokens: u32,
) -> Result<Arc<dyn OneShot>, ModelError> {
    #[cfg(test)]
    if let Some(model) = context.test_one_shot(job) {
        return Ok(Arc::new(NormalizedTestModel(model)));
    }
    let walk = walk.ok_or(ModelError("unconfigured"))?;
    Ok(Arc::new(RouteWalk::new(
        context, job, key, walk, timeout, max_tokens,
    )?))
}

/// Single-route non-retrying relay model for digest unit tests.
#[cfg(test)]
pub(crate) fn digest_relay_model(
    context: &RuntimeContext,
    cfg: &Value,
) -> Result<RouteWalk, ModelError> {
    let first = digest_walk_of(cfg)
        .and_then(|walk| walk.routes.into_iter().next())
        .ok_or(ModelError("unconfigured"))?;
    RouteWalk::single_route(
        context,
        Job::Digest,
        usage::DIGEST_KEY,
        first,
        digest_timeout(cfg),
        DIGEST_MAX_TOKENS,
    )
}

/// Maps a `ChainError` from `routes::ChainExecutor` to a classified `Failure`.
fn chain_failure(error: ChainError) -> Failure {
    match error {
        ChainError::Cancelled => Failure::new("cancelled", "cancelled"),
        ChainError::Aborted(reason) if reason == "timeout" => Failure::new("timeout", "TIMEOUT"),
        ChainError::Aborted(_) => Failure::new("network", "MODEL_CONNECTION_ERROR"),
        ChainError::Missed { class, summary } => {
            let parsed = Failure::classify("", &summary);
            if parsed.class == class {
                parsed
            } else {
                Failure::new(class, &parsed.code)
            }
        }
    }
}

/// Request configuration for `run_walk_async`.
struct WalkRequest {
    context: RuntimeContext,
    route: ModelRoute,
    chain: bool,
    key: String,
    usage_job: &'static str,
    timeout: Duration,
    max_tokens: u32,
    unit: Duration,
    system: String,
    input: String,
    within: Duration,
    policy: WalkPolicy,
}

/// Async one-shot route walk over `request.route` via `routes::ChainExecutor` and `OneShotStep`,
/// returning classified failures and the failing route model.
async fn run_walk_failure_async(
    mut request: WalkRequest,
    cancel: &mut Cancellation,
) -> Result<String, JobFailure> {
    let fallback_model = |route: &ModelRoute| {
        route
            .routes
            .get(route.start)
            .or_else(|| route.routes.first())
            .map_or_else(|| "unspecified".to_string(), |r| r.model.clone())
    };
    if cancel.is_cancelled() {
        return Err(JobFailure {
            failure: Failure::new("cancelled", "cancelled"),
            model: fallback_model(&request.route),
        });
    }
    if request.policy.use_failover_memory {
        routes::select_start(&request.context, &mut request.route, &request.key, "");
    }
    let default_model = fallback_model(&request.route);
    let max_steps = if request.policy.allow_retries {
        usize::MAX
    } else {
        1
    };
    let mut executor = ChainExecutor::new(ChainConfig {
        context: request.context.clone(),
        route: request.route,
        key: request.key,
        generation: String::new(),
        usage_job: request.usage_job,
        track_steps: false,
        observe_health: request.chain,
        use_failover_memory: request.policy.use_failover_memory,
        record_events: request.policy.record_events,
        record_usage: true,
        allow_retries: request.policy.allow_retries,
        retry_empty: request.policy.retry_empty,
        allow_stream_retry: false,
        complete_on_answer: true,
        fail_action: "the request ends",
        deadline_error: "timeout",
        retry_unit: request.unit,
        max_steps,
        max_tokens: None,
        job_max_tokens: request.max_tokens,
        deadline: tokio::time::Instant::now() + request.within.min(request.timeout),
    });
    let mut adapter = OneShotStep {
        context: request.context,
        system: request.system,
        input: request.input,
        shortened_by_bound: request.within < request.timeout,
        last_model: None,
    };
    executor
        .next_answer(&mut adapter, cancel)
        .await
        .map(|success| success.value)
        .map_err(|error| JobFailure {
            failure: chain_failure(error),
            model: adapter.last_model.unwrap_or(default_model),
        })
}

/// Async one-shot route walk over `request.route` via `routes::ChainExecutor` and `OneShotStep`.
async fn run_walk_async(
    request: WalkRequest,
    cancel: &mut Cancellation,
) -> Result<String, ModelError> {
    run_walk_failure_async(request, cancel)
        .await
        .map_err(|err| ModelError(err.failure.class))
}

/// One-shot requests over routes in order, as a harness turn walks its chain
/// (`routes::ChainExecutor`): a first 429 is retried once on the same route after its hint, a
/// repeat 429 and any other failure (HTTP error, timeout, network, refusal, error-shaped answer)
/// move to the next route, and for `Job::Digest` an empty answer retries up to
/// `routes::EMPTY_RETRIES` times first. The walk is bounded by `within` (at most `timeout`).
pub(crate) struct RouteWalk {
    context: RuntimeContext,
    route: ModelRoute,
    /// The routes are the configured chain, whose route health the walk observes.
    chain: bool,
    /// The ledger key: the usage facts' key, failover memory conversation, and run-record subject.
    key: String,
    usage_job: &'static str,
    /// The bound of one request on any route; the walk's bound is at most this.
    timeout: Duration,
    max_tokens: u32,
    /// The base wait of retries (`routes::Situation::unit`): one second; tests shorten it.
    unit: Duration,
    policy: WalkPolicy,
}

impl RouteWalk {
    pub(crate) fn new(
        context: &RuntimeContext,
        job: Job,
        key: &str,
        walk: Walk,
        timeout: Duration,
        max_tokens: u32,
    ) -> Result<Self, ModelError> {
        let route = ModelRoute::for_specs(context, walk.routes.clone(), walk.routes)
            .map_err(|_| ModelError("unconfigured"))?;
        Ok(Self {
            context: context.clone(),
            route,
            chain: walk.chain,
            key: key.to_string(),
            usage_job: job.usage_job(),
            timeout,
            max_tokens,
            unit: Duration::from_secs(1),
            policy: WalkPolicy::for_job(job),
        })
    }

    #[cfg(test)]
    pub(crate) fn single_route(
        context: &RuntimeContext,
        job: Job,
        key: &str,
        spec: RouteSpec,
        timeout: Duration,
        max_tokens: u32,
    ) -> Result<Self, ModelError> {
        let route = ModelRoute::for_specs(context, vec![spec], Vec::new())
            .map_err(|_| ModelError("unconfigured"))?;
        Ok(Self {
            context: context.clone(),
            route,
            chain: false,
            key: key.to_string(),
            usage_job: job.usage_job(),
            timeout,
            max_tokens,
            unit: Duration::from_secs(1),
            policy: WalkPolicy::single_route(),
        })
    }

    /// This walk with retries waiting `unit` instead of one second (tests).
    #[cfg(test)]
    pub(crate) fn with_unit(mut self, unit: Duration) -> Self {
        self.unit = unit;
        self
    }

    fn run_within_failure(
        &self,
        system: &str,
        input: &str,
        within: Duration,
        mut cancel: Cancellation,
    ) -> Result<String, JobFailure> {
        let default_model = self
            .route
            .routes
            .first()
            .map_or_else(|| "unspecified".to_string(), |r| r.model.clone());
        let request = WalkRequest {
            context: self.context.clone(),
            route: self.route.clone(),
            chain: self.chain,
            key: self.key.clone(),
            usage_job: self.usage_job,
            timeout: self.timeout,
            max_tokens: self.max_tokens,
            unit: self.unit,
            system: system.to_string(),
            input: input.to_string(),
            within,
            policy: self.policy,
        };
        routes::block_on_shared(async move { run_walk_failure_async(request, &mut cancel).await })
            .unwrap_or_else(|_| {
                Err(JobFailure {
                    failure: Failure::new("network", "MODEL_CONNECTION_ERROR"),
                    model: default_model,
                })
            })
    }
}

impl OneShot for RouteWalk {
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError> {
        self.complete_failure(system, input)
            .map_err(|f| ModelError(f.class))
    }

    fn complete_failure(&self, system: &str, input: &str) -> Result<String, Failure> {
        self.run_within_failure(system, input, self.timeout, Cancellation::none())
            .map_err(|err| err.failure)
    }

    fn complete_within(
        &self,
        system: &str,
        input: &str,
        within: Duration,
        abandoned: &Arc<AtomicBool>,
    ) -> Result<String, ModelError> {
        if abandoned.load(Ordering::SeqCst) {
            return Err(ModelError("cancelled"));
        }
        self.run_within_failure(
            system,
            input,
            within,
            Cancellation::from_flag(abandoned.clone()),
        )
        .map_err(|err| ModelError(err.failure.class))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream_capture::test_support as capture;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A relay that answers one request with a completion of `content`; the handle yields the
    /// request body.
    fn relay(content: &'static str) -> (String, std::thread::JoinHandle<Value>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let start = loop {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0, "the request ended early");
                bytes.extend_from_slice(&chunk[..n]);
                let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    break end + 4;
                }
            };
            let body = json!({
                "id": "fixture",
                "object": "chat.completion",
                "created": 1,
                "model": "fake-chat",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            serde_json::from_slice(&bytes[start..]).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn one_shot_sends_its_input_alone_without_tools() {
        let (url, request) = relay("[]");
        let (_root, context) = capture::runtime("one-shot-alone", &url);
        let spec = RouteSpec {
            id: "route-a".into(),
            model: "fake-chat".into(),
            effort: None,
            max_output_tokens: 321,
            retry_429: false,
        };
        let model = RouteWalk::single_route(
            &context,
            Job::Digest,
            "internal:digest",
            spec,
            Duration::from_secs(5),
            321,
        )
        .unwrap();
        assert_eq!(model.complete("", "one-shot-input-11"), Ok("[]".to_string()));
        let body = request.join().unwrap();
        assert_eq!(body["model"], "fake-chat");
        assert_eq!(body["max_tokens"], 321);
        assert!(
            body.get("reasoning_effort").is_none(),
            "no effort is sent when omitted: {body}"
        );
        assert_ne!(body["stream"], true, "a one-shot job does not stream: {body}");
        assert!(body.get("tools").is_none(), "a one-shot job offers no tool: {body}");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "an empty system text is left out: {body}");
        assert_eq!(
            (messages[0]["role"].as_str(), messages[0]["content"].as_str()),
            (Some("user"), Some("one-shot-input-11")),
            "{body}"
        );
    }

    #[test]
    fn one_shot_forwards_configured_reasoning_effort_on_relay_and_walk() {
        let (url, request) = relay("[]");
        let (_root, context) = capture::runtime("one-shot-effort-relay", &url);
        let spec = RouteSpec {
            id: "route-a".into(),
            model: "fake-chat".into(),
            effort: Some("high".into()),
            max_output_tokens: 321,
            retry_429: false,
        };
        let model = RouteWalk::single_route(
            &context,
            Job::SlangLearner,
            "group:7003",
            spec,
            Duration::from_secs(5),
            321,
        )
        .unwrap();
        assert_eq!(model.complete("", "one-shot-effort"), Ok("[]".to_string()));
        let body = request.join().unwrap();
        assert_eq!(body["reasoning_effort"], "high", "{body}");

        let (url, request) = relay("[]");
        let (_root, context) = capture::runtime("one-shot-effort-walk", &url);
        let walk = Walk {
            routes: vec![RouteSpec {
                id: "route-m".into(),
                model: "fake-chat".into(),
                effort: Some("medium".into()),
                max_output_tokens: 4096,
                retry_429: true,
            }],
            chain: false,
        };
        let walker = walk_for(
            &context,
            Job::Digest,
            "internal:digest",
            Some(walk),
            Duration::from_secs(5),
            256,
        )
        .unwrap();
        assert_eq!(walker.complete("sys", "walk-effort"), Ok("[]".to_string()));
        let body = request.join().unwrap();
        assert_eq!(body["reasoning_effort"], "medium", "{body}");
    }

    #[test]
    fn one_shot_request_is_taken_by_an_armed_capture() {
        let (url, request) = relay("[]");
        let (_root, context) = capture::runtime("one-shot-capture", &url);
        capture::arm(&context, "harness");
        let spec = RouteSpec {
            id: "route-a".into(),
            model: "fake-chat".into(),
            effort: None,
            max_output_tokens: 321,
            retry_429: false,
        };
        let model = RouteWalk::single_route(
            &context,
            Job::Digest,
            "internal:digest",
            spec,
            Duration::from_secs(5),
            321,
        )
        .unwrap();
        assert_eq!(model.complete("", "one-shot-input-11"), Ok("[]".to_string()));
        request.join().unwrap();
        let record =
            capture::settled(&context).expect("the armed capture took the one-shot request");
        assert_eq!(
            (record["route"].as_str(), record["request"]["model"].as_str()),
            (Some("harness"), Some("fake-chat")),
            "{record}"
        );
        assert_eq!(record["request"]["maxTokens"], 321, "{record}");
        assert!(
            record["request"]["request"]
                .to_string()
                .contains("one-shot-input-11"),
            "{record}"
        );
    }

    #[test]
    fn harness_usage_the_slang_one_shot_records_one_fact_under_its_key() {
        let (url, request) = relay("[]");
        let (root, context) = capture::runtime("one-shot-usage", &url);
        let cfg = json!({
            "harness": {"routes": [{"id": "route-a", "model": "fake-chat", "maxOutputTokens": 321}]}
        });
        assert_eq!(
            slang_complete(
                &context,
                &cfg,
                "group:7002",
                "one-shot-usage-input",
                Duration::from_secs(5),
            )
            .unwrap(),
            "[]"
        );
        request.join().unwrap();
        let text =
            std::fs::read_to_string(root.0.join("data/token-usage-rust.jsonl")).unwrap_or_default();
        let rows: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "one answered request is one usage fact: {rows:?}"
        );
        assert_eq!(
            (
                rows[0]["key"].as_str(),
                rows[0]["route"].as_str(),
                rows[0]["model"].as_str(),
                rows[0]["job"].as_str(),
            ),
            (
                Some("group:7002"),
                Some("route-a"),
                Some("fake-chat"),
                Some("slang"),
            ),
            "{}",
            rows[0]
        );
        assert_eq!(
            rows[0]["b"],
            json!({"cacheMiss": 1, "cacheHit": 0, "cacheWrite": 0, "output": 1, "reasoning": 0})
        );
    }

    #[test]
    fn harness_usage_the_one_shot_redacts_the_credential_from_its_row_and_capture() {
        let secret = "synthetic-model-key-183";
        let (url, request) = relay("[]");
        let (root, context) = capture::runtime("one-shot-redact", &url);
        capture::arm(&context, "harness");
        let route_id = format!("route-{secret}");
        let model_name = format!("fake-chat-{secret}");
        let spec = RouteSpec {
            id: route_id,
            model: model_name,
            effort: None,
            max_output_tokens: 321,
            retry_429: false,
        };
        let model = RouteWalk::single_route(
            &context,
            Job::Digest,
            "internal:digest",
            spec,
            Duration::from_secs(5),
            321,
        )
        .unwrap();
        assert_eq!(model.complete("", "one-shot-redact-input"), Ok("[]".to_string()));
        request.join().unwrap();
        let text =
            std::fs::read_to_string(root.0.join("data/token-usage-rust.jsonl")).unwrap_or_default();
        let rows: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0]["route"].as_str(), rows[0]["model"].as_str()),
            (Some("route-[REDACTED]"), Some("fake-chat-[REDACTED]")),
            "{}",
            rows[0]
        );
        assert!(
            !text.contains(secret),
            "the ledger never keeps the harness credential: {text}"
        );
        let record =
            capture::settled(&context).expect("the armed capture took the one-shot request");
        assert_eq!(record["request"]["model"], "fake-chat-[REDACTED]", "{record}");
    }

    struct MockOneShot(Mutex<VecDeque<Result<String, ModelError>>>);

    impl OneShot for MockOneShot {
        fn complete(&self, _system: &str, _input: &str) -> Result<String, ModelError> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ModelError("backend")))
        }
    }

    #[test]
    fn one_shot_normalizes_refusal_empty_diagnostic_and_timeout_on_mock_seam() {
        let (_root, context) = capture::runtime("one-shot-norm-mock", "http://127.0.0.1:1");
        let cfg = json!({"harness": {"model": "mock-model"}});
        let mock = Arc::new(MockOneShot(Mutex::new(VecDeque::from([
            Ok("   ".to_string()),
            Ok("content_filter".to_string()),
            Ok("Error: upstream failed".to_string()),
            Ok(r#"[{"content":"content filter","source_id":1}]"#.to_string()),
            Err(ModelError("timeout")),
            Err(ModelError("quota")),
        ]))));
        install_test_model(&context, Job::SlangLearner, mock);

        let e1 =
            slang_complete(&context, &cfg, "group:1", "p", Duration::from_secs(5)).unwrap_err();
        assert_eq!(
            (e1.failure.class, e1.failure.code.as_str()),
            ("empty-response", "EMPTY_ANSWER")
        );

        let e2 =
            slang_complete(&context, &cfg, "group:1", "p", Duration::from_secs(5)).unwrap_err();
        assert_eq!(
            (e2.failure.class, e2.failure.code.as_str()),
            ("refusal", "MODEL_REFUSAL")
        );

        let e3 =
            slang_complete(&context, &cfg, "group:1", "p", Duration::from_secs(5)).unwrap_err();
        assert_eq!(e3.failure.class, "backend");

        let ok = slang_complete(&context, &cfg, "group:1", "p", Duration::from_secs(5)).unwrap();
        assert!(ok.contains("content filter"));

        let e4 =
            slang_complete(&context, &cfg, "group:1", "p", Duration::from_secs(5)).unwrap_err();
        assert_eq!(
            (e4.failure.class, e4.failure.code.as_str()),
            ("timeout", "TIMEOUT")
        );

        let e5 =
            slang_complete(&context, &cfg, "group:1", "p", Duration::from_secs(5)).unwrap_err();
        assert_eq!(
            (e5.failure.class, e5.failure.code.as_str()),
            ("quota", "QUOTA_EXCEEDED")
        );
    }

    #[test]
    fn slang_complete_fails_over_across_routes_on_error_refusal_and_empty() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let replies = Arc::new(Mutex::new(VecDeque::from([
            (500u16, json!({"error": {"message": "busy"}}).to_string()),
            (
                200u16,
                json!({
                    "id": "r1",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "model-b",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "[]"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            ),
            (
                200u16,
                json!({
                    "id": "r2",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "model-b",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "   "},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            ),
            (
                200u16,
                json!({
                    "id": "r3",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "model-a",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "content_filter"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            ),
            (
                200u16,
                json!({
                    "id": "r4",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "model-b",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "content_filter"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            ),
        ])));
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let asked_log = asked.clone();
        let server_replies = replies.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length])
                            .unwrap();
                    }
                };
                asked_log
                    .lock()
                    .unwrap()
                    .push(body["model"].as_str().unwrap_or("").to_string());
                let (code, text) = server_replies.lock().unwrap().pop_front().unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
        });
        let (_root, context) = capture::runtime("slang-failover", &url);
        let cfg = json!({
            "harness": {
                "routes": [
                    {"id": "a", "model": "model-a"},
                    {"id": "b", "model": "model-b"}
                ]
            }
        });
        // Call 1: model-a returns 500 -> fails over to model-b -> returns "[]".
        assert_eq!(
            slang_complete(&context, &cfg, "group:fo", "p1", Duration::from_secs(5)).unwrap(),
            "[]"
        );
        // Call 2: sticky start is model-b, which returns whitespace -> normalized empty-response,
        // fails without 3x same-route retry since model-b is the tail of this walk.
        let err_empty =
            slang_complete(&context, &cfg, "group:fo", "p2", Duration::from_secs(5)).unwrap_err();
        assert_eq!(
            (err_empty.failure.class, err_empty.model.as_str()),
            ("empty-response", "model-b")
        );

        // Call 3 on fresh key: model-a returns plain-text refusal "content_filter", fails over
        // to model-b which also returns refusal -> JobFailure names model-b.
        let err_refusal =
            slang_complete(&context, &cfg, "group:fo2", "p3", Duration::from_secs(5)).unwrap_err();
        assert_eq!(
            (
                err_refusal.failure.class,
                err_refusal.failure.code.as_str(),
                err_refusal.model.as_str(),
            ),
            ("refusal", "MODEL_REFUSAL", "model-b")
        );
        server.join().unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            vec!["model-a", "model-b", "model-b", "model-a", "model-b"]
        );
    }
}
