//! A probability expressed as a JSON number between zero and one.

use super::SettingKind;
use serde_json::{json, Value};

pub(crate) struct Probability {
    pub default: f64,
}

impl SettingKind for Probability {
    fn normalize(&self, value: &Value) -> Option<Value> {
        value.as_f64().filter(|n| (0.0..=1.0).contains(n)).map(|n| json!(if n == 0.0 { 0.0 } else { n }))
    }

    fn default_value(&self) -> Value {
        json!(self.default)
    }

    fn read(&self, found: &Value) -> Option<Value> {
        found.as_f64().filter(|n| n.is_finite()).and_then(|n| self.normalize(&json!(n.clamp(0.0, 1.0))))
    }

    fn seed(&self, found: &Value) -> Option<Value> {
        self.read(found)
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be a number from 0 to 1")
    }

    fn view(&self) -> Value {
        json!({"type": "probability", "min": 0, "max": 1})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 必须是 0 到 1 的数")
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert!((0.0..=1.0).contains(&self.default), "{}", setting.key);
        for input in [0.0, 0.37, 1.0] {
            assert_eq!(setting.normalize(&json!(input)), Some(json!(input)), "{}", setting.key);
        }
        for input in [json!(-0.01), json!(1.01), json!("0.5"), Value::Null] {
            assert_eq!(setting.normalize(&input), None, "{}", setting.key);
        }
        assert_eq!(setting.read(&Value::Null), self.default_value());
    }
}
