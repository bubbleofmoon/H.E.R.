//! A named IANA time zone, or the host's time zone.

use super::SettingKind;
use jiff::tz::TimeZone;
use serde_json::{json, Value};

pub(crate) struct TimeZoneName;

impl SettingKind for TimeZoneName {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let name = value.as_str()?;
        if name == "host" {
            return Some(json!("host"));
        }
        if name.is_empty() || name.len() > 128 || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'+' | b'-')
        }) {
            return None;
        }
        TimeZone::get(name).ok()?.iana_name().map(|name| json!(name))
    }

    fn default_value(&self) -> Value {
        json!("host")
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be host or an IANA time-zone name")
    }

    fn view(&self) -> Value {
        json!({"type":"time zone"})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 只能在控制台修改")
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.read(&json!({})), json!("host"));
        assert_eq!(setting.normalize(&json!("host")), Some(json!("host")));
        assert_eq!(setting.normalize(&json!("Asia/Shanghai")), Some(json!("Asia/Shanghai")));
        for bad in [json!("../UTC"), json!(" GMT"), json!("Etc/Absent"), json!("x".repeat(129)), json!(true)] {
            assert_eq!(setting.normalize(&bad), None, "{bad}");
        }
    }
}
