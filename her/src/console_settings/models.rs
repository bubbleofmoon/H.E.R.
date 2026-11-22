//! #162: the model route list (`GET /api/settings/models`), which replaces the DSH model
//! catalog views for harness conversations: the selected runtime's harness chain in configured order, each route with
//! its settings, its health (`harness::route_health::report`) and its price now (`pricing::rates_at`), and per
//! conversation the route its harness turns ask first and the order they walk (the rules of `harness::routes::selected`
//! on the stored values, `harness::routes::conversation_order`). It reads only the configuration, the settings store and
//! the route health file, never DSH or a model. A conversation's route is the conversation setting `conversation.route`:
//! set, reset, undone and listed in history through the settings routes, which check admission under the configuration
//! gate.

use crate::runtime::RuntimeContext;
use crate::settings::{CONVERSATION_ROUTE, HARNESS_FIRST_ROUTE, HARNESS_ROUTES};
use serde_json::{json, Value};

fn route_redactor(context: &RuntimeContext) -> impl Fn(&str) -> String {
    let credentials = crate::settings::stored_secrets(context).map(|mut keys| {
        keys.extend(crate::settings::harness_credential(context));
        keys.sort_unstable_by_key(|key| std::cmp::Reverse(key.len()));
        keys
    });
    move |text| match &credentials {
        Ok(keys) => keys.iter().fold(text.to_string(), |text, key| crate::harness::redact_credential(&text, key)),
        Err(_) => "[REDACTED]".to_string(),
    }
}

/// The global settings API applies the same credential masking as the model route list. This changes only response
/// fields. The settings view marks a masked effective value so the editor can require a full replacement.
pub(super) fn redact_route_settings(context: &RuntimeContext, reply: &mut Value) {
    let shown = route_redactor(context);
    let redact = |value: &mut Value| {
        let mut redacted = false;
        if let Some(routes) = value.as_array_mut() {
            for route in routes {
                for field in ["id", "model"] {
                    let Some(slot) = route.get_mut(field) else { continue };
                    if let Some(text) = slot.as_str() {
                        let safe = shown(text);
                        if safe != text {
                            *slot = json!(safe);
                            redacted = true;
                        }
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
/// stored values cannot be read. Route ids and models are shown with credentials redacted.
pub(super) fn list(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    if !context.is_isolated() {
        return Err((503, "settings require a selected runtime".to_string()));
    }
    let cfg = context.load_config().map_err(|_| (503, "runtime config is unavailable".to_string()))?;
    let shown = route_redactor(context);
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
