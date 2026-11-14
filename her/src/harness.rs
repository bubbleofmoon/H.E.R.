//! HER-owned model and tool turn execution. The pipeline retains admission authority and delivery.
pub(crate) mod store;
mod tools;
pub use tools::Tools;

use crate::runtime::{RuntimeArea, RuntimeContext};
use futures_util::StreamExt;
use genai::{
    adapter::AdapterKind,
    chat::{
        Binary, ChatMessage, ChatOptions, ChatRequest, ChatRole, ChatStreamEvent, ContentPart,
        StopReason, StreamEnd, ToolCall, ToolResponse,
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

/// Independent of a model route or the existing CLI backend pins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Dsh,
    Harness,
}

impl Engine {
    pub fn selected(cfg: &Value, conversation: &str) -> Result<Self, String> {
        let Some(selections) = cfg.get("conversationEngines") else {
            return Ok(Self::Dsh);
        };
        let selections = selections
            .as_object()
            .ok_or("conversationEngines must be an object")?;
        match selections.get(conversation) {
            None => Ok(Self::Dsh),
            Some(Value::String(value)) if value == "dsh" => Ok(Self::Dsh),
            Some(Value::String(value)) if value == "harness" => Ok(Self::Harness),
            _ => Err("unknown conversation engine selection".into()),
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Dsh => "dsh",
            Self::Harness => "harness",
        }
    }
}

/// Credentials are opaque: never derive Debug or serialize this target.
pub struct ModelRoute {
    key: String,
    model: String,
    endpoint: String,
    target: ServiceTarget,
}

impl ModelRoute {
    pub fn model_name(&self) -> &str {
        &self.model
    }
    pub fn from_runtime(context: &RuntimeContext, cfg: &Value) -> Result<Self, String> {
        if !context.is_isolated() {
            return Err("harness requires a selected runtime".into());
        }
        let model = cfg
            .pointer("/harness/model")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .ok_or("harness model is not configured")?;
        Self::for_model(context, model)
    }

    /// The selected runtime's relay route for `model`, authenticated with the harness credential.
    pub fn for_model(context: &RuntimeContext, model: &str) -> Result<Self, String> {
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
        Ok(Self {
            key: key.to_string(),
            model: model.replace(key, "[REDACTED]"),
            endpoint: endpoint.replace(key, "[REDACTED]"),
            target: ServiceTarget {
                model: ModelIden::new(AdapterKind::OpenAI, model.to_string()),
                endpoint: Endpoint::from_owned(endpoint),
                auth: AuthData::Key(key.to_string()),
            },
        })
    }

    pub(crate) fn target(&self) -> ServiceTarget {
        self.target.clone()
    }
}

/// Captured per turn, independent of later configuration changes.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    max_steps: usize,
    max_tokens: u32,
}

impl Limits {
    pub fn from_config(cfg: &Value) -> Self {
        Self {
            max_steps: cfg
                .pointer("/harness/maxSteps")
                .and_then(Value::as_u64)
                .unwrap_or(8)
                .clamp(1, 64) as usize,
            max_tokens: cfg
                .pointer("/harness/maxTokens")
                .and_then(Value::as_u64)
                .unwrap_or(32_768)
                .clamp(1, 1_000_000) as u32,
        }
    }
}

pub struct TurnRequest {
    pub id: String,
    pub conversation: String,
    pub generation: String,
    pub prompt: String,
    pub profile: String,
    pub route: ModelRoute,
    pub deadline: Duration,
    pub tools: Tools,
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub enum Status {
    Running,
    Completed(String),
    Failed(String),
    Cancelled,
    Paused { reason: String, detail: String },
}

enum Failure {
    Failed(String),
    Cancelled,
    Uncertain(String),
}

enum ModelFailure {
    Dropped(String),
    Terminal(String),
}

impl From<String> for ModelFailure {
    fn from(reason: String) -> Self {
        Self::Terminal(reason)
    }
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
    status: watch::Receiver<Status>,
    cancel: watch::Sender<bool>,
    events: broadcast::Sender<Event>,
}

impl Turn {
    pub fn message_refs(&self) -> Vec<Value> {
        self.request_refs.lock().unwrap_or_else(|p| p.into_inner()).clone()
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

    pub async fn wait(&mut self) -> Result<String, String> {
        loop {
            match self.status() {
                Status::Running => {}
                Status::Completed(text) => return Ok(text),
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

    pub fn admit(&self, mut request: TurnRequest) -> Result<Turn, String> {
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
        let key = request.route.key.clone();
        request.prompt = request.prompt.replace(&key, "[REDACTED]");
        request.profile = request.profile.replace(&key, "[REDACTED]");
        let mut messages = self.store.admit(&request)?;
        let instruction = request.tools.instruction();
        if !instruction.is_empty() {
            messages.insert(0, ChatMessage::system(instruction));
        }
        messages.push(ChatMessage::user(request.prompt.clone()));
        let store = self.store.clone();
        let (status_tx, status) = watch::channel(Status::Running);
        let (cancel, mut cancelled) = watch::channel(false);
        let (events, _) = broadcast::channel(32);
        let notifications = events.clone();
        let client = self.client.clone();
        let context = self.context.clone();
        let sent = Arc::new(AtomicBool::new(false));
        let sent_flag = sent.clone();
        let request_refs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let refs_for_task = request_refs.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            // Keep ownership through result persistence and uncertainty publication.
            let _owner = owner;
            let mut assistant = None;
            let cancellation = cancelled.clone();
            let execution = tool_loop(
                &context,
                &client,
                &store,
                &request,
                messages,
                &notifications,
                &sent_flag,
                &refs_for_task,
                cancellation,
            );
            tokio::pin!(execution);
            let mut interrupted = None;
            let outcome = tokio::select! { biased;
                _ = cancelled.changed() => { interrupted = Some(Status::Cancelled); None },
                _ = tokio::time::sleep(request.deadline) => {
                    interrupted = Some(Status::Failed("harness model deadline exceeded".into())); None
                },
                result = &mut execution => Some(result),
            };
            let outcome = match outcome {
                Some(result) => result,
                None => {
                    // Keep ownership of the handler and its result writer. Model waits are
                    // cancellable; a dispatched blocking handler must settle under revoked authority.
                    let _ = task_cancel.send(true);
                    let revoked = request.tools.revoke();
                    let settled = execution.await;
                    match revoked {
                        Ok(()) => settled,
                        Err(reason) => Err(Failure::Uncertain(reason)),
                    }
                }
            };
            let mut result = match outcome {
                Err(Failure::Uncertain(detail)) => Status::Paused {
                    reason: "send-unresolved".into(),
                    detail,
                },
                Err(Failure::Cancelled) => interrupted.unwrap_or(Status::Cancelled),
                Err(Failure::Failed(reason)) => interrupted.unwrap_or(Status::Failed(reason)),
                Ok(event) => match interrupted {
                    Some(status) => status,
                    None => {
                        let Event::Assistant { text, .. } = &event else {
                            unreachable!()
                        };
                        let status = Status::Completed(text.clone());
                        assistant = Some(event);
                        status
                    }
                },
            };
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
                let _ = notifications.send(event);
            }
            let _ = notifications.send(Event::Ended(result.clone()));
            let _ = status_tx.send(result);
        });
        Ok(Turn {
            sent,
            request_refs,
            status,
            cancel,
            events,
        })
    }
}

fn turn_owner(context: &RuntimeContext, turn: &str) -> Result<std::fs::File, String> {
    context.open_locked_file(
        RuntimeArea::Data,
        &format!("locks/harness-turn-{}.lock", crate::send_outbox::token_epoch(turn)),
    )
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

async fn tool_loop(
    context: &RuntimeContext,
    client: &Client,
    store: &store::Store,
    request: &TurnRequest,
    mut messages: Vec<ChatMessage>,
    events: &broadcast::Sender<Event>,
    sent: &AtomicBool,
    request_refs: &std::sync::Mutex<Vec<Value>>,
    mut cancelled: watch::Receiver<bool>,
) -> Result<Event, Failure> {
    let deadline = tokio::time::Instant::now() + request.deadline;
    let mut spent = 0u32;
    let mut retried = false;
    let mut dispatched = false;
    for step in 0..request.limits.max_steps {
        check_bound(request, &cancelled, deadline)?;
        if step > 0 {
            store.next_attempt(request)?;
        }
        let delivered = filter_model_messages(context, messages.clone())?;
        // Capture only structured references from the request being issued. Later read
        // results and recalled tool facts may add sources absent from the inbound wake.
        *request_refs.lock().unwrap_or_else(|p| p.into_inner()) = model_request_refs(&delivered);
        let model = model_step(
            client,
            &request.route,
            delivered,
            request.tools.model_tools(),
            request.limits.max_tokens.saturating_sub(spent),
        );
        let end = tokio::select! { biased;
            _ = cancelled.changed() => return Err(Failure::Cancelled),
            result = model => result,
        };
        let end = match end {
            Ok(end) => end,
            Err(ModelFailure::Dropped(_))
                if !retried && !dispatched && step + 1 < request.limits.max_steps =>
            {
                check_bound(request, &cancelled, deadline)?;
                store.fail_attempt(&request.id)?;
                retried = true;
                continue;
            }
            Err(ModelFailure::Dropped(reason) | ModelFailure::Terminal(reason)) => {
                return Err(reason.into());
            }
        };
        check_bound(request, &cancelled, deadline)?;
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
        let mut raw = serde_json::to_value(end)
            .map_err(|_| "harness model response could not be recorded")?;
        // Providers without usage are charged conservatively for their generated UTF-8 payload.
        let charge = usage.unwrap_or_else(|| raw.to_string().len().min(u32::MAX as usize) as u32);
        spent = spent.saturating_add(charge);
        redact(&mut raw, &request.route.key);
        let end: StreamEnd = serde_json::from_value(raw.clone())
            .map_err(|_| "harness model response could not be recorded")?;
        let content = end.captured_content.unwrap_or_default();
        let calls: Vec<_> = content.tool_calls().into_iter().cloned().collect();
        let text = content.joined_texts().unwrap_or_default();
        if calls.is_empty() {
            if let Some(failure) = crate::backend::plain_output_failure(&text) {
                return Err(failure.summary().into());
            }
        }
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
                Err(tools::error("harness token limit exceeded"))
            } else {
                request.tools.prepare(&call)
            };
            // Revocation may precede the pipeline's cancellation notification. It ends
            // the turn, rather than becoming a refusal with a new durable tool intent.
            check_bound(request, &cancelled, deadline)?;
            let run = store.tool_start(&request.id, &attempt, &call)?;
            // Once a validated handler is scheduled, its effects may escape even if
            // it reports an error. Never retry a later model stream in this turn.
            dispatched |= prepared.is_ok();
            let tools = request.tools.clone();
            let invocation = call.clone();
            let cancel = cancelled.clone();
            let mut outcome = tokio::task::spawn_blocking(move || {
                if *cancel.borrow() || tokio::time::Instant::now() >= deadline {
                    return tools::error("harness tool dispatch cancelled or expired");
                }
                match prepared {
                    Ok(args) => tools.dispatch(&invocation, &args),
                    Err(outcome) => outcome,
                }
            })
            .await
            .map_err(|_| {
                Failure::Uncertain(
                    "harness tool worker failed before its effect could be settled".into(),
                )
            })?;
            redact(&mut outcome, &request.route.key);
            let message = tool_message(&call, &outcome);
            store
                .tool_result(&request.id, &run, &message, &outcome)
                .map_err(Failure::Uncertain)?;
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
    }
    Err("harness step limit exceeded".into())
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
        return Err("harness model deadline exceeded".into());
    }
    request.tools.check().map_err(|error| match error {
        tools::CheckError::Revoked(_) => Failure::Cancelled,
        tools::CheckError::Unavailable(reason) => Failure::Failed(reason),
    })
}

async fn model_step(
    client: &Client,
    route: &ModelRoute,
    messages: Vec<ChatMessage>,
    tools: Vec<genai::chat::Tool>,
    max_tokens: u32,
) -> Result<StreamEnd, ModelFailure> {
    // The pinned adapter drops delta.refusal. Preserve only its presence at the wire
    // boundary; never retain the provider's refusal text in diagnostics or history.
    let refused = Arc::new(AtomicBool::new(false));
    let refusal_flag = refused.clone();
    let options = ChatOptions::default()
        .with_max_tokens(max_tokens.min(4096))
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
    // SDK errors may contain provider payloads or URLs. Never format them into HER diagnostics.
    let mut response = client
        .exec_chat_stream(
            ModelSpec::Target(route.target.clone()),
            ChatRequest::new(model_messages(messages)).with_tools(tools),
            Some(&options),
        )
        .await
        .map_err(model_error)?;
    let mut end = None;
    while let Some(event) = response.stream.next().await {
        if refused.load(Ordering::SeqCst) {
            return Err(
                crate::model_failure::Failure::new("refusal", "MODEL_REFUSAL")
                    .summary()
                    .into(),
            );
        }
        if let ChatStreamEvent::End(value) = event.map_err(model_error)? {
            end = Some(value);
        }
    }
    if refused.load(Ordering::SeqCst) {
        return Err(
            crate::model_failure::Failure::new("refusal", "MODEL_REFUSAL")
                .summary()
                .into(),
        );
    }
    let end = end.ok_or_else(|| {
        ModelFailure::Dropped(
            crate::model_failure::Failure::new("network", "INCOMPLETE_STREAM").summary(),
        )
    })?;
    if matches!(end.captured_stop_reason, Some(StopReason::ContentFilter(_))) {
        return Err(
            crate::model_failure::Failure::new("refusal", "MODEL_REFUSAL")
                .summary()
                .into(),
        );
    }
    if !matches!(
        end.captured_stop_reason,
        Some(StopReason::Completed(_) | StopReason::ToolCall(_))
    ) {
        return Err(
            crate::model_failure::Failure::new("backend", "UNSAFE_FINISH_REASON")
                .summary()
                .into(),
        );
    }
    Ok(end)
}

fn model_error(error: genai::Error) -> ModelFailure {
    let reason = classify_model_error(&error);
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

fn classify_model_error(error: &genai::Error) -> String {
    model_failure(error).summary()
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
                    block["text"] = Value::String(filter_tool_text(text, cfg, None)?);
                }
            }
        }
        return Ok(data.to_string());
    }
    Ok(crate::model_view::strip_model_chat_data(&data, cfg).to_string())
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
fn model_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
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
    result
}

/// Redact decoded strings, so JSON escaping and split stream chunks cannot hide a key.
fn redact(value: &mut Value, key: &str) {
    match value {
        Value::String(text) => *text = text.replace(key, "[REDACTED]"),
        Value::Array(values) => values.iter_mut().for_each(|v| redact(v, key)),
        Value::Object(fields) => {
            *fields = std::mem::take(fields)
                .into_iter()
                .map(|(name, mut value)| {
                    redact(&mut value, key);
                    (name.replace(key, "[REDACTED]"), value)
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
}

#[cfg(test)]
#[path = "harness/cancellation_tests.rs"]
mod cancellation_tests;
