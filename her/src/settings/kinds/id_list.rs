//! The id list kind (#154): the allow and deny lists.

use super::SettingKind;
use serde_json::{json, Value};

/// The most ids an id list setting holds.
pub(crate) const ID_LIST_MAX: usize = 10_000;

/// The largest QQ id: JavaScript's largest safe integer, the bound `config::canonical_config` applies to the owner.
pub(crate) const SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;

/// A QQ id from a JSON integer or a string of ASCII digits (trimmed), when it is from 1 to `SAFE_INTEGER_MAX`; `None`
/// for anything else.
pub(crate) fn qq_id(value: &Value) -> Option<u64> {
    let id = match value {
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()?
        }
        _ => return None,
    };
    (1..=SAFE_INTEGER_MAX).contains(&id).then_some(id)
}

/// An id of a list: `qq_id`, except that a digit string with a leading zero is no id. The runtime compares a list's
/// items with a conversation's id as strings (`chat::allowed`), so "07001" is not 7001 there; reading it as 7001 would
/// show, store and deny an id the runtime never matches.
fn list_id(value: &Value) -> Option<u64> {
    if value.as_str().is_some_and(|text| text.trim().len() > 1 && text.trim().starts_with('0')) {
        return None;
    }
    qq_id(value)
}

/// `items` as ids in first-seen order without repeats; `None` when `strict` and an item is not an id (`list_id`), else
/// such an item is skipped.
fn ids(items: &[Value], strict: bool) -> Option<Vec<u64>> {
    let mut seen = std::collections::HashSet::new();
    let mut ids: Vec<u64> = Vec::new();
    for item in items {
        match list_id(item) {
            Some(id) => {
                if seen.insert(id) {
                    ids.push(id);
                }
            }
            None if strict => return None,
            None => {}
        }
    }
    Some(ids)
}

/// A list of QQ ids: a JSON array of at most `ID_LIST_MAX` ids (`list_id`), stored as integers in first-seen order
/// without repeats; an item that is no id, a digit string with a leading zero included, is refused. `alias` is the
/// older member read while the pointer's member is missing or null (the alias precedence of
/// `config::canonical_config`). In `config.json` an array reads as its usable ids, in first-seen order without repeats
/// (an item that is not an id is skipped), and any other value as the empty list.
#[derive(Clone, Copy)]
pub(crate) struct IdList {
    pub alias: Option<&'static str>,
}

impl SettingKind for IdList {
    fn normalize(&self, value: &Value) -> Option<Value> {
        let items = value.as_array().filter(|items| items.len() <= ID_LIST_MAX)?;
        ids(items, true).map(|ids| json!(ids))
    }

    fn default_value(&self) -> Value {
        json!([])
    }

    fn read(&self, found: &Value) -> Option<Value> {
        Some(json!(found.as_array().and_then(|items| ids(items, false)).unwrap_or_default()))
    }

    fn refusal(&self, key: &str, _value: &Value) -> String {
        format!("{key} must be a list of at most {ID_LIST_MAX} QQ ids (positive integers)")
    }

    fn view(&self) -> Value {
        json!({"type": "id list", "maxItems": ID_LIST_MAX})
    }

    /// No id list is chat-writable (registry test); the wording keeps the interface whole.
    fn chat_requirement(&self, name: &str) -> String {
        format!("{name} 只能在控制台修改")
    }

    fn alias(&self) -> Option<&'static str> {
        self.alias
    }

    /// A file list that reads as the stored ids (digit strings, repeats, skipped items) is left as it is.
    fn file_holds(&self, found: &Value, stored: &Value) -> bool {
        self.read(found).as_ref() == Some(stored)
    }

    /// The ids the next use reads from the file, when the store can keep them (at most `ID_LIST_MAX`).
    fn seed(&self, found: &Value) -> Option<Value> {
        self.read(found).and_then(|read| self.normalize(&read))
    }

    fn access(&self) -> bool {
        true
    }

    #[cfg(test)]
    fn check_entry(&self, setting: &crate::settings::Setting) {
        assert_eq!(setting.default_value(), json!([]), "{}", setting.key);
        assert_eq!(setting.read(&json!({})), json!([]), "{}: a missing list reads as empty", setting.key);
        assert!(setting.confirmed() && setting.access(), "{}: an id list is a confirmed access setting", setting.key);
        assert!(!setting.chat, "{}: an access list is never chat-writable", setting.key);
        let longest: Vec<u64> = (1..=ID_LIST_MAX as u64).collect();
        assert_eq!(setting.normalize(&json!(longest)), Some(json!(longest)), "{}", setting.key);
        let over: Vec<u64> = (1..=ID_LIST_MAX as u64 + 1).collect();
        assert_eq!(setting.normalize(&json!(over)), None, "{}", setting.key);
        assert_eq!(setting.normalize(&json!([SAFE_INTEGER_MAX])), Some(json!([SAFE_INTEGER_MAX])), "{}", setting.key);
        assert_eq!(setting.normalize(&json!(["12", 12, " 13 "])), Some(json!([12, 13])), "{}", setting.key);
        for refused in [json!([0]), json!([-1]), json!([1.5]), json!(["x"]), json!([SAFE_INTEGER_MAX + 1]), json!(12), Value::Null] {
            assert_eq!(setting.normalize(&refused), None, "{}: {refused}", setting.key);
        }
        // The runtime's list reader (`config::canonical_config`) reads the same ids, under the pointer and the alias.
        let mut spellings = vec![setting.pointer];
        spellings.extend(self.alias);
        for spelling in spellings {
            let document = super::document_at(spelling, json!([" 12 ", 13, "x", 13]));
            let (effective, _) = crate::config::canonical_config(&document).unwrap();
            assert_eq!(effective.pointer(setting.pointer), Some(&json!(["12", "13", "13"])), "{spelling}");
            assert_eq!(setting.read(&document), json!([12, 13]), "{spelling}");
        }
        // The runtime compares ids as strings, so a leading zero makes another id: "07001" is not 7001.
        assert_eq!(setting.normalize(&json!(["07001"])), None, "{}: a leading zero is refused", setting.key);
        let padded = super::document_at(setting.pointer, json!(["07001", 12]));
        let (effective, _) = crate::config::canonical_config(&padded).unwrap();
        assert_eq!(effective.pointer(setting.pointer), Some(&json!(["07001", "12"])), "{}", setting.key);
        assert_eq!(setting.read(&padded), json!([12]), "{}: a leading zero is skipped, as no id of the runtime's", setting.key);
        if let Some(alias) = self.alias {
            let mut both = super::document_at(setting.pointer, json!([8]));
            both.pointer_mut(setting.pointer.rsplit_once('/').unwrap().0).unwrap()[alias.rsplit_once('/').unwrap().1] = json!([7]);
            assert_eq!(setting.read(&both), json!([8]), "{}: the pointer's member wins over the alias", setting.key);
        }
    }
}
