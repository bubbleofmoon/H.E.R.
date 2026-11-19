//! The reply rhythm group.

use crate::settings::kinds::{FileRule, Integer};
use crate::settings::Setting;

/// Read by `wake_cfg_of`, which keeps its own default and bounds; they agree with these (registry test).
const WAKE_QUIET: Setting = Setting {
    key: "reply.wakeQuietMs",
    pointer: "/socialV2/reply/wakeQuietMs",
    kind: &Integer { min: 1, max: 30_000, default: 3_000, file: FileRule::Default },
    applies: "next wake",
    group: "reply rhythm",
    label: "Quiet time before a message wake",
    hint: "Milliseconds without a new message before a message wake starts; each new message restarts the wait.",
    chat: true,
    secret: false,
};

/// The group's settings, in registry order.
pub(super) const SETTINGS: &[Setting] = &[WAKE_QUIET];
