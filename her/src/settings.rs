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
//! `integer`, `text`, `switch`) on the loader's document, never with a default of its own. Backups copy the
//! store through `backup_source`; a restore names it through `store_path`.
//!
//! Security settings (#154, `registry/access.rs`: the allow and deny lists, the owner, the outbound guard switches)
//! change only with an explicit confirmation (`confirm_field`, `check_confirmation`) on every route that changes one;
//! a change of a list or the owner is an access change (`Setting::access`), revoked as a whitelist write is
//! (`console::access_setting_change`). A security setting is seeded only by a write of itself, so a write of another
//! setting never takes the access lists over from `config.json`.
//!
//! Scopes (#218): a setting of `registry()` is global. A setting of `conversation_registry()` (`ConversationSetting`)
//! keeps one value per conversation in the store only (schema version 2, `store/conversation.rs`): never overlaid,
//! never seeded from or written back into `config.json`; a conversation without a value uses its global setting.
//! `apply_conversation` and `reset_conversation` change it for one admitted conversation (`conversation_admitted`),
//! `undo` and `history` take its audit rows (scope: the conversation's key), and a write that changes an access setting
//! clears, in the same transaction, the values of every conversation it leaves unadmitted. Engine selection reads
//! `conversation.engine` and then `engine.default`. Harness turn admission reads `conversation.route` and
//! `harness.firstRoute` through `harness::routes::selected`. A setting whose default is none stores none on a reset
//! (`Setting::storable`).

mod kinds;
#[cfg(test)]
mod probe;
mod registry;
mod store;

use serde_json::Value;

pub(crate) use kinds::SettingKind;
#[cfg(test)]
pub(crate) use kinds::ID_LIST_MAX;
#[cfg(test)]
pub(crate) use probe::{AuditRow, StoreProbe};
pub(crate) use registry::{reply, wake as wake_timing, CONVERSATION_ENGINE, ENGINE_DEFAULT};
pub(crate) use registry::{
    chat_setting, conversation_registry, find, find_conversation, registry, CONVERSATION_ROUTE, DIGEST_MODEL,
    DIGEST_TIMEOUT_SECS, HARNESS_FIRST_ROUTE, HARNESS_MAX_STEPS, HARNESS_MAX_TOKENS, HARNESS_MODEL, INTERCEPT_NOTIFY,
    HARNESS_ROUTES, OUTBOUND_GUARD, OWNER, WEB_SEARCH_MODEL,
};
#[cfg(test)]
pub(crate) use store::{
    create_store, FAIL_AFTER_CLEAR, FAIL_BEFORE_AUDIT, FAIL_MIGRATION, FAIL_UNDO_LOOK, FAIL_WRITE_BACK,
    NEWER_SCHEMA_ERROR, PANIC_BEFORE_COMMIT, WAL_SWITCH_BUSY,
};
pub(crate) use store::{
    apply, apply_changes, apply_conversation, apply_switch_choice, backup_source, canonical_conversation,
    complete_timing, console_changes, conversation_admitted, conversation_value, conversation_view, direct_change_error, engine_choice,
    has_legacy_engine_choices, has_stored_values, has_wake_plan_history, history, import_engine_choices,
    holds_any, keep_file_members, keep_registry_values, last_good_text, mirror_into, overlay, place_changes, preview,
    registry_free, reset, reset_conversation, restore_gate, restore_switch_choice, security_restore, store_mark, store_path, undo, view,
    write_back, Applied, Change, CONVERSATION_ERROR,
};

/// #154: a request's `confirm` member: missing is false, a boolean is itself, anything else is 400.
pub(crate) fn confirm_field(body: &Value) -> Result<bool, (u16, String)> {
    match body.get("confirm") {
        None => Ok(false),
        Some(Value::Bool(confirm)) => Ok(*confirm),
        Some(_) => Err((400, "confirm must be true or false".to_string())),
    }
}

/// #154: a change of a security setting (`Setting::confirmed`) needs `"confirm": true`. Without it the change is 409,
/// naming the first such setting of `changed`, before anything is written; a change of no security setting needs none.
pub(crate) fn check_confirmation(confirm: bool, changed: &[&Setting]) -> Result<(), (u16, String)> {
    match changed.iter().find(|setting| setting.confirmed()) {
        Some(setting) if !confirm => Err((
            409,
            format!("{} is a security setting: send the change again with \"confirm\": true", setting.key),
        )),
        _ => Ok(()),
    }
}

/// One registry entry.
pub(crate) struct Setting {
    /// The registry key, as the console names it.
    pub key: &'static str,
    /// Where the value lives in the configuration document (a JSON pointer of plain member names).
    pub pointer: &'static str,
    /// The value type and bounds (`kinds`): how a value is checked, read, refused and shown.
    pub kind: &'static dyn SettingKind,
    /// When a change takes effect: `live`, `next wake` or `restart`; `not read yet` for a setting that is stored and
    /// listed but that no reader uses yet (the turn engine settings, #218).
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
    /// configuration value): the value at the pointer (`found_in`) as the setting's kind reads it, else the default.
    pub(crate) fn read(&self, document: &Value) -> Value {
        self.found_in(document).and_then(|found| self.kind.read(found)).unwrap_or_else(|| self.default_value())
    }

    /// The member of `document` this setting reads: the pointer's, or, while that is missing or null, the kind's alias
    /// member (`SettingKind::alias`: `allow.group` for `allow.groups`, the precedence `config::canonical_config` reads).
    pub(crate) fn found_in<'a>(&self, document: &'a Value) -> Option<&'a Value> {
        let found = document.pointer(self.pointer);
        match self.kind.alias() {
            Some(alias) if found.is_none_or(Value::is_null) => document.pointer(alias).or(found),
            _ => found,
        }
    }

    /// `read` of a switch: whether it is on; readers ask it only of switches (registry test).
    pub(crate) fn switch(&self, document: &Value) -> bool {
        self.read(document) == Value::Bool(true)
    }

    /// #154: a security setting: every change of it needs an explicit confirmation (`check_confirmation`).
    pub(crate) fn confirmed(&self) -> bool {
        self.group == registry::SECURITY_GROUP
    }

    /// #154: an access setting (an allow or deny list, the owner): a change revokes like a whitelist write.
    pub(crate) fn access(&self) -> bool {
        self.kind.access()
    }

    /// `read` of an integer setting; readers ask it only of integer settings (registry test).
    pub(crate) fn integer(&self, document: &Value) -> i64 {
        self.read(document).as_i64().unwrap_or_default()
    }

    pub(crate) fn probability(&self, document: &Value) -> f64 {
        self.read(document).as_f64().unwrap_or_default()
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

/// #218: one setting kept per conversation (`conversation_registry()`): the store keeps one value per conversation,
/// never in `config.json`, and a conversation without one uses `global`, the global setting whose kind, default and
/// bounds it shares. It is never chat-visible.
pub(crate) struct ConversationSetting {
    /// The key the console and the audit name it by; never a key of `registry()` (registry test).
    pub key: &'static str,
    /// The global setting a conversation without its own value uses.
    pub global: &'static Setting,
    pub label: &'static str,
    pub hint: &'static str,
}

impl ConversationSetting {
    /// The value as stored, or `None` when the global setting's kind does not accept it.
    pub(crate) fn normalize(&self, value: &Value) -> Option<Value> {
        self.global.normalize(value)
    }

    /// Why `value`, which this setting refused, was refused, under this setting's key.
    fn refusal(&self, value: &Value) -> String {
        self.global.kind.refusal(self.key, value)
    }
}
