use super::*;
use crate::tasks::{self, Schedule, Scheduler, TaskId};
use jiff::{tz::TimeZone, Timestamp};

fn stamp(text: &str) -> i64 {
    text.parse::<Timestamp>().unwrap().as_millisecond()
}

#[test]
fn time_zone_reaches_the_next_model_prompt_and_reschedules_the_daily_job() {
    let (_root, context) = settings_fixture("live schedule zone");
    let (status, body) = post(&context, "/api/settings/set", json!({"setting":"time.zone","value":"America/New_York"}));
    assert_eq!(status, 200, "{body}");
    let cfg = context.load_config().unwrap();
    let now = stamp("2026-01-05T08:00:00Z");
    let prompt = crate::prompt::wrap_for_delivery_with_runtime(&context, "fixture", &cfg, now);
    assert!(prompt.starts_with("【当前时间】2026/1/5 03:00:00（America/New_York）"), "prompt must use the saved zone: {prompt}");
    let mut scheduler = Scheduler::new(|def| def.id == TaskId::Backup, TimeZone::UTC, now);
    let zone = crate::local_time::configured_zone(&cfg);
    scheduler.set_zone(zone.clone(), now);
    assert_eq!(scheduler.next_run(TaskId::Backup), Some(stamp("2026-01-05T09:00:00Z")));
    assert_eq!(post(&context, "/api/settings/reset", json!({"setting":"time.zone"})).0, 200);
    assert_eq!(setting_view(&context, "time.zone")["stored"], "host");
    assert_eq!(crate::local_time::configured_zone(&context.load_config().unwrap()),
               crate::local_time::configured_zone(&json!({})));
}

#[test]
fn daily_and_interval_jobs_handle_gaps_and_folds() {
    let zone = TimeZone::get("America/New_York").unwrap();
    let spring = stamp("2026-03-08T06:59:59Z");
    let daily = Schedule::Daily { hour: 2, minute: 30 };
    assert_eq!(tasks::next_due(daily, stamp("2026-03-08T06:00:00Z"), &zone), Some(stamp("2026-03-09T06:30:00Z")), "daily slot in a gap is skipped");
    let fold = Schedule::Daily { hour: 1, minute: 30 };
    let first = stamp("2026-11-01T05:30:00Z");
    assert_eq!(tasks::next_due(fold, first - 1, &zone), Some(first));
    assert_eq!(tasks::next_due(fold, first, &zone), Some(stamp("2026-11-02T06:30:00Z")));

    let every = Schedule::Every { minutes: 30, offset: 0 };
    assert_eq!(tasks::next_due(every, spring, &zone), Some(stamp("2026-03-08T07:00:00Z")));
    assert_eq!(tasks::next_due(every, first, &zone), Some(stamp("2026-11-01T06:00:00Z")));
    assert_eq!(tasks::next_due(daily, i64::MAX, &zone), None);
}

#[test]
fn schedule_page_resolves_wall_input_in_the_runtime_zone() {
    let (_root, context) = settings_fixture("schedule wall input");
    assert_eq!(post(&context, "/api/settings/set", json!({"setting":"time.zone","value":"Asia/Kathmandu"})).0, 200);
    let now = crate::parity_clock::now_ms();
    let date = crate::local_time::at(now, &TimeZone::UTC).date().tomorrow().unwrap();
    let civil = format!("{date}T12:00");
    let (status, body) = post(&context, "/api/schedule", json!({"conversation":"group:101","text":"fixture","localAt":civil}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["nextAt"], stamp(&format!("{date}T06:15:00Z")), "wall input must use the runtime zone");
    let (status, view) = admin_request(&context, "GET", "/api/schedule", None);
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["timeZone"]["offsetSeconds"], 20_700);
}

#[test]
fn stored_instants_keep_utc_format_and_explicit_offsets() {
    let ms = crate::wake::parse_iso_ms("1970-01-01T05:44:59.999+05:45").unwrap();
    assert_eq!(ms, -1, "an explicit offset must be applied");
    assert_eq!(crate::sticker::iso_at(ms), "1969-12-31T23:59:59.999Z");
    assert_eq!(crate::wake::parse_iso_ms("1970-01-01T00:00"), Some(0));
    assert_eq!(crate::wake::parse_iso_ms("2026-02-29T00:00:00Z"), None);
}
