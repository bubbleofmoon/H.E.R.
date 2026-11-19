//! The integer kind: a JSON integer within bounds.

use super::SettingKind;
use serde_json::{json, Value};

/// A JSON integer from `min` to `max`, both included. `file` is how the next use reads a value the store would refuse.
#[derive(Clone, Copy)]
pub(crate) struct Integer {
    pub min: i64,
    pub max: i64,
    pub default: Option<i64>,
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
    /// A finite number or numeric string is rounded to the nearest integer, then clamped.
    Round,
    /// A finite legacy JavaScript number is rounded to the nearest integer, then clamped.
    JsRound,
    /// A finite legacy JavaScript number is truncated, then clamped.
    JsTruncate,
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
            FileRule::Round => found
                .as_f64()
                .or_else(|| found.as_str().and_then(|text| text.trim().parse().ok()))
                .filter(|number| number.is_finite())
                .map(|number| json!((number.round() as i64).clamp(min, max))),
            FileRule::JsRound | FileRule::JsTruncate => crate::aux_text::js_number(found)
                .filter(|number| !found.is_null() && number.is_finite())
                .map(|number| {
                    let integer = if self.file == FileRule::JsRound { number.round() } else { number.trunc() };
                    json!((integer as i64).clamp(min, max))
                }),
        }
    }

    fn seed(&self, found: &Value) -> Option<Value> {
        match self.file {
            FileRule::Clamp | FileRule::JsRound | FileRule::JsTruncate => self.read(found),
            FileRule::Default | FileRule::Round => self.normalize(found),
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
        assert!(default.is_none_or(|default| (min..=max).contains(&default)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min)), Some(json!(min)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max)), Some(json!(max)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(min - 1)), None, "{}", setting.key);
        assert_eq!(setting.normalize(&json!(max + 1)), None, "{}", setting.key);
        assert_eq!(setting.default_value(), json!(default));
        assert_eq!(setting.read(&json!({})), json!(default), "{}: a missing value reads as the default", setting.key);
        if file == FileRule::Clamp {
            assert!(min >= 0, "{}: a clamped setting has non-negative bounds", setting.key);
        }
    }
}
