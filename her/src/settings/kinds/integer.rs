//! The integer kind: a JSON integer within bounds.

use super::SettingKind;
use serde_json::{json, Value};

/// A JSON integer from `min` to `max`, both included. `file` is how the next use reads a value the store would refuse.
#[derive(Clone, Copy)]
pub(crate) struct Integer {
    pub min: i64,
    pub max: i64,
    pub default: i64,
    pub file: FileRule,
}

/// How the next use reads an integer the store would refuse, found in `config.json` (each reader's rule before the
/// registry owned it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FileRule {
    /// It reads as the default.
    Default,
    /// Reset uses an absent override.
    Automatic,
    /// The file also accepts a whole-valued JSON decimal.
    IntegralNumber,
    /// A non-negative integer is clamped into the bounds; any other value reads as the default.
    Clamp,
    /// A non-negative number is truncated to an integer and clamped into the bounds.
    TruncateAndClamp,
}

impl SettingKind for Integer {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let Integer { min, max, .. } = *self;
        value.as_i64().filter(|n| (min..=max).contains(n)).map(|n| json!(n))
    }

    fn default_value(&self) -> Value {
        if self.file == FileRule::Automatic { Value::Null } else { json!(self.default) }
    }

    fn read(&self, found: &Value) -> Option<Value> {
        let Integer { min, max, .. } = *self;
        match self.file {
            FileRule::Clamp => found.as_u64().map(|n| json!(n.clamp(min.max(0) as u64, max.max(0) as u64))),
            FileRule::TruncateAndClamp => found.as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0) // RED-W3
                .map(|n| json!((n as i64).clamp(min, max))),
            FileRule::IntegralNumber => found.as_f64().filter(|n| n.fract() == 0.0 && *n >= min as f64 && *n <= max as f64).map(|n| json!(n as i64)),
            FileRule::Default | FileRule::Automatic => self.normalize(found),
        }
    }

    fn seed(&self, found: &Value) -> Option<Value> {
        self.read(found)
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        let Integer { min, max, .. } = *self;
        format!("{key} must be an integer from {min} to {max}")
    }

    fn view(&self) -> Value {
        let Integer { min, max, .. } = *self;
        json!({"type": "integer", "min": min, "max": max})
    }

    fn chat_requirement(&self, name: &str) -> String {
        let Integer { min, max, .. } = *self;
        format!("{name} 必须是 {min} 到 {max} 的整数")
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        let Integer { min, max, default, file } = *self;
        assert!(file == FileRule::Automatic || (min..=max).contains(&default), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min)), Some(json!(min)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max)), Some(json!(max)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min - 1)), None, "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max + 1)), None, "{}", setting.key);
        assert_eq!(setting.default_value(), self.default_value());
        assert_eq!(setting.read(&json!({})), setting.default_value(), "{}: a missing value reads as the default", setting.key);
        if file == FileRule::Clamp {
            assert!(min >= 0, "{}: a clamped setting has non-negative bounds", setting.key);
        }
    }
}
