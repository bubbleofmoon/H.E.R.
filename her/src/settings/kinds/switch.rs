//! The switch kind (#154; #153's tool switches use it too).

use super::SettingKind;
use serde_json::{json, Value};

/// A switch: the store keeps a JSON boolean. In `config.json` an explicit `false` reads as off and any other present
/// value (`null` included) as on, and a missing member reads as `default`: the JavaScript `!== false` rule the
/// switches' readers apply.
#[derive(Clone, Copy)]
pub(crate) struct Switch {
    default: bool,
    chat_pointer: Option<&'static str>,
}

impl Switch {
    pub(crate) const fn new(default: bool) -> Self {
        Self { default, chat_pointer: None }
    }

    pub(crate) const fn with_chat_pointer(default: bool, pointer: &'static str) -> Self {
        Self { default, chat_pointer: Some(pointer) }
    }
}

impl SettingKind for Switch {
    fn normalize(&self, value: &Value) -> Option<Value> {
        value.as_bool().map(|on| json!(on))
    }

    fn default_value(&self) -> Value {
        json!(self.default)
    }

    fn read(&self, found: &Value) -> Option<Value> {
        Some(json!(found != &Value::Bool(false)))
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be true or false")
    }

    fn view(&self) -> Value {
        json!({"type": "switch"})
    }

    fn chat_pointer(&self) -> Option<&'static str> {
        self.chat_pointer
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 必须是 true 或 false")
    }

    /// What the next use reads from the file, so a seed never changes a switch.
    fn seed(&self, found: &Value) -> Option<Value> {
        self.read(found)
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        let default = self.default;
        assert_eq!(setting.default_value(), json!(default), "{}", setting.key);
        assert_eq!(setting.read(&json!({})), json!(default), "{}: a missing switch reads as its default", setting.key);
        for (found, read) in [(json!(false), false), (json!(true), true), (Value::Null, true), (json!("false"), true), (json!(0), true)] {
            assert_eq!(setting.read(&super::document_at(setting.pointer, found.clone())), json!(read), "{}: {found}", setting.key);
            assert_eq!(setting.switch(&super::document_at(setting.pointer, found.clone())), read, "{}: {found}", setting.key);
        }
        assert_eq!(setting.normalize(&json!(false)), Some(json!(false)), "{}", setting.key);
        for refused in [json!("false"), json!(0), Value::Null] {
            assert_eq!(setting.normalize(&refused), None, "{}: {refused}", setting.key);
        }
    }
}
