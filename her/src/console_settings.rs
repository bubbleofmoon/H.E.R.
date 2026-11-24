//! Settings console JSON routes (`GET /api/settings`, `GET /api/settings/history`,
//! `POST /api/settings/set`, `POST /api/settings/reset`, `POST /api/settings/undo`). Mounted behind the console front
//! (Host, credential, JSON and Origin checks, request audit); the registry, validation, the store and its history live
//! in `settings.rs` and `settings/`. The conversation scope's JSON routes (#218) are in
//! `console_settings/conversation.rs`; the model route list (#162, `GET /api/settings/models`) in
//! `console_settings/models.rs`. The V3 SPA renders these views at `/settings`, `/conversations`, and `/models`.

mod conversation;
mod models;

use crate::console_front::AdminCaller;
use crate::runtime::RuntimeContext;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const JSON_TYPE: &str = "application/json; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/api/settings" || path.starts_with("/api/settings/")
}

/// `caller` takes the configuration gate and re-checks the console credential; a set, reset or undo holds it until
/// it commits and its write-back into `config.json` is done (the store gate is taken inside it,
/// `settings::store::write_store`).
pub fn route(
    caller: AdminCaller<'_>,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    body: &str,
) -> (u16, &'static str, String, Option<crate::config_tx::ConfigDiff>) {
    let context = caller.context();
    // The model route list only reads state, even if startup left a legacy engine map.
    let import_diff = if (method, path) != ("GET", "/api/settings/models") {
        match crate::config_tx::import_engine_choices(context)
            .map_err(crate::social_state::SocialError::into_http)
        {
            Ok(diff) => diff,
            Err((status, error)) => {
                let (status, ctype, body) = failure(status, &error);
                return (status, ctype, body, None);
            }
        }
    } else {
        None
    };
    let result = match (method, path) {
        ("GET", "/api/settings") => crate::settings::view(context),
        ("GET", "/api/settings/conversations") => conversation::list(context),
        ("GET", "/api/settings/models") => models::list(context),
        ("GET", "/api/settings/history") => history(context, query),
        ("POST", "/api/settings/set") => set(caller, body),
        ("POST", "/api/settings/reset") => reset(caller, body),
        ("POST", "/api/settings/undo") => undo(caller, body),
        ("POST", "/api/settings/conversation/set") => conversation::set(caller, body),
        ("POST", "/api/settings/conversation/reset") => conversation::reset(caller, body),
        ("POST", "/api/settings/conversation/switch") => conversation::switch(caller, body),
        _ => Err((404, "unknown settings route".to_string())),
    };
    match result {
        Ok(mut value) => {
            match (method, path) {
                ("GET", "/api/settings" | "/api/settings/history") => {
                    models::redact_route_settings(context, &mut value);
                }
                ("POST", "/api/settings/set" | "/api/settings/reset" | "/api/settings/undo")
                    if value["setting"] == crate::settings::HARNESS_ROUTES.key =>
                {
                    models::redact_route_settings(context, &mut value);
                }
                _ => {}
            }
            (200, JSON_TYPE, value.to_string(), import_diff)
        }
        Err((status, message)) => {
            let (status, ctype, body) = failure(status, &message);
            (status, ctype, body, import_diff)
        }
    }
}

/// The members of a JSON object body, refused when it names any member but `allowed`.
fn body_fields(body: &str, allowed: &[&str]) -> Result<Map<String, Value>, (u16, String)> {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return Err((400, "body must be a JSON object".to_string()));
    };
    if fields.keys().any(|name| !allowed.contains(&name.as_str())) {
        return Err((400, "unknown settings field".to_string()));
    }
    Ok(fields)
}

/// The `setting` member, a string.
fn setting_field(fields: &Map<String, Value>) -> Result<&str, (u16, String)> {
    fields.get("setting").and_then(Value::as_str).ok_or((400, "setting is required".to_string()))
}

/// The reply of a set or reset: the setting, the stored value, its revision and whether it changed, plus `mirrorError`
/// when `config.json` could not be updated (the stored change stands).
fn applied_reply(setting: &str, applied: crate::settings::Applied) -> Value {
    let mut reply = json!({
        "ok": true,
        "setting": setting,
        "value": applied.value,
        "revision": applied.revision,
        "changed": applied.changed,
    });
    if crate::settings::find(setting).is_some_and(|setting| setting.secret) {
        reply["isSet"] = reply.as_object_mut().and_then(|fields| fields.remove("value")).unwrap_or(Value::Null);
    }
    if let Some(error) = applied.mirror_error {
        reply["mirrorError"] = json!(error);
    }
    reply
}

/// #154: the request's `confirm` (`settings::confirm_field`) and the confirmation a change of `setting` needs
/// (`settings::check_confirmation`): 409 for a security setting without `"confirm": true`, before anything is written.
/// An unknown setting needs none here (the change itself answers 400 or 404).
fn confirmed(fields: &Map<String, Value>, setting: Option<&crate::settings::Setting>) -> Result<(), (u16, String)> {
    let confirm = crate::settings::confirm_field(&Value::Object(fields.clone()))?;
    crate::settings::check_confirmation(confirm, setting.as_slice())
}

/// The registry setting history entry `entry` changes, for the undo's confirmation. Every failure of the look is the
/// error (404: no such entry yet, 409: its setting is no longer in the registry, 503: a store that cannot be read right
/// now); `undo` decides what each lets through.
fn entry_setting(context: &RuntimeContext, entry: i64) -> Result<&'static crate::settings::Setting, (u16, String)> {
    let previewed = crate::settings::preview(context, &crate::settings::Change::Undo { entry })?;
    previewed.first().map(|(setting, _)| *setting).ok_or((503, "the settings entry cannot be read".to_string()))
}

/// Test seam: runs on this thread right after the undo's first look at its entry, so a test can create the entry the
/// look did not find.
#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_UNDO_LOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// `{"setting": <registry key>, "value": <value>}`, plus `"confirm": true` for a security setting.
/// A change of an access setting revokes like a whitelist write
/// (`config_tx::commit_access_setting_change`).
fn access_change(
    context: &RuntimeContext,
    authority: crate::persona::Authority<'_>,
    change: &crate::settings::Change<'_>,
    commit: impl FnOnce() -> Result<Value, (u16, String)>,
) -> Result<Value, (u16, String)> {
    crate::config_tx::commit_access_setting_change(context, authority, change, commit)
}

fn set(caller: AdminCaller<'_>, body: &str) -> Result<Value, (u16, String)> {
    let context = caller.context();
    let fields = body_fields(body, &["setting", "value", "confirm"])?;
    let setting = setting_field(&fields)?;
    let value = fields.get("value").ok_or((400, "value is required".to_string()))?;
    let found = crate::settings::find(setting);
    confirmed(&fields, found)?;
    let commit = || -> Result<Value, (u16, String)> {
        let applied = crate::settings::apply(context, setting, value, "administrator", "console")?;
        Ok(applied_reply(setting, applied))
    };
    if found.is_some_and(|found| found.access()) {
        let change = crate::settings::Change::Set { key: setting, value };
        return caller
            .with_authority(|authority| access_change(context, authority, &change, commit));
    }
    caller.with_authority(|authority| {
        crate::config_tx::commit_setting_write(context, authority, commit)
    })
}

/// `{"setting": <registry key>}`, plus `"confirm": true` for a security setting: stores the setting's default.
fn reset(caller: AdminCaller<'_>, body: &str) -> Result<Value, (u16, String)> {
    let context = caller.context();
    let fields = body_fields(body, &["setting", "confirm"])?;
    let setting = setting_field(&fields)?;
    let found = crate::settings::find(setting);
    confirmed(&fields, found)?;
    let commit = || -> Result<Value, (u16, String)> {
        let applied = crate::settings::reset(context, setting, "administrator")?;
        Ok(applied_reply(setting, applied))
    };
    if found.is_some_and(|found| found.access()) {
        let change = crate::settings::Change::Reset { key: setting };
        return caller
            .with_authority(|authority| access_change(context, authority, &change, commit));
    }
    caller.with_authority(|authority| {
        crate::config_tx::commit_setting_write(context, authority, commit)
    })
}

/// `{"entry": <history entry id>}`, plus `"confirm": true` when the entry's setting is a security setting; the reply
/// names the entry, the setting and its stored value now (`null`: none), plus `mirrorError` when `config.json` could not
/// be updated (the undo stands). An undo of an access setting revokes like a whitelist write.
fn undo(caller: AdminCaller<'_>, body: &str) -> Result<Value, (u16, String)> {
    let context = caller.context();
    let fields = body_fields(body, &["entry", "confirm"])?;
    let entry = fields
        .get("entry")
        .and_then(Value::as_i64)
        .filter(|entry| *entry > 0)
        .ok_or((400, "entry must be a positive integer".to_string()))?;
    // Fail closed. An unknown entry (404) or a retired setting (409) is answered by the undo itself, under the store gate,
    // and since the entry may be created between this look and that answer, a look that did not name the entry's setting
    // never lets the plain path undo a security setting. When the look fails otherwise, an unconfirmed undo is refused
    // with that failure and a confirmed one takes the access path, which looks again under the configuration gate.
    let looked = entry_setting(context, entry);
    #[cfg(test)]
    if let Some(hook) = AFTER_UNDO_LOOK.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
    let (target, access) = match looked {
        Ok(target) => (Some(target), target.access()),
        Err((404 | 409, _)) => (None, false),
        Err(error) => {
            if !crate::settings::confirm_field(&Value::Object(fields.clone()))? {
                return Err(error);
            }
            (None, true)
        }
    };
    confirmed(&fields, target)?;
    let commit = || -> Result<Value, (u16, String)> {
        let permit = |setting: &crate::settings::Setting| -> Result<(), (u16, String)> {
            if target.is_none() && !access && setting.confirmed() {
                return Err((409, format!("{} is a security setting: send the undo again", setting.key)));
            }
            Ok(())
        };
        let undone = crate::settings::undo(context, entry, "administrator", &permit)?;
        let mut reply = json!({"ok": true, "entry": entry, "setting": undone.key, "stored": undone.stored});
        if let Some(conversation) = undone.conversation {
            reply["conversation"] = json!(conversation);
        }
        if let Some(error) = undone.mirror_error {
            reply["mirrorError"] = json!(error);
        }
        Ok(reply)
    };
    if access {
        return caller.with_authority(|authority| {
            access_change(context, authority, &crate::settings::Change::Undo { entry }, commit)
        });
    }
    caller.with_authority(|authority| {
        crate::config_tx::commit_setting_write(context, authority, commit)
    })
}

/// `?setting=<key>&conversation=<conversation key>&before=<entry id>`, all optional (#218: a conversation key is
/// canonicalized, anything else is 400); other query members are ignored.
fn history(context: &RuntimeContext, query: &HashMap<String, String>) -> Result<Value, (u16, String)> {
    let before = query
        .get("before")
        .map(|text| text.parse::<i64>().ok().filter(|id| *id > 0).ok_or((400, "before must be a positive integer".to_string())))
        .transpose()?;
    let conversation = query
        .get("conversation")
        .map(|text| {
            crate::settings::canonical_conversation(text).ok_or((400, crate::settings::CONVERSATION_ERROR.to_string()))
        })
        .transpose()?;
    crate::settings::history(context, query.get("setting").map(String::as_str), conversation.as_deref(), before)
}

fn failure(status: u16, message: &str) -> (u16, &'static str, String) {
    (status, JSON_TYPE, json!({"ok": false, "error": message}).to_string())
}
