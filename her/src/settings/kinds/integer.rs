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
    /// A non-negative integer is clamped into the bounds; any other value reads as the default.
    Clamp,
}

impl SettingKind for Integer {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let Integer { min, max, .. } = *self;
        value.as_i64().filter(|n| (min..=max).contains(n)).map(|n| json!(n))
    }

    fn default_value(&self) -> Value {
        json!(self.default)
    }

    fn read(&self, found: &Value) -> Option<Value> {
        let Integer { min, max, .. } = *self;
        match self.file {
            FileRule::Clamp => found.as_u64().map(|n| json!(n.clamp(min.max(0) as u64, max.max(0) as u64))),
            FileRule::Default => self.normalize(found),
        }
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
        assert!(min <= default && default <= max, "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min)), Some(json!(min)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max)), Some(json!(max)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min - 1)), None, "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max + 1)), None, "{}", setting.key);
        assert_eq!(setting.default_value(), json!(default));
        assert_eq!(setting.integer(&json!({})), default, "{}: a missing value reads as the default", setting.key);
        if file == FileRule::Clamp {
            assert!(min >= 0, "{}: a clamped setting has non-negative bounds", setting.key);
        }
    }
}
