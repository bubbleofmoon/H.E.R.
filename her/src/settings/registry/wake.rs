//! Global wake and turn timing.

use crate::settings::kinds::{FileRule, Integer, Probability, Switch};
use crate::settings::Setting;

pub(crate) const TURN_LIMIT: i64 = 7_200_000;
pub(crate) const CEILING_DEFAULT: i64 = 1_800_000;
pub(crate) const PRE_SLEEP_DEFAULT: i64 = 300_000;
// Largest integer exactly representable by the console's JavaScript number input.
const SAFE_INTEGER: i64 = (1_i64 << 53) - 1;

macro_rules! setting {
    ($group:literal, $member:literal, $kind:expr, $label:literal, $hint:literal, $chat:literal) => {
        Setting {
            key: concat!($group, ".", $member), pointer: concat!("/socialV2/", $group, "/", $member),
            kind: $kind, applies: "next wake", group: "wake", label: $label, hint: $hint, chat: $chat, secret: false,
        }
    };
}

pub(crate) const BATCH: Setting = setting!("wake", "batchWindowMs", &Integer { min: 1_000, max: 120_000, default: Some(8_000), file: FileRule::Clamp }, "Wake batch window", "Milliseconds for plans using the default wake policy.", true);
pub(crate) const RATE_MIN: Setting = setting!("wake", "maxWakePerMinute", &Integer { min: 0, max: 10_000, default: Some(1), file: FileRule::Default }, "Wakes per minute", "Zero disables this quota.", false);
pub(crate) const RATE_HOUR: Setting = setting!("wake", "maxWakePerHour", &Integer { min: 0, max: 10_000, default: Some(12), file: FileRule::Default }, "Wakes per hour", "Zero disables this quota.", false);
pub(crate) const NO_ACTION: Setting = setting!("wake", "noActionLimit", &Integer { min: 1, max: 10_000, default: Some(3), file: FileRule::Default }, "No-action limit", "Consecutive no-action turns before resetting the wake plan.", false);
pub(crate) const REMINDERS: Setting = setting!("wake", "maxWakeConfigReminders", &Integer { min: 1, max: 10_000, default: Some(2), file: FileRule::Default }, "Closure reminder limit", "Maximum closure attempts including the original turn.", false);
pub(crate) const PRE_SLEEP: Setting = setting!("wake", "preSleepWaitMs", &Integer { min: 0, max: TURN_LIMIT, default: Some(PRE_SLEEP_DEFAULT), file: FileRule::TruncateAndClamp }, "Pre-sleep observation", "Milliseconds to watch before sleeping.", false);
pub(crate) const PRE_SLEEP_ENABLED: Setting = setting!("wake", "preSleepWaitEnabled", &Switch::new(true), "Pre-sleep observation enabled", "Enables the pre-sleep gate after a send.", false);
pub(crate) const PREEMPT: Setting = setting!("wake", "preemptAfterMs", &Integer { min: 0, max: TURN_LIMIT, default: Some(0), file: FileRule::Default }, "Preemption age", "Milliseconds before an urgent wake can interrupt a turn. Zero disables preemption.", false);
pub(crate) const HARD_CAP: Setting = setting!("wake", "turnHardCapMs", &Integer { min: 1, max: TURN_LIMIT, default: None, file: FileRule::Automatic }, "Turn hard cap", "Milliseconds. Reset to derive the cap from observation and closing time.", false);
pub(crate) const CEILING: Setting = setting!("wake", "turnCeilingMs", &Integer { min: 1, max: TURN_LIMIT, default: Some(CEILING_DEFAULT), file: FileRule::Default }, "Turn ceiling", "Absolute milliseconds from dispatch.", false);
pub(crate) const QUESTION: Setting = setting!("wake", "questionExchangeWindowMs", &Integer { min: 1, max: SAFE_INTEGER, default: Some(90_000), file: FileRule::IntegralNumber }, "Question exchange window", "Milliseconds that recent directed replies can qualify a question.", false);
pub(crate) const RECOMMENDED_PROBABILITY: Setting = setting!("wake", "recommendedProbability", &Probability { default: 0.0 }, "Default message-wake probability", "Chance for an ordinary message under an inherited wake plan.", false);
pub(crate) const PROACTIVE_ENABLED: Setting = setting!("proactive", "enabled", &Switch::new(true), "Proactive wake enabled", "Allows an idle proactive wake.", false);
pub(crate) const PROACTIVE_MIN: Setting = setting!("proactive", "checkIntervalMinMs", &Integer { min: 60_000, max: 86_400_000, default: Some(1_800_000), file: FileRule::Clamp }, "Proactive minimum interval", "Shortest proactive check interval in milliseconds.", false);
pub(crate) const PROACTIVE_MAX: Setting = setting!("proactive", "checkIntervalMaxMs", &Integer { min: 60_000, max: 86_400_000, default: Some(5_400_000), file: FileRule::Clamp }, "Proactive maximum interval", "Longest proactive check interval in milliseconds.", false);
pub(crate) const PROACTIVE_IDLE: Setting = setting!("proactive", "idleThresholdMs", &Integer { min: 0, max: 86_400_000, default: Some(900_000), file: FileRule::Default }, "Proactive idle threshold", "Milliseconds of inactivity required for a proactive wake.", false);
pub(crate) const PROACTIVE_PROBABILITY: Setting = setting!("proactive", "probability", &Probability { default: 0.4 }, "Proactive wake probability", "Chance of a proactive wake when eligible.", false);

pub(crate) const WAIT_MIN: Setting = setting!("wait", "minMs", &Integer { min: 1, max: TURN_LIMIT, default: Some(5_000), file: FileRule::Default }, "Minimum tool wait", "Milliseconds before a messages wait may time out.", false);
pub(crate) const WAIT_MAX: Setting = setting!("wait", "maxMs", &Integer { min: 1, max: TURN_LIMIT, default: Some(600_000), file: FileRule::Default }, "Maximum tool wait", "Milliseconds that cap a caller-requested timeout.", false);
pub(crate) const WAIT_QUIET: Setting = setting!("wait", "minQuietAfterNewMs", &Integer { min: 0, max: TURN_LIMIT, default: Some(10_000), file: FileRule::Clamp }, "Quiet after new messages", "Milliseconds of additional silence after messages arrive.", false);
pub(crate) const WAIT_BATCH_MAX: Setting = setting!("wait", "batchMaxMs", &Integer { min: 1, max: 120_000, default: Some(30_000), file: FileRule::Default }, "Wait batch maximum", "Maximum milliseconds from the first arrival in a messages wait.", false);

pub(crate) const TURN_FIELDS: &[Setting] = &[CEILING, HARD_CAP, PRE_SLEEP, WAIT_MIN, WAIT_MAX, WAIT_QUIET, WAIT_BATCH_MAX];

pub(crate) const SETTINGS: &[Setting] = &[
    WAIT_MIN, WAIT_MAX, WAIT_QUIET, WAIT_BATCH_MAX,
    BATCH, RATE_MIN, RATE_HOUR, NO_ACTION, REMINDERS, PRE_SLEEP, PRE_SLEEP_ENABLED, PREEMPT, HARD_CAP, CEILING, QUESTION, RECOMMENDED_PROBABILITY, PROACTIVE_ENABLED, PROACTIVE_MIN, PROACTIVE_MAX, PROACTIVE_IDLE, PROACTIVE_PROBABILITY,
];

pub(crate) fn refreshes_default_plan(key: &str) -> bool {
    [BATCH.key, RECOMMENDED_PROBABILITY.key].contains(&key)
}
