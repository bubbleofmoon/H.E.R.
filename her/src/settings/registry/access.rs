//! The security group (#154): the allow and deny lists, the owner and the outbound guard switches. Every change needs
//! an explicit confirmation (`Setting::confirmed`); a change of a list or the owner revokes like a whitelist write
//! (`Setting::access`). None is chat-writable, so the model cannot widen its own access.

use crate::settings::kinds::{IdList, OwnerId, Switch};
use crate::settings::Setting;

/// The group of the security settings.
pub(crate) const SECURITY_GROUP: &str = "security";

/// Read by `config::canonical_config` (and so by every access check) through the settings overlay.
const ALLOW_PRIVATE: Setting = Setting {
    key: "allow.private",
    pointer: "/allow/private",
    kind: &IdList { alias: Some("/allow/privates") },
    applies: "live",
    group: SECURITY_GROUP,
    label: "Allowed private chats",
    hint: "QQ ids whose private messages are answered, unless denied. A change revokes every conversation that loses access before the reply. Needs confirmation.",
    chat: false,
    secret: false,
};

const ALLOW_GROUPS: Setting = Setting {
    key: "allow.groups",
    pointer: "/allow/groups",
    kind: &IdList { alias: Some("/allow/group") },
    applies: "live",
    group: SECURITY_GROUP,
    label: "Allowed groups",
    hint: "Group ids that are answered, unless denied; rollout.rustGroups must also serve them. A change revokes every conversation that loses access before the reply. Needs confirmation.",
    chat: false,
    secret: false,
};

const DENY_PRIVATE: Setting = Setting {
    key: "deny.private",
    pointer: "/deny/private",
    kind: &IdList { alias: Some("/deny/privates") },
    applies: "live",
    group: SECURITY_GROUP,
    label: "Denied private chats",
    hint: "QQ ids never answered in private, even when allowed. A change revokes every conversation that loses access before the reply. Needs confirmation.",
    chat: false,
    secret: false,
};

const DENY_GROUPS: Setting = Setting {
    key: "deny.groups",
    pointer: "/deny/groups",
    kind: &IdList { alias: Some("/deny/group") },
    applies: "live",
    group: SECURITY_GROUP,
    label: "Denied groups",
    hint: "Group ids never answered, even when allowed. A change revokes every conversation that loses access before the reply. Needs confirmation.",
    chat: false,
    secret: false,
};

/// While the admin agent mode is on, the owner it was started with wins (`RuntimeContext::load_config`); the settings
/// list shows it as `overriddenBy`.
pub(crate) const OWNER: Setting = Setting {
    key: "ownerQQ",
    pointer: "/ownerQQ",
    kind: &OwnerId,
    applies: "live",
    group: SECURITY_GROUP,
    label: "Owner",
    hint: "The owner's QQ id; reset for no owner. While the admin agent mode is on, the owner it was started with wins. A change revokes every conversation that loses access before the reply. Needs confirmation.",
    chat: false,
    secret: false,
};

/// Read per blocked reply by `pipeline::Pipeline::alert_blocked_reply`.
pub(crate) const INTERCEPT_NOTIFY: Setting = Setting {
    key: "security.interceptNotify",
    pointer: "/security/interceptNotify",
    kind: &Switch { default: true },
    applies: "live",
    group: SECURITY_GROUP,
    label: "Notify blocked replies",
    hint: "Whether the owner is told when the outbound guard blocks a reply. Needs confirmation.",
    chat: false,
    secret: false,
};

/// Read per reply and per console text or voice request by `pipeline::outbound_guard_enabled`.
pub(crate) const OUTBOUND_GUARD: Setting = Setting {
    key: "security.outboundGuard",
    pointer: "/security/outboundGuard",
    kind: &Switch { default: true },
    applies: "live",
    group: SECURITY_GROUP,
    label: "Outbound guard",
    hint: "Off stops the outbound sentinel, sensitive-reply hiding and blocked-reply notices. Needs confirmation.",
    chat: false,
    secret: false,
};

/// The group's settings, in registry order.
pub(super) const SETTINGS: &[Setting] =
    &[ALLOW_PRIVATE, ALLOW_GROUPS, DENY_PRIVATE, DENY_GROUPS, OWNER, INTERCEPT_NOTIFY, OUTBOUND_GUARD];
