//! The runtime's civil clock. Stored instants and protocol timestamps remain UTC.

use jiff::{tz::{Offset, TimeZone}, Timestamp, Zoned};
use serde_json::Value;

/// An explicit zone comes from the registry; host mode follows the operating system (including TZ).
pub(crate) fn configured_zone(config: &Value) -> TimeZone {
    crate::settings::TIME_ZONE.text(config)
        .filter(|name| name != "host")
        .and_then(|name| TimeZone::get(&name).ok())
        .unwrap_or_else(|| {
            let host = TimeZone::system();
            if host.is_unknown() { TimeZone::UTC } else { host }
        })
}

/// Formatting only: clamp externally supplied instants outside the civil calendar's representable range.
pub(crate) fn at(ms: i64, zone: &TimeZone) -> Zoned {
    let ms = ms.clamp(Timestamp::MIN.as_millisecond(), Timestamp::MAX.as_millisecond());
    Timestamp::from_millisecond(ms).expect("clamped timestamp").to_zoned(zone.clone())
}

pub(crate) fn iso_utc(ms: i64) -> String {
    let utc = at(ms, &TimeZone::UTC).datetime();
    format!("{}.{:03}Z", utc.strftime("%Y-%m-%dT%H:%M:%S"), utc.millisecond())
}

/// The unpadded month and day match the existing zh-CN time line.
pub(crate) fn display(time: &Zoned) -> String {
    format!("{}/{}/{} {}", time.year(), time.month(), time.day(), time.strftime("%H:%M:%S"))
}

/// Only database names or numeric offsets may appear in a clock label.
pub(crate) fn label(time: &Zoned) -> String {
    time.time_zone().iana_name()
        .filter(|name| crate::model_text_policy::denied_terms(name).is_empty())
        .map_or_else(|| format!("UTC{}", time.offset()), str::to_owned)
}

/// Parses the Schedule page's local input without accepting an offset or a shifted time in a clock gap.
pub(crate) fn parse_wall_minute(text: &str, zone: &TimeZone) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    if date.len() != 10 {
        return None;
    }
    let date = date.parse::<jiff::civil::Date>().ok()?;
    let (hour, minute) = crate::settings::daily_parts(time)?;
    let slot = zone.to_ambiguous_timestamp(date.at(hour as i8, minute as i8, 0, 0));
    if matches!(slot.offset(), jiff::tz::AmbiguousOffset::Gap { .. }) {
        return None;
    }
    Some(slot.earlier().ok()?.as_millisecond())
}

/// Fixed-offset calendar for Beijing tariff accounting.
pub(crate) fn beijing_tariff_zone() -> TimeZone {
    Offset::from_hours(8).expect("valid Beijing tariff offset").to_time_zone()
}
