//! The model endpoint URL kind (`harness.apiBaseUrl`): a loopback HTTP or HTTPS base URL.

use super::SettingKind;
use serde_json::{json, Value};

const MAX_CHARS: usize = 2048;

#[derive(Clone, Copy)]
pub(crate) struct ModelEndpoint;

impl SettingKind for ModelEndpoint {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let url = value.as_str()?;
        (url.chars().count() <= MAX_CHARS && crate::runtime::is_model_endpoint(url))
            .then(|| Value::String(crate::runtime::canonical_model_endpoint(url)))
    }

    fn default_value(&self) -> Value {
        Value::Null
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!(
            "{key} must be a loopback HTTP or HTTPS endpoint URL without whitespace, userinfo, query, fragment, dot segments or backslashes, up to {MAX_CHARS} characters"
        )
    }

    fn view(&self) -> Value {
        json!({"type": "model endpoint", "maxChars": MAX_CHARS})
    }

    fn chat_requirement(&self, _: &str) -> String {
        "此项只可在控制台设置".to_string()
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert!(!setting.chat, "{}: model endpoint is never chat-writable", setting.key);
        assert_eq!(setting.default_value(), Value::Null, "{}", setting.key);
        assert_eq!(setting.read(&json!({})), Value::Null, "{}", setting.key);
        assert_eq!(setting.text(&json!({})), None, "{}", setting.key);
        for (input, expected) in [
            ("https://api.example.com", "https://api.example.com"),
            ("https://api.example.com/", "https://api.example.com"),
            (
                "https://api.example.com/compatible-mode/v1/",
                "https://api.example.com/compatible-mode/v1",
            ),
            ("http://127.0.0.1:8317/", "http://127.0.0.1:8317"),
            ("http://[::1]:8317/v1/", "http://[::1]:8317/v1"),
        ] {
            assert_eq!(
                setting.normalize(&json!(input)),
                Some(json!(expected)),
                "{}: {input}",
                setting.key
            );
            assert_eq!(
                setting.read(&super::document_at(setting.pointer, json!(input))),
                json!(expected),
                "{}: {input}",
                setting.key
            );
        }
        let too_long = format!("https://api.example.com/{}", "a".repeat(MAX_CHARS));
        for refused in [
            json!(""),
            json!(" https://api.example.com"),
            json!("https://api.example.com "),
            json!("https://api.example.com/a b"),
            json!("https://api.example.com\\v1"),
            json!("https://user@api.example.com/v1"),
            json!("https://api.example.com/v1?x=1"),
            json!("https://api.example.com/v1#frag"),
            json!("https://api.example.com/../v1"),
            json!("https://api.example.com//v1"),
            json!("https://api.example.com/%2fv1"),
            json!("https://0.0.0.0/v1"),
            json!("https://api.example.com:0/v1"),
            json!("http://api.example.com/v1"),
            json!(too_long),
            json!(42),
            Value::Null,
        ] {
            assert_eq!(setting.normalize(&refused), None, "{}: {refused}", setting.key);
        }
        assert_eq!(
            setting.read(&super::document_at(setting.pointer, json!("http://api.example.com/v1"))),
            Value::Null,
            "{}",
            setting.key
        );
    }
}
