//! Tool-less one-shot model jobs: non-streaming completions on model routes of the selected runtime's relay,
//! authenticated with the harness credential, with no tools and no history. Slang learning asks one route
//! (`RelayModel`); the digest and handover summaries walk their routes as harness turns walk their chain
//! (`RouteWalk`, #238). Both execute through `harness::routes::ChainExecutor` + `OneShotStep` and reuse its shared
//! `genai::Client` and shared Tokio runtime (`routes::block_on_shared`). Tests install per-context fakes via
//! `install_test_model`.
use crate::harness::routes::{self, ChainConfig, ChainError, ChainExecutor, Cancellation, ModelRoute, OneShotStep, RouteSpec};
use crate::runtime::RuntimeContext;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[cfg(test)]
mod walk_tests;

/// A fixed failure class (`quota`, `timeout`, `backend`, `network`, `refusal`, `empty-response`, `unconfigured`, ...);
/// never provider text.
#[derive(Debug, PartialEq)]
pub(crate) struct ModelError(pub &'static str);

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
            Job::Digest => crate::harness::usage::DIGEST,
            Job::SlangLearner => crate::harness::usage::SLANG,
        }
    }
}

/// The route a one-shot request asks: its id (a usage fact's `route`), its relay model, and its optional
/// `reasoningEffort`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteModel<'a> {
    pub id: &'a str,
    pub model: &'a str,
    pub effort: Option<&'a str>,
}

pub(crate) trait OneShot: Send + Sync {
    /// One completion: the `system` text (left out when empty), then the user `input`.
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError>;

    /// One completion returning the classified `model_failure::Failure` (preserving `errorCode`) on miss.
    fn complete_failure(&self, system: &str, input: &str) -> Result<String, crate::model_failure::Failure> {
        self.complete(system, input)
            .map_err(|error| crate::model_failure::Failure::new(error.0, error.0))
    }

    /// One completion that takes no longer than `within` and sends no request once `abandoned` is set (a caller that
    /// stopped waiting sets it). The default answers through `complete`, which ignores both: test fakes only.
    fn complete_within(&self, system: &str, input: &str, within: Duration, abandoned: &Arc<AtomicBool>) -> Result<String, ModelError> {
        let _ = (within, abandoned);
        self.complete(system, input)
    }
}

/// Tests: every `job` request on `context` uses `model`.
#[cfg(test)]
pub(crate) fn install_test_model(context: &RuntimeContext, job: Job, model: Arc<dyn OneShot>) {
    context.set_test_one_shot(job, model);
}

/// The model `job` asks on `context`: `route` (`None`: not configured) on the relay, bounded by `timeout`, answering at
/// most `max_tokens` tokens. Each answered request is a usage fact (`harness::usage`) under the ledger key `key`.
pub(crate) fn model_for(
    context: &RuntimeContext,
    job: Job,
    key: &str,
    route: Option<RouteModel<'_>>,
    timeout: Duration,
    max_tokens: u32,
) -> Result<Arc<dyn OneShot>, ModelError> {
    #[cfg(test)]
    if let Some(model) = context.test_one_shot(job) {
        return Ok(model);
    }
    let route = route.ok_or(ModelError("unconfigured"))?;
    Ok(Arc::new(RelayModel::for_model(context, job, key, route, timeout, max_tokens)?))
}

/// The routes a one-shot walk tries (`RouteWalk`), in order, and whether they are the configured harness chain
/// (`harness::routes::chain`), whose route health the walk observes (`route_health`).
#[derive(Clone, Debug)]
pub(crate) struct Walk {
    pub routes: Vec<RouteSpec>,
    pub chain: bool,
}

/// The model `job` asks on `context` through a route walk (`RouteWalk`) over `walk` (`None`: not configured), bounded
/// as a whole by `timeout`, each request answering at most `max_tokens` tokens. Each response end is a usage fact
/// (`harness::usage`) under the ledger key `key`.
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
        return Ok(model);
    }
    let walk = walk.ok_or(ModelError("unconfigured"))?;
    Ok(Arc::new(RouteWalk::new(context, job, key, walk, timeout, max_tokens)?))
}

/// Maps a `ChainError` from `routes::ChainExecutor` to a classified `model_failure::Failure`.
fn chain_failure(error: ChainError) -> crate::model_failure::Failure {
    match error {
        ChainError::Cancelled => crate::model_failure::Failure::new("cancelled", "cancelled"),
        ChainError::Aborted(reason) if reason == "timeout" => {
            crate::model_failure::Failure::new("timeout", "TIMEOUT")
        }
        ChainError::Aborted(_) => crate::model_failure::Failure::new("network", "MODEL_CONNECTION_ERROR"),
        ChainError::Missed { class, summary } => {
            let parsed = crate::model_failure::Failure::classify("", &summary);
            if parsed.class == class {
                parsed
            } else {
                crate::model_failure::Failure::new(class, &parsed.code)
            }
        }
    }
}

/// The relay's OpenAI-compatible endpoint for one route with the harness credential: one non-streaming request without
/// tools via `routes::ChainExecutor` and `OneShotStep`, bounded by `timeout` and reusing the shared `genai::Client` and
/// shared Tokio runtime handle.
pub(crate) struct RelayModel {
    context: RuntimeContext,
    route: ModelRoute,
    key: String,
    usage_job: &'static str,
    timeout: Duration,
    max_tokens: u32,
}

impl RelayModel {
    pub(crate) fn for_model(
        context: &RuntimeContext,
        job: Job,
        key: &str,
        route: RouteModel<'_>,
        timeout: Duration,
        max_tokens: u32,
    ) -> Result<Self, ModelError> {
        let spec = RouteSpec {
            id: route.id.to_string(),
            model: route.model.to_string(),
            effort: route.effort.map(str::to_string),
            max_output_tokens: max_tokens,
            retry_429: false,
        };
        let resolved = ModelRoute::for_specs(context, vec![spec], Vec::new())
            .map_err(|_| ModelError("unconfigured"))?;
        Ok(Self {
            context: context.clone(),
            route: resolved,
            key: key.to_string(),
            usage_job: job.usage_job(),
            timeout,
            max_tokens,
        })
    }

    fn run_within_failure(
        &self,
        system: &str,
        input: &str,
        within: Duration,
        mut cancel: Cancellation,
    ) -> Result<String, crate::model_failure::Failure> {
        let context = self.context.clone();
        let route = self.route.clone();
        let key = self.key.clone();
        let usage_job = self.usage_job;
        let max_tokens = self.max_tokens;
        let system = system.to_string();
        let input = input.to_string();
        routes::block_on_shared(async move {
            let mut executor = ChainExecutor::new(ChainConfig {
                context: context.clone(),
                route,
                key,
                generation: String::new(),
                usage_job,
                track_steps: false,
                observe_health: false,
                use_failover_memory: false,
                record_events: false,
                record_usage: true,
                allow_retries: false,
                allow_stream_retry: false,
                complete_on_answer: false,
                fail_action: "the request ends",
                deadline_error: "timeout",
                retry_unit: Duration::from_secs(1),
                max_steps: 1,
                max_tokens: None,
                job_max_tokens: max_tokens,
                deadline: tokio::time::Instant::now() + within,
            });
            let mut adapter = OneShotStep {
                context,
                system,
                input,
                shortened_by_bound: false,
            };
            executor
                .next_answer(&mut adapter, &mut cancel)
                .await
                .map(|success| success.value)
                .map_err(chain_failure)
        })
        .unwrap_or_else(|_| Err(crate::model_failure::Failure::new("network", "MODEL_CONNECTION_ERROR")))
    }
}

impl OneShot for RelayModel {
    /// One request with a turn number of its own (`usage::next_number`), bounded by the model's timeout; a 429 is
    /// `quota`.
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError> {
        self.complete_failure(system, input).map_err(|f| ModelError(f.class))
    }

    fn complete_failure(&self, system: &str, input: &str) -> Result<String, crate::model_failure::Failure> {
        self.run_within_failure(system, input, self.timeout, Cancellation::none())
    }

    /// One request bounded by the lesser of `within` and the model's timeout, unless `abandoned` is set before it.
    fn complete_within(&self, system: &str, input: &str, within: Duration, abandoned: &Arc<AtomicBool>) -> Result<String, ModelError> {
        if abandoned.load(Ordering::SeqCst) {
            return Err(ModelError("cancelled"));
        }
        self.run_within_failure(system, input, within.min(self.timeout), Cancellation::from_flag(abandoned.clone()))
            .map_err(|f| ModelError(f.class))
    }
}

/// Request configuration for `run_walk_async`.
pub(crate) struct WalkRequest {
    pub context: RuntimeContext,
    pub route: ModelRoute,
    pub chain: bool,
    pub key: String,
    pub usage_job: &'static str,
    pub timeout: Duration,
    pub max_tokens: u32,
    pub unit: Duration,
    pub system: String,
    pub input: String,
    pub within: Duration,
}

/// Async one-shot route walk over `request.route` via `routes::ChainExecutor` and `OneShotStep`, returning classified
/// failures.
async fn run_walk_failure_async(
    mut request: WalkRequest,
    cancel: &mut Cancellation,
) -> Result<String, crate::model_failure::Failure> {
    if cancel.is_cancelled() {
        return Err(crate::model_failure::Failure::new("cancelled", "cancelled"));
    }
    routes::select_start(&request.context, &mut request.route, &request.key, "");
    let mut executor = ChainExecutor::new(ChainConfig {
        context: request.context.clone(),
        route: request.route,
        key: request.key,
        generation: String::new(),
        usage_job: request.usage_job,
        track_steps: false,
        observe_health: request.chain,
        use_failover_memory: true,
        record_events: true,
        record_usage: true,
        allow_retries: true,
        allow_stream_retry: false,
        complete_on_answer: true,
        fail_action: "the request ends",
        deadline_error: "timeout",
        retry_unit: request.unit,
        max_steps: usize::MAX,
        max_tokens: None,
        job_max_tokens: request.max_tokens,
        deadline: tokio::time::Instant::now() + request.within.min(request.timeout),
    });
    let mut adapter = OneShotStep {
        context: request.context,
        system: request.system,
        input: request.input,
        shortened_by_bound: request.within < request.timeout,
    };
    executor
        .next_answer(&mut adapter, cancel)
        .await
        .map(|success| success.value)
        .map_err(chain_failure)
}

/// Async one-shot route walk over `request.route` via `routes::ChainExecutor` and `OneShotStep`.
pub(crate) async fn run_walk_async(
    request: WalkRequest,
    cancel: &mut Cancellation,
) -> Result<String, ModelError> {
    run_walk_failure_async(request, cancel)
        .await
        .map_err(|f| ModelError(f.class))
}

/// One-shot requests over routes in order, as a harness turn walks its chain (`routes::ChainExecutor`): a first 429 is
/// retried once on the same route after its hint, a repeat 429 and any other failure (an HTTP error, a timeout, a
/// network failure, a refusal, an error-shaped answer: `backend::plain_output_failure`) move to the next route, and an
/// empty answer is retried on the same route up to `routes::EMPTY_RETRIES` times first. The walk is bounded as a whole
/// by the `within` its caller gives (at most its `timeout`): each request gets the time left, a wait never reaches the
/// end, a request that times out had all the time left and ends the walk, and nothing is sent once the end has passed
/// or cancellation/abandonment triggers.
pub(crate) struct RouteWalk {
    context: RuntimeContext,
    route: ModelRoute,
    /// The routes are the configured chain, whose route health the walk observes.
    chain: bool,
    /// The ledger key: the usage facts' key, the failover memory's conversation and the run record's subject.
    key: String,
    usage_job: &'static str,
    /// The bound of one request on any route; the walk's bound is at most this.
    timeout: Duration,
    max_tokens: u32,
    /// The base wait of retries (`routes::Situation::unit`): one second; tests shorten it.
    unit: Duration,
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
    ) -> Result<String, crate::model_failure::Failure> {
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
        };
        routes::block_on_shared(async move { run_walk_failure_async(request, &mut cancel).await })
            .unwrap_or_else(|_| Err(crate::model_failure::Failure::new("network", "MODEL_CONNECTION_ERROR")))
    }
}

impl OneShot for RouteWalk {
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError> {
        self.complete_failure(system, input).map_err(|f| ModelError(f.class))
    }

    fn complete_failure(&self, system: &str, input: &str) -> Result<String, crate::model_failure::Failure> {
        self.run_within_failure(system, input, self.timeout, Cancellation::none())
    }

    fn complete_within(&self, system: &str, input: &str, within: Duration, abandoned: &Arc<AtomicBool>) -> Result<String, ModelError> {
        if abandoned.load(Ordering::SeqCst) {
            return Err(ModelError("cancelled"));
        }
        self.run_within_failure(system, input, within, Cancellation::from_flag(abandoned.clone()))
            .map_err(|f| ModelError(f.class))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream_capture::test_support as capture;
    use serde_json::{json, Value};

    /// A relay that answers one request with a completion of `content`; the handle yields the request body.
    fn relay(content: &'static str) -> (String, std::thread::JoinHandle<Value>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let start = loop {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0, "the request ended early");
                bytes.extend_from_slice(&chunk[..n]);
                let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    break end + 4;
                }
            };
            let body = json!({"id":"fixture","object":"chat.completion","created":1,"model":"fake-chat",
                "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
            .to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            serde_json::from_slice(&bytes[start..]).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn one_shot_sends_its_input_alone_without_tools() {
        let (url, request) = relay("[]");
        let (_root, context) = capture::runtime("one-shot-alone", &url);
        let model = RelayModel::for_model(&context, Job::Digest, "internal:digest", RouteModel { id: "route-a", model: "fake-chat", effort: None }, Duration::from_secs(5), 321).unwrap();
        assert_eq!(model.complete("", "one-shot-input-11"), Ok("[]".to_string()));
        let body = request.join().unwrap();
        assert_eq!(body["model"], "fake-chat");
        assert_eq!(body["max_tokens"], 321);
        assert!(body.get("reasoning_effort").is_none(), "no effort is sent when omitted: {body}");
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
        let model = RelayModel::for_model(
            &context,
            Job::SlangLearner,
            "group:7003",
            RouteModel { id: "route-a", model: "fake-chat", effort: Some("high") },
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
        let walker = walk_for(&context, Job::Digest, "internal:digest", Some(walk), Duration::from_secs(5), 256).unwrap();
        assert_eq!(walker.complete("sys", "walk-effort"), Ok("[]".to_string()));
        let body = request.join().unwrap();
        assert_eq!(body["reasoning_effort"], "medium", "{body}");
    }

    #[test]
    fn one_shot_request_is_taken_by_an_armed_capture() {
        let (url, request) = relay("[]");
        let (_root, context) = capture::runtime("one-shot-capture", &url);
        capture::arm(&context, "harness");
        let model = RelayModel::for_model(&context, Job::Digest, "internal:digest", RouteModel { id: "route-a", model: "fake-chat", effort: None }, Duration::from_secs(5), 321).unwrap();
        assert_eq!(model.complete("", "one-shot-input-11"), Ok("[]".to_string()));
        request.join().unwrap();
        let record = capture::settled(&context).expect("the armed capture took the one-shot request");
        assert_eq!(
            (record["route"].as_str(), record["request"]["model"].as_str()),
            (Some("harness"), Some("fake-chat")),
            "{record}"
        );
        assert_eq!(record["request"]["maxTokens"], 321, "{record}");
        assert!(record["request"]["request"].to_string().contains("one-shot-input-11"), "{record}");
    }

    #[test]
    fn harness_usage_the_slang_one_shot_records_one_fact_under_its_key() {
        let (url, request) = relay("[]");
        let (root, context) = capture::runtime("one-shot-usage", &url);
        let route = RouteModel { id: "route-a", model: "fake-chat", effort: None };
        let model = model_for(&context, Job::SlangLearner, "group:7002", Some(route), Duration::from_secs(5), 321).unwrap();
        assert_eq!(model.complete("", "one-shot-usage-input"), Ok("[]".to_string()));
        request.join().unwrap();
        let text = std::fs::read_to_string(root.0.join("data/token-usage-rust.jsonl")).unwrap_or_default();
        let rows: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.len(), 1, "one answered request is one usage fact: {rows:?}");
        assert_eq!(
            (rows[0]["key"].as_str(), rows[0]["route"].as_str(), rows[0]["model"].as_str(), rows[0]["job"].as_str()),
            (Some("group:7002"), Some("route-a"), Some("fake-chat"), Some("slang")),
            "{}",
            rows[0]
        );
        assert_eq!(rows[0]["b"], json!({"cacheMiss": 1, "cacheHit": 0, "cacheWrite": 0, "output": 1, "reasoning": 0}));
    }

    #[test]
    fn harness_usage_the_one_shot_redacts_the_credential_from_its_row_and_capture() {
        let secret = "synthetic-model-key-183";
        let (url, request) = relay("[]");
        let (root, context) = capture::runtime("one-shot-redact", &url);
        capture::arm(&context, "harness");
        let route_id = format!("route-{secret}");
        let model_name = format!("fake-chat-{secret}");
        let route = RouteModel { id: &route_id, model: &model_name, effort: None };
        let model = model_for(&context, Job::Digest, "internal:digest", Some(route), Duration::from_secs(5), 321).unwrap();
        assert_eq!(model.complete("", "one-shot-redact-input"), Ok("[]".to_string()));
        request.join().unwrap();
        let text = std::fs::read_to_string(root.0.join("data/token-usage-rust.jsonl")).unwrap_or_default();
        let rows: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0]["route"].as_str(), rows[0]["model"].as_str()),
            (Some("route-[REDACTED]"), Some("fake-chat-[REDACTED]")),
            "{}",
            rows[0]
        );
        assert!(!text.contains(secret), "the ledger never keeps the harness credential: {text}");
        let record = capture::settled(&context).expect("the armed capture took the one-shot request");
        assert_eq!(record["request"]["model"], "fake-chat-[REDACTED]", "{record}");
    }
}
