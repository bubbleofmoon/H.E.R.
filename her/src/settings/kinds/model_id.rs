//! The model id kind.

use super::SettingKind;
use serde_json::{json, Value};

/// The longest model id the store accepts, in characters.
const MODEL_ID_MAX_CHARS: usize = 200;

/// A model id: a JSON string that, trimmed as JavaScript trims, has 1 to `MODEL_ID_MAX_CHARS` characters and no control
/// character; it is stored and read trimmed. `default` is what a missing or refused value reads as, and `None` reads as
/// not configured.
#[derive(Clone, Copy)]
pub(crate) struct ModelId {
    pub default: Option<&'static str>,
}

/// A model id as the store keeps it, or `None` when `value` is not one.
fn model_id(value: &Value) -> Option<&str> {
    let id = crate::aux_text::js_trim(value.as_str()?);
    let length = id.chars().count();
    ((1..=MODEL_ID_MAX_CHARS).contains(&length) && !id.chars().any(char::is_control)).then_some(id)
}

impl SettingKind for ModelId {
    fn normalize(&self, value: &Value) -> Option<Value> {
        model_id(value).map(|id| json!(id))
    }

    fn default_value(&self) -> Value {
        self.default.map_or(Value::Null, |id| json!(id))
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be a model id of 1 to {MODEL_ID_MAX_CHARS} characters without control characters")
    }

    fn view(&self) -> Value {
        json!({"type": "model id", "maxChars": MODEL_ID_MAX_CHARS})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 必须是 1 到 200 个字符、不含控制字符的模型名")
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        let default = self.default;
        if let Some(id) = default {
            assert_eq!(setting.normalize(&json!(id)), Some(json!(id)), "{}: the default is a model id", setting.key);
        }
        assert_eq!(setting.default_value(), default.map_or(Value::Null, |id| json!(id)), "{}", setting.key);
        assert_eq!(setting.text(&json!({})), default.map(str::to_string), "{}", setting.key);
    }
}
