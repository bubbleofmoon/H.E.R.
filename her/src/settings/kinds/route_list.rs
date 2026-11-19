//! The ordered model route list.

use super::SettingKind;
use crate::harness::routes::{parse_list, RouteSpec, MAX_ROUTES};
use serde_json::{json, Value};

/// An explicit route list shares the runtime parser. Its reset stores null, which selects the single-model fallback.
pub(crate) struct RouteList;

impl SettingKind for RouteList {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let routes = parse_list(value).ok()?;
        Some(Value::Array(routes.iter().map(RouteSpec::setting_value).collect()))
    }

    fn default_value(&self) -> Value {
        Value::Null
    }

    fn refusal(&self, key: &str, value: &Value) -> String {
        parse_list(value).err().unwrap_or_else(|| format!("{key} is not a valid route list"))
    }

    fn view(&self) -> Value {
        json!({"type": "route list", "minItems": 1, "maxItems": MAX_ROUTES})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 只能在控制台修改")
    }

    fn file_holds(&self, found: &Value, stored: &Value) -> bool {
        found == stored || self.normalize(found).as_ref() == Some(stored)
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.default_value(), Value::Null, "{}", setting.key);
        assert!(!setting.chat, "{}: a route list is never chat-writable", setting.key);
    }
}
