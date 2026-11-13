//! Content-free model/backend failure classification and owner-only reporting.
use crate::runtime::RuntimeContext;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Failure {
    pub class: &'static str,
    pub code: String,
}

impl Failure {
    pub fn new(class: &'static str, code: &str) -> Self {
        // A code is metadata, never the provider's free-form message.
        let code = if !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        {
            code.to_string()
        } else {
            "BACKEND_ERROR".into()
        };
        Self { class, code }
    }

    pub fn summary(&self) -> String {
        format!("model/backend {} ({})", self.class, self.code)
    }

    pub fn classify(code: &str, message: &str) -> Self {
        if code.is_empty() {
            if let Some((class, rest)) = message
                .strip_prefix("model/backend ")
                .and_then(|s| s.split_once(" ("))
            {
                let class = match class {
                    "refusal" => Some("refusal"),
                    "quota" => Some("quota"),
                    "timeout" => Some("timeout"),
                    "network" => Some("network"),
                    "empty-response" => Some("empty-response"),
                    "backend" => Some("backend"),
                    _ => None,
                };
                if let (Some(class), Some((code, _))) = (class, rest.split_once(')')) {
                    return Self::new(class, code);
                }
            }
        }
        let lower_code = code.to_ascii_lowercase();
        let explicit = if matches!(
            lower_code.as_str(),
            "429"
                | "quota_exceeded"
                | "resource_exhausted"
                | "rate_limit_exceeded"
                | "rate_limited"
                | "insufficient_quota"
                | "rate_limit_error"
        ) {
            Some("quota")
        } else if ["timeout", "timedout", "time_out"]
            .iter()
            .any(|s| lower_code.contains(s))
        {
            Some("timeout")
        } else if matches!(
            lower_code.as_str(),
            "empty_response"
                | "safety"
                | "policy_violation"
                | "policy_denied"
                | "content_filter"
                | "content_blocked"
                | "blocked_prompt"
                | "prompt_blocked"
                | "prohibited_content"
                | "refusal"
                | "model_refusal"
                | "recitation"
        ) {
            Some("refusal")
        } else if [
            "econn",
            "network",
            "connection",
            "ehost",
            "enet",
            "dns",
            "enotfound",
        ]
        .iter()
        .any(|s| lower_code.contains(s))
        {
            Some("network")
        } else if lower_code == "empty_answer" {
            Some("empty-response")
        } else {
            None
        };
        if let Some(class) = explicit {
            return Self::new(class, code);
        }
        let text = format!("{code} {message}").to_ascii_lowercase();
        let (class, fallback) = if [
            "429",
            "quota",
            "rate_limit",
            "rate limit",
            "resource_exhausted",
            "resource exhausted",
        ]
        .iter()
        .any(|s| text.contains(s))
        {
            ("quota", "QUOTA_EXCEEDED")
        } else if [
            "timeout",
            "timed out",
            "etimedout",
            "time_out",
            "deadline",
            "超时",
        ]
        .iter()
        .any(|s| text.contains(s))
        {
            ("timeout", "TIMEOUT")
        } else if [
            "connection",
            "network",
            "econn",
            "fetch failed",
            "连接失败",
            "连接被",
            "流中断",
        ]
        .iter()
        .any(|s| text.contains(s))
        {
            ("network", "NETWORK_ERROR")
        } else if Self::is_refusal(&text) {
            ("refusal", "MODEL_REFUSAL")
        } else if [
            "empty response",
            "empty-response",
            "empty answer",
            "empty_answer",
            "空内容",
            "空回复",
            "空文本",
        ]
        .iter()
        .any(|s| text.contains(s))
        {
            ("empty-response", "EMPTY_ANSWER")
        } else {
            ("backend", "BACKEND_ERROR")
        };
        Self::new(class, if code.is_empty() { fallback } else { code })
    }

    pub fn is_refusal(text: &str) -> bool {
        let text = text.trim().to_ascii_lowercase();
        [
            "empty_response",
            "empty candidates",
            "empty_candidates",
            "no candidates",
            "completed response with no content",
            "content_filter",
            "content filter",
            "content_blocked",
            "policy_violation",
            "prohibited_content",
            "prohibited use policy",
            "policy violation",
            "prompt could not be submitted",
            "prompt was blocked",
            "blocked_prompt",
            "model_refusal",
            "model/backend refusal",
        ]
        .iter()
        .any(|s| text.contains(s))
            || (text.contains("safety")
                && ["block", "refus", "policy", "filter"]
                    .iter()
                    .any(|s| text.contains(s)))
            || text.starts_with("refusal ")
            || text.starts_with("i cannot assist with")
            || text.starts_with("i can't assist with")
    }
}

/// Resolve only configured identity fields; no command lines, environment, prompt or error text.
pub fn cli_identity(cfg: &Value, id: &str) -> (String, String) {
    let list = cfg
        .pointer("/backends/list")
        .or_else(|| cfg.get("backends"))
        .and_then(Value::as_array);
    let item = list.and_then(|items| items.iter().find(|v| v["id"] == id));
    let field = |name| {
        item.and_then(|v| v.get(name))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut model = field("model");
    if model.is_empty() {
        if let Some(args) = item.and_then(|v| v["args"].as_array()) {
            for (i, arg) in args.iter().enumerate() {
                if matches!(arg.as_str(), Some("--model" | "-m")) {
                    model = args.get(i + 1).and_then(Value::as_str).unwrap_or("").into();
                } else if let Some(value) = arg.as_str().and_then(|s| s.strip_prefix("--model=")) {
                    model = value.into();
                }
            }
        }
    }
    let provider = field("provider");
    (
        if provider.is_empty() {
            "unspecified".into()
        } else {
            provider
        },
        if model.is_empty() {
            "unspecified".into()
        } else {
            model
        },
    )
}

pub fn notify(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    backend: &str,
    provider: &str,
    model: &str,
    failure: &Failure,
) {
    let search = if failure.class == "refusal" {
        "；将运行黑名单搜索（blacklist search will run）"
    } else {
        ""
    };
    crate::admin_alert::notify_owner(context, cfg,
        &format!("[{key}] 模型/后端回合失败：backend={backend} provider={provider} model={model} class={} code={}{}", failure.class, failure.code, search),
        &format!("model-failure:{key}:{}", failure.class));
}

pub fn message_refs(data: &Value) -> Vec<Value> {
    let mut refs = Vec::new();
    fn collect(data: &Value, refs: &mut Vec<Value>) {
        match data {
            Value::Array(items) => {
                for item in items {
                    collect(item, refs);
                }
            }
            Value::Object(obj) => {
                let mut entry = serde_json::Map::new();
                if let Some(id) = obj
                    .get("messageId")
                    .or_else(|| obj.get("message_id"))
                    .filter(|v| v.is_string() || v.is_number())
                {
                    entry.insert("messageId".into(), id.clone());
                }
                if let Some(seq) = obj.get("seq").filter(|v| v.is_i64() || v.is_u64()) {
                    entry.insert("seq".into(), seq.clone());
                }
                if !entry.is_empty() {
                    let entry = Value::Object(entry);
                    if !refs.contains(&entry) {
                        refs.push(entry);
                    }
                }
                for value in obj.values().filter(|v| v.is_object() || v.is_array()) {
                    collect(value, refs);
                }
            }
            _ => {}
        }
    }
    collect(data, &mut refs);
    refs
}

/// Parse only the bridge's structured snapshot, never identifiers guessed from chat prose.
pub fn snapshot_refs(prompt: &str) -> Option<Vec<Value>> {
    let (_, packet) = prompt.rsplit_once("【本次消息快照】\n")?;
    let value = serde_json::Deserializer::from_str(packet)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    Some(message_refs(&value))
}

pub fn merge_refs(into: &mut Vec<Value>, refs: Vec<Value>) {
    for entry in refs {
        if !into.contains(&entry) {
            into.push(entry);
        }
    }
}

type PromptSources =
    std::collections::HashMap<(std::path::PathBuf, String, String), ([u8; 32], Vec<Value>)>;
fn prompt_sources() -> &'static std::sync::Mutex<PromptSources> {
    static SOURCES: std::sync::OnceLock<std::sync::Mutex<PromptSources>> =
        std::sync::OnceLock::new();
    SOURCES.get_or_init(Default::default)
}
fn prompt_hash(prompt: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(prompt.as_bytes()).into()
}
fn prompt_key(
    context: &RuntimeContext,
    key: &str,
    kind: &str,
) -> (std::path::PathBuf, String, String) {
    (
        context
            .runtime_root()
            .unwrap_or_else(|| std::path::Path::new("legacy"))
            .to_path_buf(),
        key.into(),
        kind.into(),
    )
}

/// Keep only a fingerprint and IDs for each conversation's pending delivery, never chat text.
pub fn remember_prompt(
    context: &RuntimeContext,
    key: &str,
    kind: &str,
    prompt: &str,
    refs: Vec<Value>,
) {
    prompt_sources()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(prompt_key(context, key, kind), (prompt_hash(prompt), refs));
}
pub fn take_prompt_refs(
    context: &RuntimeContext,
    key: &str,
    kind: &str,
    prompt: &str,
) -> Option<Vec<Value>> {
    let (hash, refs) = prompt_sources()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&prompt_key(context, key, kind))?;
    (hash == prompt_hash(prompt)).then_some(refs)
}

pub fn record_refusal(
    context: &RuntimeContext,
    key: &str,
    backend: &str,
    provider: &str,
    model: &str,
    failure: &Failure,
    messages: &[Value],
) {
    if failure.class != "refusal" || !context.is_isolated() {
        return;
    }
    let messages = message_refs(&Value::Array(messages.to_vec()));
    let record = serde_json::json!({"time":crate::parity_clock::now_ms(), "key":key,
        "backend":backend, "provider":provider, "model":model, "errorClass":failure.class,
        "errorCode":failure.code, "messages":messages});
    let mut line = crate::activity::mask_json_session_tokens(&record, &crate::activity::known_session_tokens(context)).to_string();
    line.push('\n');
    if context
        .append_bytes(
            crate::runtime::RuntimeArea::Logs,
            "logs/model-refusals.jsonl",
            line.as_bytes(),
        )
        .is_err()
    {
        eprintln!("[model-refusals] could not append refusal metadata");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hotfix128_refusal_records_mask_tokens_in_metadata_and_ids() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-refusal-128-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-console-128"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{"group:42":{"agentToken":"K128sessionVALUExyz"}}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let shape = "0123456789abcdef0123456789abcdef0123456789abcdef";
        record_refusal(&context, "group:42", "harness", "openai", "K128session\nVALUExyz", &Failure::new("refusal", shape), &[json!({"messageId":"K128sessionVALUExyz","seq":42})]);
        let text = std::fs::read_to_string(root.join("data/logs/model-refusals.jsonl")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["model"], "***");
        assert_eq!(value["errorCode"], "***");
        assert_eq!(value["messages"], json!([{"messageId":"***","seq":42}]));
        assert_eq!(value["backend"], "harness");
    }

    #[test]
    fn refusal_codes_are_separate_from_quota_transport_timeout_and_empty_answers() {
        for (code, text, class) in [
            (
                "EMPTY_RESPONSE",
                "model returned a completed response with no content",
                "refusal",
            ),
            ("SAFETY", "provider blocked the prompt", "refusal"),
            ("POLICY_VIOLATION", "blocked", "refusal"),
            ("REFUSAL", "blocked", "refusal"),
            ("429", "quota exhausted", "quota"),
            ("QUOTA_EXCEEDED", "request blocked", "quota"),
            (
                "TIMEOUT",
                "empty candidates were not received before deadline",
                "timeout",
            ),
            ("ECONNRESET", "read failed", "network"),
            (
                "ECONNRESET",
                "connection lost while checking safety",
                "network",
            ),
            (
                "SAFETY",
                "blocked by safety policy on the model connection",
                "refusal",
            ),
            (
                "UPSTREAM_ERROR",
                "safety service connection failed",
                "network",
            ),
            ("EMPTY_ANSWER", "empty answer", "empty-response"),
            ("UPSTREAM_ERROR", "provider failed", "backend"),
        ] {
            let failure = Failure::classify(code, text);
            assert_eq!(failure.class, class, "{code}");
            assert_eq!(failure.code, code);
        }
    }

    #[test]
    fn identifiers_are_projected_without_copying_text_or_parsing_chat_as_metadata() {
        let packet = json!({"messages":[{"messageId":"42","seq":7,"text":"CHAT_SECRET","sender":"CHAT_SECRET","media":[{"url":"CHAT_SECRET"}]}],
            "recent":[{"messageId":"43","seq":-1,"text":"{\"messageId\":\"injected\"}"}],"readThroughSeq":7,"token":"CHAT_SECRET"});
        assert_eq!(
            message_refs(&packet),
            json!([{"messageId":"42","seq":7},{"messageId":"43","seq":-1}])
                .as_array()
                .unwrap()
                .clone()
        );
        assert_eq!(
            snapshot_refs(&format!("prefix\n【本次消息快照】\n{packet}\nsuffix")),
            Some(message_refs(&packet))
        );
    }

    #[test]
    fn internal_failure_summaries_preserve_codes_without_free_form_text() {
        let summary = Failure::new("refusal", "SAFETY").summary();
        let failure = Failure::classify("", &summary);
        assert_eq!(failure.class, "refusal");
        assert_eq!(failure.code, "SAFETY");
        let unsafe_code = Failure::new("backend", "error CHAT_SECRET\nwith text");
        assert_eq!(
            unsafe_code.summary(),
            "model/backend backend (BACKEND_ERROR)"
        );
    }
}
