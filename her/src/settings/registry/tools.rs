//! Global switches for the QQ tools. The order matches the console's tool configuration.

use crate::settings::{kinds::Switch, Setting};

macro_rules! tool {
    ($flag:literal, $default:literal) => {
        Setting {
            key: concat!("socialV2.tools.", $flag),
            pointer: concat!("/socialV2/tools/", $flag),
            kind: &Switch::new($default),
            applies: "live",
            group: "tools",
            label: $flag,
            hint: "Controls this tool wherever the current conversation's menu permits it.",
            chat: false,
        }
    };
}

pub(super) const SETTINGS: &[Setting] = &[
    tool!("getPrompt", true),
    tool!("getUnread", true),
    tool!("getRecent", true),
    tool!("socialState", true),
    tool!("sendGroup", true),
    tool!("sendPrivate", true),
    tool!("reply", true),
    tool!("sendBurst", true),
    tool!("sendMessage", true),
    tool!("waitMessages", true),
    tool!("feedback", true),
    tool!("getMyRecent", true),
    tool!("getMessageDetail", true),
    tool!("getActiveMembers", true),
    tool!("setWakeConfig", true),
    tool!("markRead", true),
    tool!("memory", true),
    tool!("slangQuery", true),
    tool!("slangSubmit", true),
    tool!("getImages", true),
    tool!("getForwardMsg", true),
    tool!("sendPoke", true),
    tool!("listStickers", true),
    tool!("getStickerImage", true),
    tool!("sendSticker", true),
    tool!("setStickerRemark", false),
    tool!("stickerNote", true),
    tool!("collectSticker", true),
    tool!("getSelfImage", true),
    tool!("sendVoice", false),
];

/// The configuration flags and their settings in registry order.
pub(crate) fn tool_switches() -> impl Iterator<Item = (&'static str, &'static Setting)> {
    SETTINGS.iter().map(|setting| {
        let (_, flag) = setting.pointer.rsplit_once('/').expect("a tool switch has a leaf pointer");
        (flag, setting)
    })
}

pub(crate) fn tool_enabled(cfg: &serde_json::Value, flag: &str) -> bool {
    tool_switches().find(|(name, _)| *name == flag).is_some_and(|(_, setting)| setting.switch(cfg))
}
