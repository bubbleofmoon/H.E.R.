//! Limits for owner notifications and gateway silence warnings.

use crate::settings::{kinds::{FileRule, Integer}, Setting};

pub(crate) const ALERT_KEY_WINDOW: Setting = Setting {
    key: "alerts.keyWindowSecs",
    pointer: "/alerts/keyWindowSecs",
    kind: &Integer { min: 1, max: 3_600, default: 60, file: FileRule::Clamp },
    applies: "live",
    group: "alerts",
    label: "Alert deduplication",
    hint: "Seconds between notifications with the same key for this runtime.",
    chat: false,
};

pub(crate) const ALERT_HOURLY_CAP: Setting = Setting {
    key: "alerts.hourlyCap",
    pointer: "/alerts/hourlyCap",
    kind: &Integer { min: 1, max: 100, default: 10, file: FileRule::Clamp },
    applies: "live",
    group: "alerts",
    label: "Alerts per hour",
    hint: "Maximum per process in a sliding hour, for each notification budget.",
    chat: false,
};

pub(crate) const GATEWAY_SILENCE_WARN: Setting = Setting {
    key: "snowluma.silenceWarnMs",
    pointer: "/snowluma/silenceWarnMs",
    kind: &Integer { min: 60_000, max: 86_400_000, default: 600_000, file: FileRule::PositiveClamp },
    applies: "live",
    group: "alerts",
    label: "Gateway silence warning",
    hint: "Milliseconds without an event packet before the dependency watch reports silence.",
    chat: false,
};

pub(super) const SETTINGS: &[Setting] = &[ALERT_KEY_WINDOW, ALERT_HOURLY_CAP, GATEWAY_SILENCE_WARN];
