//! #218: the conversation scope of the settings page (`GET /settings/conversations`, `GET /api/settings/conversations`,
//! `POST /api/settings/conversation/set`, `POST /api/settings/conversation/reset`). Undo and history are the settings
//! routes' own (`POST /api/settings/undo`, `GET /api/settings/history?conversation=`). Bodies name the conversation as
//! `conversation` and the setting as `setting`, never `key`, which the console's conversation-key preflight would
//! refuse. The settings store keeps the desired engine; the switch also drains and moves the active generation.

use super::{applied_reply, body_fields, setting_field};
use crate::runtime::RuntimeContext;
use serde_json::{json, Map, Value};

/// The refusal of a write to a conversation the bridge does not answer.
const NOT_ADMITTED_ERROR: &str = "conversation is not admitted; admit it first";

const SWITCH_NOTE: &str = "Switch waits for the current turn and moves this conversation. Engine reset and undo apply before its next turn. The model route applies from the next harness turn.";

/// The `conversation` member: a conversation key, canonicalized (`group:0042` is `group:42`), else 400.
fn conversation_field(fields: &Map<String, Value>) -> Result<String, (u16, String)> {
    fields
        .get("conversation")
        .and_then(Value::as_str)
        .and_then(crate::settings::canonical_conversation)
        .ok_or((400, crate::settings::CONVERSATION_ERROR.to_string()))
}

/// Every admitted conversation under `cfg`: the admitted ids of both allow lists and the owner's private chat, as
/// canonical keys, sorted. The model route list (`console_settings/models.rs`) lists the same conversations.
pub(super) fn admitted_conversations(cfg: &Value) -> Vec<String> {
    let mut keys: Vec<String> = ["group", "private"]
        .into_iter()
        .flat_map(|kind| {
            crate::console::admitted_ids(cfg, kind)
                .into_iter()
                .filter_map(move |id| crate::settings::canonical_conversation(&format!("{kind}:{id}")))
        })
        .collect();
    let owner = format!("private:{}", crate::chat::owner_qq(cfg));
    if crate::settings::conversation_admitted(cfg, &owner) && crate::settings::canonical_conversation(&owner).as_deref() == Some(owner.as_str()) {
        keys.push(owner);
    }
    keys.sort();
    keys.dedup();
    keys
}

fn legacy_refused(context: &RuntimeContext) -> Result<(), (u16, String)> {
    if context.is_isolated() { Ok(()) } else { Err((503, "settings require a selected runtime".to_string())) }
}

fn loaded_config(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    context.load_config().map_err(|_| (503, "runtime config is unavailable".to_string()))
}

/// The reply of a conversation set or reset: the global set route's reply with the conversation.
fn conversation_reply(conversation: &str, setting: &str, applied: crate::settings::Applied) -> Value {
    let mut reply = applied_reply(setting, applied);
    reply["conversation"] = json!(conversation);
    reply
}

/// `GET /api/settings/conversations` (`settings::conversation_view`), with `note` and, per conversation, `engineToday`:
/// the desired engine for its next admission (`Engine::read_choice`), or `refused` if the choice is unreadable.
pub(super) fn list(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    legacy_refused(context)?;
    let cfg = loaded_config(context)?;
    let mode = crate::console::runtime_mode(context);
    let mut reply = crate::settings::conversation_view(context, &admitted_conversations(&cfg))?;
    if let Some(rows) = reply["conversations"].as_array_mut() {
        for row in rows {
            let conversation = row["conversation"].as_str().unwrap_or_default().to_string();
            let today = crate::turn_engine::Engine::read_choice(context, &cfg, &conversation, &mode).map_or("refused", crate::turn_engine::Engine::id);
            row["engineToday"] = json!(today);
        }
    }
    reply["note"] = json!(SWITCH_NOTE);
    Ok(reply)
}

/// `{"conversation": <key>, "setting": <conversation setting key>, "value": <value>}`, and
/// `{"conversation", "setting"}` for a reset. Order of refusals: the body, the conversation key and an unknown setting
/// (400); then, under the configuration gate, a conversation that is not admitted (409) and a refused value (400).
fn write(
    context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>, reset: bool,
) -> Result<Value, (u16, String)> {
    legacy_refused(context)?;
    let allowed: &[&str] = if reset { &["conversation", "setting"] } else { &["conversation", "setting", "value"] };
    let fields = body_fields(body, allowed)?;
    let conversation = conversation_field(&fields)?;
    let setting = setting_field(&fields)?;
    crate::settings::find_conversation(setting).ok_or((400, "unknown setting".to_string()))?;
    let value = if reset { None } else { Some(fields.get("value").ok_or((400, "value is required".to_string()))?) };
    let _gate = authority()?;
    if !crate::settings::conversation_admitted(&loaded_config(context)?, &conversation) {
        return Err((409, NOT_ADMITTED_ERROR.to_string()));
    }
    let applied = match value {
        Some(value) => crate::settings::apply_conversation(context, &conversation, setting, value, "administrator", "console")?,
        None => crate::settings::reset_conversation(context, &conversation, setting, "administrator")?,
    };
    Ok(conversation_reply(&conversation, setting, applied))
}

/// `POST /api/settings/conversation/set`.
pub(super) fn set(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
    write(context, body, authority, false)
}

/// `POST /api/settings/conversation/reset`: stores the global setting's default for that conversation.
pub(super) fn reset(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
    write(context, body, authority, true)
}

/// Wait outside the configuration gate so the admitted turn can finish its sends.
pub(super) fn switch(context: &RuntimeContext, body: &str, authority: crate::persona::Authority<'_>) -> Result<Value, (u16, String)> {
    use crate::turn_engine::Engine;
    legacy_refused(context)?;
    let fields = body_fields(body, &["conversation", "engine"])?;
    let key = conversation_field(&fields)?;
    let value = fields.get("engine").ok_or((400, "engine is required".to_string()))?;
    let requested = Engine::parse(value).map_err(|error| (400, error))?;
    let _gates = crate::engine_switch::drain(context, &key)?;
    let setting = &crate::settings::CONVERSATION_ENGINE;
    let (mode, applied, rollback) = {
        let _config = authority()?;
        let cfg = loaded_config(context)?;
        let mode = crate::console::runtime_mode(context);
        if !crate::settings::conversation_admitted(&cfg, &key) || !crate::console::access_allowed(&cfg, &mode, &key) {
            return Err((409, NOT_ADMITTED_ERROR.into()));
        }
        if mode == "reserved2" && !crate::console::inbound_admitted(&cfg, &key) {
            return Err((409, NOT_ADMITTED_ERROR.into()));
        }
        if !matches!(mode.as_str(), "chat" | "closed-agent" | "reserved2") {
            return Err((409, crate::turn_engine::UNSUPPORTED_MODE.into()));
        }
        let (applied, rollback) = crate::settings::apply_switch_choice(context, &key, value)?;
        (mode, applied, rollback)
    };
    let result = crate::engine_switch::prepare(context, &key, &mode, requested);
    if result.is_err() {
        if let Some(rollback) = rollback {
            let _config = crate::console::config_gate(context)?;
            crate::settings::restore_switch_choice(context, &key, rollback)?;
        }
    }
    result?;
    Ok(conversation_reply(&key, setting.key, applied))
}

/// The page lists each conversation's settings. The engine has a select and a Switch button. The model route
/// has a text input and a Set button. Each setting has a Reset (`POST /api/settings/conversation/reset`)
/// button, and a history button that lists `GET /api/settings/history?conversation=<key>`, newest first, with an undo
/// button per entry (`POST /api/settings/undo`). Text is shown with `textContent` only.
pub(super) const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Settings per conversation</title>
<style>body{font:14px system-ui,sans-serif;margin:16px;color:#222}table{border-collapse:collapse;margin:8px 0}td,th{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top;white-space:pre-wrap;max-width:40em}</style></head><body>
<h1>Settings per conversation</h1>
<p id="note"></p>
<p>Console token <input id="token" type="password" size="32"> <button id="use">Use</button> <span id="msg" role="status"></span></p>
<table id="settings"><thead><tr><th>Setting</th><th>Hint</th><th>Default</th><th>Global setting</th></tr></thead><tbody></tbody></table>
<table id="list"><thead><tr><th>Conversation</th><th>Admitted</th><th>Engine today</th><th>Setting</th><th>Applies</th><th>Stored</th><th>New value</th><th></th><th></th><th></th></tr></thead><tbody></tbody></table>
<h2 id="historyTitle">History</h2>
<table id="history"><thead><tr><th>When</th><th>Setting</th><th>Old</th><th>New</th><th>Source</th><th>By</th><th></th></tr></thead><tbody></tbody></table>
<script>
const $=s=>document.querySelector(s);
const token=()=>sessionStorage.getItem('consoleToken')||new URLSearchParams(location.search).get('token')||'';
$('#token').value=token();
const api=async(path,body)=>{const r=await fetch(path,{method:body?'POST':'GET',headers:Object.assign({'x-console-token':token()},body?{'content-type':'application/json'}:{}),body:body?JSON.stringify(body):undefined});const j=await r.json();if(body||!j.ok)$('#msg').textContent=j.ok?(j.changed===false?'Unchanged':'Saved'):(j.error||r.status);return j};
const cell=t=>{const td=document.createElement('td');td.textContent=t==null?'':String(t);return td};
const action=(text,run)=>{const b=document.createElement('button');b.textContent=text;b.onclick=run;const td=document.createElement('td');td.append(b);return td};
const shown=v=>v==null?'(none)':JSON.stringify(v);
let shownConversation=null;
async function list(){const j=await api('/api/settings/conversations');if(!j.ok)return;$('#note').textContent=j.note;if(j.storeError)$('#msg').textContent='Stored settings cannot be read: '+j.storeError;const st=$('#settings tbody');st.replaceChildren();for(const s of j.settings){const tr=document.createElement('tr');tr.append(cell(s.label+' ('+s.key+')'),cell(s.hint),cell(s.default),cell(s.global));st.append(tr)}const tb=$('#list tbody');tb.replaceChildren();for(const c of j.conversations){for(const s of j.settings){const v=c.settings[s.key];const tr=document.createElement('tr');const select=document.createElement(s.kind.options?'select':'input');for(const o of s.kind.options||[]){const opt=document.createElement('option');opt.value=o;opt.textContent=o;select.append(opt)}select.value=v.value==null?'':v.value;const field=document.createElement('td');field.append(select);tr.append(cell(c.conversation),cell(c.admitted?'yes':'no'),cell(c.engineToday),cell(s.key),cell(v.value),cell(shown(v.stored)),field,s.key==='conversation.engine'?action('Switch',async()=>{$('#msg').textContent='Waiting for the conversation to finish its current turn…';const r=await api('/api/settings/conversation/switch',{conversation:c.conversation,engine:select.value});if(r.ok){$('#msg').textContent='Engine selected.';refresh()}}):action('Set',async()=>{const r=await api('/api/settings/conversation/set',{conversation:c.conversation,setting:s.key,value:select.value});if(r.ok)refresh()}),action('Reset',async()=>{const r=await api('/api/settings/conversation/reset',{conversation:c.conversation,setting:s.key});if(r.ok)refresh()}),action('History',()=>{shownConversation=c.conversation;history()}));tb.append(tr)}}}
async function history(){const tb=$('#history tbody');tb.replaceChildren();if(shownConversation==null)return;$('#historyTitle').textContent='History of '+shownConversation;const j=await api('/api/settings/history?conversation='+encodeURIComponent(shownConversation));if(!j.ok)return;for(const e of j.entries){const tr=document.createElement('tr');tr.append(cell(new Date(e.atMs).toLocaleString()),cell(e.setting),cell(shown(e.old)),cell(shown(e.new)),cell(e.source),cell(e.actor),action('Undo',async()=>{const r=await api('/api/settings/undo',{entry:e.entry});if(r.ok)refresh()}));tb.append(tr)}}
function refresh(){list();history()}
$('#use').onclick=()=>{sessionStorage.setItem('consoleToken',$('#token').value);refresh()};
refresh();
</script></body></html>
"##;
