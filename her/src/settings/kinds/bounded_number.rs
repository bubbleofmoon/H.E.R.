//! A finite floating-point number inside `[min, max]`, falling back to `default` when unset or out
//! of range (`harness.historyShare`).

use super::SettingKind;
use serde_json::{json, Value};

pub(crate) struct BoundedNumber {
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

impl SettingKind for BoundedNumber {
    fn normalize(&self, value: &Value) -> Option<Value> {
        value
            .as_f64()
            .filter(|n| n.is_finite() && (self.min..=self.max).contains(n))
            .map(|n| json!(if n == 0.0 { 0.0 } else { n }))
    }

    fn default_value(&self) -> Value {
        json!(self.default)
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be a number from {} to {}", self.min, self.max)
    }

    fn view(&self) -> Value {
        json!({"type": "number", "min": self.min, "max": self.max})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 必须是 {} 到 {} 的数", self.min, self.max)
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert!(self.min < self.max, "{}", setting.key);
        assert!((self.min..=self.max).contains(&self.default), "{}", setting.key);
        for input in [self.min, self.default, self.max] {
            assert_eq!(setting.normalize(&json!(input)), Some(json!(input)), "{}", setting.key);
        }
        for bad in [
            json!(self.min - 0.001),
            json!(self.max + 0.001),
            json!("0.5"),
            Value::Null,
        ] {
            assert_eq!(setting.normalize(&bad), None, "{}", setting.key);
            assert_eq!(
                setting.read(&super::document_at(setting.pointer, bad)),
                self.default_value(),
                "{}",
                setting.key
            );
        }
        assert_eq!(setting.read(&json!({})), self.default_value(), "{}", setting.key);
    }
}
