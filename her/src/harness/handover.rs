//! A harness turn's history before its first request (#6): the idle rotation of a session whose provider cache ran out
//! (`docs/agents/prompts-modes.md` "Harness idle rotation") and the budget checkpoint of a session whose history no longer
//! fits its route (`budget`). Both write a checkpoint (`Store::checkpoint`): a summary of the older rounds, asked of the
//! digest's model port, and the newest whole rounds kept verbatim (within `HANDOVER_TOKENS` for a rotation, within
//! `budget::recent` for a budget checkpoint). Later admissions replay the newest checkpoint's summary and only the rounds
//! after it; canonical facts are never rewritten. The system instruction is never summarised and the current turn is
//! never covered. A summary is made only of the model-view copy of the facts and is refused when it names a #142
//! denied term the chat it summarises does not name.
use super::store::{History, Round};
use super::{budget, check_bound, filter_model_messages, preview_tool_results, rotation, store, Failure, TurnRequest};
use crate::digest::DigestModel;
use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::RuntimeContext;
use genai::chat::{ChatMessage, ChatRole, ContentPart};
use serde_json::{json, Value};
use std::ops::Range;
use tokio::sync::watch;

/// The handover of an idle rotation keeps the newest whole rounds within this many estimated tokens.
pub(crate) const HANDOVER_TOKENS: u64 = 4_000;
/// The most estimated tokens of chat one summary request carries; older covered rounds are left out of it.
pub(crate) const SUMMARY_INPUT_TOKENS: u64 = 24_000;
/// The longest summary kept, in characters.
pub(crate) const SUMMARY_MAX_CHARS: usize = 4_000;
/// The longest tool result a summary request quotes, in characters.
const TRANSCRIPT_TOOL_CHARS: usize = 2_000;

/// What the summariser is asked (model-visible: `model_text_guard_tests`).
pub(crate) const SUMMARY_PROMPT: &str = "下面是一段较早的聊天记录，可能还附有之前写好的摘要。请把它们整理成一份新的摘要，给自己以后接着聊天时参考：用第一人称，写清楚谁说过哪些要紧的事、做过的约定和答应的事、还没办完的事、各人的称呼和喜好。保留具体的名字、时间、数字和消息编号。只写记录里有的内容，不要编造，不要评论。只输出摘要正文，不超过八百字。";
/// The first line of a replayed summary (model-visible).
pub(crate) const SUMMARY_LABEL: &str = "【之前和AI聊天的摘要】";
/// The heading of the earlier summary in a summary request (model-visible).
pub(crate) const PREVIOUS_LABEL: &str = "【之前的摘要】";
/// The heading of the chat in a summary request (model-visible).
pub(crate) const CHAT_LABEL: &str = "【要并入的聊天记录】";

/// The replayed form of a checkpoint's summary: a user message, so model-view filtering applies to it like any chat.
pub(crate) fn summary_message(summary: &str) -> ChatMessage {
    ChatMessage::user(format!("{SUMMARY_LABEL}\n{summary}"))
}

/// Why a checkpoint is written.
#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    Rotation,
    Budget,
}

impl Purpose {
    fn name(self) -> &'static str {
        match self {
            Self::Rotation => "rotation",
            Self::Budget => "budget",
        }
    }
}

/// The turn's messages and the index ranges of its history rounds, after the session's idle rotation and budget
/// checkpoint. First the idle check: the session's last turn ended at `history.idle_from`, and once
/// `rotation::due_at` for its last answered request (`store::last_call`) has passed, the session rotates. Then the
/// budget check on the turn's start route: when the replayed history and the turn's first request are estimated over
/// `budget::limit`, a budget checkpoint keeps `budget::recent` verbatim. With `harness.summaries` false, or after the
/// rotation's summary failed in this turn, a budget checkpoint keeps the earlier summary and leaves out the older
/// rounds without a new one; a rotation without summaries does the same.
pub(super) async fn prepare(
    context: &RuntimeContext,
    store: &store::Store,
    request: &TurnRequest,
    mut history: History,
    instruction: String,
    cancelled: &mut watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> Result<(Vec<ChatMessage>, Vec<Range<usize>>), Failure> {
    let cfg = context.load_config()?;
    let route = request.route.routes[request.route.start].spec.id.clone();
    let summaries = cfg
        .pointer("/harness/summaries")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let current = current(request, &instruction);
    let mut summarise = summaries;
    if let Some(idle_from) = history.idle_from {
        let due = super::store::last_call(context, &request.conversation, &request.generation)?
            .and_then(|last| rotation::due_at(&cfg, &last, idle_from));
        if due.is_some_and(|due| crate::parity_clock::now_ms() >= due) {
            let summarised = checkpoint(
                context,
                store,
                request,
                &cfg,
                &mut history,
                Purpose::Rotation,
                HANDOVER_TOKENS,
                summaries,
                cancelled,
                deadline,
            )
            .await?;
            summarise &= summarised;
        }
    }
    if size(&history, &current, request) > budget::limit(&cfg, &route) {
        checkpoint(
            context,
            store,
            request,
            &cfg,
            &mut history,
            Purpose::Budget,
            budget::recent(&cfg, &route),
            summarise,
            cancelled,
            deadline,
        )
        .await?;
    }
    Ok(assemble(history, current))
}

/// What the turn's first request holds besides its history: the system instruction (when the turn has one), then the
/// turn's user message.
pub(super) fn current(request: &TurnRequest, instruction: &str) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if !instruction.is_empty() {
        messages.push(ChatMessage::system(instruction.to_string()));
    }
    messages.push(request.user_message());
    messages
}

/// The turn's messages, `current`'s system instruction first and its user message last with the history between, and
/// the index ranges of the history rounds: the summary alone, then each round.
pub(super) fn assemble(history: History, mut current: Vec<ChatMessage>) -> (Vec<ChatMessage>, Vec<Range<usize>>) {
    let user = current.pop();
    let mut messages = current;
    let mut rounds = Vec::new();
    if let Some(summary) = &history.summary {
        rounds.push(messages.len()..messages.len() + 1);
        messages.push(summary_message(summary));
    }
    for round in history.rounds {
        let start = messages.len();
        messages.extend(round.messages);
        rounds.push(start..messages.len());
    }
    messages.extend(user);
    (messages, rounds)
}

/// The estimated tokens of the turn's first request with `history` replayed.
fn size(history: &History, current: &[ChatMessage], request: &TurnRequest) -> u64 {
    let mut messages = history.messages();
    messages.extend(current.iter().cloned());
    budget::estimate(
        &preview_tool_results(messages, request.limits.tool_result_chars),
        request.limits.image_input,
    )
}

/// Where the rounds kept verbatim begin: the newest whole rounds whose estimate together is within `keep`.
fn kept_from(rounds: &[Round], keep: u64, request: &TurnRequest) -> usize {
    let mut used = 0u64;
    let mut split = rounds.len();
    while split > 0 {
        let cost = budget::estimate(
            &preview_tool_results(rounds[split - 1].messages.clone(), request.limits.tool_result_chars),
            request.limits.image_input,
        );
        if used + cost > keep {
            break;
        }
        used += cost;
        split -= 1;
    }
    split
}

/// Writes one checkpoint of the session that keeps the newest rounds within `keep` and covers the older ones, with a new
/// summary when `summarise` is true. Nothing is written when every round is kept. A failed summary is recorded
/// (`checkpoint_failed`): a rotation then does not happen, and a budget checkpoint still covers the older rounds with
/// the earlier summary. Returns false when a summary was asked for and failed.
#[allow(clippy::too_many_arguments)]
async fn checkpoint(
    context: &RuntimeContext,
    store: &store::Store,
    request: &TurnRequest,
    cfg: &Value,
    history: &mut History,
    purpose: Purpose,
    keep: u64,
    summarise: bool,
    cancelled: &mut watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> Result<bool, Failure> {
    let split = kept_from(&history.rounds, keep, request);
    if split == 0 {
        return Ok(true);
    }
    let through = history.rounds[split - 1].through;
    let mut summary = history.summary.clone();
    let mut failed = None;
    if summarise {
        check_bound(request, cancelled, deadline)?;
        let transcript = transcript(context, history.summary.as_deref(), &history.rounds[..split])?;
        match summarise_text(context, cfg, &transcript, cancelled).await? {
            Ok(text) => summary = Some(text),
            Err(class) => failed = Some(class),
        }
    }
    check_bound(request, cancelled, deadline)?;
    if let Some(class) = failed {
        let failure = json!({"purpose": purpose.name(), "class": class});
        if purpose == Purpose::Rotation {
            store.checkpoint(&request.id, Some(failure), None)?;
            record(
                context,
                request,
                "summary-failed",
                Outcome::Failed,
                format!("rotation summary failed ({class}); the session does not rotate"),
            );
            return Ok(false);
        }
        let plain = json!({"purpose": purpose.name(), "through": through, "summary": summary});
        store.checkpoint(&request.id, Some(failure), Some(plain))?;
        record(
            context,
            request,
            "summary-failed",
            Outcome::Failed,
            format!("budget summary failed ({class}); {split} rounds leave without one"),
        );
        history.rounds.drain(..split);
        return Ok(false);
    }
    let made = summarise && summary.is_some();
    let checkpoint = json!({"purpose": purpose.name(), "through": through, "summary": summary});
    store.checkpoint(&request.id, None, Some(checkpoint))?;
    record(
        context,
        request,
        "history-checkpoint",
        Outcome::Ok,
        format!(
            "{} checkpoint covers {split} rounds and keeps {} verbatim; new summary: {made}",
            purpose.name(),
            history.rounds.len() - split
        ),
    );
    history.summary = summary;
    history.rounds.drain(..split);
    Ok(true)
}

/// One run-record entry of the handover (component `model-chain`); `detail` carries no chat text.
fn record(context: &RuntimeContext, request: &TurnRequest, event: &str, outcome: Outcome, detail: String) {
    let reason = format!("{}: {detail}", request.conversation);
    crate::recovery_log::record(context, Trigger::Turn, Component::ModelChain, event, outcome, &reason);
}

/// The summary request's input: the earlier summary, then the covered rounds' model-view copy as labelled lines, newest
/// rounds first to fit `SUMMARY_INPUT_TOKENS` and then put back in order. A single round over the bound keeps its end.
fn transcript(context: &RuntimeContext, previous: Option<&str>, covered: &[Round]) -> Result<String, String> {
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0u64;
    for round in covered.iter().rev() {
        let text = render(&filter_model_messages(context, round.messages.clone())?);
        let cost = budget::text_tokens(&text);
        if used + cost > SUMMARY_INPUT_TOKENS {
            if parts.is_empty() {
                parts.push(tail(&text, SUMMARY_INPUT_TOKENS as usize));
            }
            break;
        }
        used += cost;
        parts.push(text);
    }
    parts.reverse();
    let chat = parts.join("\n");
    Ok(match previous {
        Some(previous) => format!("{PREVIOUS_LABEL}\n{previous}\n\n{CHAT_LABEL}\n{chat}"),
        None => format!("{CHAT_LABEL}\n{chat}"),
    })
}

/// The last `chars` characters of `text` (each counts at most one estimated token).
fn tail(text: &str, chars: usize) -> String {
    let skip = text.chars().count().saturating_sub(chars);
    text.chars().skip(skip).collect()
}

/// The first `chars` characters of `text`.
fn clip(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// Facts as labelled lines (model-visible labels: `model_text_guard_tests`). Reasoning and signatures are left out.
fn render(messages: &[ChatMessage]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        for part in message.content.iter() {
            let line = match (&message.role, part) {
                (ChatRole::User, ContentPart::Text(text)) => format!("〔对方〕{text}"),
                (ChatRole::Assistant, ContentPart::Text(text)) => format!("〔我〕{text}"),
                (_, ContentPart::ToolCall(call)) => budget::call_text(call),
                (_, ContentPart::ToolResponse(response)) => budget::result_text(
                    response.fn_name.as_deref().unwrap_or("工具"),
                    &clip(&response.content, TRANSCRIPT_TOOL_CHARS),
                ),
                (_, ContentPart::Binary(_)) => "〔图片〕".to_string(),
                _ => continue,
            };
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// One summary of `transcript` from the digest's model port (`digest::model_for`: `memory.digest.model`, else the
/// chain's first route), bounded by `memory.digest.timeoutSecs`. The outer error ends the turn (cancellation); the
/// inner one is the summary's content-free failure class.
async fn summarise_text(
    context: &RuntimeContext,
    cfg: &Value,
    transcript: &str,
    cancelled: &mut watch::Receiver<bool>,
) -> Result<Result<String, &'static str>, Failure> {
    let model = match crate::digest::model_for(context, cfg) {
        Ok(model) => model,
        Err(error) => return Ok(Err(error.0)),
    };
    let input = transcript.to_string();
    // A cancelled turn returns at once; the blocking request ends on its own bound and its answer is dropped.
    let task = tokio::task::spawn_blocking(move || model.complete(SUMMARY_PROMPT, &input));
    let joined = tokio::select! { biased;
        _ = cancelled.changed() => return Err(Failure::Cancelled),
        joined = task => joined,
    };
    Ok(match joined {
        Ok(Ok(text)) => accept(transcript, &text),
        Ok(Err(error)) => Err(error.0),
        Err(_) => Err("worker"),
    })
}

/// A summary the history may carry: trimmed and not empty (`empty-response`), naming no #142 denied term
/// (`upstream_capture::denied_terms`) that `transcript` does not name (`wording`), cut to `SUMMARY_MAX_CHARS`.
pub(crate) fn accept(transcript: &str, summary: &str) -> Result<String, &'static str> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err("empty-response");
    }
    let known = crate::upstream_capture::denied_terms(transcript);
    if crate::upstream_capture::denied_terms(summary)
        .iter()
        .any(|term| known.contains(term) && !known.contains(term))
    {
        return Err("wording");
    }
    Ok(summary.chars().take(SUMMARY_MAX_CHARS).collect())
}

/// Every model-visible text of the handover, for the #142 guard (`model_text_guard_tests`).
#[cfg(test)]
pub(crate) fn label_sample() -> String {
    let call = genai::chat::ToolCall {
        call_id: "sample-call".into(),
        fn_name: "web_search".into(),
        fn_arguments: json!({"query": "周六天气"}),
        thought_signatures: None,
    };
    let mut shot = super::tool_message(&call, &json!({"content":[{"type":"text","text":"晴"}]}));
    shot.content.push(ContentPart::Binary(genai::chat::Binary::from_base64("image/png", "iVBORw0KGgo=", None)));
    let messages = vec![
        ChatMessage::user("周六去爬山".to_string()),
        ChatMessage::assistant(vec![ContentPart::Text("好".into()), ContentPart::ToolCall(call.clone())]),
        shot,
    ];
    let replayed = summary_message("摘要").content.first_text().unwrap_or_default().to_string();
    [
        SUMMARY_PROMPT.to_string(),
        replayed,
        PREVIOUS_LABEL.to_string(),
        CHAT_LABEL.to_string(),
        render(&messages),
        budget::call_text(&call),
        budget::result_text("web_search", "晴"),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream_capture::denied_terms;

    #[test]
    fn harness_summary_accepts_only_wording_its_chat_already_had() {
        assert_eq!(accept("〔对方〕周六爬山", "  周六爬山。 "), Ok("周六爬山。".to_string()));
        assert_eq!(accept("〔对方〕周六爬山", "作为AI，我记得周六爬山"), Err("wording"), "new AI wording");
        assert_eq!(accept("〔对方〕周六", "这个模型记得周六"), Err("wording"), "a new model word");
        assert_eq!(accept("〔对方〕周六", "harness 记得周六"), Err("wording"), "a harness name");
        assert_eq!(
            accept("〔对方〕你是AI吗", "对方问我是不是AI"),
            Ok("对方问我是不是AI".to_string()),
            "a term the chat itself named"
        );
        assert_eq!(accept("x", " \n "), Err("empty-response"));
        let long = "长".repeat(SUMMARY_MAX_CHARS + 1);
        assert_eq!(accept("x", &long).map(|kept| kept.chars().count()), Ok(SUMMARY_MAX_CHARS));
    }

    #[test]
    fn harness_summary_labels_name_no_harness_model_or_ai() {
        let sample = label_sample();
        assert!(denied_terms(&sample).is_empty(), "{:?}\n{sample}", denied_terms(&sample));
        assert!(sample.contains("〔我用了 web_search〕") && sample.contains("〔图片〕"), "{sample}");
    }
}
