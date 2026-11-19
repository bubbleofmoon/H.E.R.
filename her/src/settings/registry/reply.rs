//! Global reply timing.

use crate::settings::kinds::{FileRule, Integer};
use crate::settings::Setting;

macro_rules! timing {
    ($member:literal, $max:literal, $default:literal, $label:literal, $hint:literal, $chat:literal) => {
        Setting {
            key: concat!("reply.", $member), pointer: concat!("/socialV2/reply/", $member),
            kind: &Integer { min: 1, max: $max, default: $default, file: FileRule::Default },
            applies: if $chat { "next wake" } else { "live" },
            group: "reply rhythm", label: $label, hint: $hint, chat: $chat,
        }
    };
}

pub(crate) const WAKE_QUIET: Setting = timing!("wakeQuietMs", 30_000, 3_000, "Quiet time before a message wake", "Each substantive message restarts this many milliseconds of silence.", true);
pub(crate) const QUIET_MIN: Setting = timing!("quietMinMs", 120_000, 4_000, "Reply quiet minimum", "Shortest quiet hold in milliseconds.", false);
pub(crate) const QUIET_MEDIAN: Setting = timing!("quietMedianMs", 120_000, 10_000, "Reply quiet median", "Median quiet hold in milliseconds.", false);
pub(crate) const QUIET_MAX: Setting = timing!("quietMaxMs", 120_000, 25_000, "Reply quiet maximum", "Longest quiet hold in milliseconds.", false);
pub(crate) const URGENT_MIN: Setting = timing!("urgentQuietMinMs", 120_000, 1_000, "Urgent reply quiet minimum", "Shortest quiet hold for owner messages and mentions.", false);
pub(crate) const URGENT_MEDIAN: Setting = timing!("urgentQuietMedianMs", 120_000, 3_000, "Urgent reply quiet median", "Median quiet hold for owner messages and mentions.", false);
pub(crate) const URGENT_MAX: Setting = timing!("urgentQuietMaxMs", 120_000, 6_000, "Urgent reply quiet maximum", "Longest quiet hold for owner messages and mentions.", false);

pub(crate) const WINDOWS: [[Setting; 3]; 2] = [
    [QUIET_MIN, QUIET_MEDIAN, QUIET_MAX],
    [URGENT_MIN, URGENT_MEDIAN, URGENT_MAX],
];

pub(crate) const SETTINGS: &[Setting] = &[
    WAKE_QUIET, QUIET_MIN, QUIET_MEDIAN, QUIET_MAX, URGENT_MIN, URGENT_MEDIAN, URGENT_MAX,
];
