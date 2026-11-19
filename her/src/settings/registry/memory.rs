//! The memory and digest group.

use crate::settings::kinds::{FileRule, Integer, Switch};
use crate::settings::Setting;

pub(crate) const RELATED_TOP_K: Setting = Setting {
    key: "memory.inject.relatedTopK",
    pointer: "/memory/inject/relatedTopK",
    kind: &Integer { min: 1, max: 20, default: Some(3), file: FileRule::Round },
    applies: "next wake",
    group: "memory and digest",
    label: "Recalled memories per wake",
    hint: "Maximum related memories offered to a wake.",
    chat: true,
};

pub(crate) const AUTO_FLUSH_ENABLED: Setting = Setting {
    key: "memory.autoFlush.enabled",
    pointer: "/memory/autoFlush/enabled",
    kind: &Switch::new(true),
    applies: "live",
    group: "memory and digest",
    label: "Scheduled digest",
    hint: "Enables the daily digest and the backlog check.",
    chat: true,
};

pub(crate) const AUTO_FLUSH_CONTEXT_PCT: Setting = Setting {
    key: "memory.autoFlush.contextPct",
    pointer: "/memory/autoFlush/contextPct",
    kind: &Integer { min: 1, max: 100, default: Some(70), file: FileRule::JsRound },
    applies: "live",
    group: "memory and digest",
    label: "Digest check threshold",
    hint: "Percent of the recent-message limit needed by the backlog check.",
    chat: true,
};

pub(crate) const AUTO_FLUSH_MAX_SESSIONS: Setting = Setting {
    key: "memory.autoFlush.maxSessionsPerDay",
    pointer: "/memory/autoFlush/maxSessionsPerDay",
    kind: &Integer { min: 0, max: 100, default: Some(8), file: FileRule::JsTruncate },
    applies: "live",
    group: "memory and digest",
    label: "Conversations digested per run",
    hint: "Maximum number of conversations written by one run. Zero disables writes.",
    chat: true,
};

pub(crate) const AUTO_FLUSH_MIN_INTERVAL: Setting = Setting {
    key: "memory.autoFlush.minIntervalMinutes",
    pointer: "/memory/autoFlush/minIntervalMinutes",
    kind: &Integer { min: 0, max: 1440, default: Some(60), file: FileRule::JsRound },
    applies: "live",
    group: "memory and digest",
    label: "Minutes between digests",
    hint: "Minimum time between attempts for the same conversation.",
    chat: false,
};

pub(crate) const DIGEST_MAX_INPUT_CHARS: Setting = Setting {
    key: "memory.digest.maxInputChars",
    pointer: "/memory/digest/maxInputChars",
    kind: &Integer { min: 1000, max: 100000, default: Some(12_000), file: FileRule::Clamp },
    applies: "live",
    group: "memory and digest",
    label: "Digest input characters",
    hint: "Maximum characters included in a digest request.",
    chat: false,
};

pub(crate) const LAYER_IDENTITY: Setting = Setting {
    key: "memory.layers.identityTokens",
    pointer: "/memory/layers/identityTokens",
    kind: &Integer { min: 1, max: 100000, default: Some(1_500), file: FileRule::Default },
    applies: "next wake",
    group: "memory and digest",
    label: "Identity layer budget",
    hint: "Estimated tokens for the Identity layer.",
    chat: false,
};

pub(crate) const LAYER_RELATIONSHIP: Setting = Setting {
    key: "memory.layers.relationshipTokens",
    pointer: "/memory/layers/relationshipTokens",
    kind: &Integer { min: 1, max: 100000, default: Some(1_500), file: FileRule::Default },
    applies: "next wake",
    group: "memory and digest",
    label: "Relationship layer budget",
    hint: "Estimated tokens for the Relationship layer.",
    chat: false,
};

pub(crate) const LAYER_CONVERSATION: Setting = Setting {
    key: "memory.layers.conversationTokens",
    pointer: "/memory/layers/conversationTokens",
    kind: &Integer { min: 1, max: 100000, default: Some(800), file: FileRule::Default },
    applies: "next wake",
    group: "memory and digest",
    label: "Conversation layer budget",
    hint: "Estimated tokens for the Conversation layer.",
    chat: false,
};

pub(crate) const LAYER_MATERIAL: Setting = Setting {
    key: "memory.layers.materialTokens",
    pointer: "/memory/layers/materialTokens",
    kind: &Integer { min: 1, max: 100000, default: Some(1_000), file: FileRule::Default },
    applies: "next wake",
    group: "memory and digest",
    label: "Material layer budget",
    hint: "Estimated tokens for the Material layer.",
    chat: false,
};

pub(crate) const MATERIAL_ITEMS: Setting = Setting {
    key: "memory.layers.materialItems",
    pointer: "/memory/layers/materialItems",
    kind: &Integer { min: 1, max: 20, default: None, file: FileRule::Default },
    applies: "next wake",
    group: "memory and digest",
    label: "Material layer items",
    hint: "Reset to follow the configured recall depth, with five items when it is unset.",
    chat: false,
};

/// A configured integer recall depth is the inherited item limit; use five when no valid depth was set.
pub(crate) fn material_item_limit(document: &serde_json::Value) -> usize {
    MATERIAL_ITEMS.read(document).as_i64().or_else(|| {
        RELATED_TOP_K.found_in(document).and_then(|value| RELATED_TOP_K.normalize(value)).and_then(|value| value.as_i64())
    }).unwrap_or(5) as usize
}

pub(super) const SETTINGS: &[Setting] = &[
    RELATED_TOP_K,
    AUTO_FLUSH_ENABLED,
    AUTO_FLUSH_CONTEXT_PCT,
    AUTO_FLUSH_MAX_SESSIONS,
    AUTO_FLUSH_MIN_INTERVAL,
    DIGEST_MAX_INPUT_CHARS,
    LAYER_IDENTITY,
    LAYER_RELATIONSHIP,
    LAYER_CONVERSATION,
    LAYER_MATERIAL,
    MATERIAL_ITEMS,
];
