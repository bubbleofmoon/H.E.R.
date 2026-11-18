//! The text kind: model-visible text (#140, `prompt_text`).

use super::SettingKind;
use serde_json::{json, Value};

/// Model-visible text: a JSON string of at most `max_chars` Unicode scalar values with no control character but line
/// feed and tab, and no #142 denied term (`model_text_policy::denied_terms`). An empty string is accepted and renders
/// nothing.
/// A carriage return is refused, never converted, so the stored text is the text that renders. `default` passes the
/// same check (registry test) and is what a missing or refused value reads as. Not for model ids (`ModelId`), which may
/// hold a denied word.
#[derive(Clone, Copy)]
pub(crate) struct Text {
    pub max_chars: usize,
    pub default: &'static str,
}

/// Whether `text` has the shape a text setting accepts: at most `max_chars` Unicode scalar values and no control
/// character but line feed and tab.
fn text_shape_ok(text: &str, max_chars: usize) -> bool {
    text.chars().count() <= max_chars && text.chars().all(|c| c == '\n' || c == '\t' || !c.is_control())
}

impl SettingKind for Text {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let max_chars = self.max_chars;
        value
            .as_str()
            .filter(|text| text_shape_ok(text, max_chars) && crate::model_text_policy::denied_terms(text).is_empty())
            .map(|text| json!(text))
    }

    fn default_value(&self) -> Value {
        json!(self.default)
    }

    /// A text of the right shape names its denied terms.
    fn refusal(&self, key: &str, value: &Value) -> String {
        let max_chars = self.max_chars;
        let denied = value
            .as_str()
            .filter(|text| text_shape_ok(text, max_chars))
            .map(crate::model_text_policy::denied_terms)
            .unwrap_or_default();
        if denied.is_empty() {
            format!("{key} must be text of at most {max_chars} characters, with no control character but line feed and tab")
        } else {
            format!("{key} names a denied term: {}", denied.join(", "))
        }
    }

    fn view(&self) -> Value {
        json!({"type": "text", "maxChars": self.max_chars})
    }

    /// No text setting is chat-writable (registry test); the wording keeps the interface whole.
    fn chat_requirement(&self, name: &str) -> String {
        let max_chars = self.max_chars;
        format!("{name} 必须是不超过 {max_chars} 个字符的文字，除换行和制表符外不含控制字符，也不含禁用词")
    }

    fn text_default(&self) -> Option<&'static str> {
        Some(self.default)
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        let Text { max_chars, default } = *self;
        // The compiled default passes its own check, so the fallback is never a refused text.
        assert_eq!(setting.normalize(&json!(default)), Some(json!(default)), "{}", setting.key);
        assert_eq!(setting.default_value(), json!(default));
        let missing = setting.text(&json!({}));
        assert_eq!(missing.as_deref(), Some(default), "{}: a missing value reads as the default", setting.key);
        let longest = "鲸".repeat(max_chars);
        assert_eq!(setting.normalize(&json!(longest)), Some(json!(longest)), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(format!("{longest}鲸"))), None, "{}", setting.key);
        assert_eq!(setting.normalize(&json!("")), Some(json!("")), "{}", setting.key);
        for refused in [json!("AI"), json!("ai"), json!("ＤＳＨ"), json!("a\rb"), json!(1), Value::Null] {
            assert_eq!(setting.normalize(&refused), None, "{}: {refused}", setting.key);
        }
        // The reader (`prompt_text::text`) agrees: the default without a value, the configured text when
        // accepted, the default when refused.
        let configured = |text: &str| {
            let mut document = json!(text);
            for name in setting.pointer.split('/').skip(1).collect::<Vec<_>>().into_iter().rev() {
                let mut object = serde_json::Map::new();
                object.insert(name.to_string(), document);
                document = Value::Object(object);
            }
            document
        };
        assert_eq!(crate::prompt_text::text(&json!({}), setting.key), default, "{}", setting.key);
        assert_eq!(crate::prompt_text::text(&configured("鲸 140"), setting.key), "鲸 140", "{}", setting.key);
        assert_eq!(crate::prompt_text::text(&configured("AI 140"), setting.key), default, "{}", setting.key);
        // Prompt text is never chat-writable, so the model cannot rewrite its own prompt.
        assert!(setting.pointer.starts_with("/prompts/"), "{}", setting.key);
        assert!(!setting.chat, "{}: prompt text is never chat-writable", setting.key);
    }
}
