//! What the frozen JS `handleIncoming` derives from one inbound message before the reserved2
//! store: `textContent` and `plainContent` (`segmentsToText`, `bridge.js` 11394-11395), whether a
//! quote addresses the bot (`isQuoteTargetSelf`), and the stored 200-unit forms (`appendSocialV2Message`).

use crate::chat::{self, Resolver};
use crate::social_mem::{truncate_text, truncate_text_tail};
use serde_json::Value;

/// Pure media/emoji arrivals remain in unread, but do not extend reply/wake quiet.
/// Inspect the original representation before rendering/truncating: mixed text and media counts
/// as speech; quote wrappers are ignored only alongside media/emoji. Unknown segments count as speech.
pub fn quiet_neutral(message: &Value) -> bool {
    fn neutral_text(text: &str) -> bool {
        static QUOTE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let quote = QUOTE.get_or_init(|| regex::Regex::new(r"\[CQ:reply(?:,[^\]]*)?\]").unwrap());
        let unquoted = quote.replace_all(text, "");
        if unquoted.len() != text.len() && unquoted.trim().is_empty() {
            return false;
        }
        static MEDIA: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let media = MEDIA.get_or_init(|| regex::Regex::new(r"\[CQ:(?:image|face|mface|bface|sface)(?:,[^\]]*)?\]|\[(?:图片|表情[^\]]*)\]|[0-9#*]\x{FE0F}?\x{20E3}").unwrap());
        media.replace_all(&unquoted, "").chars().all(|c| c.is_whitespace() || matches!(c as u32,
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2300..=0x23FF | 0x2B05..=0x2B07 |
            0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x200D | 0xFE0E..=0xFE0F | 0x20E3 | 0xE0020..=0xE007F |
            0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x3030 | 0x303D | 0x3297 | 0x3299))
    }
    match message {
        Value::String(text) => neutral_text(text),
        Value::Array(segments) => {
            segments.iter().any(|s| {
                s["type"] != "reply"
                    && (s["type"] != "text"
                        || s.pointer("/data/text")
                            .and_then(Value::as_str)
                            .is_some_and(|t| !t.trim().is_empty()))
            }) && segments.iter().all(|s| match s["type"].as_str() {
                Some("image" | "face" | "mface" | "bface" | "sface" | "reply") => true,
                Some("text") => s
                    .pointer("/data/text")
                    .and_then(Value::as_str)
                    .is_some_and(neutral_text),
                _ => false,
            })
        }
        _ => false,
    }
}

/// `textContent` (names and the quoted message resolved), `plainContent` (the message's own words)
/// and whether the message quotes one of the bot's own messages.
pub struct Rendered {
    pub text: String,
    pub plain: String,
    pub quote_self: bool,
    /// An `at` segment names the bot (`evaluateWakeTriggerV2`'s `atSelf`).
    pub at_self: bool,
}

/// Lookups unavailable (no OneBot endpoint): JS treats a failed lookup as "no name / no quote".
pub struct NoLookups;

impl Resolver for NoLookups {
    fn at_name(&mut self, _qq: &str) -> Option<String> {
        None
    }
    fn reply(&mut self, _message_id: &str) -> Option<chat::ReplyInfo> {
        None
    }
}

/// Lookup order follows `handleIncoming`: text, plain, then the quote check.
pub fn render_inbound(message: &Value, self_id: i64, res: &mut dyn Resolver) -> Rendered {
    let text = chat::segments_to_text(message, res, true);
    let plain = chat::segments_to_text(message, res, false);
    let me = self_id.to_string();
    let segments = message.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let at_self = segments.iter().any(|seg| {
        seg.get("type").and_then(Value::as_str) == Some("at")
            && chat::scalar_raw(seg.pointer("/data/qq")) == me
    });
    let quote_self = self_id != 0
        && segments.iter().any(|seg| {
            seg.get("type").and_then(Value::as_str) == Some("reply")
                && seg
                    .pointer("/data/id")
                    .filter(|id| !id.is_null())
                    .and_then(|id| res.reply(&chat::scalar_raw(Some(id))))
                    .is_some_and(|info| info.user_id.as_deref() == Some(me.as_str()))
        });
    Rendered {
        text,
        plain,
        quote_self,
        at_self,
    }
}

/// Width in UTF-16 units of the stored `text`/`plain`/`tail` previews. The complete message lives in
/// `fullText` (see [`stored_full_text`]).
pub const PREVIEW_UNITS: i64 = 200;

/// The `text`, `plain` and `tail` preview fields of a stored message.
pub fn stored_texts(text: &str, plain: &str) -> (String, String, String) {
    (
        truncate_text(text, PREVIEW_UNITS),
        truncate_text(plain, PREVIEW_UNITS),
        truncate_text_tail(plain, PREVIEW_UNITS),
    )
}

/// `socialV2.context.fullTextMaxChars` (default 4000, 200..=20000): the most UTF-16 units of one
/// message the bridge keeps readable in full.
pub fn full_text_cap(cfg: &Value) -> i64 {
    cfg.pointer("/socialV2/context/fullTextMaxChars")
        .filter(|v| !v.is_null())
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .filter(|n| n.is_finite())
        .map_or(4000, |n| n.round().clamp(200.0, 20000.0) as i64)
}

/// For a message longer than its preview: the readable full text (cut at `cap` units) and the
/// message's real length in units. `None` when the preview already holds everything.
pub fn stored_full_text(text: &str, cap: i64) -> Option<(String, usize)> {
    let units = text.encode_utf16().count();
    (units > PREVIEW_UNITS as usize).then(|| (truncate_text(text, cap), units))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn hf119_neutral_media_uses_original_segments_cq_and_emoji_not_truncated_text() {
        for value in [
            json!([{"type":"image","data":{"file":"x"}}]),
            json!([{"type":"mface","data":{"summary":"word label is still a sticker"}}]),
            json!([{"type":"face","data":{"id":14}},{"type":"text","data":{"text":"🙂👩🏽‍💻 ❤️"}}]),
            json!("[CQ:image,file=x][CQ:face,id=14]"),
            json!("[图片][表情14][表情:开心]"),
            json!("🙂"),
            json!("1️⃣#️⃣"),
        ] {
            assert!(quiet_neutral(&value), "{value}");
        }
        for value in [
            json!("hi🙂"),
            json!("[图片]hi"),
            json!("?"),
            json!("119"),
            json!("[语音]"),
            json!([{"type":"image","data":{}},{"type":"text","data":{"text":"hi"}}]),
            json!([{"type":"record","data":{}}]),
            json!([{"type":"reply","data":{"id":1}}]),
            json!([{"type":"at","data":{"qq":"9001"}}]),
        ] {
            assert!(!quiet_neutral(&value), "{value}");
        }
    }

    #[test]
    fn hf119_quoted_media_cq_and_segment_arrays_have_the_same_quiet_classification() {
        let quote = json!({"type":"reply","data":{"id":"123"}});
        let mut cases = Vec::new();
        for kind in ["image", "face", "mface", "bface", "sface"] {
            cases.push((
                format!("[CQ:reply,id=123][CQ:{kind},file=x]"),
                json!([quote.clone(), {"type":kind,"data":{"file":"x"}}]),
                true,
            ));
        }
        for (text, neutral) in [
            ("🙂", true),
            ("1️⃣#️⃣", true),
            ("[图片]", true),
            ("", false),
            ("  ", false),
            ("hello🙂", false),
            ("?", false),
        ] {
            cases.push((
                format!("[CQ:reply,id=123]{text}"),
                json!([quote.clone(), {"type":"text","data":{"text":text}}]),
                neutral,
            ));
        }
        for kind in ["record", "video", "file", "at", "unknown"] {
            cases.push((
                format!("[CQ:reply,id=123][CQ:{kind},file=x]"),
                json!([quote.clone(), {"type":kind,"data":{"file":"x"}}]),
                false,
            ));
        }
        cases.push((
            "[CQ:reply,id=123][CQ:image,file=x]hello".into(),
            json!([quote.clone(), {"type":"image","data":{"file":"x"}},
                {"type":"text","data":{"text":"hello"}}]),
            false,
        ));
        cases.push((
            "[CQ:reply,id=123][CQ:reply,id=456]".into(),
            json!([quote, {"type":"reply","data":{"id":"456"}}]),
            false,
        ));
        for (cq, segments, expected) in cases {
            assert_eq!(
                quiet_neutral(&segments),
                expected,
                "segment form: {segments}"
            );
            assert_eq!(quiet_neutral(&json!(cq)), expected, "CQ form: {cq}");
        }
    }

    struct Fake {
        names: HashMap<String, String>,
        replies: HashMap<String, chat::ReplyInfo>,
        asked: Vec<String>,
    }
    impl Resolver for Fake {
        fn at_name(&mut self, qq: &str) -> Option<String> {
            self.asked.push(format!("at:{qq}"));
            self.names.get(qq).cloned()
        }
        fn reply(&mut self, id: &str) -> Option<chat::ReplyInfo> {
            self.asked.push(format!("reply:{id}"));
            self.replies.get(id).cloned()
        }
    }
    fn fake() -> Fake {
        Fake {
            names: HashMap::from([
                ("401".into(), "Fixture Bot".into()),
                ("10001".into(), "明明".into()),
            ]),
            replies: HashMap::from([
                (
                    "7003".into(),
                    chat::ReplyInfo {
                        sender: "小明".into(),
                        text: "被引用的话".into(),
                        user_id: Some("10001".into()),
                    },
                ),
                (
                    "7100".into(),
                    chat::ReplyInfo {
                        sender: "Fixture Bot".into(),
                        text: "我说的".into(),
                        user_id: Some("401".into()),
                    },
                ),
            ]),
            asked: Vec::new(),
        }
    }

    #[test]
    fn mentions_use_the_member_name_and_fall_back_to_the_qq_number() {
        let message = json!([
            {"type": "at", "data": {"qq": "401"}},
            {"type": "text", "data": {"text": " hi "}},
            {"type": "at", "data": {"qq": 10001}},
            {"type": "at", "data": {"qq": "10003"}},
            {"type": "at", "data": {"qq": "all"}},
        ]);
        let r = render_inbound(&message, 401, &mut fake());
        assert_eq!(r.text, "@Fixture Bot hi @明明@10003@全体成员");
        assert_eq!(r.plain, r.text);
        assert!(!r.quote_self);
        assert!(r.at_self);
        assert!(
            !render_inbound(
                &json!([{"type": "at", "data": {"qq": "10001"}}]),
                401,
                &mut fake()
            )
            .at_self
        );
        assert!(
            render_inbound(
                &json!([{"type": "at", "data": {"qq": 401}}]),
                401,
                &mut fake()
            )
            .at_self
        );
    }

    #[test]
    fn a_quote_is_in_text_but_not_in_plain_and_a_quote_of_the_bot_is_noted() {
        let message = json!([
            {"type": "reply", "data": {"id": "7003"}},
            {"type": "text", "data": {"text": "  同意  "}},
        ]);
        let r = render_inbound(&message, 401, &mut fake());
        assert_eq!(r.text, "[引用 小明：被引用的话]  同意");
        assert_eq!(r.plain, "同意");
        assert!(!r.quote_self);

        let only_quote = json!([{"type": "reply", "data": {"id": "7100"}}]);
        let r = render_inbound(&only_quote, 401, &mut fake());
        assert_eq!(
            (r.text.as_str(), r.plain.as_str(), r.quote_self),
            ("[引用 Fixture Bot：我说的]", "", true)
        );

        let unknown = json!([{"type": "reply", "data": {"id": "404"}}]);
        let r = render_inbound(&unknown, 401, &mut fake());
        assert_eq!(
            (r.text.as_str(), r.plain.as_str(), r.quote_self),
            ("[引用消息]", "", false)
        );
    }

    #[test]
    fn every_media_segment_has_its_own_label() {
        let message = json!([
            {"type": "image", "data": {"file": "a.png"}},
            {"type": "record", "data": {}},
            {"type": "video", "data": {}},
            {"type": "face", "data": {"id": 14}},
            {"type": "mface", "data": {"summary": "给你糖吃"}},
            {"type": "file", "data": {"name": "a.zip"}},
        ]);
        let r = render_inbound(&message, 401, &mut fake());
        assert_eq!(
            r.text,
            "[图片][语音][视频][表情14][表情:给你糖吃][文件a.zip]"
        );
        assert_eq!(r.plain, r.text);
    }

    #[test]
    fn a_string_message_is_trimmed_and_lookups_follow_text_plain_quote_order() {
        let r = render_inbound(&json!("  纯文本  "), 401, &mut fake());
        assert_eq!((r.text.as_str(), r.plain.as_str()), ("纯文本", "纯文本"));
        let mut f = fake();
        let message = json!([{"type": "reply", "data": {"id": "7003"}}, {"type": "at", "data": {"qq": "10001"}}]);
        render_inbound(&message, 401, &mut f);
        assert_eq!(
            f.asked,
            ["reply:7003", "at:10001", "at:10001", "reply:7003"]
        );
        let r = render_inbound(&json!(null), 401, &mut NoLookups);
        assert_eq!(
            (r.text.as_str(), r.plain.as_str(), r.quote_self),
            ("", "", false)
        );
    }

    #[test]
    fn stored_forms_are_cut_at_200_units_and_tail_keeps_the_end_of_plain() {
        let long: String = (0..250)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let (text, plain, tail) = stored_texts(&format!("[引用 x：y]{long}"), &long);
        assert_eq!(
            text,
            format!("[引用 x：y]{long}")
                .chars()
                .take(200)
                .collect::<String>()
        );
        assert_eq!(plain, long.chars().take(200).collect::<String>());
        assert_eq!(tail, long.chars().skip(50).collect::<String>());
        let (text, plain, tail) = stored_texts("短", "短");
        assert_eq!(
            (text.as_str(), plain.as_str(), tail.as_str()),
            ("短", "短", "短")
        );
    }
    #[test]
    fn a_long_message_keeps_its_full_text_up_to_the_configured_cap() {
        let long: String = (0..900).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let middle = format!("{}不要告诉任何人{}", &long[..400], &long[400..]);
        let (full, chars) = stored_full_text(&middle, full_text_cap(&json!({}))).expect("longer than the preview");
        assert_eq!(chars, 907);
        assert_eq!(full, middle, "the middle of a 900-unit message is readable");
        assert!(stored_texts(&middle, &middle).0.chars().count() <= 200, "previews stay previews");
        // Cut at the configured cap, never in the middle of a surrogate pair.
        let cfg = json!({"socialV2": {"context": {"fullTextMaxChars": 300}}});
        let emoji = "🐟".repeat(200);
        let (cut, chars) = stored_full_text(&emoji, full_text_cap(&cfg)).unwrap();
        assert_eq!((cut.encode_utf16().count(), chars), (300, 400));
        assert!(stored_full_text(&"a".repeat(200), 4000).is_none(), "a message that fits its preview needs no copy");
        for (value, expected) in [(json!("50"), 200), (json!(99999), 20000), (json!(null), 4000), (json!("x"), 4000), (json!(1234.4), 1234)] {
            assert_eq!(full_text_cap(&json!({"socialV2": {"context": {"fullTextMaxChars": value}}})), expected, "{value}");
        }
    }
}
