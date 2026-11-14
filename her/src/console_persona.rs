//! Persona library console routes (`/personas`, `/api/personas*`). Mounted behind the console
//! front (Host, credential, JSON and Origin checks, audit); storage and validation live in `persona.rs`.

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};
use std::collections::HashMap;

const JSON_TYPE: &str = "application/json; charset=utf-8";
const HTML_TYPE: &str = "text/html; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/personas" || path == "/api/personas" || path.starts_with("/api/personas/")
}

pub fn route(
    context: &RuntimeContext, method: &str, path: &str, query: &HashMap<String, String>, body: &str, agent_call: bool,
    authority: crate::persona::Authority<'_>,
) -> (u16, &'static str, String) {
    if agent_call {
        return failure(403, "persona routes are restricted to administrators");
    }
    let object = || -> Result<Value, (u16, String)> {
        match serde_json::from_str::<Value>(body) {
            Ok(value) if value.is_object() => Ok(value),
            _ => Err((400, "body must be a JSON object".to_string())),
        }
    };
    let result = match (method, path) {
        ("GET", "/personas") => return (200, HTML_TYPE, PAGE.to_string()),
        ("GET", "/api/personas") => crate::persona::list(context),
        ("GET", "/api/personas/item") => crate::persona::item(context, query.get("id").map(String::as_str).unwrap_or("")),
        ("POST", "/api/personas/create") => object().and_then(|b| crate::persona::create(context, &b, authority)),
        ("POST", "/api/personas/update") => object().and_then(|b| crate::persona::update(context, &b, authority)),
        ("POST", "/api/personas/duplicate") => object().and_then(|b| crate::persona::duplicate(context, &b, authority)),
        ("POST", "/api/personas/delete") => object().and_then(|b| crate::persona::delete(context, &b, authority)),
        ("POST", "/api/personas/approve") => object().and_then(|b| crate::persona::approve(context, &b, authority)),
        ("POST", "/api/personas/assign") => object().and_then(|b| crate::persona::assign(context, &b, authority)),
        ("POST", "/api/personas/import-roles") => crate::persona::import_roles(context, authority),
        _ => Err((404, "unknown persona route".to_string())),
    };
    match result {
        Ok(value) => (200, JSON_TYPE, value.to_string()),
        Err((status, message)) => failure(status, &message),
    }
}

fn failure(status: u16, message: &str) -> (u16, &'static str, String) {
    (status, JSON_TYPE, json!({"ok": false, "error": message}).to_string())
}

const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Personas</title>
<style>body{font:14px system-ui,sans-serif;margin:16px;color:#222}textarea{width:100%;min-height:6em}table{border-collapse:collapse;margin:8px 0}td,th{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top}label{display:block;margin:6px 0}</style></head><body>
<h1>Personas</h1>
<p>Console token <input id="token" type="password" size="32"> <button id="use">Use</button> <button id="import">Import role cards</button> <span id="msg" role="status"></span></p>
<table id="list"><thead><tr><th>ID</th><th>Name</th><th>Active version</th><th>Pending</th><th>Default</th></tr></thead><tbody></tbody></table>
<h2>Assignment</h2>
<p><label>Default persona ID <input id="default" size="8"></label> <button id="setDefault">Set default</button></p>
<p><label>Conversation <input id="conv" placeholder="group:123"></label> <label>Persona ID (empty clears) <input id="convPersona" size="8"></label> <button id="setOverride">Set override</button></p>
<pre id="assign"></pre>
<h2>Edit</h2>
<p><label>Persona ID (empty creates) <input id="id" size="8"></label> <button id="load">Load latest version</button></p>
<label>Name <input id="name"></label>
<label>Core <textarea id="core"></textarea></label>
<label>Notes <textarea id="notes"></textarea></label>
<label>Compact <textarea id="compact"></textarea></label>
<label>Names (comma separated) <input id="names" size="40"></label>
<label>Neediness (0-3) <input id="neediness" type="number" min="0" max="3"></label>
<label>Edit mode <select id="editMode"><option value="auto">auto</option><option value="manual">manual</option></select></label>
<label>Auto rules (JSON) <input id="autoRules" size="80"></label>
<p><button id="save">Save as new version</button> <button id="duplicate">Duplicate with this name</button> <button id="delete">Delete</button></p>
<h2>Versions</h2><table id="versions"><thead><tr><th>Version</th><th>Source</th><th>Approved</th><th>Core</th><th></th></tr></thead><tbody></tbody></table>
<script>
const $=s=>document.querySelector(s);
const token=()=>sessionStorage.getItem('consoleToken')||new URLSearchParams(location.search).get('token')||'';
$('#token').value=token();
const api=async(path,body)=>{const r=await fetch(path,{method:body?'POST':'GET',headers:Object.assign({'x-console-token':token()},body?{'content-type':'application/json'}:{}),body:body?JSON.stringify(body):undefined});const j=await r.json();const failed=(j.rebriefFailed||[]).map(f=>f.conversation);if(body||!j.ok)$('#msg').textContent=j.ok?(failed.length?'Saved; re-brief failed for '+failed.join(', ')+'; assign again to retry':'OK'):(j.error||r.status);return j};
const cell=t=>{const td=document.createElement('td');td.textContent=t==null?'':String(t);return td};
async function list(){const j=await api('/api/personas');if(!j.ok)return;const tb=$('#list tbody');tb.replaceChildren();for(const p of j.personas){const tr=document.createElement('tr');tr.append(cell(p.id),cell(p.name),cell(p.activeVersion),cell(p.pendingVersions.join(', ')),cell(j.default===p.id?'yes':''));tr.onclick=()=>{$('#id').value=p.id;load()};tb.append(tr)}$('#assign').textContent=JSON.stringify({default:j.default,overrides:j.overrides,rebriefDue:j.rebriefDue},null,1)}
async function load(){const j=await api('/api/personas/item?id='+encodeURIComponent($('#id').value));if(!j.ok)return;const vs=j.persona.versions;const v=vs[vs.length-1];for(const k of ['name','core','notes','compact','editMode'])$('#'+k).value=v[k];$('#names').value=v.names.join(', ');$('#neediness').value=v.traits.neediness;$('#autoRules').value=JSON.stringify(v.autoRules);const tb=$('#versions tbody');tb.replaceChildren();for(const x of vs.slice().reverse()){const tr=document.createElement('tr');const b=document.createElement('button');b.textContent=j.persona.activeVersion===x.version?'Active':'Approve';b.onclick=async()=>{await api('/api/personas/approve',{id:j.persona.id,version:x.version});load();list()};const td=document.createElement('td');td.append(b);tr.append(cell(x.version),cell(x.source),cell(x.approved?'yes':'no'),cell(x.core),td);tb.append(tr)}}
function fields(){let rules;try{rules=JSON.parse($('#autoRules').value||'null')}catch(e){rules=undefined}return{name:$('#name').value,core:$('#core').value,notes:$('#notes').value,compact:$('#compact').value,names:$('#names').value.split(',').map(s=>s.trim()).filter(Boolean),traits:{neediness:Number($('#neediness').value||1)},editMode:$('#editMode').value,autoRules:rules}}
$('#use').onclick=()=>{sessionStorage.setItem('consoleToken',$('#token').value);list()};
$('#load').onclick=load;
$('#save').onclick=async()=>{const id=$('#id').value.trim();const j=id?await api('/api/personas/update',Object.assign({id},fields())):await api('/api/personas/create',fields());if(j.ok){$('#id').value=j.id;load();list()}};
$('#duplicate').onclick=async()=>{const j=await api('/api/personas/duplicate',{id:$('#id').value,name:$('#name').value});if(j.ok){$('#id').value=j.id;load();list()}};
$('#delete').onclick=async()=>{if(confirm('Delete persona '+$('#id').value+'?')){await api('/api/personas/delete',{id:$('#id').value});list()}};
$('#import').onclick=async()=>{await api('/api/personas/import-roles',{});list()};
$('#setDefault').onclick=async()=>{const v=$('#default').value.trim();await api('/api/personas/assign',{default:v||null});list()};
$('#setOverride').onclick=async()=>{const v=$('#convPersona').value.trim();await api('/api/personas/assign',{conversation:$('#conv').value.trim(),persona:v||null});list()};
list();
</script></body></html>
"##;
