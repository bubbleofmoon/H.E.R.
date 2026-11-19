//! Global pacing and limits for delivery.

use crate::settings::kinds::{FileRule, Integer, Probability, Switch};
use crate::settings::Setting;
use serde_json::Value;

const SAFE_INTEGER: i64 = (1_i64 << 53) - 1;

macro_rules! setting {
    ($key:literal, $pointer:literal, $kind:expr, $label:literal, $hint:literal, $chat:literal, $group:literal) => {
        Setting {
            key: $key, pointer: $pointer, kind: $kind, applies: "live",
            group: $group, label: $label, hint: $hint, chat: $chat,
        }
    };
}

pub(crate) const SEND_DELAY: Setting = setting!("sendDelayMs", "/sendDelayMs", &Integer { min: 0, max: 120_000, default: 300, file: FileRule::Default }, "Chat part delay", "Milliseconds between chat delivery parts.", true, "reply rhythm");
pub(crate) const FIRST_ENABLED: Setting = setting!("send.firstGapEnabled", "/socialV2/send/firstGapEnabled", &Switch { default: true }, "Typing delay enabled", "Allows the bridge to spend a typing budget on a reply.", true, "reply rhythm");
pub(crate) const FILL_TYPING: Setting = setting!("send.fillTypingBudget", "/socialV2/send/fillTypingBudget", &Switch { default: true }, "Credit thinking time", "Counts thinking time towards the typing budget.", false, "reply rhythm");
pub(crate) const FIRST_BASE: Setting = setting!("send.firstGapBaseMs", "/socialV2/send/firstGapBaseMs", &Integer { min: 0, max: 120_000, default: 1_000, file: FileRule::LegacyDecimal }, "First-gap base", "Milliseconds added before the per-character typing budget.", false, "reply rhythm");
pub(crate) const FIRST_PER_CHAR: Setting = setting!("send.firstGapPerCharMs", "/socialV2/send/firstGapPerCharMs", &Integer { min: 0, max: 1_000, default: 60, file: FileRule::LegacyDecimal }, "First-gap per character", "Milliseconds per UTF-16 unit in the typing budget.", false, "reply rhythm");
pub(crate) const GAP_BASE: Setting = setting!("send.gapBaseMs", "/socialV2/send/gapBaseMs", &Integer { min: 0, max: 120_000, default: 800, file: FileRule::Default }, "Length-based gap base", "Milliseconds before adding the length-based gap.", false, "reply rhythm");
pub(crate) const GAP_PER_CHAR: Setting = setting!("send.gapPerCharMs", "/socialV2/send/gapPerCharMs", &Integer { min: 0, max: 1_000, default: 20, file: FileRule::Default }, "Gap per character", "Milliseconds per character in a length-based inter-bubble gap.", false, "reply rhythm");
pub(crate) const GAP_MAX: Setting = setting!("send.maxGapMs", "/socialV2/send/maxGapMs", &Integer { min: 0, max: 120_000, default: 10_000, file: FileRule::Default }, "Inter-bubble gap cap", "Maximum requested inter-bubble delay in milliseconds.", false, "reply rhythm");
pub(crate) const STICKER_MIN: Setting = setting!("reply.stickerPauseMinMs", "/socialV2/reply/stickerPauseMinMs", &Integer { min: 0, max: 30_000, default: 800, file: FileRule::Default }, "Sticker pause minimum", "Shortest pause before submitting a sticker.", false, "reply rhythm");
pub(crate) const STICKER_MAX: Setting = setting!("reply.stickerPauseMaxMs", "/socialV2/reply/stickerPauseMaxMs", &Integer { min: 0, max: 30_000, default: 2_000, file: FileRule::Default }, "Sticker pause maximum", "Longest pause before submitting a sticker.", false, "reply rhythm");
pub(crate) const BURST: Setting = setting!("send.burstEnabled", "/socialV2/send/burstEnabled", &Switch { default: true }, "Burst enabled", "Allows an unmerged multi-bubble send.", true, "send limits");
pub(crate) const BURST_MAX: Setting = setting!("send.burstMaxMessages", "/socialV2/send/burstMaxMessages", &Integer { min: 1, max: 1_000, default: 8, file: FileRule::Default }, "Burst size", "Maximum messages in one burst.", false, "send limits");
pub(crate) const BURST_MIN_MS: Setting = setting!("send.burstIntervalMinMs", "/socialV2/send/burstIntervalMinMs", &Integer { min: 0, max: 120_000, default: 1_000, file: FileRule::Default }, "Burst gap minimum", "Shortest regular automatic gap in milliseconds.", true, "send limits");
pub(crate) const BURST_MAX_MS: Setting = setting!("send.burstIntervalMaxMs", "/socialV2/send/burstIntervalMaxMs", &Integer { min: 0, max: 120_000, default: 3_000, file: FileRule::Default }, "Burst gap maximum", "Longest regular automatic gap in milliseconds.", true, "send limits");
pub(crate) const LONG_MIN: Setting = setting!("send.longGapMinMs", "/socialV2/send/longGapMinMs", &Integer { min: 0, max: 120_000, default: 5_000, file: FileRule::Default }, "Long gap minimum", "Shortest long automatic gap in milliseconds.", false, "send limits");
pub(crate) const LONG_MAX: Setting = setting!("send.longGapMaxMs", "/socialV2/send/longGapMaxMs", &Integer { min: 0, max: 120_000, default: 10_000, file: FileRule::Default }, "Long gap maximum", "Longest long automatic gap in milliseconds.", false, "send limits");
pub(crate) const LONG_CHANCE: Setting = setting!("send.longGapProbability", "/socialV2/send/longGapProbability", &Probability { default: 0.0 }, "Long gap probability", "Chance of choosing a long gap between automatic burst messages.", false, "send limits");
pub(crate) const RATE_MIN: Setting = setting!("send.maxSendPerMinute", "/socialV2/send/maxSendPerMinute", &Integer { min: 0, max: 500, default: 8, file: FileRule::Default }, "Sends per minute", "Zero disables the minute quota.", true, "send limits");
pub(crate) const RATE_HOUR: Setting = setting!("send.maxSendPerHour", "/socialV2/send/maxSendPerHour", &Integer { min: 0, max: 500, default: 60, file: FileRule::Default }, "Sends per hour", "Zero disables the hour quota.", true, "send limits");
pub(crate) const TURN_MAX: Setting = setting!("send.maxPerTurn", "/socialV2/send/maxPerTurn", &Integer { min: 1, max: SAFE_INTEGER, default: 20, file: FileRule::Default }, "Text sends per turn", "Maximum delivered or uncertain text bubbles in a turn.", false, "send limits");
pub(crate) const MAX_CHARS: Setting = setting!("send.maxMessageChars", "/socialV2/send/maxMessageChars", &Integer { min: 1, max: 200_000, default: 500, file: FileRule::Default }, "Message length limit", "Maximum characters before a text message must be split or refused.", false, "send limits");
pub(crate) const REPEAT: Setting = setting!("send.repeatLimit", "/socialV2/send/repeatLimit", &Integer { min: 1, max: SAFE_INTEGER, default: 2, file: FileRule::Default }, "Identical text limit", "Maximum identical normalized bubbles in the repeat window.", false, "send limits");
pub(crate) const REPEAT_WINDOW: Setting = setting!("send.repeatWindowSends", "/socialV2/send/repeatWindowSends", &Integer { min: 0, max: SAFE_INTEGER, default: 0, file: FileRule::Default }, "Repeat window", "Number of recent delivered or uncertain sends. Zero uses the current and prior turn.", false, "send limits");
pub(crate) const DUPLICATE_WINDOW: Setting = setting!("send.duplicateWindowMs", "/socialV2/send/duplicateWindowMs", &Integer { min: 0, max: 86_400_000, default: 0, file: FileRule::LegacyDecimal }, "Duplicate tool-send window", "Milliseconds before another tool send in the conversation. Zero disables the window.", false, "send limits");

pub(crate) const SETTINGS: &[Setting] = &[
    SEND_DELAY, FIRST_ENABLED, FILL_TYPING, FIRST_BASE, FIRST_PER_CHAR, GAP_BASE, GAP_PER_CHAR, GAP_MAX, STICKER_MIN, STICKER_MAX, BURST, BURST_MAX, BURST_MIN_MS, BURST_MAX_MS, LONG_MIN, LONG_MAX, LONG_CHANCE, RATE_MIN, RATE_HOUR, TURN_MAX, MAX_CHARS, REPEAT, REPEAT_WINDOW, DUPLICATE_WINDOW,
];

pub(crate) const ORDERED: [[Setting; 2]; 3] = [
    [STICKER_MIN, STICKER_MAX], [BURST_MIN_MS, BURST_MAX_MS], [LONG_MIN, LONG_MAX],
];

/// An older file could give its first-gap values through the ordinary gap settings.
pub(crate) fn first_gap_setting(document: &Value, first: &Setting, fallback: &Setting) -> i64 {
    if let Some(value) = first.found_in(document).filter(|value| !value.is_null()) {
        return first.kind.read(value).and_then(|value| value.as_i64())
            .unwrap_or_else(|| first.integer(&Value::Null));
    }
    fallback.found_in(document).and_then(|value| fallback.kind.read(value)).and_then(|value| value.as_i64())
        .filter(|value| *value != 0).unwrap_or_else(|| first.integer(&Value::Null))
}
