//! The registry: one table of settings per settings group (`registry/`), joined by `registry()`. The settings kept per
//! conversation (#218) are a scope of their own (`conversation_registry()`), never in `registry()`.

mod access;
mod engine;
mod memory;
mod model;
mod prompt;
mod reply;

pub(crate) use access::{INTERCEPT_NOTIFY, OUTBOUND_GUARD, OWNER, SECURITY_GROUP};
#[cfg(test)]
pub(crate) use engine::{CONVERSATION_ENGINE, ENGINE_DEFAULT};
pub(crate) use memory::{material_item_limit,
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
};
pub(crate) use model::{
    CONVERSATION_ROUTE, DIGEST_MODEL, DIGEST_TIMEOUT_SECS, HARNESS_FIRST_ROUTE, HARNESS_MAX_STEPS, HARNESS_MAX_TOKENS,
    HARNESS_MODEL, WEB_SEARCH_MODEL,
};

use super::{ConversationSetting, Setting};

/// The group tables in registry order, which is the order the store seeds them and the settings page lists them. A
/// group a later change adds is one more table here; the registry test checks that keys and pointers stay unique.
const GROUPS: [&[Setting]; 6] = [
    reply::SETTINGS,
    model::SETTINGS,
    prompt::SETTINGS,
    access::SETTINGS,
    engine::SETTINGS,
    memory::SETTINGS,
];

/// Global settings whose defaults and file normalization are applied by the runtime loader.
pub(super) const RESOLVED_GROUPS: &[&[Setting]] = &[memory::SETTINGS];

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
    GROUPS.into_iter().flatten()
}

/// The registry setting with key `key`.
pub(crate) fn find(key: &str) -> Option<&'static Setting> {
    registry().find(|setting| setting.key == key)
}

/// The registry setting the owner's chat names `name` (`Setting::chat_name`), when the chat may use it.
pub(crate) fn chat_setting(name: &str) -> Option<&'static Setting> {
    registry().find(|setting| setting.chat && setting.chat_name() == name)
}
