//! The owner id kind (#154): `ownerQQ`.

use super::id_list::qq_id;
use super::SettingKind;
use serde_json::{json, Value};

/// The owner's QQ id: a positive safe integer (`id_list::qq_id`; a string of ASCII digits is taken as its number). The
/// default is `null`, no owner, which a reset stores. In `config.json` a value that is not an id reads as no owner.
#[derive(Clone, Copy)]
pub(crate) struct OwnerId;

impl SettingKind for OwnerId {
    fn normalize(&self, value: &Value) -> Option<Value> {
        qq_id(value).map(|id| json!(id))
    }

    fn default_value(&self) -> Value {
        Value::Null
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be a QQ id (a positive integer); reset it for no owner")
    }

    fn view(&self) -> Value {
        json!({"type": "owner id"})
    }

    /// The owner is never chat-writable (registry test); the wording keeps the interface whole.
    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 只能在控制台修改")
    }

    /// A file owner spelled as a digit string is left as it is.
    fn file_holds(&self, found: &Value, stored: &Value) -> bool {
        found == stored || self.normalize(found).as_ref() == Some(stored)
    }

    /// A `null` or blank owner is no owner (`config::canonical_config`), which the store keeps as `null`.
    fn seed(&self, found: &Value) -> Option<Value> {
        if found.is_null() || found.as_str().is_some_and(|text| text.trim().is_empty()) {
            return Some(Value::Null);
        }
        self.normalize(found)
    }

    fn access(&self) -> bool {
        true
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.default_value(), Value::Null, "{}", setting.key);
        assert_eq!(setting.read(&json!({})), Value::Null, "{}: no owner by default", setting.key);
        assert!(setting.confirmed() && setting.access(), "{}: the owner is a confirmed access setting", setting.key);
        assert!(!setting.chat, "{}: the owner is never chat-writable", setting.key);
        assert_eq!(setting.normalize(&json!(" 424242 ")), Some(json!(424242)), "{}", setting.key);
        let max = super::id_list::SAFE_INTEGER_MAX;
        assert_eq!(setting.normalize(&json!(max)), Some(json!(max)), "{}", setting.key);
        for refused in [json!(0), json!(-1), json!(1.5), json!("x"), json!(""), json!(max + 1), Value::Null] {
            assert_eq!(setting.normalize(&refused), None, "{}: {refused}", setting.key);
        }
        // The runtime's owner reader (`config::canonical_config`) reads the same owner.
        let document = super::document_at(setting.pointer, json!("424242"));
        let (effective, _) = crate::config::canonical_config(&document).unwrap();
        assert_eq!(effective.pointer(setting.pointer), Some(&setting.read(&document)), "{}", setting.key);
    }
}
