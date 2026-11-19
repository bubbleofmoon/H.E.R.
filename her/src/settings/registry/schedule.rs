//! Settings for the daily system jobs and the half-hourly digest check.

use crate::settings::kinds::{daily_parts, DailyTime, FileRule, Integer, TimeZoneName};
use crate::settings::Setting;
use serde_json::Value;

pub(crate) const DIGEST_CLOCK: DailyTime = DailyTime { hour: 3, minute: 0 };
pub(crate) const BACKUP_CLOCK: DailyTime = DailyTime { hour: 4, minute: 0 };
pub(crate) const DIGEST_CHECK_PHASE: u32 = 21;

pub(crate) const DIGEST_TIME: Setting = Setting {
    key: "memory.digest.time",
    pointer: "/memory/digest/time",
    kind: &DIGEST_CLOCK,
    applies: "live",
    group: "schedule",
    label: "Daily memory digest time",
    hint: "Local time in HH:MM. Keep at least 30 minutes between the daily digest and backup.",
    chat: false,
};

pub(crate) const DIGEST_CHECK_MINUTE: Setting = Setting {
    key: "memory.digest.checkMinute",
    pointer: "/memory/digest/checkMinute",
    kind: &Integer { min: 0, max: 29, default: Some(DIGEST_CHECK_PHASE as i64), file: FileRule::Clamp },
    applies: "live",
    group: "schedule",
    label: "Digest check minute",
    hint: "The minute within each half hour. The check cannot share the starting minute of a daily heavy job.",
    chat: false,
};

pub(crate) const BACKUP_TIME: Setting = Setting {
    key: "backup.time",
    pointer: "/backup/time",
    kind: &BACKUP_CLOCK,
    applies: "live",
    group: "schedule",
    label: "Daily backup time",
    hint: "Local time in HH:MM. Keep at least 30 minutes between the backup and daily digest.",
    chat: false,
};

pub(crate) const TIME_ZONE: Setting = Setting {
    key: "time.zone",
    pointer: "/time/zone",
    kind: &TimeZoneName,
    applies: "live",
    group: "schedule",
    label: "Time zone",
    hint: "Use host to follow the machine, or enter an IANA name such as Asia/Shanghai.",
    chat: false,
};

pub(super) const SETTINGS: &[Setting] = &[DIGEST_TIME, DIGEST_CHECK_MINUTE, BACKUP_TIME, TIME_ZONE];
const HEAVY_JOB_GAP_MINUTES: u32 = 30;

pub(crate) fn daily(setting: &Setting, document: &Value) -> (u32, u32) {
    setting.read(document).as_str().and_then(daily_parts).expect("validated daily time")
}

pub(crate) fn checks_schedule(key: &str) -> bool {
    [DIGEST_TIME.key, BACKUP_TIME.key, DIGEST_CHECK_MINUTE.key].contains(&key)
}

/// Validates the complete proposed set, including the unchanged times.
pub(crate) fn check_schedule(document: &Value) -> Result<(), (u16, String)> {
    let minutes = |setting| {
        let (hour, minute) = daily(setting, document);
        hour * 60 + minute
    };
    let digest = minutes(&DIGEST_TIME);
    let backup = minutes(&BACKUP_TIME);
    let apart = digest.abs_diff(backup);
    if apart.min(1440 - apart) < HEAVY_JOB_GAP_MINUTES {
        return Err((409, "Daily digest and backup must be at least 30 minutes apart".into()));
    }
    let check = DIGEST_CHECK_MINUTE.integer(document) as u32;
    if [digest, backup].iter().any(|minute| minute % 30 == check) {
        return Err((409, "Digest check cannot share the starting minute of the daily digest or backup".into()));
    }
    Ok(())
}
