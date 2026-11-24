//! The history budget of a harness request (#6): what a request is estimated to cost in tokens (`estimate`), the share
//! of its route's context window it may fill (`limit`) and the oldest whole rounds left out so it fits (`fit`). A round
//! is one completed turn's replayed facts (or the replayed summary), so a tool call always leaves together with its
//! result; the system instruction and the current turn (the owner's latest message and every call and result of the
//! turn) are never left out. `fit` also previews oversized tool results (#175) and turns a replayed call to a tool the
//! request does not offer, with its result, into plain text (#160).
use super::{model_messages, preview_tool_results, Limits};
use genai::chat::{ChatMessage, ChatRole, ContentPart, Tool, ToolCall};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// A route's context window, in estimated tokens, when `harness.contextTokens.<route id>` sets none.
pub(crate) const DEFAULT_CONTEXT_TOKENS: u64 = 64_000;
/// The smallest and largest window `harness.contextTokens` accepts; another value reads as the default.
pub(crate) const CONTEXT_TOKENS_MIN: u64 = 1_000;
pub(crate) const CONTEXT_TOKENS_MAX: u64 = 10_000_000;
/// The share of the window one request may fill when `harness.historyShare` sets none.
pub(crate) const DEFAULT_HISTORY_SHARE: f64 = 0.82;
/// The smallest and largest share `harness.historyShare` accepts; another value reads as the default.
pub(crate) const HISTORY_SHARE_MIN: f64 = 0.1;
pub(crate) const HISTORY_SHARE_MAX: f64 = 0.95;
/// The share of the window, in thousandths, a budget checkpoint keeps verbatim (the newest whole rounds).
pub(crate) const RECENT_PERMILLE: u64 = 150;
/// What one image part counts, in estimated tokens.
pub(crate) const IMAGE_TOKENS: u64 = 1_500;
/// What each message adds besides its parts (its role and framing).
pub(crate) const MESSAGE_TOKENS: u64 = 4;

/// The context window of the route `route` names: `harness.contextTokens.<route id>`, an integer from
/// `CONTEXT_TOKENS_MIN` to `CONTEXT_TOKENS_MAX`, else `DEFAULT_CONTEXT_TOKENS`.
pub(crate) fn window(cfg: &Value, route: &str) -> u64 {
    crate::settings::HARNESS_CONTEXT_TOKENS
        .read(cfg)
        .get(route)
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_CONTEXT_TOKENS)
}

/// The share of the window one request may fill: `harness.historyShare`, a number from `HISTORY_SHARE_MIN` to
/// `HISTORY_SHARE_MAX`, else `DEFAULT_HISTORY_SHARE`.
pub(crate) fn share(cfg: &Value) -> f64 {
    crate::settings::HARNESS_HISTORY_SHARE
        .read(cfg)
        .as_f64()
        .unwrap_or(DEFAULT_HISTORY_SHARE)
}

/// The most estimated tokens one request on route `route` may deliver: its window times the share, the share rounded
/// to thousandths, in whole tokens rounded down.
pub(crate) fn limit(cfg: &Value, route: &str) -> u64 {
    window(cfg, route) * (share(cfg) * 1_000.0).round() as u64 / 1_000
}

/// The estimated tokens a budget checkpoint keeps verbatim on route `route`: `RECENT_PERMILLE` of its window.
pub(crate) fn recent(cfg: &Value, route: &str) -> u64 {
    window(cfg, route) * RECENT_PERMILLE / 1_000
}

/// The estimated tokens of `text`: four ASCII characters count one, rounded up, and every other character counts one.
pub(crate) fn text_tokens(text: &str) -> u64 {
    let ascii = text.bytes().filter(u8::is_ascii).count() as u64;
    let other = text.chars().filter(|c| !c.is_ascii()).count() as u64;
    ascii.div_ceil(4) + other
}

/// The estimated tokens `messages` cost as a request delivers them (`model_messages`): `MESSAGE_TOKENS` a message and
/// the tokens of its parts, `IMAGE_TOKENS` for every image, inbound or from a tool, live or replayed. On a route
/// without image input each image counts as the text that replaces it.
pub(crate) fn estimate(messages: &[ChatMessage], image_input: bool) -> u64 {
    model_messages(messages.to_vec(), image_input)
        .iter()
        .map(|message| MESSAGE_TOKENS + message.content.iter().map(part_tokens).sum::<u64>())
        .sum()
}

fn part_tokens(part: &ContentPart) -> u64 {
    match part {
        ContentPart::Text(text) | ContentPart::ReasoningContent(text) => text_tokens(text),
        ContentPart::Binary(_) => IMAGE_TOKENS,
        ContentPart::ToolCall(call) => {
            text_tokens(&call.call_id) + text_tokens(&call.fn_name) + text_tokens(&call.fn_arguments.to_string())
        }
        ContentPart::ToolResponse(response) => text_tokens(&response.call_id) + text_tokens(&response.content),
        ContentPart::ThoughtSignature(_) => 0,
        ContentPart::Custom(custom) => text_tokens(&custom.data.to_string()),
    }
}

/// One request's copies of the turn's messages. `filtered` is the model-view copy (`filter_model_messages`) and
/// `rounds` the index ranges of its history rounds, oldest first, contiguous and all before the current turn. The
/// delivered copy previews oversized tool results, turns the history's calls to a tool `offered` does not hold into
/// text (a call of the current turn stays a call: the turn captured its menu) and leaves out the oldest whole rounds
/// until it is estimated within `limit`. Returns the filtered messages the request kept (whole, for its message
/// references) and the delivered copy; both have the same messages.
pub(super) fn fit(
    filtered: Vec<ChatMessage>,
    rounds: &[Range<usize>],
    offered: &[Tool],
    limits: Limits,
    limit: u64,
) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    let names: HashSet<&str> = offered.iter().map(|tool| tool.name.as_str()).collect();
    let mut delivered = preview_tool_results(filtered.clone(), limits.tool_result_chars);
    if let (Some(first), Some(last)) = (rounds.first(), rounds.last()) {
        let current = delivered.split_off(last.end);
        let history = delivered.split_off(first.start);
        delivered.extend(off_menu_as_text(history, &names));
        delivered.extend(current);
    }
    let mut kept = filtered;
    let dropped = dropped_rounds(&delivered, rounds, limits.image_input, limit);
    if dropped > 0 {
        let cut = rounds[0].start..rounds[dropped - 1].end;
        kept.drain(cut.clone());
        delivered.drain(cut);
    }
    (kept, delivered)
}

/// How many of the oldest `rounds` of `messages` leave so the rest is estimated within `limit`. Only history rounds
/// ever leave: when the system instruction and the current turn alone are over the limit, every round leaves and the
/// request still goes.
pub(crate) fn dropped_rounds(messages: &[ChatMessage], rounds: &[Range<usize>], image_input: bool, limit: u64) -> usize {
    let (Some(first), Some(last)) = (rounds.first(), rounds.last()) else {
        return 0;
    };
    let costs: Vec<u64> = rounds.iter().map(|round| estimate(&messages[round.clone()], image_input)).collect();
    let fixed = estimate(&messages[..first.start], image_input) + estimate(&messages[last.end..], image_input);
    let mut history: u64 = costs.iter().sum();
    let mut dropped = 0;
    while dropped < costs.len() && fixed + history > limit {
        history -= costs[dropped];
        dropped += 1;
    }
    dropped
}

/// The text a call leaves in the history when it cannot stay a call (model-visible: `model_text_guard_tests`).
pub(crate) fn call_text(call: &ToolCall) -> String {
    format!("〔我用了 {}〕{}", call.fn_name, call.fn_arguments)
}

/// The text a tool result leaves in the history when its call became text (model-visible: `model_text_guard_tests`).
pub(crate) fn result_text(name: &str, content: &str) -> String {
    format!("〔{name} 的结果〕{content}")
}

/// #160: an assistant message with a call to a tool `offered` does not hold has every call turned into text
/// (`call_text`), and each tool message answering those calls becomes a user message with the result as text
/// (`result_text`) and its images kept, so the provider never sees a call to a tool it was not offered and the facts
/// stay. Every other message passes unchanged; the count and order of messages never change.
pub(crate) fn off_menu_as_text(messages: Vec<ChatMessage>, offered: &HashSet<&str>) -> Vec<ChatMessage> {
    let mut rewritten: HashMap<String, String> = HashMap::new();
    let mut result = Vec::with_capacity(messages.len());
    for mut message in messages {
        let off_menu = message.role == ChatRole::Assistant
            && message
                .content
                .tool_calls()
                .iter()
                .any(|call| !offered.contains(call.fn_name.as_str()));
        let answers = message.role == ChatRole::Tool
            && message
                .content
                .tool_responses()
                .iter()
                .any(|response| rewritten.contains_key(&response.call_id));
        if off_menu {
            let mut parts = Vec::new();
            for part in message.content.into_parts() {
                parts.push(match part {
                    ContentPart::ToolCall(call) => {
                        let text = call_text(&call);
                        rewritten.insert(call.call_id, call.fn_name);
                        ContentPart::Text(text)
                    }
                    other => other,
                });
            }
            message.content = parts.into();
        } else if answers {
            let mut parts = Vec::new();
            for part in message.content.into_parts() {
                parts.push(match part {
                    ContentPart::ToolResponse(response) => {
                        let name = rewritten.get(&response.call_id).map_or("", String::as_str);
                        ContentPart::Text(result_text(name, &response.content))
                    }
                    other => other,
                });
            }
            message = ChatMessage::user(parts);
        }
        result.push(message);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::Binary;
    use serde_json::json;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            fn_name: name.into(),
            fn_arguments: json!({"q": id}),
            thought_signatures: None,
        }
    }

    fn result(call: &ToolCall, text: &str) -> ChatMessage {
        super::super::tool_message(call, &json!({"content":[{"type":"text","text":text}]}))
    }

    fn image() -> ContentPart {
        ContentPart::Binary(Binary::from_base64("image/png", "iVBORw0KGgo=", None))
    }

    #[test]
    fn harness_history_budget_limit_is_the_routes_window_times_the_share() {
        assert_eq!(limit(&json!({}), "r"), 52_480, "64000 × 0.82");
        let cfg = |window: Value, share: Value| json!({"harness":{"contextTokens":{"r":window},"historyShare":share}});
        assert_eq!(limit(&cfg(json!(10_000), json!(0.5)), "r"), 5_000, "the route's own window and the share");
        assert_eq!(limit(&cfg(json!(10_000), json!(0.5)), "other"), 32_000, "another route has the default window");
        assert_eq!(recent(&cfg(json!(10_000), json!(null)), "r"), 1_500, "a checkpoint keeps 15% verbatim");
        assert_eq!(window(&cfg(json!(1_000), json!(null)), "r"), 1_000, "the smallest window");
        assert_eq!(window(&cfg(json!(999), json!(null)), "r"), DEFAULT_CONTEXT_TOKENS, "below the smallest");
        assert_eq!(window(&cfg(json!(10_000_000), json!(null)), "r"), 10_000_000, "the largest window");
        assert_eq!(window(&cfg(json!(10_000_001), json!(null)), "r"), DEFAULT_CONTEXT_TOKENS, "above the largest");
        for bad in [json!("8000"), json!(-1), json!(1.5e3), json!(null), json!(true)] {
            assert_eq!(window(&cfg(bad.clone(), json!(null)), "r"), DEFAULT_CONTEXT_TOKENS, "{bad}");
        }
        assert_eq!(share(&cfg(json!(null), json!(0.1))), 0.1, "the smallest share");
        assert_eq!(share(&cfg(json!(null), json!(0.0999))), DEFAULT_HISTORY_SHARE, "below the smallest");
        assert_eq!(share(&cfg(json!(null), json!(0.95))), 0.95, "the largest share");
        assert_eq!(share(&cfg(json!(null), json!(0.9501))), DEFAULT_HISTORY_SHARE, "above the largest");
        assert_eq!(share(&cfg(json!(null), json!("0.5"))), DEFAULT_HISTORY_SHARE);
    }

    #[test]
    fn harness_history_budget_estimate_counts_text_and_every_image() {
        assert_eq!(text_tokens(""), 0);
        assert_eq!(text_tokens("abcd"), 1);
        assert_eq!(text_tokens("abcde"), 2, "ASCII rounds up");
        assert_eq!(text_tokens("你好"), 2, "every other character counts one");
        assert_eq!(text_tokens("ab你"), 2);
        assert_eq!(estimate(&[ChatMessage::user("abcd".to_string())], true), MESSAGE_TOKENS + 1);
        let inbound = ChatMessage::user(vec![ContentPart::Text("abcd".into()), image()]);
        assert_eq!(
            estimate(&[inbound.clone()], true),
            MESSAGE_TOKENS + 1 + IMAGE_TOKENS,
            "a replayed inbound image counts"
        );
        assert_eq!(
            estimate(&[inbound], false),
            MESSAGE_TOKENS + 1 + text_tokens(super::super::NO_IMAGE_INPUT),
            "a route without image input counts the text that replaces the image"
        );
        let shot = call("shot", "qq_get_message_images");
        let mut tool = result(&shot, "ok");
        tool.content.push(image());
        let messages = [ChatMessage::assistant(vec![ContentPart::ToolCall(shot.clone())]), tool];
        let with = estimate(&messages, true);
        assert!(with >= IMAGE_TOKENS, "a tool image counts too: {with}");
        assert_eq!(
            with - estimate(&messages, false),
            IMAGE_TOKENS - text_tokens(super::super::NO_IMAGE_INPUT)
        );
    }

    /// The system instruction, two history rounds (the first with a tool call and its result) and a current turn with a
    /// pending tool result; the history rounds are messages 1..5 and 5..7.
    fn sample() -> (Vec<ChatMessage>, Vec<Range<usize>>) {
        let old = call("old-call", "web_search");
        let now = call("now-call", "web_search");
        let messages = vec![
            ChatMessage::system("instruction".to_string()),
            ChatMessage::user("x".repeat(400)),
            ChatMessage::assistant(vec![ContentPart::ToolCall(old.clone())]),
            result(&old, "old result"),
            ChatMessage::assistant("y".repeat(40)),
            ChatMessage::user("z".repeat(400)),
            ChatMessage::assistant("w".repeat(40)),
            ChatMessage::user("latest owner message".to_string()),
            ChatMessage::assistant(vec![ContentPart::ToolCall(now.clone())]),
            result(&now, "pending tool result"),
        ];
        (messages, vec![1..5, 5..7])
    }

    #[test]
    fn harness_history_budget_leaves_out_the_oldest_whole_rounds_and_never_the_current_turn() {
        let (messages, rounds) = sample();
        let total = estimate(&messages, true);
        let first = estimate(&messages[1..5], true);
        assert_eq!(dropped_rounds(&messages, &rounds, true, total), 0, "at the limit nothing is left out");
        assert_eq!(dropped_rounds(&messages, &rounds, true, total - 1), 1, "one token over leaves out the oldest round");
        assert_eq!(dropped_rounds(&messages, &rounds, true, total - first), 1);
        assert_eq!(dropped_rounds(&messages, &rounds, true, total - first - 1), 2);
        assert_eq!(dropped_rounds(&messages, &rounds, true, 0), 2, "only history rounds are ever left out");
        assert_eq!(dropped_rounds(&messages, &[], true, 0), 0, "without history nothing is left out");
        let limits = Limits::from_config(&json!({}));
        let offered = [Tool::new("web_search")];
        let (kept, delivered) = fit(messages.clone(), &rounds, &offered, limits, total - first - 1);
        assert_eq!(delivered.len(), 4, "the system instruction and the current turn stay");
        assert_eq!(kept.len(), delivered.len(), "the kept filtered copy has the same messages");
        assert_eq!(delivered[0].role, ChatRole::System);
        assert_eq!(delivered[1].content.first_text(), Some("latest owner message"), "the owner's latest message stays");
        assert!(
            delivered[3].content.tool_responses().iter().any(|response| response.call_id == "now-call"),
            "the pending tool result stays"
        );
        let (_, one_left) = fit(messages, &rounds, &offered, limits, total - 1);
        let calls: Vec<String> = one_left
            .iter()
            .flat_map(|message| message.content.tool_calls())
            .map(|call| call.call_id.clone())
            .collect();
        let answered: Vec<String> = one_left
            .iter()
            .flat_map(|message| message.content.tool_responses())
            .map(|response| response.call_id.clone())
            .collect();
        assert_eq!(calls, answered, "a call leaves only together with its result");
        assert_eq!(calls, ["now-call"]);
        let (_, unoffered) = fit(sample().0, &rounds, &[], limits, u64::MAX);
        assert!(unoffered[2].content.tool_calls().is_empty(), "a replayed call to a tool not offered becomes text");
        assert_eq!(unoffered[3].role, ChatRole::User, "and its result too");
        assert_eq!(unoffered[8].content.tool_calls().len(), 1, "a call of the current turn stays a call");
        assert_eq!(unoffered[9].role, ChatRole::Tool);
    }

    #[test]
    fn harness_history_budget_turns_calls_to_a_tool_off_the_menu_and_their_results_into_text() {
        let gone = call("gone-call", "qq_send_group_message");
        let also = call("also-call", "web_search");
        let still = call("still-call", "web_search");
        let mut sent = result(&gone, "sent");
        sent.content.push(image());
        let messages = vec![
            ChatMessage::assistant(vec![
                ContentPart::Text("both".into()),
                ContentPart::ToolCall(gone.clone()),
                ContentPart::ToolCall(also.clone()),
            ]),
            sent,
            result(&also, "found"),
            ChatMessage::assistant(vec![ContentPart::ToolCall(still.clone())]),
            result(&still, "found again"),
        ];
        let offered: HashSet<&str> = ["web_search"].into_iter().collect();
        let rewritten = off_menu_as_text(messages, &offered);
        assert_eq!(rewritten.len(), 5, "one message for each");
        assert!(
            rewritten[0].content.tool_calls().is_empty(),
            "every call of a message with an off-menu call becomes text"
        );
        assert_eq!(rewritten[0].content.texts(), ["both", &call_text(&gone)[..], &call_text(&also)[..]]);
        let sent_text = json!({"content":[{"type":"text","text":"sent"}]}).to_string();
        assert_eq!(rewritten[1].role, ChatRole::User, "its result becomes text the owner's side carries");
        assert_eq!(
            rewritten[1].content.first_text(),
            Some(&result_text("qq_send_group_message", &sent_text)[..])
        );
        assert_eq!(rewritten[1].content.binaries().len(), 1, "the result's image stays");
        assert_eq!(rewritten[2].role, ChatRole::User, "the offered call of the same message goes with it");
        assert_eq!(rewritten[3].content.tool_calls().len(), 1, "a message whose calls are all offered keeps them");
        assert_eq!(rewritten[4].role, ChatRole::Tool);
        let none = off_menu_as_text(vec![ChatMessage::assistant(vec![ContentPart::ToolCall(still)])], &HashSet::new());
        assert!(none[0].content.tool_calls().is_empty(), "an empty menu leaves no call");
    }
}
