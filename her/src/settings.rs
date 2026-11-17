//! Live settings: the registry and the settings store. Run 1 of #146 holds only the seam; run 2 replaces this file.

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};

pub(crate) const NEWER_SCHEMA_ERROR: &str = "settings database is newer than this release";

#[cfg(test)]
thread_local! {
    /// Makes the next `apply` on this thread fail after it wrote the value and before the audit row.
    pub(crate) static FAIL_BEFORE_AUDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The value type and bounds of a setting.
pub(crate) enum Kind {
    /// A JSON integer from `min` to `max`, both included.
    Integer { min: i64, max: i64, default: i64 },
}

/// One registry entry.
pub(crate) struct Setting {
    pub key: &'static str,
    pub pointer: &'static str,
    pub kind: Kind,
    pub applies: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

impl Setting {
    pub(crate) fn normalize(&self, value: &Value) -> Option<Value> {
        match self.kind {
            Kind::Integer { min, max, .. } => value.as_i64().filter(|n| (min..=max).contains(n)).map(|n| json!(n)),
        }
    }

    pub(crate) fn default_value(&self) -> Value {
        match self.kind {
            Kind::Integer { default, .. } => json!(default),
        }
    }
}

pub(crate) const REGISTRY: &[Setting] = &[];

pub(crate) fn find(key: &str) -> Option<&'static Setting> {
    REGISTRY.iter().find(|setting| setting.key == key)
}

/// The result of one `apply`.
pub(crate) struct Applied {
    pub value: Value,
    pub revision: i64,
    pub changed: bool,
}

pub(crate) fn overlay(_context: &RuntimeContext, document: Value) -> Value {
    document
}

pub(crate) fn apply(
    _context: &RuntimeContext,
    _key: &str,
    _value: &Value,
    _actor: &str,
    _source: &str,
) -> Result<Applied, (u16, String)> {
    Err((501, "settings store is not implemented".to_string()))
}

pub(crate) fn view(_context: &RuntimeContext) -> Result<Value, (u16, String)> {
    Err((501, "settings store is not implemented".to_string()))
}
