//! Setting kinds: each kind is a struct in its own file implementing `SettingKind`, the one interface the registry,
//! the store, the settings page and the chat tools use, so no code outside this folder matches on a kind.

mod choice;
mod daily_time;
mod id_list;
mod integer;
mod model_id;
mod owner_id;
mod probability;
mod route_id;
mod route_list;
mod secret;
mod switch;
mod text;
mod time_zone;
mod voice_number;

pub(crate) use choice::Choice;
pub(crate) use daily_time::{daily_parts, DailyTime};
pub(crate) use id_list::IdList;
#[cfg(test)]
pub(crate) use id_list::ID_LIST_MAX;
pub(crate) use integer::{FileRule, Integer};
pub(crate) use model_id::ModelId;
pub(crate) use owner_id::OwnerId;
pub(crate) use probability::Probability;
pub(crate) use route_id::RouteId;
pub(crate) use route_list::RouteList;
pub(crate) use secret::{AdministratorPasswordPhc, ApiKey};
pub(crate) use switch::Switch;
pub(crate) use text::Text;
pub(crate) use time_zone::TimeZoneName;
pub(crate) use voice_number::VoiceNumber;

use serde_json::Value;

/// A setting's value type and bounds: how a value of it is checked, read from a configuration document, refused, shown
/// on the settings page and named in the owner's chat. A registry entry holds its kind as `&'static dyn SettingKind`.
pub(crate) trait SettingKind: Sync {
    /// The value as stored and read, or `None` when this kind does not accept it.
    fn normalize(&self, value: &Value) -> Option<Value>;

    /// The default; `Null` for a model id without one (not configured) and for the owner (no owner).
    fn default_value(&self) -> Value;

    /// How the next use reads `found`, the value at the setting's pointer in a configuration document (the loader's
    /// document with stored values applied, or any configuration value); `None` reads as the default. A value the store
    /// would refuse reads as the default, unless the kind's own file rule says otherwise (`FileRule::Clamp`, a switch,
    /// an id list).
    fn read(&self, found: &Value) -> Option<Value> {
        self.normalize(found)
    }

    /// Why `value`, which this kind refused, was refused, under the registry key `key`.
    fn refusal(&self, key: &str, value: &Value) -> String;

    /// The kind as the settings page shows it: `type`, then its bounds (`min` and `max`, `maxChars`, `maxItems` or
    /// `options`).
    fn view(&self) -> Value;

    /// Why a chat value was refused, under the chat's name `name`.
    fn chat_requirement(&self, name: &str) -> String;

    /// A retained chat spelling for a setting whose configuration member has a different name.
    fn chat_pointer(&self) -> Option<&'static str> {
        None
    }

    /// The compiled default of a text setting, `None` for every other kind: `prompt_text` renders it as the fallback,
    /// and the overlay gives a text setting's refused stored value its last good version.
    fn text_default(&self) -> Option<&'static str> {
        None
    }

    /// The member that stands in, in a configuration document, while the pointer's member is missing or null (an older
    /// spelling the runtime still reads, such as `allow.group`); `None` for every kind but an id list with an alias.
    fn alias(&self) -> Option<&'static str> {
        None
    }

    /// Whether `found`, the value `config.json` holds for this setting, already holds `stored`, so the write-back leaves
    /// the file's spelling as it is: equality, unless the kind reads two spellings the same (an id list, the owner).
    fn file_holds(&self, found: &Value, stored: &Value) -> bool {
        found == stored
    }

    /// The value the store keeps when it seeds the setting from `found`, its value in `config.json`; `None` when the
    /// store cannot keep one. The accepted value (`normalize`), unless the kind keeps what the next use reads (a
    /// clamped integer, a switch, an id list) or reads a blank value as its default (the owner).
    fn seed(&self, found: &Value) -> Option<Value> {
        self.normalize(found)
    }

    /// Whether a change of this kind is an access change, revoked like a whitelist write (an allow or deny list, the
    /// owner: `console::access_setting_change`).
    fn access(&self) -> bool {
        false
    }

    /// The registry test's checks of one registry entry of this kind: its bounds, its default and its reader.
    #[cfg(test)]
    fn check_entry(&self, setting: &super::Setting);
}

/// A configuration document that holds `value` at `pointer` and nothing else, for the kinds' registry checks.
#[cfg(test)]
fn document_at(pointer: &str, value: Value) -> Value {
    let mut document = value;
    for name in pointer.split('/').skip(1).collect::<Vec<_>>().into_iter().rev() {
        let mut object = serde_json::Map::new();
        object.insert(name.to_string(), document);
        document = Value::Object(object);
    }
    document
}
