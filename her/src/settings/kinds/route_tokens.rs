//! Per-route integer context windows (`harness.contextTokens`).

use super::SettingKind;
use serde_json::{json, Map, Value};

const ROUTE_KEY_MAX_CHARS: usize = 200;

pub(crate) struct RouteTokens {
    pub min: i64,
    pub max: i64,
}

fn valid_route_key(key: &str) -> bool {
    let trimmed = crate::aux_text::js_trim(key);
    let len = key.chars().count();
    trimmed == key
        && (1..=ROUTE_KEY_MAX_CHARS).contains(&len)
        && !key.chars().any(char::is_control)
}

impl RouteTokens {
    fn valid_value(&self, value: &Value) -> Option<i64> {
        value.as_i64().filter(|n| (self.min..=self.max).contains(n))
    }

    fn filter_map(&self, map: &Map<String, Value>) -> Value {
        let mut out = Map::new();
        for (k, v) in map {
            if valid_route_key(k) {
                if let Some(n) = self.valid_value(v) {
                    out.insert(k.clone(), json!(n));
                }
            }
        }
        Value::Object(out)
    }
}

impl SettingKind for RouteTokens {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let map = value.as_object()?;
        let mut out = Map::with_capacity(map.len());
        for (k, v) in map {
            if !valid_route_key(k) {
                return None;
            }
            let n = self.valid_value(v)?;
            out.insert(k.clone(), json!(n));
        }
        Some(Value::Object(out))
    }

    fn default_value(&self) -> Value {
        json!({})
    }

    fn read(&self, found: &Value) -> Option<Value> {
        found.as_object().map(|map| self.filter_map(map))
    }

    fn seed(&self, found: &Value) -> Option<Value> {
        self.read(found)
    }

    fn file_holds(&self, found: &Value, stored: &Value) -> bool {
        found == stored || (found.is_object() && self.read(found).as_ref() == Some(stored))
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!(
            "{key} must be a JSON object mapping route ids to integers from {} to {}",
            self.min, self.max
        )
    }

    fn view(&self) -> Value {
        json!({"type": "route tokens", "min": self.min, "max": self.max})
    }

    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 必须是线路到整数（{} 到 {}）的映射对象", self.min, self.max)
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.default_value(), json!({}), "{}", setting.key);
        assert_eq!(setting.read(&json!({})), json!({}), "{}", setting.key);
        let valid = json!({"a": self.min, "org/model:fast": self.max});
        assert_eq!(setting.normalize(&valid), Some(valid.clone()), "{}", setting.key);
        assert_eq!(
            setting.read(&super::document_at(setting.pointer, valid.clone())),
            valid,
            "{}",
            setting.key
        );
        for bad in [
            json!([]),
            json!("x"),
            Value::Null,
            json!({"": self.min}),
            json!({" r": self.min}),
            json!({"r": self.min - 1}),
            json!({"r": self.max + 1}),
            json!({"r": 1500.5}),
            json!({"r": "8000"}),
        ] {
            assert_eq!(setting.normalize(&bad), None, "{}: {bad}", setting.key);
        }
        let mixed = json!({"good": self.min, "bad": self.min - 1, "str": "x"});
        assert_eq!(
            setting.read(&super::document_at(setting.pointer, mixed)),
            json!({"good": self.min}),
            "{}",
            setting.key
        );
    }
}
