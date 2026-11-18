//! Tool-less one-shot model jobs: one non-streaming completion on one model route of the selected runtime's relay,
//! authenticated with the harness credential, with no tools and no history. The digest and slang learning use it and
//! pick the route's model themselves (`harness::routes`). Tests install a fake per runtime root and job.
use crate::runtime::RuntimeContext;
use std::{sync::Arc, time::Duration};

/// A fixed failure class (`quota`, `timeout`, `backend`, `network`, `refusal`, `unconfigured`, ...); never provider text.
#[derive(Debug, PartialEq)]
pub(crate) struct ModelError(pub &'static str);

/// The job that asks. Tests install their fake per runtime root and job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    Digest,
    SlangLearner,
}

pub(crate) trait OneShot: Send + Sync {
    /// One completion: the `system` text (left out when empty), then the user `input`.
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError>;
}

#[cfg(test)]
type TestModels = std::sync::Mutex<Vec<(std::path::PathBuf, Job, Arc<dyn OneShot>)>>;

#[cfg(test)]
static TEST_MODELS: TestModels = std::sync::Mutex::new(Vec::new());

/// Tests: every `job` request on the runtime at `root` uses `model`.
#[cfg(test)]
pub(crate) fn install_test_model(root: &std::path::Path, job: Job, model: Arc<dyn OneShot>) {
    let mut models = TEST_MODELS.lock().unwrap_or_else(|p| p.into_inner());
    models.retain(|(r, j, _)| !(r == root && *j == job));
    models.push((root.to_path_buf(), job, model));
}

/// The model `job` asks on `context`: `model` (`None`: not configured) on the relay, bounded by `timeout`, answering at
/// most `max_tokens` tokens.
pub(crate) fn model_for(
    context: &RuntimeContext,
    job: Job,
    model: Option<&str>,
    timeout: Duration,
    max_tokens: u32,
) -> Result<Arc<dyn OneShot>, ModelError> {
    #[cfg(test)]
    if let Some(root) = context.runtime_root() {
        if let Some((_, _, model)) =
            TEST_MODELS.lock().unwrap_or_else(|p| p.into_inner()).iter().find(|(r, j, _)| r == root && *j == job)
        {
            return Ok(model.clone());
        }
    }
    #[cfg(not(test))]
    let _ = job;
    let model = model.ok_or(ModelError("unconfigured"))?;
    Ok(Arc::new(RelayModel::for_model(context, model, timeout, max_tokens)?))
}

/// The relay's OpenAI-compatible endpoint for one model with the harness credential (`ModelRoute::for_model`): one
/// non-streaming request without tools, bounded by `timeout`. An armed upstream capture (route `harness`) takes the
/// request as it is handed to the model client.
pub(crate) struct RelayModel {
    context: RuntimeContext,
    model: String,
    route: crate::harness::ModelRoute,
    timeout: Duration,
    max_tokens: u32,
}

impl RelayModel {
    pub(crate) fn for_model(
        context: &RuntimeContext,
        model: &str,
        timeout: Duration,
        max_tokens: u32,
    ) -> Result<Self, ModelError> {
        let route = crate::harness::ModelRoute::for_model(context, model).map_err(|_| ModelError("unconfigured"))?;
        Ok(Self { context: context.clone(), model: model.to_string(), route, timeout, max_tokens })
    }
}

impl OneShot for RelayModel {
    fn complete(&self, system: &str, input: &str) -> Result<String, ModelError> {
        use genai::chat::{ChatMessage, ChatOptions, ChatRequest, StopReason};
        let mut messages = Vec::with_capacity(2);
        if !system.is_empty() {
            messages.push(ChatMessage::system(system.to_string()));
        }
        messages.push(ChatMessage::user(input.to_string()));
        let request = ChatRequest::new(messages);
        let max_tokens = self.max_tokens;
        // #183: a capture the operator armed takes this request as it is handed to the model client.
        crate::upstream_capture::offer(&self.context, crate::upstream_capture::Route::Harness, || {
            serde_json::json!({
                "model": self.model,
                "maxTokens": max_tokens,
                "request": serde_json::to_value(&request).unwrap_or(serde_json::Value::Null),
            })
        });
        let target = self.route.target();
        let timeout = self.timeout;
        let call = async move {
            let client = genai::Client::new().map_err(|_| ModelError("network"))?;
            let options = ChatOptions::default().with_max_tokens(max_tokens);
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
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| ModelError("network"))?
                .block_on(call)
        })
        .join()
        .unwrap_or(Err(ModelError("network")))
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
        let model = RelayModel::for_model(&context, "fake-chat", Duration::from_secs(5), 321).unwrap();
        assert_eq!(model.complete("", "one-shot-input-11"), Ok("[]".to_string()));
        let body = request.join().unwrap();
        assert_eq!(body["model"], "fake-chat");
        assert_eq!(body["max_tokens"], 321);
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
    fn one_shot_request_is_taken_by_an_armed_capture() {
        let (url, request) = relay("[]");
        let (_root, context) = capture::runtime("one-shot-capture", &url);
        capture::arm(&context, "harness");
        let model = RelayModel::for_model(&context, "fake-chat", Duration::from_secs(5), 321).unwrap();
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
}
