//! Setting kinds: each kind is a struct in its own file implementing `SettingKind`, the one interface the registry,
//! the store, the settings page and the chat tools use, so no code outside this folder matches on a kind.

mod integer;
mod model_id;
mod text;

pub(crate) use integer::{FileRule, Integer};
pub(crate) use model_id::ModelId;
pub(crate) use text::Text;

use serde_json::Value;

/// A setting's value type and bounds: how a value of it is checked, read from a configuration document, refused, shown
/// on the settings page and named in the owner's chat. A registry entry holds its kind as `&'static dyn SettingKind`.
pub(crate) trait SettingKind: Sync {
    /// The value as stored and read, or `None` when this kind does not accept it.
    fn normalize(&self, value: &Value) -> Option<Value>;

    /// The default; `Null` for a model id without one (not configured).
    fn default_value(&self) -> Value;

    /// How the next use reads `found`, the value at the setting's pointer in a configuration document (the loader's
    /// document with stored values applied, or any configuration value); `None` reads as the default. A value the store
    /// would refuse reads as the default, unless the kind's own file rule says otherwise (`FileRule::Clamp`).
    fn read(&self, found: &Value) -> Option<Value> {
        self.normalize(found)
    }

    /// Why `value`, which this kind refused, was refused, under the registry key `key`.
    fn refusal(&self, key: &str, value: &Value) -> String;

    /// The kind as the settings page shows it: `type`, then its bounds (`min` and `max`, or `maxChars`).
    fn view(&self) -> Value;

    /// Why a chat value was refused, under the chat's name `name`.
    fn chat_requirement(&self, name: &str) -> String;

    /// The compiled default of a text setting, `None` for every other kind: `prompt_text` renders it as the fallback,
    /// and the overlay gives a text setting's refused stored value its last good version.
    fn text_default(&self) -> Option<&'static str> {
        None
    }

    /// The registry test's checks of one registry entry of this kind: its bounds, its default and its reader.
    #[cfg(test)]
    fn check_entry(&self, setting: &super::Setting);
}
