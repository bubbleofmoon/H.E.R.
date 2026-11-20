//! The canonical conversation list offered to the live owner at launch.

use super::SettingKind;
use serde_json::{json, Value};

pub(crate) struct RolloutList;

impl SettingKind for RolloutList {
    fn normalize(&self, value: &Value) -> Option<Value> {
        crate::watchdog::rollout_groups(value).ok().map(|_| value.clone())
    }

    fn default_value(&self) -> Value {
        json!([])
    }

    fn refusal(&self, key: &str, value: &Value) -> String {
        match crate::watchdog::rollout_groups(value) {
            Ok(_) => format!("{key} has an invalid value"),
            Err(error) => format!("{key}: {error}"),
        }
    }

    fn view(&self) -> Value {
        json!({"type": "conversation list"})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 只能在控制台修改")
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.default_value(), json!([]));
        assert!(setting.confirmed() && !setting.chat && !setting.access());
        let keys = json!(["private:2", "group:1"]);
        assert_eq!(setting.normalize(&keys), Some(keys));
        assert_eq!(setting.normalize(&Value::Null), None);
    }
}
