//! Settings console page and routes (`GET /settings`, `GET /api/settings`, `GET /api/settings/history`,
//! `POST /api/settings/set`, `POST /api/settings/reset`, `POST /api/settings/undo`). Mounted behind the console front
//! (Host, credential, JSON and Origin checks, request audit); the registry, validation, the store and its history live
//! in `settings.rs` and `settings/`. Bodies and queries name the setting as `setting`, never `key`, which the console's
//! conversation-key preflight would refuse. The conversation scope's page and routes (#218) are in
//! `console_settings/conversation.rs`; the model route page and list (#162, `GET /settings/models`,
//! `GET /api/settings/models`) in `console_settings/models.rs`.

mod conversation;
mod models;

use crate::runtime::RuntimeContext;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const JSON_TYPE: &str = "application/json; charset=utf-8";
const HTML_TYPE: &str = "text/html; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/settings"
        || path == "/settings/conversations"
        || path == "/settings/models"
        || path == "/api/settings"
        || path.starts_with("/api/settings/")
}

/// `authority` takes the configuration gate and re-checks the console credential; a set, reset or undo holds it until
/// it commits and its write-back into `config.json` is done (the store gate is taken inside it,
/// `settings::store::write_store`).
pub fn route(
    context: &RuntimeContext, method: &str, path: &str, query: &HashMap<String, String>, body: &str, agent_call: bool,
    authority: crate::persona::Authority<'_>,
) -> (u16, &'static str, String) {
    if agent_call {
        return failure(403, "settings routes are restricted to administrators");
    }
    let result = match (method, path) {
        ("GET", "/settings") => return (200, HTML_TYPE, PAGE.to_string()),
        ("GET", "/settings/conversations") => return (200, HTML_TYPE, conversation::PAGE.to_string()),
        ("GET", "/settings/models") => return (200, HTML_TYPE, models::PAGE.to_string()),
        ("GET", "/api/settings") => crate::settings::view(context),
        ("GET", "/api/settings/conversations") => conversation::list(context),
        ("GET", "/api/settings/models") => models::list(context),
        ("GET", "/api/settings/history") => history(context, query),
        ("POST", "/api/settings/set") => set(context, body, authority),
        ("POST", "/api/settings/reset") => reset(context, body, authority),
        ("POST", "/api/settings/undo") => undo(context, body, authority),
        ("POST", "/api/settings/conversation/set") => conversation::set(context, body, authority),
        ("POST", "/api/settings/conversation/reset") => conversation::reset(context, body, authority),
        _ => Err((404, "unknown settings route".to_string())),
    };
    match result {
        Ok(mut value) => {
            match (method, path) {
                ("GET", "/api/settings" | "/api/settings/history") => models::redact_route_settings(context, &mut value),
                ("POST", "/api/settings/set" | "/api/settings/reset" | "/api/settings/undo")
                    if value["setting"] == crate::settings::HARNESS_ROUTES.key =>
                {
                    models::redact_route_settings(context, &mut value);
                }
                _ => {}
            }
            (200, JSON_TYPE, value.to_string())
        }
        Err((status, message)) => failure(status, &message),
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

/// `{"setting": <registry key>, "value": <value>}`, plus `"confirm": true` for a security setting. A change of an
/// access setting revokes like a whitelist write (`console::access_setting_change`).
fn set(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
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
        return crate::console::access_setting_change(context, authority, &change, commit);
    }
    let _gate = authority()?;
    commit()
}

/// `{"setting": <registry key>}`, plus `"confirm": true` for a security setting: stores the setting's default.
fn reset(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
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
        return crate::console::access_setting_change(context, authority, &change, commit);
    }
    let _gate = authority()?;
    commit()
}

/// `{"entry": <history entry id>}`, plus `"confirm": true` when the entry's setting is a security setting; the reply
/// names the entry, the setting and its stored value now (`null`: none), plus `mirrorError` when `config.json` could not
/// be updated (the undo stands). An undo of an access setting revokes like a whitelist write.
fn undo(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
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
        return crate::console::access_setting_change(context, authority, &crate::settings::Change::Undo { entry }, commit);
    }
    let _gate = authority()?;
    commit()
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

/// Lists every setting from `GET /api/settings` with a set (`POST /api/settings/set`: a number input for an integer
/// setting, a text input for a model id, a text area for a text setting (#140 prompt text) sent as typed, a checkbox for
/// a switch, a text input of comma- or space-separated ids for an id list and a text input for the owner, #154) and a
/// reset (`POST /api/settings/reset`) button, and the history from `GET /api/settings/history`, newest first, with an
/// undo button per entry (`POST /api/settings/undo`) and an older-entries button. A set, reset or undo of a security
/// setting (`confirm` in the list) first asks with `window.confirm` and then sends `"confirm": true`; the owner's
/// `overriddenBy` is shown beside its value. Text is shown with `textContent` only.
const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Settings</title>
<style>body{font:14px system-ui,sans-serif;margin:16px;color:#222}table{border-collapse:collapse;margin:8px 0}td,th{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top;white-space:pre-wrap;max-width:40em}</style></head><body>
<h1>Settings</h1>
<p>Console token <input id="token" type="password" size="32"> <button id="use">Use</button> <span id="msg" role="status"></span></p>
<table id="list"><thead><tr><th>Setting</th><th>Hint</th><th>Current</th><th>Default</th><th>Applies</th><th>New value</th><th></th><th></th></tr></thead><tbody></tbody></table>
<h2>History</h2>
<table id="history"><thead><tr><th>When</th><th>Setting</th><th>Old</th><th>New</th><th>Source</th><th>By</th><th></th></tr></thead><tbody></tbody></table>
<p><button id="older" hidden>Older</button></p>
<script>
const $=s=>document.querySelector(s);
const token=()=>sessionStorage.getItem('consoleToken')||new URLSearchParams(location.search).get('token')||'';
$('#token').value=token();
const api=async(path,body)=>{const r=await fetch(path,{method:body?'POST':'GET',headers:Object.assign({'x-console-token':token()},body?{'content-type':'application/json'}:{}),body:body?JSON.stringify(body):undefined});const j=await r.json();if(body||!j.ok)$('#msg').textContent=j.ok?(j.changed===false?'Unchanged':'Saved'):(j.error||r.status);return j};
const cell=t=>{const td=document.createElement('td');td.textContent=t==null?'':String(t);return td};
const action=(text,run)=>{const b=document.createElement('button');b.textContent=text;b.onclick=run;const td=document.createElement('td');td.append(b);return td};
const shown=v=>v==null?'(none)':JSON.stringify(v);
let next=null;
let security=new Set();
const ask=key=>confirm('Change the security setting '+key+'? A change of an allow or deny list or the owner revokes every conversation that loses access at once.');
async function list(){const j=await api('/api/settings');if(!j.ok)return;if(j.storeError)$('#msg').textContent='Stored settings are not applied: '+j.storeError;security=new Set(j.settings.filter(s=>s.confirm).map(s=>s.key));const tb=$('#list tbody');tb.replaceChildren();for(const s of j.settings){const tr=document.createElement('tr');const type=s.kind.type;const text=type==='text';const input=document.createElement(text?'textarea':'input');if(type==='integer'){input.type='number';input.min=s.kind.min;input.max=s.kind.max;input.value=s.value==null?'':s.value}else if(type==='switch'){input.type='checkbox';input.checked=s.value===true}else if(type==='id list'){input.type='text';input.size=40;input.value=(s.value||[]).join(', ')}else if(text){input.rows=8;input.cols=60;input.value=s.value==null?'':s.value}else{input.type='text';input.value=s.value==null?'':s.value}const value=()=>type==='integer'?Number(input.value):type==='switch'?input.checked:type==='id list'?input.value.split(/[\s,]+/).filter(Boolean):type==='owner id'?input.value.trim():input.value;const send=async(path,body)=>{if(s.confirm){if(!ask(s.key))return;body.confirm=true}const r=await api(path,body);if(r.ok)refresh()};const current=s.overriddenBy?shown(s.value)+' (in effect: '+shown(s.overriddenBy.value)+' from the '+s.overriddenBy.source+')':s.value;const field=document.createElement('td');field.append(input);tr.append(cell(s.label+' ('+s.key+')'+(s.confirm?' [needs confirmation]':'')),cell(s.hint),cell(current),cell(s.default),cell(s.applies),field,action('Set',()=>send('/api/settings/set',{setting:s.key,value:value()})),action('Reset',()=>send('/api/settings/reset',{setting:s.key})));tb.append(tr)}}
async function history(more){const j=await api('/api/settings/history'+(more&&next!=null?'?before='+next:''));if(!j.ok)return;const tb=$('#history tbody');if(!more)tb.replaceChildren();for(const e of j.entries){const tr=document.createElement('tr');tr.append(cell(new Date(e.atMs).toLocaleString()),cell(e.setting),cell(shown(e.old)),cell(shown(e.new)),cell(e.source),cell(e.actor),action('Undo',async()=>{const body={entry:e.entry};if(security.has(e.setting)){if(!ask(e.setting))return;body.confirm=true}const r=await api('/api/settings/undo',body);if(r.ok)refresh()}));tb.append(tr)}next=j.next;$('#older').hidden=next==null}
function refresh(){list();history(false)}
$('#use').onclick=()=>{sessionStorage.setItem('consoleToken',$('#token').value);refresh()};
$('#older').onclick=()=>history(true);
refresh();
</script></body></html>
"##;
