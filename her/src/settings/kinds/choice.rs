//! The choice kind (#218): one of a fixed list of strings.

use super::SettingKind;
use serde_json::{json, Value};

/// One of `options`, given exactly as listed (not trimmed, in its own case); `default` is one of them. A `config.json`
/// value that is not an option reads as the default.
#[derive(Clone, Copy)]
pub(crate) struct Choice {
    pub options: &'static [&'static str],
    pub default: &'static str,
}

impl SettingKind for Choice {
    fn normalize(&self, value: &Value) -> Option<Value> {
        value.as_str().filter(|text| self.options.iter().any(|option| option == text)).map(|text| json!(text))
    }

    fn default_value(&self) -> Value {
        json!(self.default)
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be one of {}", self.options.join(", "))
    }

    fn view(&self) -> Value {
        json!({"type": "choice", "options": self.options})
    }

    /// Never shown today: no choice setting is chat-writable (registry test), and an option may name a word the
    /// model must not read.
    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 只能是 {} 之一", self.options.join("、"))
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert!(!self.options.is_empty(), "{}: a choice has options", setting.key);
        assert!(self.options.contains(&self.default), "{}: the default is an option", setting.key);
        for option in self.options {
            assert_eq!(setting.normalize(&json!(option)), Some(json!(option)), "{}: {option}", setting.key);
            assert_eq!(setting.normalize(&json!(format!(" {option}"))), None, "{}: an option is given exactly", setting.key);
            let upper = option.to_uppercase();
            if upper != *option {
                assert_eq!(setting.normalize(&json!(upper)), None, "{}: an option keeps its case", setting.key);
            }
            assert_eq!(setting.read(&super::document_at(setting.pointer, json!(option))), json!(option), "{}", setting.key);
        }
        assert_eq!(setting.read(&json!({})), json!(self.default), "{}: a missing choice reads as its default", setting.key);
        assert_eq!(setting.read(&super::document_at(setting.pointer, json!("other"))), json!(self.default), "{}", setting.key);
        assert_eq!(setting.normalize(&Value::Null), None, "{}", setting.key);
        assert_eq!(setting.normalize(&json!(1)), None, "{}", setting.key);
        assert_eq!(setting.default_value(), json!(self.default), "{}", setting.key);
    }
}
