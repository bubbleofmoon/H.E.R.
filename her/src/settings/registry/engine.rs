//! The default and per-conversation turn engine settings. Only the administrator can change them.

use crate::settings::kinds::Choice;
use crate::settings::{ConversationSetting, Setting};

/// The engine of every conversation without its own choice (`CONVERSATION_ENGINE`).
pub(crate) const ENGINE_DEFAULT: Setting = Setting {
    key: "engine.default",
    pointer: "/engineDefault",
    kind: &Choice { options: &["dsh", "harness"], default: "dsh" },
    applies: "next wake",
    group: "turn engine",
    label: "Default turn engine",
    hint: "Conversations without their own choice move to this engine before their next turn.",
    chat: false,
};

/// The group's settings, in registry order.
pub(super) const SETTINGS: &[Setting] = &[ENGINE_DEFAULT];

/// The engine chosen for one conversation; a conversation without one uses `ENGINE_DEFAULT`.
pub(crate) const CONVERSATION_ENGINE: ConversationSetting = ConversationSetting {
    key: "conversation.engine",
    global: &ENGINE_DEFAULT,
    label: "Turn engine",
    hint: "Switch waits for the current turn. Without a stored choice, the default turn engine applies.",
};

/// The conversation scope's settings, in the order the page lists them.
pub(super) const CONVERSATION: &[ConversationSetting] = &[CONVERSATION_ENGINE];
