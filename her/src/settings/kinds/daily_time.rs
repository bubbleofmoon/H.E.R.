//! A recurring civil time, written as five ASCII characters.

use super::SettingKind;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
pub(crate) struct DailyTime {
    pub hour: u32,
    pub minute: u32,
}

/// Parses exactly HH:MM, after checking the byte boundaries.
pub(crate) fn daily_parts(text: &str) -> Option<(u32, u32)> {
    let [a, b, b':', c, d] = *text.as_bytes() else { return None };
    if ![a, b, c, d].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let hour = u32::from(a - b'0') * 10 + u32::from(b - b'0');
    let minute = u32::from(c - b'0') * 10 + u32::from(d - b'0');
    (hour < 24 && minute < 60).then_some((hour, minute))
}

impl SettingKind for DailyTime {
    fn normalize(&self, value: &Value) -> Option<Value> {
        daily_parts(value.as_str()?)?;
        Some(value.clone())
    }

    fn default_value(&self) -> Value {
        json!(format!("{:02}:{:02}", self.hour, self.minute))
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be HH:MM (00:00-23:59)")
    }

    fn view(&self) -> Value {
        json!({"type": "daily time"})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 只能在控制台修改")
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        let default = setting.default_value();
        assert_eq!(setting.normalize(&default), Some(default.clone()));
        assert_eq!(setting.read(&json!({})), default);
        for good in [json!("00:00"), json!("23:59")] {
            assert_eq!(setting.normalize(&good), Some(good));
        }
        for bad in [json!("24:00"), json!("23:60"), json!("3:00"), json!(" 03:00"), json!("零:00"), json!(300)] {
            assert_eq!(setting.normalize(&bad), None, "{bad}");
        }
    }
}
