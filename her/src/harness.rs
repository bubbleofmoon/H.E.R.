//! HER-owned model and tool turn execution. The pipeline retains admission authority and delivery.
pub(crate) mod budget;
pub(crate) mod handover;
pub(crate) mod interactions;
pub(crate) mod one_shot;
pub(crate) mod profile;
pub(crate) mod rotation;
pub(crate) mod route_health;
pub(crate) mod routes;
pub(crate) mod store;
mod tools;
pub(crate) mod usage;

pub use tools::Tools;
#[cfg(test)]
pub(crate) use tools::session_instruction;

use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::{RuntimeArea, RuntimeContext};
use futures_util::StreamExt;
use genai::{
    adapter::AdapterKind,
    chat::{
        Binary, ChatMessage, ChatOptions, ChatRequest, ChatRole, ChatStreamEvent, ContentPart,
        ReasoningEffort, StopReason, StreamEnd, ToolCall, ToolResponse,
    },
    resolver::{AuthData, Endpoint},
    Client, ModelIden, ModelSpec, ServiceTarget,
};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

/// The failure of a turn whose wall deadline (`socialV2.turnTimeoutSecs`) ran out.
pub(crate) const DEADLINE_EXCEEDED: &str = "harness model deadline exceeded";

/// Default cap, in characters, on the model copy of one tool result (`harness.toolResultChars`).
const TOOL_RESULT_CHARS: u64 = 8_000;

/// One route of a turn's chain (`harness/routes.rs`) with its relay target. Credentials are opaque: never derive Debug
/// or serialize this value.
pub(crate) struct Route {
    spec: routes::RouteSpec,
    /// The route id and model id as records keep them, with the credential redacted.
    id: String,
    model: String,
    target: ServiceTarget,
}

/// `text` with the harness credential `key` replaced by `[REDACTED]`: the form every record and capture keeps.
pub(crate) fn redact_credential(text: &str, key: &str) -> String {
    text.replace(key, "[REDACTED]")
}

/// A turn's candidate chain on the selected runtime's relay, authenticated with the harness credential: the configured
/// routes (`routes::chain`) in the order the turn walks them (`for_conversation`: the conversation's selected route
/// first), tried in that order from `start`. Credentials are opaque: never derive Debug or serialize this value.
pub struct ModelRoute {
    credentials: Vec<String>,
    endpoint: String,
    /// The routes in the order this turn walks them.
    routes: Vec<Route>,
    /// The same routes in configured order: route health keys on it whatever order the turn walks (`observe`).
    configured: Vec<routes::RouteSpec>,
    /// The route the turn starts on: the conversation's sticky degraded route, set at admission.
    start: usize,
}

impl ModelRoute {
    /// The first route's model id (tests).
    #[cfg(test)]
    pub fn model_name(&self) -> &str {
        &self.routes[0].model
    }

    /// The selected runtime's chain (`routes::chain`) in configured order, whatever a conversation selected (tests;
    /// turns use `for_conversation`); without a valid one the turn fails as not configured.
    #[cfg(test)]
    pub fn from_runtime(context: &RuntimeContext, cfg: &Value) -> Result<Self, String> {
        if !context.is_isolated() {
            return Err("harness requires a selected runtime".into());
        }
        let chain = routes::chain(cfg)?;
        Self::build(context, chain.clone(), chain)
    }

    /// #162: the chain conversation `conversation`'s turn walks: its selected route first (`routes::selected`: its own
    /// model route, else `harness.firstRoute`), then the other routes in configured order (`routes::conversation_order`);
    /// the configured order when the selection names no route of the chain.
    pub fn for_conversation(context: &RuntimeContext, cfg: &Value, conversation: &str) -> Result<Self, String> {
        if !context.is_isolated() {
            return Err("harness requires a selected runtime".into());
        }
        let chain = routes::chain(cfg)?;
        let order = routes::conversation_order(&chain, routes::selected(context, cfg, conversation).as_deref());
        Self::build(context, order, chain)
    }

    /// The selected runtime's relay route for `model` alone, authenticated with the harness credential.
    pub fn for_model(context: &RuntimeContext, model: &str) -> Result<Self, String> {
        let chain = vec![routes::RouteSpec::implicit(model.to_string())];
        Self::build(context, chain.clone(), chain)
    }

    /// `order` is the walk, `configured` the same routes in configured order.
    fn build(
        context: &RuntimeContext,
        order: Vec<routes::RouteSpec>,
        configured: Vec<routes::RouteSpec>,
    ) -> Result<Self, String> {
        if !context.is_isolated() {
            return Err("harness requires a selected runtime".into());
        }
        let origin = context
            .dependency_url("model")
            .ok_or("harness model endpoint is unavailable")?;
        let key = context
            .read_text(RuntimeArea::Config, "harness-api-key")
            .map_err(|_| "harness model credential is unavailable")?;
        let key = key.trim();
        if key.is_empty() {
            return Err("harness model credential is empty".into());
        }
        let endpoint = format!("{}/v1/", origin.trim_end_matches('/'));
        let mut credentials = crate::settings::stored_secrets(context)
            .map_err(|_| "stored model credentials are unavailable")?;
        credentials.push(key.to_string());
        credentials.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
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

    /// The first route's target (the digest's one route).
    pub(crate) fn target(&self) -> ServiceTarget {
        self.routes[0].target.clone()
    }

    /// `text` with this route's harness credential redacted (`redact_credential`).
    pub(crate) fn redact(&self, text: &str) -> String {
        redact_credentials(text, &self.credentials)
    }

    /// Mask a copy of the delivered request using this turn's credential snapshot.
    fn capture_request(&self, model: &str, max_tokens: u32, request: &ChatRequest) -> Value {
        let mut capture = serde_json::json!({
            "model": model,
            "maxTokens": max_tokens,
            "request": serde_json::to_value(request).unwrap_or(Value::Null),
        });
        redact(&mut capture, &self.credentials);
        capture
    }

    /// The chain's fingerprint (`routes::chain_signature`).
    fn signature(&self) -> String {
        routes::chain_signature(&self.specs())
    }

    /// The chain's routes in the order this turn walks them (its failover memory's fingerprint, `signature`).
    fn specs(&self) -> Vec<routes::RouteSpec> {
        self.routes.iter().map(|route| route.spec.clone()).collect()
    }

    /// #162: records route health (`route_health::observe`) for route `at` of this turn's walk at its place in the
    /// configured chain, so the health state and its all-routes-failed episode stay one per configured chain whatever
    /// order a conversation walks. The walk holds the configured routes, each once (`routes::conversation_order`), so
    /// every walked route has a place; an `at` past the walk records nothing.
    fn observe(&self, context: &RuntimeContext, at: usize, seen: route_health::Seen) {
        let Some(id) = self.routes.get(at).map(|route| route.spec.id.as_str()) else { return };
        let Some(place) = self.configured.iter().position(|spec| spec.id == id) else { return };
        route_health::observe(context, &self.configured, place, seen);
    }
}

/// Captured per turn, independent of later configuration changes.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    max_steps: usize,
    max_tokens: u32,
    /// `harness.imageInput` (default true): the route accepts image parts. When false, every image
    /// part of a delivered request becomes `NO_IMAGE_INPUT` text (`model_messages`).
    image_input: bool,
    tool_result_chars: usize,
    /// The base wait of the route module's retries (`routes::Situation::unit`): one second; tests shorten it.
    retry_unit: Duration,
}

impl Limits {
    pub fn from_config(cfg: &Value) -> Self {
        Self {
            retry_unit: Duration::from_secs(1),
            max_steps: crate::settings::HARNESS_MAX_STEPS.integer(cfg) as usize,
            max_tokens: crate::settings::HARNESS_MAX_TOKENS.integer(cfg) as u32,
            image_input: cfg
                .pointer("/harness/imageInput")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            tool_result_chars: cfg
                .pointer("/harness/toolResultChars")
                .and_then(Value::as_f64)
                .map(|chars| chars.clamp(1_000.0, 100_000.0) as usize)
                .unwrap_or(TOOL_RESULT_CHARS as usize),
        }
    }

    /// The step and token budgets (tests).
    #[cfg(test)]
    pub(crate) fn budgets(&self) -> (usize, u32) {
        (self.max_steps, self.max_tokens)
    }

    /// These limits with retries waiting `unit` instead of one second (tests).
    #[cfg(test)]
    pub(crate) fn with_retry_unit(mut self, unit: Duration) -> Self {
        self.retry_unit = unit;
        self
    }
}

/// Reserved2 keeps the current wake's full form available if replay loses its base briefing.
#[derive(Default)]
pub(crate) struct TurnDelivery {
    pub recorder: Option<crate::turn_ledger::Recorder>,
    pub briefing: Option<Value>,
    pub full_prompt: Option<String>,
    pub recalled: Vec<i64>,
    pub reserved2_budget: Option<crate::pipeline::Reserved2Budget>,
}

impl TurnDelivery {
    fn check_deadline(
        budget: Option<crate::pipeline::Reserved2Budget>, context: &RuntimeContext, key: &str,
    ) -> Result<(), String> {
        if let Some(budget) = budget {
            let state = context.read_social_state()?;
            if budget.expired(&state["conversations"][key]) {
                return Err(DEADLINE_EXCEEDED.into());
            }
        }
        Ok(())
    }
}

pub struct TurnRequest {
    pub id: String,
    pub conversation: String,
    pub generation: String,
    pub prompt: String,
    /// Inbound media parts of the turn as `media::model_parts` renders them for DSH (`text` labels
    /// and normalized `image` parts, in order); empty for a text-only turn.
    pub media: Vec<Value>,
    pub profile: String,
    pub route: ModelRoute,
    pub deadline: Duration,
    pub tools: Tools,
    pub limits: Limits,
}

/// What a route without image input sees in place of each image part (`harness.imageInput: false`).
/// Model-visible: it names no model or maker (#142, `model_text_guard_tests`).
pub(crate) const NO_IMAGE_INPUT: &str = "（这张图片现在看不到）";

impl TurnRequest {
    /// The turn's canonical user message (`user_message`): stored at admission and sent live.
    pub(crate) fn user_message(&self) -> ChatMessage {
        user_message(&self.prompt, &self.media)
    }
}

/// The prompt, then each inbound media part in order: a `text` label as text, an `image` part as a
/// binary image. A text-only turn keeps the single-text user message. Parts of another shape, and
/// image parts without an image MIME type or data, are dropped.
pub(crate) fn user_message(prompt: &str, media: &[Value]) -> ChatMessage {
    if media.is_empty() {
        return ChatMessage::user(prompt.to_string());
    }
    let mut parts = vec![ContentPart::Text(prompt.to_string())];
    for part in media {
        match part["type"].as_str() {
            Some("text") => {
                if let Some(text) = part["text"].as_str() {
                    parts.push(ContentPart::Text(text.to_string()));
                }
            }
            Some("image") => {
                let mime = part["mediaType"]
                    .as_str()
                    .filter(|mime| mime.starts_with("image/"));
                let data = part["data"].as_str().filter(|data| !data.is_empty());
                if let (Some(mime), Some(data)) = (mime, data) {
                    let name = part["name"].as_str().map(str::to_string);
                    parts.push(ContentPart::Binary(Binary::from_base64(mime, data, name)));
                }
            }
            _ => {}
        }
    }
    ChatMessage::user(parts)
}

#[derive(Clone, Debug)]
pub enum Status {
    Running,
    Completed(String),
    ObservationSleep,
    Failed(String),
    Cancelled,
    Paused { reason: String, detail: String },
}

enum Failure {
    Failed(String),
    /// The owner received the timeout notice for this interaction.
    InteractionClosed,
    Cancelled,
    Uncertain(String),
}

enum ModelFailure {
    Dropped(String),
    /// HTTP 429, with the wait the response asked for (`rate_limit_wait`).
    RateLimited {
        reason: String,
        wait: Option<Duration>,
    },
    /// A response that ended on a stop no answer may use (a content filter, or a stop other than completion or a
    /// tool call). Its usage is charged before the miss is decided, so a failover asks the next route for what is left.
    Stopped {
        reason: String,
        end: Box<StreamEnd>,
    },
    Terminal(String),
}

impl From<String> for ModelFailure {
    fn from(reason: String) -> Self {
        Self::Terminal(reason)
    }
}

impl ModelFailure {
    /// The route module's view of this failure, and its content-free summary.
    fn miss(self) -> (routes::Miss, String) {
        match self {
            Self::Dropped(reason) => (routes::Miss::Dropped, reason),
            Self::RateLimited { reason, wait } => (routes::Miss::RateLimited { wait }, reason),
            Self::Terminal(reason) | Self::Stopped { reason, .. } => {
                let class = crate::model_failure::Failure::classify("", &reason).class;
                let miss = if class == "empty-response" {
                    routes::Miss::Empty
                } else {
                    routes::Miss::Failed { class }
                };
                (miss, reason)
            }
        }
    }
}

/// One route request that failed over or ended its turn, as the pipeline reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteFailure {
    pub route: String,
    pub model: String,
    /// The content-free failure summary (`model_failure::Failure::summary`).
    pub summary: String,
}

/// The routes a turn has used so far.
#[derive(Clone, Debug, Default)]
struct Progress {
    /// The model of the route the turn's last request went to.
    model: String,
    failures: Vec<RouteFailure>,
    route_chain_exhausted: bool,
    owner_interaction_closed: bool,
}

impl From<String> for Failure {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

impl From<&str> for Failure {
    fn from(reason: &str) -> Self {
        Self::Failed(reason.into())
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Assistant {
        text: String,
        raw: Value,
        message: ChatMessage,
    },
    Ended(Status),
}

pub struct Turn {
    sent: Arc<AtomicBool>,
    request_refs: Arc<std::sync::Mutex<Vec<Value>>>,
    progress: Arc<std::sync::Mutex<Progress>>,
    status: watch::Receiver<Status>,
    cancel: watch::Sender<bool>,
    observation_sleep: Arc<AtomicBool>,
    events: broadcast::Sender<Event>,
}

impl Turn {
    pub fn message_refs(&self) -> Vec<Value> {
        self.request_refs.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The model of the route the turn's last request went to (its start route before any request).
    pub fn model(&self) -> String {
        self.progress.lock().unwrap_or_else(|p| p.into_inner()).model.clone()
    }

    /// Each route request that failed over or ended the turn, in order.
    pub fn route_failures(&self) -> Vec<RouteFailure> {
        self.progress.lock().unwrap_or_else(|p| p.into_inner()).failures.clone()
    }

    pub(crate) fn route_chain_exhausted(&self) -> bool {
        self.progress.lock().unwrap_or_else(|p| p.into_inner()).route_chain_exhausted
    }

    pub(crate) fn owner_interaction_closed(&self) -> bool {
        self.progress.lock().unwrap_or_else(|p| p.into_inner()).owner_interaction_closed
    }

    pub fn sent_tool_reply(&self) -> bool {
        self.sent.load(Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }

    pub(crate) fn mark_observation_sleep(&self) {
        self.observation_sleep.store(true, Ordering::SeqCst);
    }

    pub async fn wait(&mut self) -> Result<String, String> {
        loop {
            match self.status() {
                Status::Running => {}
                Status::Completed(text) => return Ok(text),
                Status::ObservationSleep => return Ok(String::new()),
                Status::Failed(reason) => return Err(reason),
                Status::Cancelled => return Err("回合已取消（harness）".into()),
                Status::Paused { reason, detail } => {
                    return Err(format!("harness turn paused ({reason}): {detail}"))
                }
            }
            self.status
                .changed()
                .await
                .map_err(|_| "harness task ended without a result")?;
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub struct Harness {
    context: RuntimeContext,
    client: Client,
    store: Arc<store::Store>,
}

impl Harness {
    pub fn open(context: &RuntimeContext) -> Result<Self, String> {
        if !context.is_isolated() {
            return Err("harness requires a selected runtime".into());
        }
        Ok(Self {
            context: context.clone(),
            store: Arc::new(store::Store::open(context)?),
            client: Client::new().map_err(|_| "harness model client is unavailable")?,
        })
    }

    /// A durable status lookup also works after reopening the runtime.
    pub fn status(&self, turn: &str) -> Result<Option<Status>, String> {
        self.store.status(turn)
    }

    #[cfg(test)]
    pub fn admit(&self, request: TurnRequest) -> Result<Turn, String> {
        self.admit_with_delivery(request, TurnDelivery::default())
    }

    pub(crate) fn wake_briefing(&self, conversation: &str, generation: &str) -> Result<Value, String> {
        self.store.wake_briefing(conversation, generation)
    }

    pub(crate) fn admit_with_delivery(&self, mut request: TurnRequest, mut delivery: TurnDelivery) -> Result<Turn, String> {
        if request.id.is_empty() || request.conversation.is_empty() || request.generation.is_empty()
        {
            return Err("harness admission requires turn, conversation and generation".into());
        }
        if !request
            .tools
            .matches(&request.conversation, &request.generation)
        {
            return Err("harness tools belong to a different conversation generation".into());
        }
        // Reset probes the same OS lock across console/bridge processes. Acquire before
        // the authority check so reset cannot mistake an admitted handler for crashed work.
        let owner = turn_owner(&self.context, &request.id)?;
        request.tools.check().map_err(String::from)?;
        request.prompt = request.route.redact(&request.prompt);
        request.profile = request.route.redact(&request.profile);
        if let Some(full) = &mut delivery.full_prompt {
            *full = request.route.redact(full);
        }
        // Media labels are text too; image data stays the bytes the pipeline normalized.
        for part in request.media.iter_mut() {
            let redacted = part["text"]
                .as_str()
                .map(|text| request.route.redact(text));
            if let Some(text) = redacted {
                part["text"] = Value::String(text);
            }
        }
        // The turn starts on the conversation's sticky degraded route (`routes::RouteMemory::start`).
        let chain = routes::ChainKey {
            conversation: request.conversation.clone(),
            generation: request.generation.clone(),
            signature: request.route.signature(),
        };
        let len = request.route.routes.len();
        request.route.start = routes::with_memory(&routes::scope_of(&self.context), |memory| {
            memory.start(&chain, len, std::time::Instant::now())
        });
        // The history after the session's newest checkpoint; the turn's task completes it (`handover::prepare`).
        let history = self.store.admit_with_briefing(&request, delivery.briefing.take())?;
        let instruction = request.tools.instruction();
        let store = self.store.clone();
        let (status_tx, status) = watch::channel(Status::Running);
        let (cancel, cancelled) = watch::channel(false);
        let (events, _) = broadcast::channel(32);
        let notifications = events.clone();
        let client = self.client.clone();
        let context = self.context.clone();
        let sent = Arc::new(AtomicBool::new(false));
        let sent_flag = sent.clone();
        let request_refs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let refs_for_task = request_refs.clone();
        let progress = Arc::new(std::sync::Mutex::new(Progress {
            model: request.route.routes[request.route.start].model.clone(),
            ..Progress::default()
        }));
        let progress_for_task = progress.clone();
        let task_cancel = cancel.clone();
        let observation_sleep = Arc::new(AtomicBool::new(false));
        let task_observation = observation_sleep.clone();
        tokio::spawn(async move {
            // Keep ownership through result persistence and uncertainty publication.
            let _owner = owner;
            let mut assistant = None;
            let cancellation = cancelled.clone();
            let deadline = tokio::time::Instant::now() + request.deadline;
            let deadline_generation = std::sync::OnceLock::new();
            let mut recalled = delivery.recalled.as_slice();
            let execution = tool_loop(
                &context,
                &client,
                &store,
                &request,
                history,
                &delivery,
                instruction,
                &notifications,
                &sent_flag,
                &refs_for_task,
                &progress_for_task,
                deadline,
                &deadline_generation,
                cancellation,
                &mut recalled,
            );
            let (outcome, interrupted) = settle_turn(
                execution,
                delivery.reserved2_budget.is_none().then_some(deadline),
                &task_cancel,
                cancelled,
                &request.tools,
                &deadline_generation,
            ).await;
            let mut result = match outcome {
                Err(Failure::Uncertain(detail)) => Status::Paused {
                    reason: "send-unresolved".into(),
                    detail,
                },
                Err(Failure::Cancelled) => interrupted.unwrap_or(Status::Cancelled),
                Err(Failure::InteractionClosed) => {
                    progress_for_task.lock().unwrap_or_else(|p| p.into_inner()).owner_interaction_closed = true;
                    interrupted.unwrap_or_else(|| Status::Failed(interactions::TIMED_OUT.into()))
                }
                Err(Failure::Failed(reason)) => interrupted.unwrap_or(Status::Failed(reason)),
                Ok(event) => match interrupted {
                    Some(status) => status,
                    None => match TurnDelivery::check_deadline(delivery.reserved2_budget, &context, &request.conversation) {
                        Err(reason) => Status::Failed(reason),
                        Ok(()) => {
                            let Event::Assistant { text, .. } = &event else {
                                unreachable!()
                            };
                            let status = Status::Completed(text.clone());
                            assistant = Some(event);
                            status
                        }
                    }
                },
            };
            let witnessed_cap = matches!(&result, Status::Failed(reason) if reason == DEADLINE_EXCEEDED)
                && delivery.reserved2_budget.is_some()
                && crate::console::observation_wait_turn(&context, &request.conversation)
                    .ok().flatten().as_deref() == Some(request.id.as_str());
            if (task_observation.load(Ordering::SeqCst) || witnessed_cap)
                && matches!(result, Status::Failed(_) | Status::Cancelled)
            {
                result = Status::ObservationSleep;
            }
            if let Err(reason) = store.finish(&request.id, &result, assistant.as_ref()) {
                result = Status::Paused {
                    reason: "send-unresolved".into(),
                    detail: reason,
                };
                assistant = None;
            }
            if let Status::Paused { reason, detail } = &result {
                let _ = request.tools.pause(&request.id, reason, detail);
            }
            if let Some(event) = assistant {
                // Final text counts only after the runner accepts and persists completion.
                crate::prompt::record_recall_hits(&context, recalled);
                let _ = notifications.send(event);
            }
            let _ = notifications.send(Event::Ended(result.clone()));
            let _ = status_tx.send(result);
        });
        Ok(Turn {
            sent,
            request_refs,
            progress,
            status,
            cancel,
            observation_sleep,
            events,
        })
    }
}

async fn settle_turn<T>(
    execution: impl std::future::Future<Output = Result<T, Failure>>,
    deadline: Option<tokio::time::Instant>,
    cancel: &watch::Sender<bool>,
    mut cancelled: watch::Receiver<bool>,
    tools: &Tools,
    deadline_generation: &std::sync::OnceLock<String>,
) -> (Result<T, Failure>, Option<Status>) {
    tokio::pin!(execution);
    let interrupted = tokio::select! { biased;
        _ = cancelled.changed() => Status::Cancelled,
        _ = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        } => Status::Failed(DEADLINE_EXCEEDED.into()),
        result = &mut execution => return (result, None),
    };

    // Keep ownership of the handler and result writer. A dispatched blocking handler
    // must settle under revoked authority.
    let _ = cancel.send(true);
    let revoked = tools.revoke();
    if matches!(interrupted, Status::Failed(_)) {
        if let Ok(Some(generation)) = &revoked {
            let _ = deadline_generation.set(generation.clone());
        }
    }
    let settled = execution.await;
    let outcome = match revoked {
        Ok(_) => settled,
        Err(reason) => Err(Failure::Uncertain(reason)),
    };
    (outcome, Some(interrupted))
}

fn turn_owner(context: &RuntimeContext, turn: &str) -> Result<std::fs::File, String> {
    context.open_locked_file(
        RuntimeArea::Data,
        &format!("locks/harness-turn-{}.lock", crate::send_outbox::token_epoch(turn)),
    )
}

fn turn_is_live(context: &RuntimeContext, turn: &str) -> bool {
    matches!(turn_owner(context, turn), Err(error) if error == "runtime file is already locked")
}

/// Reset has already revoked the generation. A live owner must settle its handler;
/// paused/crashed work has no owner and can be resolved immediately.
/// False leaves both storage and ledger finalization to the live owner.
pub fn resolve_reset(context: &RuntimeContext, turn: &str) -> Result<bool, String> {
    let _owner = match turn_owner(context, turn) {
        Ok(owner) => owner,
        Err(error) if error == "runtime file is already locked" => return Ok(false),
        Err(error) => return Err(error),
    };
    resolve_interrupted(context, turn)?;
    Ok(true)
}

/// Settle only the resolved interrupted turn before releasing bridge-ledger input.
/// Repeated application is harmless; completed facts and unrelated turns stay intact.
pub fn resolve_interrupted(context: &RuntimeContext, turn: &str) -> Result<(), String> {
    let store = store::Store::open(context)?;
    if matches!(
        store.status(turn)?,
        Some(Status::Running | Status::Paused { .. })
    ) {
        store.finish(turn, &Status::Cancelled, None)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn tool_loop(
    context: &RuntimeContext,
    client: &Client,
    store: &store::Store,
    request: &TurnRequest,
    history: store::History,
    delivery: &TurnDelivery,
    instruction: String,
    events: &broadcast::Sender<Event>,
    sent: &AtomicBool,
    request_refs: &std::sync::Mutex<Vec<Value>>,
    progress: &std::sync::Mutex<Progress>,
    deadline: tokio::time::Instant,
    deadline_generation: &std::sync::OnceLock<String>,
    mut cancelled: watch::Receiver<bool>,
    recalled: &mut &[i64],
) -> Result<Event, Failure> {
    let session = history.session.clone();
    // #6: the session's idle rotation and budget checkpoint, before the turn's first request.
    let (mut messages, rounds, checkpoint_clipped) =
        handover::prepare(context, store, request, history, instruction, &mut cancelled, deadline).await?;
    let mut chain = ChainState::new(context, request, usage::next_number());
    let mut spent = 0u32;
    for step in 0..request.limits.max_steps {
        check_bound(request, &cancelled, deadline)?;
        if step > 0 {
            store.next_attempt(request, chain.route)?;
        }
        let route = &request.route.routes[chain.route];
        progress.lock().unwrap_or_else(|p| p.into_inner()).model = route.model.clone();
        // #160: the menu is narrowed by the current authority and tool switches before every
        // request, a retry included; revoked authority ends the turn here, as in `check_bound`.
        let offered = request.tools.model_tools().map_err(|error| match error {
            tools::CheckError::Revoked(_) => Failure::Cancelled,
            tools::CheckError::Unavailable(reason) => Failure::Failed(reason),
        })?;
        // #6: the request fits its route's history budget by leaving out the oldest whole rounds;
        // #175: an oversized tool result reaches the model as a preview; #160: a replayed call to a
        // tool this request does not offer reaches it as text (`budget::fit`).
        let limit = budget::limit(&context.load_config()?, &route.spec.id);
        let fit = |messages| -> Result<_, String> {
            Ok(budget::fit(
                filter_model_messages(context, messages)?,
                &rounds,
                &offered,
                request.limits,
                limit,
            ))
        };
        let (mut kept, mut delivered) = fit(messages.clone())?;
        if let Some(full) = delivery.full_prompt.as_ref().filter(|_| {
            checkpoint_clipped || kept.len() != messages.len()
        }) {
            let current = rounds.last().map_or_else(
                || usize::from(messages.first().is_some_and(|message| message.role == ChatRole::System)),
                |last| last.end,
            );
            let mut with_full = messages.clone();
            with_full[current] = user_message(full, &request.media);
            (kept, delivered) = fit(with_full)?;
        }
        // Capture only structured references from the request being issued. Later read results
        // and recalled tool facts may add sources absent from the inbound wake. They are read from
        // the whole filtered copy of the rounds kept, so they still cover every ID a preview shows.
        *request_refs.lock().unwrap_or_else(|p| p.into_inner()) = model_request_refs(&kept);
        // Each request caps its output at the remaining token budget, up to its route's cap.
        let max_tokens = request
            .limits
            .max_tokens
            .saturating_sub(spent)
            .min(route.spec.max_output_tokens);
        // #183: a capture the operator armed takes this request as `model_step` hands it to the
        // model client, which adds only the OpenAI wire framing.
        crate::upstream_capture::offer(context, crate::upstream_capture::Route::Harness, || {
            let chat = ChatRequest::new(model_messages(
                delivered.clone(),
                request.limits.image_input,
            ))
            .with_tools(offered.clone());
            request.route.capture_request(&route.model, max_tokens, &chat)
        });
        // A request that misses is recorded with its size (`ChainState::missed`).
        let request_bytes = serde_json::to_vec(&delivered).map_or(0, |bytes| bytes.len());
        check_bound(request, &cancelled, deadline)?;
        TurnDelivery::check_deadline(delivery.reserved2_budget, context, &request.conversation)?;
        if step == 0 {
            if let Some(recorder) = &delivery.recorder {
                recorder.started()?;
            }
        }
        let model = model_step(
            client,
            route,
            delivered,
            offered,
            max_tokens,
            request.limits.image_input,
        );
        let end = tokio::select! { biased;
            _ = cancelled.changed() => return Err(Failure::Cancelled),
            result = model => result,
        };
        let at = MissAt {
            step,
            spent,
            request_bytes,
            deadline,
        };
        let end = match end {
            Ok(end) => end,
            Err(ModelFailure::Stopped { reason, end }) => {
                // The stop's usage is charged before the miss is decided, as an answer's is.
                chain.record_usage(context, request, step, end.captured_usage.as_ref());
                spent = spent.saturating_add(charge_of(&end)?);
                let at = MissAt { spent, ..at };
                chain
                    .missed(context, store, request, progress, sent, at, ModelFailure::Stopped { reason, end }, &mut cancelled)
                    .await?;
                continue;
            }
            Err(failure) => {
                chain
                    .missed(context, store, request, progress, sent, at, failure, &mut cancelled)
                    .await?;
                continue;
            }
        };
        // Every response end is a usage fact: an answer that is later cancelled, misses or spends the budget included.
        chain.record_usage(context, request, step, end.captured_usage.as_ref());
        check_bound(request, &cancelled, deadline)?;
        spent = spent.saturating_add(charge_of(&end)?);
        let mut raw = serde_json::to_value(end)
            .map_err(|_| "harness model response could not be recorded")?;
        redact(&mut raw, &request.route.credentials);
        let end: StreamEnd = serde_json::from_value(raw.clone())
            .map_err(|_| "harness model response could not be recorded")?;
        let content = end.captured_content.unwrap_or_default();
        let calls: Vec<_> = content.tool_calls().into_iter().cloned().collect();
        let text = content.joined_texts().unwrap_or_default();
        if calls.is_empty() {
            if let Some(failure) = crate::backend::plain_output_failure(&text) {
                // An empty or failure-shaped answer is a miss too; it was charged above.
                let at = MissAt { spent, ..at };
                let failure = ModelFailure::Terminal(failure.summary());
                chain
                    .missed(context, store, request, progress, sent, at, failure, &mut cancelled)
                    .await?;
                continue;
            }
        }
        // The route answered: its health is ok again (`route_health`).
        request.route.observe(context, chain.route, route_health::Seen::Answered);
        let message =
            ChatMessage::assistant(content).with_reasoning_content(end.captured_reasoning_content);
        let event = Event::Assistant {
            text,
            raw,
            message: message.clone(),
        };
        let exhausted = spent > request.limits.max_tokens
            || (spent == request.limits.max_tokens && !calls.is_empty());
        if calls.is_empty() {
            if exhausted {
                store.complete_step(&request.id, &event)?;
                let _ = events.send(event);
                return Err("harness token limit exceeded".into());
            }
            chain.completed();
            return Ok(event);
        }
        let attempt = store.complete_step(&request.id, &event)?;
        let _ = events.send(event);
        messages.push(message);
        let mut call_ids = std::collections::HashSet::new();
        if calls
            .iter()
            .any(|call| call.call_id.is_empty() || !call_ids.insert(&call.call_id))
        {
            return Err("harness duplicate or empty tool call id".into());
        }
        drop(call_ids);
        for call in calls {
            let prepared = if exhausted {
                Err(tools::error("token limit exceeded"))
            } else {
                request.tools.prepare(&call)
            };
            // Revocation may precede the pipeline's cancellation notification. It ends
            // the turn, rather than becoming a refusal with a new durable tool intent.
            check_bound(request, &cancelled, deadline)?;
            TurnDelivery::check_deadline(delivery.reserved2_budget, context, &request.conversation)?;
            let run = store.tool_start(&request.id, &attempt, &call)?;
            crate::prompt::record_recall_hits(context, std::mem::take(recalled));
            // Once a validated handler is scheduled, its effects may escape even if
            // it reports an error. Never retry a later model stream in this turn.
            chain.dispatched |= prepared.is_ok();
            let (mut outcome, stop) = match prepared {
                Err(outcome) => (outcome, None),
                Ok(args) if request.tools.handles_interaction(&call.fn_name) => {
                    let result = match &request.tools.interactions {
                        Some(mailbox) => {
                            let binding = interactions::Binding {
                                mailbox, key: &request.conversation, session: &session,
                                turn: &request.id, epoch: &request.generation, deadline, deadline_generation,
                            };
                            if call.fn_name == interactions::ASK {
                                binding.question(&args, cancelled.clone()).await
                            } else {
                                binding.group_send(&args, cancelled.clone()).await
                            }
                        }
                        None => Ok(tools::error("Owner interaction is unavailable.")),
                    };
                    match result {
                        Ok(outcome) => (outcome, None),
                        Err(reason) => {
                            let mut outcome = tools::error("Owner interaction ended.");
                            if matches!(&reason, Failure::Uncertain(_)) {
                                outcome["_meta"] = serde_json::json!({"herDelivery":"unknown"});
                            }
                            (outcome, Some(reason))
                        }
                    }
                }
                Ok(args) => {
                    let tools = request.tools.clone();
                    let invocation = call.clone();
                    let cancel = cancelled.clone();
                    let budget = delivery.reserved2_budget;
                    let context = context.clone();
                    let key = request.conversation.clone();
                    tokio::task::spawn_blocking(move || {
                        if *cancel.borrow() || tokio::time::Instant::now() >= deadline {
                            return (tools::error("tool dispatch cancelled or expired"), None);
                        }
                        if let Err(reason) = TurnDelivery::check_deadline(budget, &context, &key) {
                            return (tools::error("tool dispatch cancelled or expired"), Some(reason.into()));
                        }
                        (tools.dispatch(&invocation, &args), None)
                    })
                    .await
                    .map_err(|_| {
                        Failure::Uncertain(
                            "harness tool worker failed before its effect could be settled".into(),
                        )
                    })?
                }
            };
            redact(&mut outcome, &request.route.credentials);
            let message = tool_message(&call, &outcome);
            store
                .tool_result(&request.id, &run, &message, &outcome)
                .map_err(Failure::Uncertain)?;
            if let Some(reason) = stop {
                return Err(reason);
            }
            if outcome["_meta"]["herDelivery"] == "unknown" {
                return Err(Failure::Uncertain("QQ send delivery could not be confirmed; administrator confirmation required before retry".into()));
            }
            if crate::pipeline::is_send_tool(&call.fn_name) && outcome["isError"] != true {
                sent.store(true, Ordering::SeqCst);
            }
            // Settle the submitted handler first, even when authority changed in flight.
            check_bound(request, &cancelled, deadline)?;
            messages.push(message);
        }
        if exhausted {
            return Err("harness token limit exceeded".into());
        }
        chain.answered();
    }
    Err("harness step limit exceeded".into())
}

/// Where a request that missed stood in its turn.
#[derive(Clone, Copy)]
struct MissAt {
    step: usize,
    /// Tokens the turn has spent, the missed answer's included.
    spent: u32,
    /// The size of the request's delivered messages, serialized.
    request_bytes: usize,
    deadline: tokio::time::Instant,
}

/// A turn's place in its candidate chain (`harness/routes.rs`) and what its current request has met so far.
struct ChainState {
    scope: String,
    key: routes::ChainKey,
    /// The route the next request goes to.
    route: usize,
    /// The turn's one dropped-stream retry is spent.
    retried: bool,
    /// A validated tool handler was dispatched in this turn.
    dispatched: bool,
    /// The current, unchanged request already met a 429 on this route.
    rate_limited: bool,
    /// Empty answers the current, unchanged request got on this route.
    empty_answers: u32,
    /// The turn's number in the token ledger (`usage::next_number`).
    usage_turn: i64,
    /// Requests already sent for the current, unchanged request: its retries and failovers.
    repeats: i64,
}

impl ChainState {
    fn new(context: &RuntimeContext, request: &TurnRequest, usage_turn: i64) -> Self {
        Self {
            scope: routes::scope_of(context),
            key: routes::ChainKey {
                conversation: request.conversation.clone(),
                generation: request.generation.clone(),
                signature: request.route.signature(),
            },
            route: request.route.start,
            retried: false,
            dispatched: false,
            rate_limited: false,
            empty_answers: 0,
            usage_turn,
            repeats: 0,
        }
    }

    /// The current request was answered: the next request starts with fresh 429, empty-answer and repeat counts.
    fn answered(&mut self) {
        self.rate_limited = false;
        self.empty_answers = 0;
        self.repeats = 0;
    }

    /// The usage fact of the response end the current route just returned to the turn's step `step` (`usage::record`).
    fn record_usage(&self, context: &RuntimeContext, request: &TurnRequest, step: usize, reported: Option<&genai::chat::Usage>) {
        let route = &request.route.routes[self.route];
        let place = usage::Place {
            key: request.conversation.clone(),
            turn: self.usage_turn,
            step: step as i64,
            retry: self.repeats,
        };
        usage::record(context, &place, &route.id, &route.model, usage::TURN, reported);
    }

    /// The turn's model answered in full: the conversation's failover budget starts over.
    fn completed(&self) {
        routes::with_memory(&self.scope, |memory| memory.on_completed(&self.key));
    }

    /// The current request brought no usable answer: retry it on the same route, move it to the next route or end the
    /// turn (`routes::decide`). A route request that fails over or ends the turn is kept for the pipeline's reports,
    /// and every outcome goes to the run record with the route and the request's size.
    #[allow(clippy::too_many_arguments)]
    async fn missed(
        &mut self,
        context: &RuntimeContext,
        store: &store::Store,
        request: &TurnRequest,
        progress: &std::sync::Mutex<Progress>,
        sent: &AtomicBool,
        at: MissAt,
        failure: ModelFailure,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Result<(), Failure> {
        let (miss, summary) = failure.miss();
        let route = &request.route.routes[self.route];
        let now = std::time::Instant::now();
        let next_route = self.route + 1 < request.route.routes.len()
            && routes::with_memory(&self.scope, |memory| memory.can_fail_over(&self.key, now));
        let situation = routes::Situation {
            retry_429: route.spec.retry_429,
            rate_limited_before: self.rate_limited,
            empty_answers: self.empty_answers,
            stream_retried: self.retried,
            dispatched: self.dispatched,
            sent: sent.load(Ordering::SeqCst),
            next_route,
            steps_left: at.step + 1 < request.limits.max_steps && at.spent < request.limits.max_tokens,
            unit: request.limits.retry_unit,
            remaining: at.deadline.saturating_duration_since(tokio::time::Instant::now()),
        };
        let next = routes::decide(&miss, &situation);
        let record = |event: &str, outcome: Outcome, action: String| {
            let reason = format!(
                "{}: route {} ({summary}) on a request of {} bytes; {action}",
                request.conversation, route.id, at.request_bytes
            );
            crate::recovery_log::record(context, Trigger::Turn, Component::ModelChain, event, outcome, &reason);
        };
        let keep = || {
            progress.lock().unwrap_or_else(|p| p.into_inner()).failures.push(RouteFailure {
                route: route.id.clone(),
                model: route.model.clone(),
                summary: summary.clone(),
            });
            // A route failure is the route's health too (`route_health`); a retry on the same route is not.
            let class = crate::model_failure::Failure::classify("", &summary).class;
            request.route.observe(context, self.route, route_health::Seen::Failed { class });
        };
        match next {
            routes::Next::Fail => {
                keep();
                let class = crate::model_failure::Failure::classify("", &summary).class;
                let unsafe_miss = self.dispatched
                    && (matches!(miss, routes::Miss::Dropped) || matches!(class, "timeout" | "network"));
                if !next_route && !situation.sent && !unsafe_miss && class != "refusal"
                    && tokio::time::Instant::now() < at.deadline
                {
                    progress.lock().unwrap_or_else(|p| p.into_inner()).route_chain_exhausted = true;
                }
                record("route-failed", Outcome::Failed, "the turn ends".to_string());
                Err(summary.into())
            }
            routes::Next::FailOver => {
                keep();
                let next = self.route + 1;
                record("failover-step", Outcome::Ok, format!("next route {}", request.route.routes[next].id));
                routes::with_memory(&self.scope, |memory| memory.on_failover(&self.key, next, now));
                check_bound(request, cancelled, at.deadline)?;
                store.fail_attempt(&request.id)?;
                self.route = next;
                self.rate_limited = false;
                self.empty_answers = 0;
                self.repeats += 1;
                Ok(())
            }
            routes::Next::Retry { wait } => {
                record(
                    "route-retry",
                    Outcome::Ok,
                    format!("retrying the same route after {} ms", wait.as_millis()),
                );
                match miss {
                    routes::Miss::RateLimited { .. } => self.rate_limited = true,
                    routes::Miss::Empty => self.empty_answers += 1,
                    routes::Miss::Dropped => self.retried = true,
                    routes::Miss::Failed { .. } => {}
                }
                self.repeats += 1;
                check_bound(request, cancelled, at.deadline)?;
                store.fail_attempt(&request.id)?;
                if !wait.is_zero() {
                    tokio::select! { biased;
                        _ = cancelled.changed() => return Err(Failure::Cancelled),
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                Ok(())
            }
        }
    }
}

fn check_bound(
    request: &TurnRequest,
    cancelled: &watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> Result<(), Failure> {
    if *cancelled.borrow() {
        return Err(Failure::Cancelled);
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(DEADLINE_EXCEEDED.into());
    }
    request.tools.check().map_err(|error| match error {
        tools::CheckError::Revoked(_) => Failure::Cancelled,
        tools::CheckError::Unavailable(reason) => Failure::Failed(reason),
    })
}

/// The tokens a model response is charged: its reported usage (total, else prompt plus completion), else its
/// serialized size, since a provider without usage is charged conservatively for its generated payload.
fn charge_of(end: &StreamEnd) -> Result<u32, &'static str> {
    let usage = end
        .captured_usage
        .as_ref()
        .and_then(|u| {
            u.total_tokens.or_else(|| {
                u.prompt_tokens
                    .zip(u.completion_tokens)
                    .map(|(a, b)| a.max(0).saturating_add(b.max(0)))
            })
        })
        .filter(|v| *v > 0)
        .map(|v| v as u32);
    match usage {
        Some(tokens) => Ok(tokens),
        None => {
            let raw = serde_json::to_value(end).map_err(|_| "harness model response could not be recorded")?;
            Ok(raw.to_string().len().min(u32::MAX as usize) as u32)
        }
    }
}

/// One model request on `route`; `max_tokens` is already capped by the route's limit.
async fn model_step(
    client: &Client,
    route: &Route,
    messages: Vec<ChatMessage>,
    tools: Vec<genai::chat::Tool>,
    max_tokens: u32,
    image_input: bool,
) -> Result<StreamEnd, ModelFailure> {
    // The pinned adapter drops delta.refusal. Preserve only its presence at the wire
    // boundary; never retain the provider's refusal text in diagnostics or history.
    let refused = Arc::new(AtomicBool::new(false));
    let refusal_flag = refused.clone();
    let mut options = ChatOptions::default()
        .with_max_tokens(max_tokens)
        .with_capture_content(true)
        .with_capture_tool_calls(true)
        .with_capture_reasoning_content(true)
        .with_capture_usage(true)
        .with_raw_frame_fn(move |_, frame| {
            if frame
                .json()
                .and_then(|value| {
                    value["choices"].as_array().map(|choices| {
                        choices.iter().any(|choice| {
                            choice["finish_reason"] == "content_filter"
                                || choice
                                    .pointer("/delta/refusal")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| !text.is_empty())
                        })
                    })
                })
                .unwrap_or(false)
            {
                refusal_flag.store(true, Ordering::SeqCst);
            }
        });
    // A route without an effort sends none, as before routes.
    if let Some(effort) = route.spec.effort.as_deref().and_then(ReasoningEffort::from_keyword) {
        options = options.with_reasoning_effort(effort);
    }
    // SDK errors may contain provider payloads or URLs. Never format them into HER diagnostics.
    let mut response = client
        .exec_chat_stream(
            ModelSpec::Target(route.target.clone()),
            ChatRequest::new(model_messages(messages, image_input)).with_tools(tools),
            Some(&options),
        )
        .await
        .map_err(model_error)?;
    let mut end = None;
    while let Some(event) = response.stream.next().await {
        match event {
            Ok(ChatStreamEvent::End(value)) => end = Some(value),
            Ok(_) => {}
            // A refusal already seen is the answer's failure, whatever the stream does after it.
            Err(_) if refused.load(Ordering::SeqCst) => break,
            Err(error) => return Err(model_error(error)),
        }
    }
    if refused.load(Ordering::SeqCst) {
        let reason = crate::model_failure::Failure::new("refusal", "MODEL_REFUSAL").summary();
        // A refused answer that reached its end is charged its usage before the miss is decided (`Stopped`); a stream
        // that ended without its end has no usage to charge.
        return Err(match end {
            Some(end) => ModelFailure::Stopped {
                reason,
                end: Box::new(end),
            },
            None => reason.into(),
        });
    }
    let end = end.ok_or_else(|| {
        ModelFailure::Dropped(
            crate::model_failure::Failure::new("network", "INCOMPLETE_STREAM").summary(),
        )
    })?;
    if matches!(end.captured_stop_reason, Some(StopReason::ContentFilter(_))) {
        return Err(ModelFailure::Stopped {
            reason: crate::model_failure::Failure::new("refusal", "MODEL_REFUSAL").summary(),
            end: Box::new(end),
        });
    }
    if !matches!(
        end.captured_stop_reason,
        Some(StopReason::Completed(_) | StopReason::ToolCall(_))
    ) {
        return Err(ModelFailure::Stopped {
            reason: crate::model_failure::Failure::new("backend", "UNSAFE_FINISH_REASON").summary(),
            end: Box::new(end),
        });
    }
    Ok(end)
}

fn model_error(error: genai::Error) -> ModelFailure {
    let failure = model_failure(&error);
    let reason = failure.summary();
    if failure.class == "quota" && failure.code == "429" {
        return ModelFailure::RateLimited {
            reason,
            wait: rate_limit_wait(&error),
        };
    }
    match &error {
        genai::Error::WebStream { error: inner, .. }
            if inner.downcast_ref::<genai::Error>().is_none()
                && inner.downcast_ref::<genai::webc::StreamTimeout>().is_none() =>
        {
            ModelFailure::Dropped(reason)
        }
        _ => ModelFailure::Terminal(reason),
    }
}

/// The wait a 429 response asks for (`routes::retry_after`), through the streaming wrapper; the digest's route walk
/// reads it too (`one_shot::RouteWalk`).
pub(crate) fn rate_limit_wait(error: &genai::Error) -> Option<Duration> {
    if let genai::Error::WebStream { error: inner, .. } = error {
        if let Some(error) = inner.downcast_ref::<genai::Error>() {
            return rate_limit_wait(error);
        }
    }
    let headers = error.headers()?;
    let milliseconds = headers.get("retry-after-ms").and_then(|value| value.to_str().ok());
    let seconds = headers.get("retry-after").and_then(|value| value.to_str().ok());
    routes::retry_after(milliseconds, seconds)
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
        genai::Error::ChatResponse { body, .. } => Some(serde_json::json!({"error":body})),
        genai::Error::HttpError { body, .. } => serde_json::from_str(body).ok(),
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus { body, .. } => {
                serde_json::from_str(body).ok()
            }
            genai::webc::Error::Reqwest(error) if error.is_timeout() => {
                return Failure::new("timeout", "TIMEOUT")
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

/// Structured source references carried by the model request's tool-result packets.
fn model_request_refs(messages: &[ChatMessage]) -> Vec<Value> {
    fn tool_refs(packet: &Value, refs: &mut Vec<Value>) {
        refs.extend(crate::model_failure::message_refs(packet));
        // MCP text blocks contain serialized structured packets. Decode only those
        // envelopes, never chat fields such as raw_message or ordinary user prose.
        if let Some(blocks) = packet["content"].as_array() {
            for block in blocks.iter().filter(|block| block["type"] == "text") {
                if let Some(packet) = block["text"]
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                {
                    tool_refs(&packet, refs);
                }
            }
        }
    }
    let mut refs = Vec::new();
    for message in messages {
        for part in &message.content {
            match part {
                ContentPart::ToolResponse(response) => {
                    if let Ok(packet) = serde_json::from_str::<Value>(&response.content) {
                        tool_refs(&packet, &mut refs);
                    }
                }
                _ => {}
            }
        }
    }
    crate::model_failure::message_refs(&Value::Array(refs))
}

/// Filter delivery copies after history replay; never rewrite canonical conversation facts.
fn filter_model_messages(
    context: &RuntimeContext,
    messages: Vec<ChatMessage>,
) -> Result<Vec<ChatMessage>, String> {
    let cfg = crate::model_view::with_blacklist(context, &context.load_config()?);
    let mut result = Vec::new();
    for mut message in messages {
        if message.role == ChatRole::System {
            result.push(message);
            continue;
        }
        let mut parts = Vec::new();
        for part in message.content {
            parts.push(match part {
                ContentPart::Text(text) => {
                    ContentPart::Text(crate::model_view::strip_text(&text, &cfg))
                }
                ContentPart::ReasoningContent(text) => ContentPart::ReasoningContent(
                    crate::model_view::strip_from_model_text(&text, &cfg),
                ),
                ContentPart::ToolCall(mut call) => {
                    call.fn_arguments =
                        crate::model_view::strip_model_chat_data(&call.fn_arguments, &cfg);
                    if call.fn_name == interactions::ASK {
                        let filter = |value: &mut Value| {
                            if let Some(text) = value.as_str() {
                                *value = Value::String(crate::model_view::strip_from_model_text(text, &cfg));
                            }
                        };
                        if let Some(question) = call.fn_arguments.get_mut("question") {
                            filter(question);
                        }
                        if let Some(options) = call.fn_arguments.get_mut("options").and_then(Value::as_array_mut) {
                            options.iter_mut().for_each(filter);
                        }
                    }
                    ContentPart::ToolCall(call)
                }
                ContentPart::ToolResponse(mut response) => {
                    response.content =
                        filter_tool_text(&response.content, &cfg, response.fn_name.as_deref())?;
                    ContentPart::ToolResponse(response)
                }
                other => other,
            });
        }
        message.content = parts.into();
        result.push(message);
    }
    Ok(result)
}

fn filter_tool_text(text: &str, cfg: &Value, tool: Option<&str>) -> Result<String, String> {
    let Ok(mut data) = serde_json::from_str::<Value>(text) else {
        return Ok(crate::model_view::strip_from_model_text(text, cfg));
    };
    if let (Some(tool), Some(source)) = (tool, data.pointer("/_meta/herMemoryResult")) {
        if !tool.starts_with("memory_") || source["v"] != 1 || source["name"] != tool {
            return Err("harness memory result source is invalid".into());
        }
        let filtered = crate::model_view::strip_model_chat_data(&source["data"], cfg);
        let rendered = crate::mcp::format_tool_result(tool, &source["args"], &filtered)
            .map_err(|_| "harness memory result could not be formatted")?;
        data["content"] = serde_json::json!([{"type":"text","text":rendered}]);
        if let Some(meta) = data.get_mut("_meta").and_then(Value::as_object_mut) {
            meta.remove("herMemoryResult");
        }
        if data["_meta"]
            .as_object()
            .is_some_and(|meta| meta.is_empty())
        {
            if let Some(object) = data.as_object_mut() {
                object.remove("_meta");
            }
        }
        return Ok(data.to_string());
    }
    // Decode serialized QQ packets before stripping chat fields; IDs and cursors stay intact.
    if let Some(blocks) = data.get_mut("content").and_then(Value::as_array_mut) {
        for block in blocks {
            if block["type"] == "text" {
                if let Some(text) = block["text"].as_str() {
                    let copy = if tool == Some(interactions::ASK) {
                        crate::model_view::strip_from_model_text(text, cfg)
                    } else {
                        filter_tool_text(text, cfg, None)?
                    };
                    block["text"] = Value::String(copy);
                }
            }
        }
        return Ok(data.to_string());
    }
    Ok(crate::model_view::strip_model_chat_data(&data, cfg).to_string())
}

/// Preview every oversized tool response of a delivery copy; other parts, images included, pass unchanged.
fn preview_tool_results(messages: Vec<ChatMessage>, cap: usize) -> Vec<ChatMessage> {
    let mut result = Vec::new();
    for mut message in messages {
        let mut parts = Vec::new();
        for part in message.content {
            parts.push(match part {
                ContentPart::ToolResponse(mut response) => {
                    response.content = preview_tool_text(&response.content, cap);
                    ContentPart::ToolResponse(response)
                }
                other => other,
            });
        }
        message.content = parts.into();
        result.push(message);
    }
    result
}

/// The model copy of one tool result. A result longer than `cap` characters becomes one MCP result:
/// the longest leading part of its text that keeps the whole copy within `cap`, a note that the rest
/// was omitted, and the original error flag. Canonical facts keep the whole result.
fn preview_tool_text(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let data = serde_json::from_str::<Value>(text).ok();
    let body = data
        .as_ref()
        .and_then(|data| data["content"].as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_else(|| text.to_string());
    let error_flag = data.as_ref().and_then(|data| data.get("isError")).cloned();
    // Byte offsets of every character boundary, so a prefix never splits a character.
    let ends: Vec<usize> = body
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(body.len()))
        .collect();
    let total = ends.len() - 1;
    let build = |shown: usize| {
        let leading = &body[..ends[shown]];
        let note = format!("[Tool result too long: showing the first {shown} of {total} characters of its text; the rest of the result was omitted.]");
        let mut copy = serde_json::json!({"content":[
            {"type":"text","text":leading},
            {"type":"text","text":note}
        ]});
        if let Some(flag) = &error_flag {
            copy["isError"] = flag.clone();
        }
        copy.to_string()
    };
    // The serialized copy never shrinks as the prefix grows: bisect for the longest one that fits.
    let (mut low, mut high) = (0, total);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if build(mid).chars().count() <= cap {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    build(low)
}

/// Keep binary image parts in the canonical tool fact; never embed their base64 in model text.
fn tool_message(call: &ToolCall, outcome: &Value) -> ChatMessage {
    let mut text = outcome.clone();
    let mut images = Vec::new();
    if let Some(blocks) = text["content"].as_array_mut() {
        blocks.retain(|block| {
            if block["type"] != "image" {
                return true;
            }
            if let (Some(mime), Some(data)) = (block["mimeType"].as_str(), block["data"].as_str()) {
                images.push(ContentPart::Binary(Binary::from_base64(mime, data, None)));
            }
            false
        });
    }
    let mut message = ChatMessage::tool(ToolResponse::from_tool_call(call, text.to_string()));
    message.content.extend(images);
    message
}

/// Chat Completions accepts vision on user messages. Complete every tool response in a round
/// before attaching its images, preserving provider call/result adjacency on live use and replay.
/// A route without image input (`image_input` false) gets `NO_IMAGE_INPUT` text in place of every
/// image part, inbound or from a tool, live and on replay; canonical facts keep the image.
fn model_messages(messages: Vec<ChatMessage>, image_input: bool) -> Vec<ChatMessage> {
    let mut result = Vec::new();
    let mut images = Vec::new();
    for mut message in messages {
        if message.role == ChatRole::Tool {
            let mut parts = Vec::new();
            for part in message.content {
                match part {
                    ContentPart::Binary(binary) => images.push(ContentPart::Binary(binary)),
                    ContentPart::ToolResponse(ref response) => {
                        images.push(ContentPart::Text(format!(
                            "Images from tool call {}:",
                            response.call_id
                        )));
                        parts.push(part);
                    }
                    _ => parts.push(part),
                }
            }
            message.content = parts.into();
        } else if !images.is_empty() {
            if images.iter().any(|p| matches!(p, ContentPart::Binary(_))) {
                result.push(ChatMessage::user(std::mem::take(&mut images)));
            } else {
                images.clear();
            }
        }
        result.push(message);
    }
    if images.iter().any(|p| matches!(p, ContentPart::Binary(_))) {
        result.push(ChatMessage::user(images));
    }
    if !image_input {
        for message in result
            .iter_mut()
            .filter(|message| message.role == ChatRole::User)
        {
            for part in message.content.iter_mut() {
                if matches!(part, ContentPart::Binary(binary) if binary.is_image()) {
                    *part = ContentPart::Text(NO_IMAGE_INPUT.to_string());
                }
            }
        }
    }
    result
}

/// Redact decoded strings, so JSON escaping and split stream chunks cannot hide a key.
fn redact_credentials(text: &str, credentials: &[String]) -> String {
    credentials.iter().fold(text.to_string(), |text, key| redact_credential(&text, key))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ticket219_core_turn_capture_masks_its_credential_snapshot() {
        let route = ModelRoute {
            endpoint: String::new(),
            credentials: vec!["fixture-previous-key".into()],
            routes: Vec::new(),
            configured: Vec::new(),
            start: 0,
        };
        let request = ChatRequest::new(vec![ChatMessage::user("before fixture-previous-key after")]);
        let capture = route.capture_request("fixture", 1, &request).to_string();
        assert!(capture.contains("before [REDACTED] after"), "turn capture missed the captured credential");
    }

    #[test]
    fn qq_tool_text_schemas_filter_delivery_and_replay_without_mutating_facts() {
        let cfg = json!({"socialV2":{"context":{"stripFromModelText":["WORD"]}}});
        for (field, value, expected) in [
            ("formatted", json!("WORD memory"), json!(" memory")),
            ("block", json!("WORD slang"), json!(" slang")),
            ("meaning", json!("WORD meaning"), json!(" meaning")),
            ("usage", json!("WORD usage"), json!(" usage")),
            ("example", json!("WORD example"), json!(" example")),
            ("messages", json!("WORD message"), json!(" message")),
            ("messages", json!(["WORD first","WORD second"]), json!([" first"," second"])),
        ] {
            let packet = json!({field:value,"messageId":"WORD-id"});
            let original = json!({"content":[{"type":"text","text":packet.to_string()}]}).to_string();
            // Live delivery and history replay both regenerate from this same canonical fact.
            for _ in 0..2 {
                let copy: Value = serde_json::from_str(&filter_tool_text(&original, &cfg, Some("qq_read")).unwrap()).unwrap();
                let delivered: Value = serde_json::from_str(copy["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(delivered[field], expected, "{field}");
                assert_eq!(delivered["messageId"], "WORD-id");
            }
            assert_eq!(filter_tool_text(&original, &json!({}), Some("qq_read")).unwrap(), original,
                "removing the filter restores the original {field}");
        }
    }

    #[test]
    fn prompt_text_delivery_filter_never_adds_a_denied_term() {
        // A saved rule `D鲸鲸鱼鱼SH` reads `D鲸鱼SH` after the render's own strip; the harness strips the delivery copy again.
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-140-delivery-{}",
            crate::turn_ledger::mint_id("")
        )));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let cfg = json!({"consoleToken":"fixture-console-token-140","socialV2":{"context":{"stripFromModelText":["鲸鱼","\n\n"]}}});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let sent = vec![
            ChatMessage::user("【别像客服】D鲸鱼SH 鲸鱼好"),
            ChatMessage::user("【别像客服】D\n\nSH"),
            ChatMessage::assistant("a鲸鱼b"),
        ];
        let delivered = filter_model_messages(&context, sent).unwrap();
        let texts: Vec<String> = delivered.iter().map(|m| m.content.joined_texts().unwrap_or_default()).collect();
        std::fs::remove_dir_all(root).unwrap();
        for text in &texts {
            assert!(!crate::model_text_policy::denied_terms(text).contains(&"dsh".to_string()), "{text}");
        }
        // Per occurrence (#239): the occurrence that would join "D" and "SH" stays, the safe one is stripped.
        assert_eq!(texts, ["【别像客服】D鲸鱼SH 好", "【别像客服】D\n\nSH", "ab"]);
    }

    #[test]
    fn two_conversations_can_admit_together_on_a_fresh_database() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-67-concurrent-{}",
            crate::turn_ledger::mint_id("")
        )));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        // Reserve the only model endpoint; cancellation happens before any request.
        let model = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}", model.local_addr().unwrap());
        let cfg =
            json!({"consoleToken":"fixture-console-token-67", "harness":{"model":"fake-chat"}});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(
            root.join("config/harness-api-key"),
            "synthetic-concurrency-key",
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            json!({"dsh":endpoint,"model":endpoint,"onebot":endpoint}).to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let db = rusqlite::Connection::open(root.join("data/conversations.db")).unwrap();
        // No schema exists. Hold the first writer so both initializers start together.
        db.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")
            .unwrap();
        let start = Arc::new(std::sync::Barrier::new(3));
        let (tx, rx) = std::sync::mpsc::channel();
        let workers: Vec<_> = (1..=2)
            .map(|n| {
                let (context, cfg, start, tx) =
                    (context.clone(), cfg.clone(), start.clone(), tx.clone());
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    start.wait();
                    let result = runtime.block_on(async {
                        let harness = Harness::open(&context)?;
                        let id = format!("concurrent-{n}");
                        let mut turn = harness.admit(TurnRequest {
                            id: id.clone(),
                            conversation: format!("group:{n}"),
                            generation: "first".into(),
                            prompt: "simultaneous first input".into(),
                            media: Vec::new(),
                            profile: "plain".into(),
                            route: ModelRoute::from_runtime(&context, &cfg)?,
                            deadline: Duration::from_secs(3),
                            tools: Tools::empty(),
                            limits: Limits::from_config(&cfg),
                        })?;
                        turn.cancel();
                        let _ = turn.wait().await;
                        if !matches!(harness.status(&id)?, Some(Status::Cancelled)) {
                            return Err("admitted turn was not durably cancelled".to_string());
                        }
                        Ok(())
                    });
                    tx.send(result).unwrap();
                })
            })
            .collect();
        start.wait();
        let early = rx.recv_timeout(Duration::from_millis(400));
        db.execute_batch("COMMIT;").unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(
            early.is_err(),
            "admission escaped the held initialization lock: {early:?}"
        );
        for result in rx.try_iter() {
            assert!(
                result.is_ok(),
                "concurrent first admission failed: {result:?}"
            );
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM turns", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    /// A one-text-block MCP result whose serialized form is exactly `chars` characters.
    fn tool_preview_result(chars: usize) -> String {
        let empty = json!({"content":[{"type":"text","text":""}]}).to_string();
        json!({"content":[{"type":"text","text":"a".repeat(chars - empty.chars().count())}]})
            .to_string()
    }

    /// The shown text, the note and the parsed copy of a preview, after checking it fits the cap.
    fn tool_preview_parts(copy: &str, cap: usize) -> (String, String, Value) {
        assert!(
            copy.chars().count() <= cap,
            "preview exceeds the cap: {} > {cap}",
            copy.chars().count()
        );
        let data: Value = serde_json::from_str(copy).unwrap();
        let blocks = data["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "a preview holds its text and one note: {copy}");
        (
            blocks[0]["text"].as_str().unwrap().to_string(),
            blocks[1]["text"].as_str().unwrap().to_string(),
            data,
        )
    }

    #[test]
    fn tool_preview_keeps_a_result_at_the_cap_and_previews_one_character_over() {
        let at_cap = tool_preview_result(1_000);
        assert_eq!(at_cap.chars().count(), 1_000);
        assert_eq!(
            preview_tool_text(&at_cap, 1_000),
            at_cap,
            "a result at the cap is delivered whole"
        );
        let over = tool_preview_result(1_001);
        let (shown, note, data) = tool_preview_parts(&preview_tool_text(&over, 1_000), 1_000);
        assert!(
            !shown.is_empty() && shown.chars().all(|c| c == 'a'),
            "{shown}"
        );
        let total = 1_001
            - json!({"content":[{"type":"text","text":""}]})
                .to_string()
                .chars()
                .count();
        assert_eq!(
            note,
            format!(
                "[Tool result too long: showing the first {} of {total} characters of its text; the rest of the result was omitted.]",
                shown.chars().count()
            )
        );
        assert!(
            data.get("isError").is_none(),
            "a successful result stays successful: {data}"
        );
    }

    #[test]
    fn tool_preview_of_a_web_fetch_result_keeps_its_leading_text_and_error_flag_within_the_cap() {
        // The web_fetch outcome shape: the page as pretty JSON in one text block. Its quotes and
        // newlines are escaped again on the wire, so the copy is longer than the text it shows.
        let page = serde_json::to_string_pretty(&json!({
            "url": "https://example.invalid/long-page",
            "text": format!("HEAD175 {} TAIL175", "a \"quoted\" line\n".repeat(4_000)),
        }))
        .unwrap();
        for is_error in [false, true] {
            let mut outcome = json!({"content":[{"type":"text","text":page}]});
            outcome["isError"] = json!(is_error);
            let (shown, note, data) =
                tool_preview_parts(&preview_tool_text(&outcome.to_string(), 8_000), 8_000);
            assert!(page.starts_with(&shown), "the preview is the leading text");
            assert!(shown.contains("HEAD175") && !shown.contains("TAIL175"));
            assert!(
                note.contains(&format!("of {} characters", page.chars().count())),
                "{note}"
            );
            assert_eq!(data["isError"], json!(is_error), "{data}");
        }
    }

    #[test]
    fn tool_preview_counts_characters_not_bytes_and_accepts_plain_text() {
        let wide = json!({"content":[{"type":"text","text":"你".repeat(3_000)}]}).to_string();
        let (shown, note, _) = tool_preview_parts(&preview_tool_text(&wide, 1_000), 1_000);
        // The envelope and the note take under 200 characters; a byte count would show about 270.
        assert!(
            shown.chars().count() >= 800,
            "the preview was cut by bytes: {}",
            shown.chars().count()
        );
        assert!(
            note.contains(&format!(
                "the first {} of 3000 characters",
                shown.chars().count()
            )),
            "{note}"
        );
        let plain = "x".repeat(1_500);
        let (shown, _, _) = tool_preview_parts(&preview_tool_text(&plain, 1_000), 1_000);
        assert!(!shown.is_empty() && plain.starts_with(&shown), "{shown}");
    }

    #[test]
    fn tool_preview_of_a_memory_search_result_previews_the_rendered_model_copy() {
        let content = format!("HEAD175 {} TAIL175", "filler ".repeat(300));
        let hit = json!({"id":7,"owner_id":"private:8808","scope":"session","kind":"fact",
            "subject":null,"content":content,"score":1.0,"tags":null,"source":"agent",
            "importance":0.5,"pinned":0,"hits":0,"created_at":1,"last_used_at":null,
            "owner_type":"session","archived":0,"expires_at":null});
        let original = json!({"content":[{"type":"text","text":"canonical rendering"}],
            "_meta":{"herMemoryResult":{"v":1,"name":"memory_search","args":{},
            "data":{"ok":true,"count":1,"memories":[hit]}}}})
        .to_string();
        let filtered = filter_tool_text(&original, &json!({}), Some("memory_search")).unwrap();
        let (shown, _, data) = tool_preview_parts(&preview_tool_text(&filtered, 1_000), 1_000);
        assert!(shown.starts_with("1. [#7 fact] HEAD175 filler"), "{shown}");
        assert!(!shown.contains("TAIL175"), "{shown}");
        assert!(
            data.get("_meta").is_none(),
            "the internal source never reaches the model: {data}"
        );
    }

    #[test]
    fn tool_preview_cap_defaults_to_8000_characters_and_is_clamped() {
        for (harness, expected) in [
            (json!({}), 8_000_usize),
            (json!({"toolResultChars":2_500}), 2_500),
            (json!({"toolResultChars":1}), 1_000),
            (json!({"toolResultChars":-1}), 1_000),
            (json!({"toolResultChars":2500.0}), 2_500),
            (json!({"toolResultChars":5_000_000}), 100_000),
            (json!({"toolResultChars":"large"}), 8_000),
        ] {
            assert_eq!(
                Limits::from_config(&json!({ "harness": harness })).tool_result_chars,
                expected,
                "{harness}"
            );
        }
    }

    #[test]
    fn tool_preview_shortens_each_long_tool_response_and_keeps_other_parts() {
        let call = |id: &str| ToolCall {
            call_id: id.into(),
            fn_name: "web_fetch".into(),
            fn_arguments: json!({}),
            thought_signatures: None,
        };
        let long = json!({"content":[{"type":"text","text":"b".repeat(2_000)},
            {"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]});
        let short = json!({"content":[{"type":"text","text":"short result"}]});
        let prompt = "c".repeat(2_000);
        let delivered = preview_tool_results(
            vec![
                ChatMessage::user(prompt.clone()),
                tool_message(&call("long-call"), &long),
                tool_message(&call("short-call"), &short),
            ],
            1_000,
        );
        let responses: Vec<(String, String)> = delivered
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|part| match part {
                ContentPart::ToolResponse(r) => Some((r.call_id.clone(), r.content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].0, "long-call");
        tool_preview_parts(&responses[0].1, 1_000);
        assert_eq!(responses[1], ("short-call".to_string(), short.to_string()));
        assert!(
            delivered
                .iter()
                .flat_map(|m| &m.content)
                .any(|p| matches!(p, ContentPart::Text(text) if *text == prompt)),
            "user text is not a tool result"
        );
        assert!(
            delivered
                .iter()
                .flat_map(|m| &m.content)
                .any(|p| matches!(p, ContentPart::Binary(_))),
            "tool images are separate parts and stay"
        );
    }
}

#[cfg(test)]
#[path = "harness/cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
#[path = "harness/media_tests.rs"]
mod media_tests;

#[cfg(test)]
#[path = "harness/route_tests.rs"]
mod route_tests;

#[cfg(test)]
#[path = "harness/handover_tests.rs"]
mod handover_tests;
