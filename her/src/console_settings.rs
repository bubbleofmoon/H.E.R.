//! Settings console page and routes (`GET /settings`, `GET /api/settings`, `GET /api/settings/history`,
//! `POST /api/settings/set`, `POST /api/settings/reset`, `POST /api/settings/undo`). Mounted behind the console front
//! (Host, credential, JSON and Origin checks, request audit); the registry, validation, the store and its history live
//! in `settings.rs`. Bodies and queries name the setting as `setting`, never `key`, which the console's
//! conversation-key preflight would refuse.

use crate::runtime::RuntimeContext;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const JSON_TYPE: &str = "application/json; charset=utf-8";
const HTML_TYPE: &str = "text/html; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/settings" || path == "/api/settings" || path.starts_with("/api/settings/")
}

/// `authority` takes the configuration gate and re-checks the console credential; a set, reset or undo holds it until
/// it commits.
pub fn route(
    context: &RuntimeContext, method: &str, path: &str, query: &HashMap<String, String>, body: &str, agent_call: bool,
    authority: crate::persona::Authority<'_>,
) -> (u16, &'static str, String) {
    if agent_call {
        return failure(403, "settings routes are restricted to administrators");
    }
    let result = match (method, path) {
        ("GET", "/settings") => return (200, HTML_TYPE, PAGE.to_string()),
        ("GET", "/api/settings") => crate::settings::view(context),
        ("GET", "/api/settings/history") => history(context, query),
        ("POST", "/api/settings/set") => set(context, body, authority),
        ("POST", "/api/settings/reset") => reset(context, body, authority),
        ("POST", "/api/settings/undo") => undo(context, body, authority),
        _ => Err((404, "unknown settings route".to_string())),
    };
    match result {
        Ok(value) => (200, JSON_TYPE, value.to_string()),
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

/// The reply of a set or reset: the setting, the stored value, its revision and whether it changed.
fn applied_reply(setting: &str, applied: crate::settings::Applied) -> Value {
    json!({
        "ok": true,
        "setting": setting,
        "value": applied.value,
        "revision": applied.revision,
        "changed": applied.changed,
    })
}

/// `{"setting": <registry key>, "value": <value>}`.
fn set(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
    let fields = body_fields(body, &["setting", "value"])?;
    let setting = setting_field(&fields)?;
    let value = fields.get("value").ok_or((400, "value is required".to_string()))?;
    let _gate = authority()?;
    let applied = crate::settings::apply(context, setting, value, "administrator", "console")?;
    Ok(applied_reply(setting, applied))
}

/// `{"setting": <registry key>}`: stores the setting's default.
fn reset(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
    let fields = body_fields(body, &["setting"])?;
    let setting = setting_field(&fields)?;
    let _gate = authority()?;
    let applied = crate::settings::reset(context, setting, "administrator")?;
    Ok(applied_reply(setting, applied))
}

/// `{"entry": <history entry id>}`; the reply names the entry, the setting and its stored value now (`null`: none).
fn undo(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
    let fields = body_fields(body, &["entry"])?;
    let entry = fields
        .get("entry")
        .and_then(Value::as_i64)
        .filter(|entry| *entry > 0)
        .ok_or((400, "entry must be a positive integer".to_string()))?;
    let _gate = authority()?;
    let undone = crate::settings::undo(context, entry, "administrator")?;
    Ok(json!({"ok": true, "entry": entry, "setting": undone.key, "stored": undone.stored}))
}

/// `?setting=<registry key>&before=<entry id>`, both optional; other query members are ignored.
fn history(context: &RuntimeContext, query: &HashMap<String, String>) -> Result<Value, (u16, String)> {
    let before = query
        .get("before")
        .map(|text| text.parse::<i64>().ok().filter(|id| *id > 0).ok_or((400, "before must be a positive integer".to_string())))
        .transpose()?;
    crate::settings::history(context, query.get("setting").map(String::as_str), before)
}

fn failure(status: u16, message: &str) -> (u16, &'static str, String) {
    (status, JSON_TYPE, json!({"ok": false, "error": message}).to_string())
}

/// Lists every setting from `GET /api/settings` with a set (`POST /api/settings/set`: a number input for an integer
/// setting, a text input for a model id) and a reset (`POST /api/settings/reset`) button, and the history from
/// `GET /api/settings/history`, newest first, with an undo button per entry (`POST /api/settings/undo`) and an
/// older-entries button. Text is shown with `textContent` only.
const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Settings</title>
<style>body{font:14px system-ui,sans-serif;margin:16px;color:#222}table{border-collapse:collapse;margin:8px 0}td,th{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top}</style></head><body>
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
async function list(){const j=await api('/api/settings');if(!j.ok)return;if(j.storeError)$('#msg').textContent='Stored settings are not applied: '+j.storeError;const tb=$('#list tbody');tb.replaceChildren();for(const s of j.settings){const tr=document.createElement('tr');const input=document.createElement('input');const integer=s.kind.type==='integer';if(integer){input.type='number';input.min=s.kind.min;input.max=s.kind.max}else{input.type='text'}input.value=s.value==null?'':s.value;const field=document.createElement('td');field.append(input);tr.append(cell(s.label+' ('+s.key+')'),cell(s.hint),cell(s.value),cell(s.default),cell(s.applies),field,action('Set',async()=>{const r=await api('/api/settings/set',{setting:s.key,value:integer?Number(input.value):input.value});if(r.ok)refresh()}),action('Reset',async()=>{const r=await api('/api/settings/reset',{setting:s.key});if(r.ok)refresh()}));tb.append(tr)}}
async function history(more){const j=await api('/api/settings/history'+(more&&next!=null?'?before='+next:''));if(!j.ok)return;const tb=$('#history tbody');if(!more)tb.replaceChildren();for(const e of j.entries){const tr=document.createElement('tr');tr.append(cell(new Date(e.atMs).toLocaleString()),cell(e.setting),cell(shown(e.old)),cell(shown(e.new)),cell(e.source),cell(e.actor),action('Undo',async()=>{const r=await api('/api/settings/undo',{entry:e.entry});if(r.ok)refresh()}));tb.append(tr)}next=j.next;$('#older').hidden=next==null}
function refresh(){list();history(false)}
$('#use').onclick=()=>{sessionStorage.setItem('consoleToken',$('#token').value);refresh()};
$('#older').onclick=()=>history(true);
refresh();
</script></body></html>
"##;
