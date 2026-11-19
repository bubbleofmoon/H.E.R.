//! The turn engine group (#218): the global default engine, and the engine chosen per conversation
//! (`ConversationSetting`, the conversation scope). Neither is read to route turns yet: turns follow
//! `conversationEngines` in `config.json` (`turn_engine::Engine::selected`) until the conversation switch (#10) moves
//! routing onto them, with its drain and handover. Until then both are stored, listed and audited only (`applies`
//! `not read yet`). None is chat-writable, so the model cannot pick its own engine.

use crate::settings::kinds::Choice;
use crate::settings::{ConversationSetting, Setting};

/// The engine of every conversation without its own choice (`CONVERSATION_ENGINE`). Not read yet.
pub(crate) const ENGINE_DEFAULT: Setting = Setting {
    key: "engine.default",
    pointer: "/engineDefault",
    kind: &Choice { options: &["dsh", "harness"], default: "dsh" },
    applies: "not read yet",
    group: "turn engine",
    label: "Default turn engine",
    hint: "The engine of every conversation without its own choice (the per-conversation settings page). Stored only: no turn reads it yet; turns still follow conversationEngines in config.json until the conversation switch lands.",
    chat: false,
    secret: false,
};

/// The group's settings, in registry order.
pub(super) const SETTINGS: &[Setting] = &[ENGINE_DEFAULT];

/// The engine chosen for one conversation; a conversation without one uses `ENGINE_DEFAULT`. Not read yet.
pub(crate) const CONVERSATION_ENGINE: ConversationSetting = ConversationSetting {
    key: "conversation.engine",
    global: &ENGINE_DEFAULT,
    label: "Turn engine",
    hint: "The engine chosen for this conversation; without one, the default turn engine applies. Stored only: no turn reads it yet, and choosing it moves no session and writes no handover.",
};

/// The conversation scope's settings, in the order the page lists them.
pub(super) const CONVERSATION: &[ConversationSetting] = &[CONVERSATION_ENGINE];
