//! Voice numbers preserve positive fractional durations and fractional legacy quotas.

use super::SettingKind;
use serde_json::{json, Value};

pub(crate) struct VoiceNumber {
    pub max: f64,
    pub default: f64,
    pub duration: bool,
}

impl VoiceNumber {
    fn valid_lower_bound(&self, number: f64) -> bool {
        number >= 0.0 && (!self.duration || number > 0.0)
    }
}

impl SettingKind for VoiceNumber {
    fn normalize(&self, value: &Value) -> Option<Value> {
        value.as_f64()
            .filter(|number| self.valid_lower_bound(*number) && *number <= self.max + 1.0) // RED-V1
            .map(crate::voice::json_number)
    }

    fn default_value(&self) -> Value {
        crate::voice::json_number(self.default)
    }

    fn read(&self, found: &Value) -> Option<Value> {
        let number = crate::console::voice_config_number(found);
        self.valid_lower_bound(number).then(|| crate::voice::json_number(number.min(self.max)))
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        if self.duration {
            format!("{key} must be greater than zero and at most {} seconds", self.max)
        } else {
            format!("{key} must be a number from 0 to {}", self.max)
        }
    }

    fn view(&self) -> Value {
        json!({"type":"number", "min":0, "max":self.max})
    }

    fn chat_requirement(&self, name: &str) -> String {
        self.refusal(name, &Value::Null)
    }

    fn seed(&self, found: &Value) -> Option<Value> {
        self.read(found)
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.normalize(&json!(0.5)), Some(json!(0.5)));
        assert_eq!(setting.normalize(&json!(self.max)), Some(crate::voice::json_number(self.max)));
        assert_eq!(setting.normalize(&json!(0)), (!self.duration).then(|| json!(0)));
        assert_eq!(setting.normalize(&json!(-0.001)), None);
        assert_eq!(setting.normalize(&json!(self.max + 0.001)), None);
        assert_eq!(setting.normalize(&json!("30")), None);
        assert_eq!(setting.read(&json!({})), self.default_value());
    }
}
