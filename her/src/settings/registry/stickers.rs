//! The global stickers settings.

use crate::settings::{kinds::{FileRule, Integer, Switch}, Setting};

macro_rules! setting {
    ($name:ident, $key:literal, $path:literal, $kind:expr, $label:literal, $hint:literal, $chat:literal) => {
        pub(crate) const $name: Setting = Setting {
            key: $key,
            pointer: $path,
            kind: &$kind,
            applies: "live",
            group: "stickers",
            label: $label,
            hint: $hint,
            chat: $chat,
        };
    };
}

setting!(
    STICKERS_ENABLED, "socialV2.sticker.enabled", "/socialV2/sticker/enabled",
    Switch::new(true), // RED-S2
    "Stickers", "Enables the sticker library and sticker tools.", true
);
setting!(
    STICKERS_TTL, "socialV2.sticker.syncTtlMs", "/socialV2/sticker/syncTtlMs",
    Integer { min: 0, max: 86_400_000, default: 60_000, file: FileRule::SignedClamp },
    "Sticker cache age", "Milliseconds before a sticker list is refreshed. Zero refreshes on each eligible request.", false
);
setting!(
    STICKERS_LIST, "socialV2.sticker.maxListCount", "/socialV2/sticker/maxListCount",
    Integer { min: 1, max: 500, default: 100, file: FileRule::SignedClamp },
    "Sticker list size", "Maximum number of stickers fetched or shown in a list.", false
);
setting!(
    STICKERS_IN_PROMPT, "socialV2.sticker.includeInPrompt", "/socialV2/sticker/includeInPrompt",
    Switch::new(true),
    "Sticker prompt", "Includes the sticker inventory and strategy in prompts.", false
);
setting!(
    STICKERS_PROMPT_COUNT, "socialV2.sticker.promptMaxStickers", "/socialV2/sticker/promptMaxStickers",
    Integer { min: 1, max: 30, default: 8, file: FileRule::Clamp },
    "Sticker prompt size", "Maximum number of stickers in the prompt inventory.", false
);
setting!(
    COLLECT_ENABLED, "socialV2.sticker.collect.enabled", "/socialV2/sticker/collect/enabled",
    Switch::new(true),
    "Sticker collection", "Lets authorized callers collect a sticker from a message.", false
);
setting!(
    COLLECT_MINUTE, "socialV2.sticker.collect.maxPerMinute", "/socialV2/sticker/collect/maxPerMinute",
    Integer { min: 0, max: 60, default: 0, file: FileRule::SignedClamp },
    "Sticker collections per minute", "The minute budget per conversation. Zero disables this budget.", false
);
setting!(
    COLLECT_HOUR, "socialV2.sticker.collect.maxPerHour", "/socialV2/sticker/collect/maxPerHour",
    Integer { min: 0, max: 500, default: 0, file: FileRule::SignedClamp },
    "Sticker collections per hour", "The hour budget per conversation. Zero disables this budget.", false
);
setting!(
    COLLECT_REMARK, "socialV2.sticker.collect.maxRemarkChars", "/socialV2/sticker/collect/maxRemarkChars",
    Integer { min: 1, max: 200, default: 20, file: FileRule::SignedClamp },
    "Sticker remark length", "Maximum number of characters in a collection remark.", false
);
setting!(
    COLLECT_BYTES, "socialV2.sticker.collect.maxBytes", "/socialV2/sticker/collect/maxBytes",
    Integer { min: 0, max: 20_971_520, default: 8_388_608, file: FileRule::Clamp },
    "Sticker collection size", "Maximum source image size in bytes.", true
);
pub(super) const SETTINGS: &[Setting] = &[
    STICKERS_ENABLED, STICKERS_TTL, STICKERS_LIST, STICKERS_IN_PROMPT, STICKERS_PROMPT_COUNT, COLLECT_ENABLED, COLLECT_MINUTE, COLLECT_HOUR, COLLECT_REMARK, COLLECT_BYTES,
];
