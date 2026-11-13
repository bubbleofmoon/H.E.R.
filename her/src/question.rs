//! Conversation evidence for the reserved2 boolean question trigger.
use serde_json::Value;
use std::sync::OnceLock;

pub(crate) const DEFAULT_WINDOW_MS: i64 = 90_000;

/// Short `forYouReason` hints on messages returned by `qq_wait_for_messages`. A hint never
/// forces a reply; the model decides.
pub(crate) const FOR_YOU_QUOTE: &str = "引用回复了你的消息";
pub(crate) const FOR_YOU_AT: &str = "@了你";
pub(crate) const FOR_YOU_NAME: &str = "提到了你的昵称或群名片";
pub(crate) const FOR_YOU_PRIVATE: &str = "私聊你";
pub(crate) const FOR_YOU_POKE: &str = "拍了拍你";
pub(crate) const FOR_YOU_EXCHANGE: &str = "正在和你来回交流";
pub(crate) const NOT_FOR_YOU: &str = "没有指向你";

pub(crate) fn exchange_window_ms(cfg: &Value) -> i64 {
    cfg.pointer("/socialV2/wake/questionExchangeWindowMs")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && n.fract() == 0.0 && *n > 0.0 && *n <= 9_007_199_254_740_991.0)
        .map(|n| n as i64)
        .unwrap_or(DEFAULT_WINDOW_MS)
}

pub(crate) fn is_question_text(text: &str) -> bool {
    static QUESTION: OnceLock<regex::Regex> = OnceLock::new();
    QUESTION.get_or_init(|| regex::Regex::new(r"[?？吗呢么]|怎么|为什么|哪|谁|能不能|会不会|是不是|有没有|行不行|是否|如何|怎样|多少").unwrap()).is_match(text)
}

fn nagging(text: &str) -> bool {
    static NAG: OnceLock<regex::Regex> = OnceLock::new();
    NAG.get_or_init(|| {
        regex::Regex::new("怎么不说话|人呢|回我|说话啊|理我|别装死|在不在|装死|说话").unwrap()
    })
    .is_match(text)
}

fn scalar(value: &Value) -> String {
    crate::chat::scalar_raw(Some(value))
}

fn mentions_other(message: &Value, self_id: i64) -> bool {
    let me = self_id.to_string();
    if let Some(segments) = message.as_array() {
        return segments
            .iter()
            .any(|s| s["type"] == "at" && scalar(&s["data"]["qq"]) != me);
    }
    // Raw CQ text can occur in replay inputs as well as gateway string messages.
    static AT: OnceLock<regex::Regex> = OnceLock::new();
    AT.get_or_init(|| regex::Regex::new(r"\[CQ:at,([^\]]*)\]").unwrap())
        .captures_iter(message.as_str().unwrap_or(""))
        .any(|c| c[1].split(',').find_map(|field| field.strip_prefix("qq=")) != Some(me.as_str()))
}

pub(crate) fn directed(
    ev: &crate::Event,
    text: &str,
    quote_self: bool,
    conv: &Value,
    cfg: &Value,
    now: i64,
) -> bool {
    if mentions_other(&ev.message, ev.self_id.unwrap_or(0)) {
        return false;
    }
    let in_exchange = exchange_member(ev, conv, cfg, now);
    if nagging(text) {
        return in_exchange;
    }
    is_question_text(text) && (quote_self || in_exchange)
}

/// The #130 exchange rule: within the window of her latest message, she replied to/@ this
/// sender, or this sender quoted her.
fn exchange_member(ev: &crate::Event, conv: &Value, cfg: &Value, now: i64) -> bool {
    let window = exchange_window_ms(cfg);
    let recent = conv["recentMessages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let within = |time: i64| time > 0 && time <= now && now.saturating_sub(time) <= window;
    let last_self = recent
        .iter()
        .filter(|m| m["isSelf"] == true)
        .filter_map(|m| m["time"].as_i64())
        .max()
        .unwrap_or(0)
        .max(conv["lastAiReplyAt"].as_i64().unwrap_or(0));
    let sender = ev.user_id.map(|id| id.to_string()).unwrap_or_default();
    !sender.is_empty()
        && within(last_self)
        && recent.iter().any(|m| {
            if !within(m["time"].as_i64().unwrap_or(0)) {
                return false;
            }
            if m["isSelf"] == true {
                scalar(&m["replyToUserId"]) == sender || scalar(&m["atUserId"]) == sender
            } else {
                m["quoteTargetIsSelf"] == true && scalar(&m["userId"]) == sender
            }
        })
}

/// Ingress classification of whether a message is for her. A poke (`poke` = whether it targets
/// her) is for her exactly when it targets her. Any other message in a private conversation is
/// for her; in a group: a quote-reply to her, an @ of her, her nickname/group card, or the #130
/// exchange rule without an @ of another member. `conv` is the conversation before this message.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ingress_for_you(
    ev: &crate::Event,
    key: &str,
    poke: Option<bool>,
    quote_self: bool,
    at_self: bool,
    names_self: bool,
    conv: &Value,
    cfg: &Value,
    now: i64,
) -> Option<&'static str> {
    if let Some(at_her) = poke {
        at_her.then_some(FOR_YOU_POKE)
    } else if key.starts_with("private:") {
        Some(FOR_YOU_PRIVATE)
    } else if quote_self {
        Some(FOR_YOU_QUOTE)
    } else if at_self {
        Some(FOR_YOU_AT)
    } else if names_self {
        Some(FOR_YOU_NAME)
    } else if !mentions_other(&ev.message, ev.self_id.unwrap_or(0)) && exchange_member(ev, conv, cfg, now) {
        Some(FOR_YOU_EXCHANGE)
    } else {
        None
    }
}

/// The hint on a stored message: its ingress classification, or for older records the facts
/// they carry (any private message, a quote of her, an @ of her).
pub(crate) fn for_you(message: &Value, key: &str) -> (bool, String) {
    if let (Some(yes), Some(why)) = (message["forYou"].as_bool(), message["forYouReason"].as_str()) {
        return (yes, why.to_string());
    }
    let why = if key.starts_with("private:") {
        Some(FOR_YOU_PRIVATE)
    } else if message["quoteTargetIsSelf"] == true {
        Some(FOR_YOU_QUOTE)
    } else if message["atSelf"] == true {
        Some(FOR_YOU_AT)
    } else {
        None
    };
    (why.is_some(), why.unwrap_or(NOT_FOR_YOU).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn hf130_exchange_window_defaults_and_validates_without_coercion() {
        for value in [
            json!(null),
            json!(0),
            json!(-1),
            json!(1.5),
            json!("1000"),
            json!(true),
            json!(9_007_199_254_740_992u64),
        ] {
            assert_eq!(
                exchange_window_ms(
                    &json!({"socialV2":{"wake":{"questionExchangeWindowMs":value}}})
                ),
                90_000
            );
        }
        assert_eq!(
            exchange_window_ms(&json!({"socialV2":{"wake":{"questionExchangeWindowMs":1250}}})),
            1250
        );
    }
    #[test]
    fn hf130_mentions_of_others_include_raw_cq_and_all() {
        for message in [
            json!([{"type":"at","data":{"qq":"1002"}}]),
            json!("[CQ:at,qq=all]你好吗？"),
            json!("[CQ:at,name=x,qq=1002]你好吗？"),
        ] {
            assert!(mentions_other(&message, 9001));
        }
        assert!(!mentions_other(
            &json!([{"type":"at","data":{"qq":9001}}]),
            9001
        ));
    }
    #[test]
    fn hf130_exchange_membership_is_bounded_by_both_clocks() {
        let ev: crate::Event = serde_json::from_value(json!({"post_type":"message","message_type":"group","self_id":9001,"user_id":1001,"message":[]})).unwrap();
        let cfg = json!({"socialV2":{"wake":{"questionExchangeWindowMs":1000}}});
        let mut conv = json!({"lastAiReplyAt":1000,"recentMessages":[{"userId":"1001","quoteTargetIsSelf":true,"time":1000}]});
        assert!(directed(&ev, "你有空吗？", false, &conv, &cfg, 2000));
        assert!(!directed(&ev, "你有空吗？", false, &conv, &cfg, 2001));
        conv["lastAiReplyAt"] = json!(2001);
        assert!(
            !directed(&ev, "你有空吗？", false, &conv, &cfg, 2001),
            "a new bot message cannot refresh old participant evidence"
        );
        conv["recentMessages"][0]["time"] = json!(3000);
        assert!(
            !directed(&ev, "你有空吗？", false, &conv, &cfg, 2001),
            "future exchange evidence is refused"
        );
        assert!(
            directed(&ev, "今天吃啥？", true, &conv, &cfg, 2001),
            "a quote question needs no recent exchange"
        );
        assert!(
            !directed(&ev, "说话", true, &conv, &cfg, 2001),
            "nagging still needs the window"
        );
    }
    /// Review fixes 3 and 10: every non-poke private message is for her; a poke is for her
    /// exactly when it targets her.
    #[test]
    fn engage_private_messages_and_pokes_at_her_are_for_her() {
        let cfg = json!({});
        let conv = json!({});
        let private: crate::Event = serde_json::from_value(json!({"post_type":"message","message_type":"private","self_id":9001,"user_id":1001,"message":[]})).unwrap();
        let group: crate::Event = serde_json::from_value(json!({"post_type":"message","message_type":"group","self_id":9001,"user_id":1001,"message":[]})).unwrap();
        assert_eq!(ingress_for_you(&private, "private:1001", None, false, false, false, &conv, &cfg, 1000), Some(FOR_YOU_PRIVATE));
        assert_eq!(FOR_YOU_PRIVATE, "私聊你");
        assert_eq!(ingress_for_you(&private, "private:1001", Some(true), false, false, false, &conv, &cfg, 1000), Some(FOR_YOU_POKE));
        assert_eq!(ingress_for_you(&private, "private:1001", Some(false), false, false, false, &conv, &cfg, 1000), None);
        assert_eq!(ingress_for_you(&group, "group:42", Some(true), false, false, false, &conv, &cfg, 1000), Some(FOR_YOU_POKE));
        assert_eq!(ingress_for_you(&group, "group:42", None, false, false, false, &conv, &cfg, 1000), None);
        // A poke of someone else is not for her, even inside an exchange with its sender.
        let exchange = json!({"lastAiReplyAt":900,"recentMessages":[{"userId":"1001","quoteTargetIsSelf":true,"time":900}]});
        assert_eq!(ingress_for_you(&group, "group:42", None, false, false, false, &exchange, &cfg, 1000), Some(FOR_YOU_EXCHANGE));
        assert_eq!(ingress_for_you(&group, "group:42", Some(false), false, false, false, &exchange, &cfg, 1000), None);
        assert!(!FOR_YOU_POKE.is_empty() && FOR_YOU_POKE != NOT_FOR_YOU);
        // Records stored before ingress classification: any private message is for her.
        assert_eq!(for_you(&json!({"text":"在吗"}), "private:1001"), (true, FOR_YOU_PRIVATE.to_string()));
        assert_eq!(for_you(&json!({"text":"在吗"}), "group:42"), (false, NOT_FOR_YOU.to_string()));
    }
}
