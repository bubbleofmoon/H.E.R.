//! The route id kind (#162): the id of one route of `harness.routes`.

use super::SettingKind;
use crate::harness::routes::{valid_id, ROUTE_ID_MAX_CHARS};
use serde_json::{json, Value};

/// A route id as `harness.routes` writes it (`harness::routes::valid_id`: 1 to `ROUTE_ID_MAX_CHARS` ASCII letters,
/// digits, `.`, `_` or `-`, never `default`), given exactly (not trimmed). No default: none reads as the configured
/// order. Whether the id names a route of the chain is decided where the chain is read, when each turn starts
/// (`harness::routes::conversation_order`), because the chain can change after a value is stored.
#[derive(Clone, Copy)]
pub(crate) struct RouteId;

impl SettingKind for RouteId {
    fn normalize(&self, value: &Value) -> Option<Value> {
        value.as_str().filter(|id| valid_id(id)).map(|id| json!(id))
    }

    fn default_value(&self) -> Value {
        Value::Null
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be a route id of 1 to {ROUTE_ID_MAX_CHARS} ASCII letters, digits, '.', '_' or '-', not default")
    }

    fn view(&self) -> Value {
        json!({"type": "route id", "maxChars": ROUTE_ID_MAX_CHARS})
    }

    /// Never shown today: no route id setting is chat-writable (registry test).
    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 必须是 1 到 {ROUTE_ID_MAX_CHARS} 个英文字母、数字、点、下划线或连字符组成的线路名")
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert!(!setting.chat, "{}: a route id is never chat-writable", setting.key);
        assert_eq!(setting.default_value(), Value::Null, "{}: no default", setting.key);
        assert_eq!(setting.read(&json!({})), Value::Null, "{}: missing reads as none", setting.key);
        let longest = "x".repeat(ROUTE_ID_MAX_CHARS);
        for id in ["a", "A.b_c-9", longest.as_str()] {
            assert_eq!(setting.normalize(&json!(id)), Some(json!(id)), "{}: {id}", setting.key);
            assert_eq!(setting.read(&super::document_at(setting.pointer, json!(id))), json!(id), "{}: {id}", setting.key);
        }
        let over = "x".repeat(ROUTE_ID_MAX_CHARS + 1);
        for refused in [json!("default"), json!(""), json!(" a"), json!("a b"), json!("a/b"), json!("é"), json!(over), json!(1), Value::Null] {
            assert_eq!(setting.normalize(&refused), None, "{}: {refused}", setting.key);
        }
        assert_eq!(setting.read(&super::document_at(setting.pointer, json!("a b"))), Value::Null, "{}", setting.key);
    }
}
