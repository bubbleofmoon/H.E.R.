//! Mode profiles: per participation mode or harness job, the exact tool menu the harness offers and
//! who may use it. A turn's admission captures its profile (`Tools::capture`), and every tool call
//! rechecks it against the current mode, owner and tool switches (`tools.rs`). Each profile a
//! harness turn admits also names its mode prompt (#140, `prompt_key`), a text setting the turn's
//! system message starts with. The reply policy (#159) is not a column of this table yet.
//!
//! Menus hold only tools HER registers and handles in process (`mcp::tool_list`,
//! `aux_mcp::tool_list`): the bare names of the tools DSH's presets allow under the
//! `mcp__snowluma__`, `mcp__snowluma-host__` and `mcp__web-search-safe__` namespaces. The
//! configuration's tool switches narrow every menu further (`qq_tools::registered`). No menu holds
//! a shell, file, plugin or agent tool.
use serde_json::Value;

/// One row of the mode-profile table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    /// `chat` and `reserved` modes: groups and every private chat but the owner's.
    Chat,
    /// `chat` and `reserved` modes: the owner's private chat.
    OwnerPrivate,
    /// `closed-agent` mode (the Admin agent): only the owner's private chat.
    ClosedAgent,
    /// `reserved2` mode: groups and every private chat but the owner's.
    Reserved2,
    /// `reserved2` mode: the owner's private chat.
    Reserved2Owner,
    /// The nightly digest's menu. The digest runs as a tool-less one-shot job (`harness/one_shot.rs`), so this row
    /// is not used yet.
    #[cfg_attr(not(test), allow(dead_code))]
    Digest,
    /// The slang learner's research menu with the read-only web tools. Slang learning runs as a tool-less one-shot
    /// job (`harness/one_shot.rs`), so this row is not used yet.
    #[cfg_attr(not(test), allow(dead_code))]
    SlangLearner,
}

/// `chat` mode offers no QQ tool but the status read: the bridge relays replies, and the console
/// refuses the direct sends, the legacy reads, the image lookup and the reserved2 tools there.
const CHAT: &[&str] = &[
    "qq_status",
    "memory_write",
    "memory_search",
    "memory_index",
    "memory_forget",
    "web_search",
    "web_fetch",
    "snowluma_status",
];

/// `CHAT` plus the owner-only ACL and settings tools.
const OWNER_PRIVATE: &[&str] = &[
    "qq_status",
    "memory_write",
    "memory_search",
    "memory_index",
    "memory_forget",
    "memory_acl",
    "settings_get",
    "settings_set",
    "web_search",
    "web_fetch",
    "snowluma_status",
];

/// The legacy reads the console serves in `closed-agent`, the direct private send (the console
/// accepts the turn's bound token only for the owner's own chat), the tokenless image lookup,
/// memory, the owner-only tools, the auxiliary tools and the owner-approved group send.
const CLOSED_AGENT: &[&str] = &[
    "qq_status",
    "qq_list_groups",
    "qq_get_group_members",
    "qq_get_group_history",
    super::interactions::GROUP_SEND,
    "qq_send_private_message",
    "qq_get_message_images",
    "memory_write",
    "memory_search",
    "memory_index",
    "memory_forget",
    "memory_acl",
    "settings_get",
    "settings_set",
    "web_search",
    "web_fetch",
    "snowluma_status",
    super::interactions::ASK,
];

/// Every token-bound social tool and direct send the console serves in `reserved2`; never the
/// closed-agent legacy reads.
const RESERVED2: &[&str] = &[
    "qq_status",
    "qq_send_group_message",
    "qq_reply",
    "qq_send_private_message",
    "qq_get_prompt",
    "qq_get_unread_messages",
    "qq_get_recent_messages",
    "qq_social_state",
    "qq_mark_read",
    "qq_set_wake_config",
    "qq_send_burst",
    "qq_send_message",
    "qq_send_poke",
    "qq_wait_for_messages",
    "qq_report_feedback",
    "qq_get_my_recent_messages",
    "qq_get_message_detail",
    "qq_get_active_members",
    "qq_memory_append",
    "qq_memory_query",
    "qq_memory_remove",
    "qq_memory_clear",
    "qq_slang_query",
    "qq_slang_submit",
    "qq_get_message_images",
    "qq_list_stickers",
    "qq_get_sticker_image",
    "qq_send_sticker",
    "qq_collect_sticker",
    "qq_send_voice",
    "qq_list_voices",
    "qq_get_self_image",
    "qq_sticker_note",
    "qq_set_sticker_remark",
    "qq_get_forward_msg",
    "memory_write",
    "memory_search",
    "memory_index",
    "memory_forget",
    "web_search",
    "web_fetch",
    "snowluma_status",
    super::reminders::CREATE,
    super::reminders::LIST,
    super::reminders::CHANGE,
    super::reminders::CANCEL,
];

/// `RESERVED2` plus the owner-only ACL and settings tools.
const RESERVED2_OWNER: &[&str] = &[
    "qq_status",
    "qq_send_group_message",
    "qq_reply",
    "qq_send_private_message",
    "qq_get_prompt",
    "qq_get_unread_messages",
    "qq_get_recent_messages",
    "qq_social_state",
    "qq_mark_read",
    "qq_set_wake_config",
    "qq_send_burst",
    "qq_send_message",
    "qq_send_poke",
    "qq_wait_for_messages",
    "qq_report_feedback",
    "qq_get_my_recent_messages",
    "qq_get_message_detail",
    "qq_get_active_members",
    "qq_memory_append",
    "qq_memory_query",
    "qq_memory_remove",
    "qq_memory_clear",
    "qq_slang_query",
    "qq_slang_submit",
    "qq_get_message_images",
    "qq_list_stickers",
    "qq_get_sticker_image",
    "qq_send_sticker",
    "qq_collect_sticker",
    "qq_send_voice",
    "qq_list_voices",
    "qq_get_self_image",
    "qq_sticker_note",
    "qq_set_sticker_remark",
    "qq_get_forward_msg",
    "memory_write",
    "memory_search",
    "memory_index",
    "memory_forget",
    "memory_acl",
    "settings_get",
    "settings_set",
    "web_search",
    "web_fetch",
    "snowluma_status",
    super::reminders::CREATE,
    super::reminders::LIST,
    super::reminders::CHANGE,
    super::reminders::CANCEL,
];

/// The digest is one completion over stored evidence.
const DIGEST: &[&str] = &[];

/// The slang learner researches candidates with the read-only web tools.
const SLANG_LEARNER: &[&str] = &["web_search", "web_fetch"];

impl Profile {
    /// The profile of a turn the pipeline admitted under `mode` for `conversation`; `None` when no
    /// profile serves them, which refuses the turn. `mode` is the stored mode name (`chat`,
    /// `closed-agent`, `reserved`, `reserved2`); an alias or another spelling has no profile.
    pub(crate) fn of_conversation(cfg: &Value, mode: &str, conversation: &str) -> Option<Self> {
        let owner = crate::chat::is_owner_key(cfg, conversation);
        match mode {
            "chat" | "reserved" if owner => Some(Self::OwnerPrivate),
            "chat" | "reserved" => Some(Self::Chat),
            "closed-agent" if owner => Some(Self::ClosedAgent),
            "reserved2" if owner => Some(Self::Reserved2Owner),
            "reserved2" => Some(Self::Reserved2),
            _ => None,
        }
    }

    /// The exact tool names this profile may offer, in registration order.
    pub(crate) fn menu(self) -> &'static [&'static str] {
        match self {
            Self::Chat => CHAT,
            Self::OwnerPrivate => OWNER_PRIVATE,
            Self::ClosedAgent => CLOSED_AGENT,
            Self::Reserved2 => RESERVED2,
            Self::Reserved2Owner => RESERVED2_OWNER,
            Self::Digest => DIGEST,
            Self::SlangLearner => SLANG_LEARNER,
        }
    }

    /// Shared profile and live capability rules for discovered tools.
    pub(crate) fn allows_tool(self, cfg: &Value, conversation: Option<&str>, name: &str) -> bool {
        self.menu().contains(&name)
            && (!matches!(self, Self::Reserved2 | Self::Reserved2Owner)
                || crate::social_state::reserved2_menu_tool(cfg, conversation, name))
            && match name {
                "qq_send_private_message" => crate::settings::tool_enabled(cfg, "sendPrivate"),
                "qq_send_group_message" => crate::settings::tool_enabled(cfg, "sendGroup"),
                "qq_reply" => crate::settings::tool_enabled(cfg, "reply"),
                "memory_acl" | "settings_get" | "settings_set" =>
                    crate::social_state::owner_conversation(cfg)
                        .is_some_and(|owner| conversation.is_none_or(|key| key == owner)),
                _ => true,
            }
    }

    /// The text setting holding this profile's mode prompt, if it has one.
    pub(crate) fn prompt_key(self) -> Option<&'static str> {
        match self {
            Self::Chat => Some(crate::prompt_text::MODE_CHAT_KEY),
            Self::OwnerPrivate => Some(crate::prompt_text::MODE_OWNER_PRIVATE_KEY),
            Self::ClosedAgent => Some(crate::prompt_text::MODE_CLOSED_AGENT_KEY),
            Self::Reserved2 => Some(crate::prompt_text::MODE_RESERVED2_KEY),
            Self::Reserved2Owner => Some(crate::prompt_text::MODE_RESERVED2_OWNER_KEY),
            Self::Digest | Self::SlangLearner => None,
        }
    }
}
