//! One digest run over one conversation's new evidence.
use super::{evidence, model::DigestModel};
use crate::memory::{MemoryStore, NewMemory};
use crate::runtime::RuntimeContext;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

/// The one digest prompt (system message); the conversation evidence follows as the user message.
pub(crate) const PROMPT: &str = "你在整理自己对这个会话里的人和事的记忆。下面是这个会话自上次整理以来的新消息（#序号 发言人：内容；“我”是你自己说过的话）。用你自己的判断，挑出以后和他们相处还会用得上的东西：这个人是谁、他在意什么、约定和承诺、发生过的事。你写下的都是你的推断，不是原话。只输出一个 JSON 数组，每项 {\"subject\":\"人名或话题名\",\"content\":\"一句话，200 字以内\"}，最多 8 项；没有值得记的就输出 []。不要输出其他内容。";

const MAX_ITEMS: usize = 8;
const MAX_CONTENT_CHARS: usize = 200;
const MAX_SUBJECT_CHARS: usize = 40;

#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    /// Nothing new (or not enough, or digested too recently, or another run was first).
    Quiet,
    Written { through: i64, memories: usize },
}

/// Fixed vocabulary only; never model or storage text.
#[derive(Debug, PartialEq)]
pub(crate) enum DigestError {
    Evidence,
    Model(&'static str),
    Output(&'static str),
    Storage,
    Cancelled,
}

impl DigestError {
    pub(crate) fn step(&self) -> &'static str {
        match self { Self::Evidence => "evidence", Self::Model(_) => "model", Self::Output(_) => "digest-result",
            Self::Storage => "memory-write", Self::Cancelled => "cancel" }
    }
    pub(crate) fn kind(&self) -> &'static str {
        match self { Self::Evidence | Self::Storage => "storage", Self::Model(kind) | Self::Output(kind) => *kind, Self::Cancelled => "cancelled" }
    }
}

pub(crate) struct Limits {
    pub min_messages: usize,
    pub min_interval_ms: i64,
}

pub(crate) struct Digest<'a> {
    context: &'a RuntimeContext,
    cfg: &'a Value,
    model: &'a dyn DigestModel,
}

impl<'a> Digest<'a> {
    pub(crate) fn new(context: &'a RuntimeContext, cfg: &'a Value, model: &'a dyn DigestModel) -> Self {
        Self { context, cfg, model }
    }

    pub(crate) fn conversations(&self) -> Result<Vec<String>, DigestError> {
        evidence::conversations(self.context).map_err(|_| DigestError::Evidence)
    }

    /// Digest `key`'s evidence after its cursor. Failure writes nothing and leaves the cursor.
    pub(crate) fn run_conversation(&self, key: &str, limits: &Limits, cancel: &AtomicBool) -> Result<Outcome, DigestError> {
        if cancel.load(Ordering::SeqCst) { return Err(DigestError::Cancelled); }
        let store = MemoryStore::open(self.context, self.cfg).map_err(|_| DigestError::Storage)?;
        let reader = store.for_reader(key);
        let space = evidence::space(self.context);
        let stored = reader.digest_cursor(space).map_err(|_| DigestError::Storage)?;
        if let Some((_, at)) = stored {
            if limits.min_interval_ms > 0 && crate::parity_clock::now_ms() - at < limits.min_interval_ms {
                return Ok(Outcome::Quiet);
            }
        }
        let Some(batch) = evidence::read(self.context, self.cfg, key, space, stored, max_input_chars(self.cfg))
            .map_err(|_| DigestError::Evidence)? else { return Ok(Outcome::Quiet) };
        if batch.messages < limits.min_messages.max(1) { return Ok(Outcome::Quiet); }
        if cancel.load(Ordering::SeqCst) { return Err(DigestError::Cancelled); }
        let answer = self.model.complete(PROMPT, &batch.input).map_err(|error| DigestError::Model(error.0))?;
        let items = parse_items(&answer)?;
        if cancel.load(Ordering::SeqCst) { return Err(DigestError::Cancelled); }
        let source = format!("digest:{key}:{space}:{}-{}", batch.first, batch.through);
        let memories: Vec<NewMemory<'_>> = items.iter().map(|(subject, content)| NewMemory {
            owner_type: "session", scope: None, kind: "inference", subject: subject.as_deref(), content,
            tags: None, source: &source, importance: 0.5,
        }).collect();
        match reader.write_digest(space, stored.map(|(through, _)| through), batch.through, &memories).map_err(|_| DigestError::Storage)? {
            Some(written) => Ok(Outcome::Written { through: batch.through, memories: written.len() }),
            None => Ok(Outcome::Quiet),
        }
    }
}

fn max_input_chars(cfg: &Value) -> usize {
    cfg.pointer("/memory/digest/maxInputChars").and_then(Value::as_u64).unwrap_or(12_000).clamp(1_000, 100_000) as usize
}

/// The model's items; its `source`, `kind` or any other field is ignored.
fn parse_items(answer: &str) -> Result<Vec<(Option<String>, String)>, DigestError> {
    let text = answer.trim();
    if text.is_empty() { return Err(DigestError::Output("empty")); }
    if crate::model_failure::Failure::is_refusal(text) || crate::backend::plain_output_failure(text).is_some()
        || text.to_ascii_lowercase().starts_with("model/backend ") {
        return Err(DigestError::Output("refusal-or-error"));
    }
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else { return Err(DigestError::Output("unparseable")) };
    if end < start { return Err(DigestError::Output("unparseable")); }
    let Ok(Value::Array(values)) = serde_json::from_str::<Value>(&text[start..=end]) else { return Err(DigestError::Output("unparseable")) };
    Ok(values.iter().filter_map(|value| {
        let content: String = value.get("content")?.as_str()?.trim().chars().take(MAX_CONTENT_CHARS).collect();
        if content.is_empty() { return None; }
        let subject = value.get("subject").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
            .map(|s| s.chars().take(MAX_SUBJECT_CHARS).collect());
        Some((subject, content))
    }).take(MAX_ITEMS).collect())
}
