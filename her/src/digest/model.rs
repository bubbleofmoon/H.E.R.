//! The digest's model port. Production uses the relay; tests install a fake per runtime root.
use crate::runtime::RuntimeContext;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

/// A fixed failure class (`quota`, `timeout`, `backend`, `network`, `refusal`, `unconfigured`, ...).
#[derive(Debug, PartialEq)]
pub(crate) struct ModelError(pub &'static str);

pub(crate) trait DigestModel: Send + Sync {
    /// One completion: `system` is the digest prompt, `input` the conversation evidence.
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError>;
}

#[cfg(test)]
static TEST_MODELS: std::sync::Mutex<Vec<(std::path::PathBuf, Arc<dyn DigestModel>)>> = std::sync::Mutex::new(Vec::new());

/// Tests: every digest run on the runtime at `root` uses `model`.
#[cfg(test)]
pub(crate) fn install_test_model(root: &std::path::Path, model: Arc<dyn DigestModel>) {
    let mut models = TEST_MODELS.lock().unwrap_or_else(|p| p.into_inner());
    models.retain(|(r, _)| r != root);
    models.push((root.to_path_buf(), model));
}

pub(crate) fn model_for(context: &RuntimeContext, cfg: &Value) -> Result<Arc<dyn DigestModel>, ModelError> {
    #[cfg(test)]
    if let Some(root) = context.runtime_root() {
        if let Some((_, model)) = TEST_MODELS.lock().unwrap_or_else(|p| p.into_inner()).iter().find(|(r, _)| r == root) {
            return Ok(model.clone());
        }
    }
    Ok(Arc::new(RelayModel::from_runtime(context, cfg)?))
}

/// The relay's OpenAI-compatible endpoint: `memory.digest.model`, else the model of the harness chain's first route,
/// with the harness credential; one non-streaming request bounded by `memory.digest.timeoutSecs`. Each answered
/// request is a usage fact (`harness::usage`) under the ledger key `internal:digest`.
pub(crate) struct RelayModel {
    route: crate::harness::ModelRoute,
    timeout: Duration,
    /// The runtime whose token ledger records each request's usage.
    context: RuntimeContext,
    /// The route id the digest uses (`route_of`).
    route_id: String,
}

impl RelayModel {
    pub(crate) fn from_runtime(context: &RuntimeContext, cfg: &Value) -> Result<Self, ModelError> {
        let (route_id, model) = route_of(cfg).ok_or(ModelError("unconfigured"))?;
        let route = crate::harness::ModelRoute::for_model(context, &model).map_err(|_| ModelError("unconfigured"))?;
        Ok(Self {
            route,
            timeout: timeout(cfg),
            context: context.clone(),
            route_id,
        })
    }

    /// The usage fact of one answered request (`harness::usage`), with a turn number of its own.
    fn record_usage(&self, reported: &genai::chat::Usage) {
        use crate::harness::usage;
        let place = usage::Place {
            key: usage::DIGEST_KEY.to_string(),
            turn: usage::next_number(),
            step: 0,
            retry: 0,
        };
        usage::record(&self.context, &place, &self.route_id, self.route.first_model(), usage::DIGEST, Some(reported));
    }
}

/// The digest's route id and model id: `memory.digest.model` (its own route, whose id is the model id), else the harness
/// chain's first route (`harness::routes::chain`: `harness.routes`, else `harness.model`); `None` when neither is
/// configured or the chain is invalid.
fn route_of(cfg: &Value) -> Option<(String, String)> {
    if let Some(model) = crate::settings::DIGEST_MODEL.text(cfg) {
        return Some((model.clone(), model));
    }
    crate::harness::routes::chain(cfg).ok().and_then(|chain| chain.into_iter().next()).map(|route| (route.id, route.model))
}

/// The digest's model id: `memory.digest.model`, else the model of the harness chain's first route
/// (`harness::routes::chain`: `harness.routes`, else `harness.model`); `None` when neither is configured or the chain
/// is invalid.
pub(crate) fn model_id(cfg: &Value) -> Option<String> {
    route_of(cfg).map(|(_, model)| model)
}

/// The bound on one digest model request: `memory.digest.timeoutSecs`.
pub(crate) fn timeout(cfg: &Value) -> Duration {
    Duration::from_secs(crate::settings::DIGEST_TIMEOUT_SECS.integer(cfg).unsigned_abs())
}

impl DigestModel for RelayModel {
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError> {
        use genai::chat::{ChatMessage, ChatOptions, ChatRequest, StopReason};
        let target = self.route.target();
        let timeout = self.timeout;
        let request = ChatRequest::new(vec![ChatMessage::system(system.to_string()), ChatMessage::user(input.to_string())]);
        let call = async move {
            let client = genai::Client::new().map_err(|_| ModelError("network"))?;
            let options = ChatOptions::default().with_max_tokens(2048);
            tokio::time::timeout(timeout, client.exec_chat(genai::ModelSpec::Target(target), request, Some(&options)))
                .await
                .map_err(|_| ModelError("timeout"))?
                .map_err(|error| ModelError(crate::harness::model_failure(&error).class))
        };
        // A fresh thread has no runtime context, so this works from sync jobs and spawn_blocking alike.
        let response = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(|_| ModelError("network"))?
                .block_on(call)
        })
        .join()
        .unwrap_or(Err(ModelError("network")))?;
        // Every response is a usage fact, a refused one included.
        self.record_usage(&response.usage);
        if matches!(response.stop_reason, Some(StopReason::ContentFilter(_))) {
            return Err(ModelError("refusal"));
        }
        Ok(response.into_first_text().unwrap_or_default())
    }
}
