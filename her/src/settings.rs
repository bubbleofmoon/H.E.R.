//! Live settings: the registry of owner-tunable settings and the settings store `data/settings.db`.
//!
//! Layout: `kinds/` holds one file per setting kind, each a struct implementing `SettingKind` (parse a value, default,
//! read a `config.json` value, refusal text, page view, chat wording); a `Setting` holds its kind as `&'static dyn
//! SettingKind`, so no code matches on a kind. `registry/` holds one table per settings group, joined in a fixed order by
//! `registry()`. `store.rs` holds the store. This file holds `Setting` and the re-exports callers name as
//! `crate::settings::X`. Text settings check denied terms through `model_text_policy::denied_terms`.
//!
//! The store is its own SQLite file with its own forward-only schema version (`PRAGMA user_version`); a newer version
//! is refused. It opens no other database. The configuration loader applies stored values over `config.json`
//! (`overlay`, called from `RuntimeContext::overlay_config`); `apply`, `reset` and `undo` change one value with one
//! audit row each, under the store gate (`write_store`), for the console routes and for the owner's chat settings tools
//! (`chat_setting`, `console::settings_action`); `console_changes` and `apply_changes` store the registry settings a
//! console configuration page changes (#177), and `keep_file_members` keeps every console write of `config.json` off
//! the registry members; `history` lists the audit rows. Each write first seeds the store from
//! `config.json` (`seed`: every registry setting never in the store, with one `migration` audit row each) and keeps
//! `config/config.json.pre-settings`; after it, `write_back` mirrors every stored value into `config.json`, so a
//! release without the store reads the current values. Every writer holds the configuration gate
//! (`console::config_gate`) and takes the store gate inside it, never the reverse. A legacy (no-root) context has no
//! store. The registry owns each setting's default and bounds: a reader reads its setting through `Setting::read` (or
//! `integer`, `text`) on the loader's document, never with a default of its own. Backups copy the
//! store through `backup_source`; a restore names it through `store_path`.

mod kinds;
#[cfg(test)]
mod probe;
mod registry;
mod store;

use serde_json::Value;

pub(crate) use kinds::SettingKind;
#[cfg(test)]
pub(crate) use probe::{AuditRow, StoreProbe};
pub(crate) use registry::{
    chat_setting, find, registry, DIGEST_MODEL, DIGEST_TIMEOUT_SECS, HARNESS_MAX_STEPS, HARNESS_MAX_TOKENS, HARNESS_MODEL,
    WEB_SEARCH_MODEL,
};
#[cfg(test)]
pub(crate) use store::{create_store, FAIL_BEFORE_AUDIT, FAIL_WRITE_BACK, NEWER_SCHEMA_ERROR, WAL_SWITCH_BUSY};
pub(crate) use store::{
    apply, apply_changes, backup_source, console_changes, direct_change_error, has_stored_values, history,
    keep_file_members, keep_registry_values, last_good_text, mirror_into, overlay, registry_free, reset, restore_gate,
    store_path, undo, view, write_back, Applied,
};

/// One registry entry.
pub(crate) struct Setting {
    /// The registry key, as the console names it.
    pub key: &'static str,
    /// Where the value lives in the configuration document (a JSON pointer of plain member names).
    pub pointer: &'static str,
    /// The value type and bounds (`kinds`): how a value is checked, read, refused and shown.
    pub kind: &'static dyn SettingKind,
    /// When a change takes effect: `live`, `next wake` or `restart`.
    pub applies: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    /// Whether the owner's private chat may read and change it through `settings_get` / `settings_set`, under
    /// `chat_name`. Never set for a secret or a text setting (registry tests).
    pub chat: bool,
}

impl Setting {
    /// The value as stored and read, or `None` when this setting does not accept it.
    pub(crate) fn normalize(&self, value: &Value) -> Option<Value> {
        self.kind.normalize(value)
    }

    /// A value the store keeps for this setting: a valid value, or `Null` when that is the default (a model id
    /// without one), which a reset stores so the read is "not configured" whatever `config.json` says. A text default
    /// is a valid value, the empty text included.
    fn storable(&self, value: &Value) -> Option<Value> {
        self.normalize(value).or_else(|| (value.is_null() && self.default_value().is_null()).then_some(Value::Null))
    }

    /// The default; `Null` for a model id without one (not configured).
    pub(crate) fn default_value(&self) -> Value {
        self.kind.default_value()
    }

    /// What the next use reads from `document` (the loader's document with stored values applied, or any
    /// configuration value): the value at the pointer as the setting's kind reads it, else the default.
    pub(crate) fn read(&self, document: &Value) -> Value {
        document.pointer(self.pointer).and_then(|found| self.kind.read(found)).unwrap_or_else(|| self.default_value())
    }

    /// `read` of an integer setting; readers ask it only of integer settings (registry test).
    pub(crate) fn integer(&self, document: &Value) -> i64 {
        self.read(document).as_i64().unwrap_or_default()
    }

    /// `read` of a model id or a text setting: `None` when a model id is not configured; a text setting always has
    /// one.
    pub(crate) fn text(&self, document: &Value) -> Option<String> {
        self.read(document).as_str().map(str::to_string)
    }

    /// Why `value`, which this setting refused, was refused.
    fn refusal(&self, value: &Value) -> String {
        self.kind.refusal(self.key, value)
    }

    /// The name the chat tools use: the configuration path with dots (`/socialV2/reply/wakeQuietMs` is
    /// `socialV2.reply.wakeQuietMs`), the form of the other chat-writable paths, never the registry key.
    pub(crate) fn chat_name(&self) -> String {
        self.pointer.trim_start_matches('/').replace('/', ".")
    }

    /// Why a chat value was refused, under the chat's name.
    pub(crate) fn chat_requirement(&self) -> String {
        self.kind.chat_requirement(&self.chat_name())
    }
}
