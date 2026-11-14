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

/// The relay's OpenAI-compatible endpoint: `memory.digest.model`, else `harness.model`, with the
/// harness credential; one non-streaming request bounded by `memory.digest.timeoutSecs`.
pub(crate) struct RelayModel {
    route: crate::harness::ModelRoute,
    timeout: Duration,
}

impl RelayModel {
    pub(crate) fn from_runtime(context: &RuntimeContext, cfg: &Value) -> Result<Self, ModelError> {
        let configured = |path: &str| cfg.pointer(path).and_then(Value::as_str).filter(|v| !v.trim().is_empty());
        let model = configured("/memory/digest/model").or_else(|| configured("/harness/model")).ok_or(ModelError("unconfigured"))?;
        let route = crate::harness::ModelRoute::for_model(context, model).map_err(|_| ModelError("unconfigured"))?;
        let secs = cfg.pointer("/memory/digest/timeoutSecs").and_then(Value::as_u64).unwrap_or(120).clamp(1, 600);
        Ok(Self { route, timeout: Duration::from_secs(secs) })
    }
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
            let response = tokio::time::timeout(timeout, client.exec_chat(genai::ModelSpec::Target(target), request, Some(&options)))
                .await
                .map_err(|_| ModelError("timeout"))?
                .map_err(|error| ModelError(crate::harness::model_failure(&error).class))?;
            if matches!(response.stop_reason, Some(StopReason::ContentFilter(_))) {
                return Err(ModelError("refusal"));
            }
            Ok(response.into_first_text().unwrap_or_default())
        };
        // A fresh thread has no runtime context, so this works from sync jobs and spawn_blocking alike.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(|_| ModelError("network"))?
                .block_on(call)
        })
        .join()
        .unwrap_or(Err(ModelError("network")))
    }
}
