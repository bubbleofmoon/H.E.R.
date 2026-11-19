//! The registry: one table of settings per settings group (`registry/`), joined by `registry()`. The settings kept per
//! conversation (#218) are a scope of their own (`conversation_registry()`), never in `registry()`.

mod access;
mod alerts;
mod engine;
mod media;
mod model;
mod prompt;
mod reply;
mod stickers;
mod tools;
mod voice;

pub(crate) use media::{MEDIA_BYTES, MEDIA_COUNT, MEDIA_PIXELS};
pub(crate) use stickers::{COLLECT_BYTES, COLLECT_ENABLED, COLLECT_HOUR, COLLECT_MINUTE, COLLECT_REMARK,
    STICKERS_ENABLED, STICKERS_IN_PROMPT, STICKERS_LIST, STICKERS_PROMPT_COUNT, STICKERS_TTL};
pub(crate) use tools::{tool_enabled, tool_switches};
pub(crate) use voice::{VOICE_AUTO_READBACK, VOICE_ENABLED, VOICE_MAX_BYTES, VOICE_MAX_HOUR, VOICE_MAX_MINUTE, VOICE_MAX_SECONDS};

pub(crate) use alerts::{ALERT_HOURLY_CAP, ALERT_KEY_WINDOW, GATEWAY_SILENCE_WARN};
pub(crate) use access::{INTERCEPT_NOTIFY, OUTBOUND_GUARD, OWNER, SECURITY_GROUP};
pub(crate) use engine::{CONVERSATION_ENGINE, ENGINE_DEFAULT};
pub(crate) use model::{
    CONVERSATION_ROUTE, DIGEST_MODEL, DIGEST_TIMEOUT_SECS, HARNESS_FIRST_ROUTE, HARNESS_MAX_STEPS, HARNESS_MAX_TOKENS,
    HARNESS_MODEL, HARNESS_ROUTES, WEB_SEARCH_MODEL,
};

use super::{ConversationSetting, Setting};

/// The group tables in registry order, which is the order the store seeds them and the settings page lists them. A
/// group a later change adds is one more table here; the registry test checks that keys and pointers stay unique.
const GROUPS: &[&[Setting]] = &[
    reply::SETTINGS, model::SETTINGS, prompt::SETTINGS, access::SETTINGS, engine::SETTINGS,
];

/// Settings whose effective values are also materialized on the runtime configuration document.
pub(super) fn live_batch() -> impl Iterator<Item = &'static Setting> {
    [tools::SETTINGS, voice::SETTINGS, stickers::SETTINGS, media::SETTINGS, alerts::SETTINGS]
        .into_iter()
        .flatten()
}

/// Every setting kept per conversation, in the order the per-conversation page lists them. Each falls back to its
/// global setting (`ConversationSetting::global`), which is in `registry()`.
pub(crate) fn conversation_registry() -> impl Iterator<Item = &'static ConversationSetting> {
    engine::CONVERSATION.iter().chain(model::CONVERSATION.iter())
}

/// The conversation-scoped setting with key `key`.
pub(crate) fn find_conversation(key: &str) -> Option<&'static ConversationSetting> {
    conversation_registry().find(|setting| setting.key == key)
}

/// Every setting the store accepts, in registry order. A key that is not here is refused by `apply` and ignored by
/// `overlay`. The prompt text settings (#140) are read by `prompt_text::text` and are never chat-writable, so the model
/// cannot rewrite its own prompt; neither are the security settings (#154), so it cannot widen its own access.
pub(crate) fn registry() -> impl Iterator<Item = &'static Setting> {
    GROUPS.iter().flat_map(|group| group.iter()).chain(live_batch())
}

/// The registry setting with key `key`.
pub(crate) fn find(key: &str) -> Option<&'static Setting> {
    registry().find(|setting| setting.key == key)
}

/// The registry setting the owner's chat names `name` (`Setting::chat_name`), when the chat may use it.
pub(crate) fn chat_setting(name: &str) -> Option<&'static Setting> {
    registry().find(|setting| setting.chat && setting.chat_name() == name)
}
