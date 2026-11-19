use super::SettingKind;
use serde_json::{json, Value};

pub(crate) struct ApiKey;

const MIN_BYTES: usize = 8;
const MAX_BYTES: usize = 4096;

impl SettingKind for ApiKey {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let key = value.as_str()?;
        ((MIN_BYTES..=MAX_BYTES).contains(&key.len()) && key.bytes().all(|byte| byte.is_ascii_graphic()))
            .then(|| value.clone())
    }

    fn default_value(&self) -> Value {
        Value::Null
    }

    fn refusal(&self, key: &str, _: &Value) -> String {
        format!("{key} requires 8 to 4096 printable ASCII bytes without spaces")
    }

    fn view(&self) -> Value {
        json!({"type": "password", "minBytes": MIN_BYTES, "maxBytes": MAX_BYTES})
    }

    fn chat_requirement(&self, _: &str) -> String {
        "此项只可在本地设置".to_string()
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.default_value(), Value::Null);
        for (size, accepted) in [(7, false), (8, true), (4096, true), (4097, false)] {
            assert_eq!(setting.normalize(&json!("x".repeat(size))).is_some(), accepted, "length {size}");
        }
        for invalid in [json!(42), json!("prefix\r\nheader"), json!("has space"), json!("fixture中文")] {
            assert!(setting.normalize(&invalid).is_none());
        }
    }
}
