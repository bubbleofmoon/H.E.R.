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

impl Job {
    /// The `job` value of this job's usage facts (`harness::usage`).
    fn usage_job(self) -> &'static str {
        match self {
            Job::Digest => crate::harness::usage::DIGEST,
            Job::SlangLearner => crate::harness::usage::SLANG,
        }
    }
}

/// The route a one-shot request asks: its id (a usage fact's `route`) and its relay model.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteModel<'a> {
    pub id: &'a str,
    pub model: &'a str,
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
    if let Some(root) = context.runtime_root() {
        if let Some((_, _, model)) =
            TEST_MODELS.lock().unwrap_or_else(|p| p.into_inner()).iter().find(|(r, j, _)| r == root && *j == job)
        {
            return Ok(model.clone());
        }
    }
    let route = route.ok_or(ModelError("unconfigured"))?;
    Ok(Arc::new(RelayModel::for_model(context, job, key, route, timeout, max_tokens)?))
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
    /// The ledger key, the route id and the `job` value of each request's usage fact (`harness::usage`).
    usage_key: String,
    usage_route: String,
    usage_job: &'static str,
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
        let relay = crate::harness::ModelRoute::for_model(context, route.model).map_err(|_| ModelError("unconfigured"))?;
        // The request target keeps the model as configured (`ModelRoute::target`); every record and capture keeps it redacted.
        let model = relay.redact(route.model);
        let usage_route = relay.redact(route.id);
        Ok(Self {
            context: context.clone(),
            model,
            route: relay,
            timeout,
            max_tokens,
            usage_key: key.to_string(),
            usage_route,
            usage_job: job.usage_job(),
        })
    }

    /// The usage fact of one answered request (`harness::usage`), with a turn number of its own.
    fn record_usage(&self, reported: &genai::chat::Usage) {
        use crate::harness::usage;
        let place = usage::Place { key: self.usage_key.clone(), turn: usage::next_number(), step: 0, retry: 0 };
        usage::record(&self.context, &place, &self.usage_route, &self.model, self.usage_job, Some(reported));
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
            tokio::time::timeout(timeout, client.exec_chat(genai::ModelSpec::Target(target), request, Some(&options)))
                .await
                .map_err(|_| ModelError("timeout"))?
                .map_err(|error| ModelError(crate::harness::model_failure(&error).class))
        };
        // A fresh thread has no runtime context, so this works from sync jobs and spawn_blocking alike.
        let response = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| ModelError("network"))?
                .block_on(call)
        })
        .join()
        .unwrap_or(Err(ModelError("network")))?;
        // Every answered request is a usage fact, a refused one included: it is recorded before the refusal is decided.
        self.record_usage(&response.usage);
        if matches!(response.stop_reason, Some(StopReason::ContentFilter(_))) {
            return Err(ModelError("refusal"));
        }
        Ok(response.into_first_text().unwrap_or_default())
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
        let model = RelayModel::for_model(&context, Job::Digest, "internal:digest", RouteModel { id: "route-a", model: "fake-chat" }, Duration::from_secs(5), 321).unwrap();
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
        let model = RelayModel::for_model(&context, Job::Digest, "internal:digest", RouteModel { id: "route-a", model: "fake-chat" }, Duration::from_secs(5), 321).unwrap();
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
        let route = RouteModel { id: "route-a", model: "fake-chat" };
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
        let route = RouteModel { id: &route_id, model: &model_name };
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
