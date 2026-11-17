//! Settings console page and routes (`GET /settings`, `GET /api/settings`, `POST /api/settings/set`). Mounted behind
//! the console front (Host, credential, JSON and Origin checks, request audit); the registry, validation and the store
//! live in `settings.rs`. Bodies name the setting as `setting`, never `key`, which the console's conversation-key
//! preflight would refuse.

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};

const JSON_TYPE: &str = "application/json; charset=utf-8";
const HTML_TYPE: &str = "text/html; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/settings" || path == "/api/settings" || path.starts_with("/api/settings/")
}

/// `authority` takes the configuration gate and re-checks the console credential; a set holds it until it commits.
pub fn route(
    context: &RuntimeContext, method: &str, path: &str, body: &str, agent_call: bool,
    authority: crate::persona::Authority<'_>,
) -> (u16, &'static str, String) {
    if agent_call {
        return failure(403, "settings routes are restricted to administrators");
    }
    let result = match (method, path) {
        ("GET", "/settings") => return (200, HTML_TYPE, PAGE.to_string()),
        ("GET", "/api/settings") => crate::settings::view(context),
        ("POST", "/api/settings/set") => set(context, body, authority),
        _ => Err((404, "unknown settings route".to_string())),
    };
    match result {
        Ok(value) => (200, JSON_TYPE, value.to_string()),
        Err((status, message)) => failure(status, &message),
    }
}

/// `{"setting": <registry key>, "value": <value>}`; the reply names the setting, the stored value, its revision and
/// whether it changed.
fn set(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return Err((400, "body must be a JSON object".to_string()));
    };
    if fields.keys().any(|name| !matches!(name.as_str(), "setting" | "value")) {
        return Err((400, "unknown settings field".to_string()));
    }
    let setting = fields.get("setting").and_then(Value::as_str).ok_or((400, "setting is required".to_string()))?;
    let value = fields.get("value").ok_or((400, "value is required".to_string()))?;
    let _gate = authority()?;
    let applied = crate::settings::apply(context, setting, value, "administrator", "console")?;
    Ok(json!({
        "ok": true,
        "setting": setting,
        "value": applied.value,
        "revision": applied.revision,
        "changed": applied.changed,
    }))
}

fn failure(status: u16, message: &str) -> (u16, &'static str, String) {
    (status, JSON_TYPE, json!({"ok": false, "error": message}).to_string())
}

/// Lists every setting from `GET /api/settings` and sets one through `POST /api/settings/set`: a number input for an
/// integer setting, a text input for a model id. Text is shown with `textContent` only.
const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Settings</title>
<style>body{font:14px system-ui,sans-serif;margin:16px;color:#222}table{border-collapse:collapse;margin:8px 0}td,th{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top}</style></head><body>
<h1>Settings</h1>
<p>Console token <input id="token" type="password" size="32"> <button id="use">Use</button> <span id="msg" role="status"></span></p>
<table id="list"><thead><tr><th>Setting</th><th>Hint</th><th>Current</th><th>Default</th><th>Applies</th><th>New value</th><th></th></tr></thead><tbody></tbody></table>
<script>
const $=s=>document.querySelector(s);
const token=()=>sessionStorage.getItem('consoleToken')||new URLSearchParams(location.search).get('token')||'';
$('#token').value=token();
const api=async(path,body)=>{const r=await fetch(path,{method:body?'POST':'GET',headers:Object.assign({'x-console-token':token()},body?{'content-type':'application/json'}:{}),body:body?JSON.stringify(body):undefined});const j=await r.json();if(body||!j.ok)$('#msg').textContent=j.ok?(j.changed?'Saved':'Unchanged'):(j.error||r.status);return j};
const cell=t=>{const td=document.createElement('td');td.textContent=t==null?'':String(t);return td};
async function list(){const j=await api('/api/settings');if(!j.ok)return;if(j.storeError)$('#msg').textContent='Stored settings are not applied: '+j.storeError;const tb=$('#list tbody');tb.replaceChildren();for(const s of j.settings){const tr=document.createElement('tr');const input=document.createElement('input');const integer=s.kind.type==='integer';if(integer){input.type='number';input.min=s.kind.min;input.max=s.kind.max}else{input.type='text'}input.value=s.value==null?'':s.value;const b=document.createElement('button');b.textContent='Set';b.onclick=async()=>{const r=await api('/api/settings/set',{setting:s.key,value:integer?Number(input.value):input.value});if(r.ok)list()};const field=document.createElement('td');field.append(input);const action=document.createElement('td');action.append(b);tr.append(cell(s.label+' ('+s.key+')'),cell(s.hint),cell(s.value),cell(s.default),cell(s.applies),field,action);tb.append(tr)}}
$('#use').onclick=()=>{sessionStorage.setItem('consoleToken',$('#token').value);list()};
list();
</script></body></html>
"##;
