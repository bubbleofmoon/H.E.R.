//! #162: the model route page (`GET /settings/models`, `GET /api/settings/models`), which replaces the DSH model
//! catalog views for harness conversations: the selected runtime's harness chain in configured order, each route with
//! its settings, its health (`harness::route_health::report`) and its price now (`pricing::rates_at`), and per
//! conversation the route its harness turns ask first and the order they walk (the rules of `harness::routes::selected`
//! on the stored values, `harness::routes::conversation_order`). It reads only the configuration, the settings store and
//! the route health file, never DSH or a model. A conversation's route is the conversation setting `conversation.route`:
//! set, reset, undone and listed in history through the settings routes, which check admission under the configuration
//! gate.

use crate::runtime::{RuntimeArea, RuntimeContext};
use crate::settings::{CONVERSATION_ROUTE, HARNESS_FIRST_ROUTE, HARNESS_ROUTES};
use serde_json::{json, Value};

fn harness_credential(context: &RuntimeContext) -> Option<String> {
    context
        .read_text(RuntimeArea::Config, "harness-api-key")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// The global settings API applies the same credential masking as the model route list. This changes only response
/// fields. The settings view marks a masked effective value so the editor can require a full replacement.
pub(super) fn redact_route_settings(context: &RuntimeContext, reply: &mut Value) {
    let Some(key) = harness_credential(context) else { return };
    let redact = |value: &mut Value| {
        let mut redacted = false;
        if let Some(routes) = value.as_array_mut() {
            for route in routes {
                for field in ["id", "model"] {
                    let Some(slot) = route.get_mut(field) else { continue };
                    if let Some(text) = slot.as_str().filter(|text| text.contains(&key)) {
                        *slot = json!(crate::harness::redact_credential(text, &key));
                        redacted = true;
                    }
                }
            }
        }
        redacted
    };
    if let Some(settings) = reply.get_mut("settings").and_then(Value::as_array_mut) {
        if let Some(entry) = settings.iter_mut().find(|entry| entry["key"] == HARNESS_ROUTES.key) {
            redact(&mut entry["stored"]);
            if redact(&mut entry["value"]) {
                entry["redacted"] = json!(true);
            }
        }
    }
    if let Some(entries) = reply.get_mut("entries").and_then(Value::as_array_mut) {
        for entry in entries.iter_mut().filter(|entry| entry["setting"] == HARNESS_ROUTES.key) {
            redact(&mut entry["old"]);
            redact(&mut entry["new"]);
        }
    }
    if reply["setting"] == HARNESS_ROUTES.key {
        for field in ["value", "stored"] {
            if let Some(value) = reply.get_mut(field) {
                redact(value);
            }
        }
    }
}

/// `GET /api/settings/models`: `setting` (the conversation setting the page sets), `firstRoute` (the global
/// `harness.firstRoute`, `null` for none), `routes` in configured order (`id`, `model`, `reasoningEffort`,
/// `maxOutputTokens`, `retry429`, `health` with `state` `ok`, `failed` or `unknown` and, when known, `since` and `class`,
/// `price` with its label, currency, symbol, unit, tier, whether it is an estimate and its four rates), `allFailed`, and
/// `conversations` (each admitted conversation or one with a stored value: `conversation`, `admitted`, `stored`, `route`,
/// the route its next turn asks first or `null`, `inChain`, and `order`, the route ids its next turn walks). A chain that
/// is not configured or invalid gives `error` and no routes; `healthError` and `storeError` say why route health or
/// stored values cannot be read. Route ids and models are shown with the harness credential redacted.
pub(super) fn list(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, "settings require a selected runtime".to_string()));
    }
    let cfg = context.load_config().map_err(|_| (503, "runtime config is unavailable".to_string()))?;
    let credential = harness_credential(context);
    let shown = |text: &str| match credential.as_deref() {
        Some(key) => crate::harness::redact_credential(text, key),
        None => text.to_string(),
    };
    let mut reply = json!({"ok": true, "setting": CONVERSATION_ROUTE.key});
    reply["firstRoute"] = json!(HARNESS_FIRST_ROUTE.read(&cfg).as_str().map(|id| shown(id)));
    let chain = match crate::harness::routes::chain(&cfg) {
        Ok(chain) => chain,
        Err(error) => {
            reply["error"] = json!(error);
            Vec::new()
        }
    };
    let health = crate::harness::route_health::report(context, &cfg);
    let table = crate::pricing::resolve(cfg.get("pricing"));
    let now = crate::parity_clock::now_ms();
    let routes: Vec<Value> = chain
        .iter()
        .map(|route| {
            let seen = health
                .as_ref()
                .and_then(|report| report["routes"].as_array())
                .and_then(|entries| entries.iter().find(|entry| entry["id"] == route.id.as_str()));
            let rates = crate::pricing::rates_at(&route.model, now, &table);
            json!({
                "id": shown(&route.id),
                "model": shown(&route.model),
                "reasoningEffort": route.effort,
                "maxOutputTokens": route.max_output_tokens,
                "retry429": route.retry_429,
                "health": match seen {
                    Some(entry) => entry.as_object().map(|map| map.iter().filter(|(key, _)| *key != "id").map(|(key, value)| (key.clone(), value.clone())).collect::<serde_json::Map<_, _>>().into()).unwrap_or_else(|| json!({"state": "unknown"})),
                    None => json!({"state": "unknown"}),
                },
                "price": {
                    "label": rates.label, "currency": rates.currency, "symbol": rates.symbol, "unit": rates.unit,
                    "tier": rates.tier, "estimated": rates.estimated, "cacheHit": rates.cache_hit,
                    "cacheMiss": rates.cache_miss, "cacheWrite": rates.cache_write, "output": rates.output,
                },
            })
        })
        .collect();
    reply["routes"] = json!(routes);
    reply["allFailed"] = json!(health.as_ref().is_some_and(|report| report["allFailed"] == true));
    if !chain.is_empty() {
        if let Some(error) = health.as_ref().and_then(|report| report["error"].as_str()) {
            reply["healthError"] = json!(error);
        }
    }
    let view = crate::settings::conversation_view(context, &super::conversation::admitted_conversations(&cfg))?;
    if let Some(error) = view.get("storeError") {
        reply["storeError"] = error.clone();
    }
    let conversations: Vec<Value> = view["conversations"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let entry = &row["settings"][CONVERSATION_ROUTE.key];
                    let route = entry["value"].as_str();
                    let order: Vec<String> = crate::harness::routes::conversation_order(&chain, route)
                        .iter()
                        .map(|spec| shown(&spec.id))
                        .collect();
                    json!({
                        "conversation": row["conversation"],
                        "admitted": row["admitted"],
                        "stored": entry["stored"].as_str().map(|id| shown(id)),
                        "route": route.map(|id| shown(id)),
                        "inChain": route.is_some_and(|id| chain.iter().any(|known| known.id == id)),
                        "order": order,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    reply["conversations"] = json!(conversations);
    Ok(reply)
}

/// The page: the chain from `GET /api/settings/models` (each route's settings, health and price), then one row per
/// conversation with the route its next turn asks first and the order it walks, a select of the chain's routes with a
/// set button (`POST /api/settings/conversation/set` with the setting the list names) and a button that resets the
/// conversation to the configured order (`POST /api/settings/conversation/reset`); history and undo are on the
/// per-conversation settings page. Text is shown with `textContent` only.
pub(super) const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Model routes</title>
<style>body{font:14px system-ui,sans-serif;margin:16px;color:#222}table{border-collapse:collapse;margin:8px 0}td,th{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top;white-space:pre-wrap;max-width:40em}</style></head><body>
<h1>Model routes</h1>
<p><a href="/settings">Edit the global model route list</a></p>
<p>Harness turns ask a conversation's route first, then the other routes in configured order. History and undo: <a href="/settings/conversations">settings per conversation</a>.</p>
<p>Console token <input id="token" type="password" size="32"> <button id="use">Use</button> <span id="msg" role="status"></span></p>
<p id="chain"></p>
<p>First route of conversations without their own: <span id="first"></span></p>
<table id="routes"><thead><tr><th>Route</th><th>Model</th><th>Effort</th><th>Output cap</th><th>Retry 429</th><th>Health</th><th>Price</th></tr></thead><tbody></tbody></table>
<table id="conversations"><thead><tr><th>Conversation</th><th>Admitted</th><th>Route</th><th>Order</th><th>New route</th><th></th><th></th></tr></thead><tbody></tbody></table>
<script>
const $=s=>document.querySelector(s);
const token=()=>sessionStorage.getItem('consoleToken')||new URLSearchParams(location.search).get('token')||'';
$('#token').value=token();
const api=async(path,body)=>{const r=await fetch(path,{method:body?'POST':'GET',headers:Object.assign({'x-console-token':token()},body?{'content-type':'application/json'}:{}),body:body?JSON.stringify(body):undefined});const j=await r.json();if(body||!j.ok)$('#msg').textContent=j.ok?(j.changed===false?'Unchanged':'Saved'):(j.error||r.status);return j};
const cell=t=>{const td=document.createElement('td');td.textContent=t==null?'':String(t);return td};
const action=(text,run)=>{const b=document.createElement('button');b.textContent=text;b.onclick=run;const td=document.createElement('td');td.append(b);return td};
async function list(){const j=await api('/api/settings/models');if(!j.ok)return;$('#chain').textContent=j.error?'The route chain cannot be used: '+j.error:(j.allFailed?'Every route failed on its last request.':'');const problems=[j.healthError,j.storeError].filter(Boolean);if(problems.length)$('#msg').textContent=problems.join('; ');$('#first').textContent=j.firstRoute==null?'(configured order)':j.firstRoute;const rb=$('#routes tbody');rb.replaceChildren();for(const r of j.routes){const h=r.health;const p=r.price;const tr=document.createElement('tr');tr.append(cell(r.id),cell(r.model),cell(r.reasoningEffort),cell(r.maxOutputTokens),cell(r.retry429?'yes':'no'),cell(h.state+(h.class?' ('+h.class+')':'')+(h.since?' since '+new Date(h.since).toLocaleString():'')),cell(p.label+(p.estimated?' (estimated)':'')+': '+p.symbol+p.cacheMiss+' input, '+p.symbol+p.cacheHit+' cached input, '+p.symbol+p.output+' output per '+p.unit+' tokens, '+p.tier));rb.append(tr)}const cb=$('#conversations tbody');cb.replaceChildren();for(const c of j.conversations){const select=document.createElement('select');for(const r of j.routes){const o=document.createElement('option');o.value=r.id;o.textContent=r.id;select.append(o)}if(c.route!=null)select.value=c.route;const field=document.createElement('td');field.append(select);const tr=document.createElement('tr');tr.append(cell(c.conversation),cell(c.admitted?'yes':'no'),cell(c.route==null?'(configured order)':c.route+(c.inChain?'':' (not in the chain: ignored)')),cell(c.order.join(' > ')),field,action('Set',async()=>{const r=await api('/api/settings/conversation/set',{conversation:c.conversation,setting:j.setting,value:select.value});if(r.ok)list()}),action('Use configured order',async()=>{const r=await api('/api/settings/conversation/reset',{conversation:c.conversation,setting:j.setting});if(r.ok)list()}));cb.append(tr)}}
$('#use').onclick=()=>{sessionStorage.setItem('consoleToken',$('#token').value);list()};
list();
</script></body></html>
"##;
