//! #218: the conversation scope of the settings store (schema version 2). A setting of `conversation_registry()` keeps
//! one value per conversation in `conversation_settings`, keyed by the conversation's canonical key (`group:<id>` or
//! `private:<id>`, as `initial_prompts::canonical_key` writes it); its audit rows carry that key as their `scope`.
//!
//! The values live in the store only. Upgrade imports the legacy engine map once. A conversation's value resolves
//! as its stored value, else its global setting's
//! value (`ConversationSetting::global`, read on the loader's document: stored, then `config.json`, then the default).
//! A conversation key never becomes part of a JSON pointer, and a row whose conversation is not canonical is skipped.
//!
//! A store of version 1 holds no conversation value; the first write that keeps one migrates it inside its own
//! transaction (`store::migrate`). Only admitted conversations (`conversation_admitted`) get a value, and a write that
//! changes an access setting clears the values of every conversation it leaves unadmitted (`clear_unadmitted`), in its
//! own transaction. Writers hold the configuration gate (the console route's authority) and take the store gate inside
//! it (`write_store_with`), as every settings writer does.

use super::{
    inspect, now_ms, raw_config, read_stored_values, request_id, schema_version, write_store_with, Applied, Inspection,
    Undone, CHANGED_SINCE_ERROR, CONFIG_ERROR, LEGACY_ERROR, NOT_UNDOABLE_ERROR, OLD_VALUE_REFUSED_ERROR, SEED_ACTOR,
    STORAGE_ERROR,
};
use crate::runtime::RuntimeContext;
use crate::settings::{conversation_registry, find_conversation, registry, ConversationSetting, Setting, OWNER};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

pub(crate) fn has_legacy_engine_choices(context: &RuntimeContext) -> Result<bool, (u16, String)> {
    Ok(context.is_isolated() && raw_config(context).map_err(|error| (503, error))?.get("conversationEngines").is_some())
}

/// Caller holds the configuration gate. An existing scope's audit history is a receipt,
/// including when a later access change cleared the imported choice.
pub(crate) fn import_engine_choices(context: &RuntimeContext) -> Result<(), (u16, String)> {
    use crate::runtime::RuntimeArea;
    use crate::settings::CONVERSATION_ENGINE;

    let read = || raw_config(context).map_err(|error| (503, error));
    let document = read()?;
    let Some(old) = document.get("conversationEngines") else { return Ok(()) };
    let choices = old.as_object().ok_or((503, "conversationEngines must be an object".into()))?;
    if !choices.is_empty() {
        let (skipped, _) = write_store_with(context, false, &[], true, |conn| {
            let (effective, configured) = held_configuration(conn, context)?
                .ok_or((503, CONFIG_ERROR.to_string()))?;
            let mut skipped = 0;
            for (key, value) in choices {
                if !is_canonical(key) || !admitted_held(&effective, &configured, key) {
                    continue;
                }
                let changed: bool = conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM settings_audit WHERE scope=?1 AND key=?2)",
                    params![key, CONVERSATION_ENGINE.key],
                    |row| row.get(0),
                ).map_err(|_| (503, STORAGE_ERROR.to_string()))?;
                if (changed && false) || stored_row(conn, key, CONVERSATION_ENGINE.key)?.is_some() { // RED-M1
                    continue;
                }
                let Some(value) = CONVERSATION_ENGINE.normalize(value) else {
                    record_change(conn, &CONVERSATION_ENGINE, key, None, None, SEED_ACTOR, "migration")?;
                    skipped += 1;
                    continue; // The migration audit records this invalid choice.
                };
                record_change(conn, &CONVERSATION_ENGINE, key, None, Some(&value.to_string()), SEED_ACTOR, "migration")?;
            }
            Ok(skipped)
        })?;
        if skipped != 0 {
            crate::safe_println!("[settings] legacy engine import skipped {skipped} invalid choices; see settings history");
        }
    }
    // Read after the settings write-back. A crash before this replacement leaves the audit receipts.
    let mut fresh = read()?;
    fresh.as_object_mut().expect("raw config is an object").remove("conversationEngines");
    context.write_atomic(RuntimeArea::Config, "config.json", fresh.to_string().as_bytes())
        .map_err(|_| (503, "conversation engine import could not update config.json".into()))
}

/// The scoped choice and global default from one settings snapshot. The caller supplies
/// the loader's document for the registry's ordinary config and default fallback.
pub(crate) fn engine_choice(context: &RuntimeContext, cfg: &Value, key: &str) -> Result<Value, String> {
    use crate::settings::{CONVERSATION_ENGINE, ENGINE_DEFAULT};
    let Inspection::Ready(conn) = inspect(context)? else { return Ok(ENGINE_DEFAULT.read(cfg)) };
    let scoped = if schema_version(&conn)? >= 2 {
        stored_row(&conn, key, CONVERSATION_ENGINE.key).map_err(|(_, error)| error)?
    } else {
        None
    };
    if let Some((text, _)) = scoped {
        return serde_json::from_str::<Value>(&text).ok().and_then(|value| CONVERSATION_ENGINE.normalize(&value))
            .ok_or_else(|| "stored conversation engine is invalid".into());
    }
    Ok(read_stored_values(&conn)?.into_iter().find(|(setting, _)| setting.key == ENGINE_DEFAULT.key)
        .map_or_else(|| ENGINE_DEFAULT.read(cfg), |(_, value)| value))
}

/// The refusal of a conversation that is not `group:<id>` or `private:<id>`.
pub(crate) const CONVERSATION_ERROR: &str = "conversation must be group:<id> or private:<id>";

/// The refusal of a write that would store a value for a conversation that is not admitted.
const NOT_ADMITTED_ERROR: &str = "conversation is not admitted; admit it first";

/// The audit source of a value cleared because its conversation lost access; such an entry is never undone.
const ACCESS_SOURCE: &str = "access";

/// One stored conversation value the registry accepts.
struct Stored {
    setting: &'static ConversationSetting,
    conversation: String,
    value: Value,
    text: String,
    revision: i64,
}

/// The canonical key of a conversation as the console names it (`group:0042` is `group:42`; JavaScript whitespace
/// around it is trimmed), `None` when it is not a conversation key.
pub(crate) fn canonical_conversation(text: &str) -> Option<String> {
    crate::initial_prompts::canonical_key(text)
}

/// Whether `conversation` is already a canonical conversation key: ASCII letters, one `:` and digits only, so it holds
/// neither `~` nor `/`.
pub(super) fn is_canonical(conversation: &str) -> bool {
    canonical_conversation(conversation).as_deref() == Some(conversation)
}

/// Whether the bridge answers `conversation` under `cfg`, a configuration as `load_config` returns it: the inbound gate
/// admits it (its allow list, not denied), or it is the owner's private chat (closed-agent answers only that one).
pub(crate) fn conversation_admitted(cfg: &Value, conversation: &str) -> bool {
    crate::console::inbound_admitted(cfg, conversation) || crate::chat::is_owner_key(cfg, conversation)
}

/// The stored conversation values on an open connection, ordered by conversation and key; none on a store of version 1.
/// A row whose key the conversation registry does not have, whose conversation is not canonical, or whose value its
/// setting does not keep (`Setting::storable` of its global setting: a valid value, or none where the default is none,
/// which a reset stores) is skipped.
fn read_values(conn: &Connection) -> Result<Vec<Stored>, String> {
    if schema_version(conn)? < 2 {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare("SELECT conversation, key, value, revision FROM conversation_settings ORDER BY conversation, key")
        .map_err(|_| STORAGE_ERROR.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
        })
        .map_err(|_| STORAGE_ERROR.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| STORAGE_ERROR.to_string())?;
    let mut values = Vec::new();
    for (conversation, key, text, revision) in rows {
        let Some(setting) = find_conversation(&key) else { continue };
        if !is_canonical(&conversation) {
            continue;
        }
        let Some(value) = serde_json::from_str::<Value>(&text).ok().and_then(|value| setting.global.storable(&value)) else {
            continue;
        };
        values.push(Stored { setting, conversation, value, text, revision });
    }
    Ok(values)
}

/// #162: the stored value of the conversation setting `key` for `conversation` (canonicalized), read through the store's
/// read-only connection, for a reader that runs when a turn starts: `Some` with the value its global setting keeps
/// (`Setting::storable`: `null` for none, which a reset stores where the default is none), else `None`: a legacy context,
/// an unknown key, a key that is not a conversation key, no store, a store of version 1, no row, a value the registry
/// does not keep, or a store that cannot be read. The caller then reads the global setting; a store problem never fails a
/// turn. It never writes, migrates or creates the store, and takes no gate.
pub(crate) fn conversation_value(context: &RuntimeContext, conversation: &str, key: &str) -> Option<Value> {
    if !context.is_isolated() {
        return None;
    }
    let setting = find_conversation(key)?;
    let conversation = canonical_conversation(conversation)?;
    let Ok(Inspection::Ready(conn)) = inspect(context) else { return None };
    if schema_version(&conn).ok()? < 2 {
        return None;
    }
    let (text, _) = stored_row(&conn, &conversation, setting.key).ok()??;
    serde_json::from_str::<Value>(&text).ok().and_then(|value| setting.global.storable(&value))
}

/// The stored text and revision of `key` for `conversation`, read in the caller's transaction.
fn stored_row(conn: &Connection, conversation: &str, key: &str) -> Result<Option<(String, i64)>, (u16, String)> {
    conn.query_row(
        "SELECT value, revision FROM conversation_settings WHERE conversation=?1 AND key=?2",
        params![conversation, key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(|_| (503, STORAGE_ERROR.to_string()))
}

/// Writes one change of `conversation`'s value of `setting` and its audit row (scope: the conversation) in the caller's
/// transaction: `new` is the text to store, `None` removes it. Returns the stored revision (0 when removed). The
/// revision continues from the stored row or, without one, after this conversation's audit rows of the setting, so it
/// is never reused (the global rule of `record_change`, per conversation).
fn record_change(
    conn: &Connection,
    setting: &ConversationSetting,
    conversation: &str,
    old: Option<(String, i64)>,
    new: Option<&str>,
    actor: &str,
    source: &str,
) -> Result<i64, (u16, String)> {
    let failed = |_: rusqlite::Error| (503, STORAGE_ERROR.to_string());
    let at = now_ms();
    let revision = match new {
        Some(text) => {
            let revision = match &old {
                Some((_, revision)) => revision + 1,
                None => {
                    let changes: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM settings_audit WHERE scope=?1 AND key=?2",
                            params![conversation, setting.key],
                            |row| row.get(0),
                        )
                        .map_err(failed)?;
                    changes + 1
                }
            };
            conn.execute(
                "INSERT INTO conversation_settings(conversation, key, value, revision, updated_ms, updated_by)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(conversation, key) DO UPDATE SET value=excluded.value, revision=excluded.revision,
                 updated_ms=excluded.updated_ms, updated_by=excluded.updated_by",
                params![conversation, setting.key, text, revision, at, actor],
            )
            .map_err(failed)?;
            revision
        }
        None => {
            conn.execute(
                "DELETE FROM conversation_settings WHERE conversation=?1 AND key=?2",
                params![conversation, setting.key],
            )
            .map_err(failed)?;
            0
        }
    };
    #[cfg(test)]
    if super::FAIL_BEFORE_AUDIT.with(|fail| fail.replace(false)) {
        return Err((503, STORAGE_ERROR.to_string()));
    }
    conn.execute(
        "INSERT INTO settings_audit(at_ms, actor, source, scope, key, old_value, new_value, request_id)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![at, actor, source, conversation, setting.key, old.map(|(old_text, _)| old_text), new, request_id(at)],
    )
    .map_err(failed)?;
    Ok(revision)
}

/// The checks every conversation write makes before it writes: a selected runtime (else 503), a canonical conversation
/// key (else 400) and a setting of the conversation registry (else 400).
fn conversation_setting(
    context: &RuntimeContext,
    conversation: &str,
    key: &str,
) -> Result<&'static ConversationSetting, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    if !is_canonical(conversation) {
        return Err((400, CONVERSATION_ERROR.to_string()));
    }
    find_conversation(key).ok_or((400, "unknown setting".to_string()))
}

/// Sets `conversation`'s value of the conversation setting `key`: validated against its global setting's kind (400
/// with the refusal), then written with one audit row (actor, source) through `write_store_with`, which migrates a
/// version-1 store first. The same value again writes no value and no audit row (a migration still commits). Nothing
/// is written into `config.json`. The caller holds the configuration gate and has checked that the conversation is
/// admitted (`conversation_admitted`); the store does not check it again.
pub(crate) fn apply_conversation(
    context: &RuntimeContext,
    conversation: &str,
    key: &str,
    value: &Value,
    actor: &str,
    source: &str,
) -> Result<Applied, (u16, String)> {
    let setting = conversation_setting(context, conversation, key)?;
    let value = setting.normalize(value).ok_or_else(|| (400, setting.refusal(value)))?;
    store(context, setting, conversation, value, actor, source)
}

/// Stores the global setting's default for `conversation` (source `reset`): the conversation then has its own value,
/// equal to the default, whatever the global setting holds. A stored default already changes nothing.
pub(crate) fn reset_conversation(
    context: &RuntimeContext,
    conversation: &str,
    key: &str,
    actor: &str,
) -> Result<Applied, (u16, String)> {
    let setting = conversation_setting(context, conversation, key)?;
    store(context, setting, conversation, setting.global.default_value(), actor, "reset")
}

/// Writes one valid value for one conversation with its audit row (`apply_conversation`, `reset_conversation`).
fn store(
    context: &RuntimeContext,
    setting: &'static ConversationSetting,
    conversation: &str,
    value: Value,
    actor: &str,
    source: &str,
) -> Result<Applied, (u16, String)> {
    let text = value.to_string();
    let (applied, mirror_error) = write_store_with(context, false, &[], true, |conn| {
        let old = stored_row(conn, conversation, setting.key)?;
        if let Some((old_text, revision)) = &old {
            if *old_text == text {
                return Ok(Applied { value, revision: *revision, changed: false, mirror_error: None });
            }
        }
        let revision = record_change(conn, setting, conversation, old, Some(&text), actor, source)?;
        Ok(Applied { value, revision, changed: true, mirror_error: None })
    })?;
    Ok(Applied { mirror_error, ..applied })
}

/// Undoes a conversation's history entry for `store::undo`, in its transaction, with the global rules for that
/// conversation and setting: 409 and nothing written unless the entry's setting is in the conversation registry, its
/// scope is a canonical conversation key, it is not a value cleared by an access change (source `access`) and the
/// store is version 2; 409 too when the undo would store a value for a conversation that is not admitted (removing a
/// value is always allowed); and unless no later audit row names that conversation and setting, the stored value is still the
/// entry's new value and the registry still accepts the entry's old value. The old value is restored with one audit
/// row (source `undo`); an old value of none removes the stored value (nothing in `config.json` to remove).
pub(super) fn undo_entry(
    conn: &Connection,
    context: &RuntimeContext,
    entry: i64,
    row: (String, String, String, Option<String>, Option<String>),
    actor: &str,
) -> Result<Undone, (u16, String)> {
    let (conversation, key, source, old_text, new_text) = row;
    let failed = |_: rusqlite::Error| (503, STORAGE_ERROR.to_string());
    let setting = find_conversation(&key)
        .filter(|_| is_canonical(&conversation) && !matches!(source.as_str(), ACCESS_SOURCE | "migration" | "switch-rollback"))
        .ok_or((409, NOT_UNDOABLE_ERROR.to_string()))?;
    if schema_version(conn).map_err(|error| (503, error))? < 2 {
        return Err((409, NOT_UNDOABLE_ERROR.to_string()));
    }
    let later: i64 = conn
        .query_row(
            "SELECT count(*) FROM settings_audit WHERE scope=?1 AND key=?2 AND id>?3",
            params![conversation, setting.key, entry],
            |row| row.get(0),
        )
        .map_err(failed)?;
    let current = stored_row(conn, &conversation, setting.key)?;
    if later > 0 || current.as_ref().map(|(text, _)| text.as_str()) != new_text.as_deref() {
        return Err((409, CHANGED_SINCE_ERROR.to_string()));
    }
    let restored = old_text
        .map(|text| {
            serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| setting.global.storable(&value))
                .ok_or((409, OLD_VALUE_REFUSED_ERROR.to_string()))
        })
        .transpose()?;
    if restored.is_some() {
        // Only an admitted conversation gets a value: an undo that would store one checks admission on the configuration
        // as it stands (the caller holds the configuration gate), as a set does; one that removes a value stays allowed.
        let admitted = held_configuration(conn, context)?
            .is_some_and(|(effective, configured)| admitted_held(&effective, &configured, &conversation));
        if !admitted {
            return Err((409, NOT_ADMITTED_ERROR.to_string()));
        }
    }
    let restored_text = restored.as_ref().map(Value::to_string);
    record_change(conn, setting, &conversation, current, restored_text.as_deref(), actor, "undo")?;
    Ok(Undone { key: setting.key, conversation: Some(conversation), stored: restored, mirror_error: None })
}

/// The configuration the write in `conn`'s transaction leaves, for admission: `config.json` as it is now with the access
/// settings the transaction holds (each one's stored value, else the file's as the setting reads it), normalized as
/// `load_config` does (`RuntimeContext::effective_config_with`), and the configured owner's setting alone
/// (`{"ownerQQ": ...}`). `None` when the runtime cannot normalize that configuration; a `config.json` that cannot be read
/// is 503. It never reads the store through the overlay, so it is safe inside the store transaction.
fn held_configuration(conn: &Connection, context: &RuntimeContext) -> Result<Option<(Value, Value)>, (u16, String)> {
    let file = raw_config(context).map_err(|_| (503, CONFIG_ERROR.to_string()))?;
    let held = read_stored_values(conn).map_err(|error| (503, error))?;
    let access: Vec<(&'static Setting, Option<Value>)> = registry()
        .filter(|setting| setting.access())
        .map(|setting| {
            let value = held
                .iter()
                .find(|(stored, _)| stored.key == setting.key)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| setting.read(&file));
            (setting, Some(value))
        })
        .collect();
    let Ok(effective) = context.effective_config_with(&file, &access) else { return Ok(None) };
    let owner = access.iter().find(|(setting, _)| setting.key == OWNER.key).and_then(|(_, value)| value.clone());
    Ok(Some((effective, json!({"ownerQQ": owner}))))
}

/// Whether `conversation` is admitted on a `held_configuration`: the inbound gate admits it, or it is the configured
/// owner's private chat (so the admin agent mode's owner never takes the configured owner's chat away).
fn admitted_held(effective: &Value, configured: &Value, conversation: &str) -> bool {
    conversation_admitted(effective, conversation) || crate::chat::is_owner_key(configured, conversation)
}

/// Clears, in the caller's transaction (a write that changed an access setting, `write_store_with`), the stored value of
/// every conversation that is no longer admitted: the row is removed with one audit row each (actor `system`, source
/// `access`, scope the conversation; never undone). Admission is decided on the configuration that write leaves:
/// `config.json` as it is now with the access settings this transaction holds (each access setting's stored value,
/// else the file's as the setting reads it), normalized as `load_config` does (`RuntimeContext::effective_config_with`);
/// the owner's private chat counts by the owner setting too, so the admin agent mode's owner never clears the
/// configured owner's chat. A configuration the runtime cannot normalize clears nothing (the next access change looks
/// again); a `config.json` that cannot be read refuses the write (503).
pub(super) fn clear_unadmitted(conn: &Connection, context: &RuntimeContext) -> Result<(), (u16, String)> {
    let values = read_values(conn).map_err(|error| (503, error))?;
    if values.is_empty() {
        return Ok(());
    }
    let Some((effective, configured)) = held_configuration(conn, context)? else { return Ok(()) };
    for stored in values {
        if admitted_held(&effective, &configured, &stored.conversation) {
            continue;
        }
        let old = Some((stored.text, stored.revision));
        record_change(conn, stored.setting, &stored.conversation, old, None, SEED_ACTOR, ACCESS_SOURCE)?;
    }
    #[cfg(test)]
    if super::FAIL_AFTER_CLEAR.with(|fail| fail.replace(false)) {
        return Err((503, STORAGE_ERROR.to_string()));
    }
    Ok(())
}

/// The conversation scope as the console lists it: each conversation setting with its scope, label, hint, group and
/// when it applies (its global setting's), type and options (`SettingKind::view`), default and global setting; and
/// each conversation that is admitted (`admitted`, which the caller works out from the configuration) or holds a
/// stored value, sorted by key. Per conversation: whether it is admitted, and per setting the stored value (`null` when
/// none) and `value`, the stored value or else the global setting's value on the loader's document. `storeError` says
/// why stored values cannot be read.
pub(crate) fn conversation_view(context: &RuntimeContext, admitted: &[String]) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, LEGACY_ERROR.to_string()));
    }
    let document = context.load_config_document().map_err(|_| (503, CONFIG_ERROR.to_string()))?;
    let read = || -> Result<Vec<Stored>, String> {
        match inspect(context)? {
            Inspection::Missing | Inspection::Fresh => Ok(Vec::new()),
            Inspection::Ready(conn) => read_values(&conn),
        }
    };
    let (values, store_error) = match read() {
        Ok(values) => (values, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let mut conversations: Vec<String> = admitted.iter().filter(|key| is_canonical(key)).cloned().collect();
    conversations.extend(values.iter().map(|stored| stored.conversation.clone()));
    conversations.sort();
    conversations.dedup();
    let settings: Vec<Value> = conversation_registry()
        .map(|setting| {
            json!({
                "key": setting.key,
                "scope": "conversation",
                "group": setting.global.group,
                "label": setting.label,
                "hint": setting.hint,
                "applies": setting.global.applies,
                "kind": setting.global.kind.view(),
                "default": setting.global.default_value(),
                "global": setting.global.key,
            })
        })
        .collect();
    let rows: Vec<Value> = conversations
        .iter()
        .map(|conversation| {
            let mut entries = Map::new();
            for setting in conversation_registry() {
                let stored = values
                    .iter()
                    .find(|stored| stored.setting.key == setting.key && stored.conversation == *conversation)
                    .map(|stored| stored.value.clone());
                let value = stored.clone().unwrap_or_else(|| setting.global.read(&document));
                entries.insert(setting.key.to_string(), json!({"stored": stored, "value": value}));
            }
            json!({"conversation": conversation, "admitted": admitted.contains(conversation), "settings": entries})
        })
        .collect();
    let mut reply = json!({"ok": true, "settings": settings, "conversations": rows});
    if let Some(error) = store_error {
        reply["storeError"] = json!(error);
    }
    Ok(reply)
}
