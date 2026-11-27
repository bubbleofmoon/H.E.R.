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

/// The global settings API applies the same credential masking as the model route list. This changes only response
/// fields. The settings view marks a masked effective value so the editor can require a full replacement.
pub(super) fn redact_route_settings(context: &RuntimeContext, reply: &mut Value) {
    let credentials = crate::settings::stored_secrets(context).map(|mut keys| {
        keys.extend(crate::settings::harness_credential(context));
        keys.sort_unstable_by_key(|key| std::cmp::Reverse(key.len()));
        keys
    });
    let creds_ref = credentials.as_deref().ok();
    if let Some(settings) = reply.get_mut("settings").and_then(Value::as_array_mut) {
        if let Some(entry) = settings.iter_mut().find(|entry| entry["key"] == HARNESS_ROUTES.key) {
            crate::settings::redact_route_array(&mut entry["stored"], creds_ref);
            if crate::settings::redact_route_array(&mut entry["value"], creds_ref) {
                entry["redacted"] = json!(true);
            }
        }
    }
    if let Some(entries) = reply.get_mut("entries").and_then(Value::as_array_mut) {
        for entry in entries.iter_mut().filter(|entry| entry["setting"] == HARNESS_ROUTES.key) {
            crate::settings::redact_route_array(&mut entry["old"], creds_ref);
            crate::settings::redact_route_array(&mut entry["new"], creds_ref);
        }
    }
    if reply["setting"] == HARNESS_ROUTES.key {
        for field in ["value", "stored"] {
            if let Some(value) = reply.get_mut(field) {
                crate::settings::redact_route_array(value, creds_ref);
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
    let models_snap = crate::settings::models_snapshot(context, super::conversation::admitted_conversations)?;
    let cfg = &models_snap.cfg;
    let route_credentials = models_snap
        .store_error
        .is_none()
        .then(|| models_snap.endpoint_snapshot.as_ref().map(|snap| snap.all_secrets.as_slice()))
        .flatten();
    let shown = |text: &str| match route_credentials {
        Some(keys) => keys
            .iter()
            .fold(text.to_string(), |acc, key| crate::harness::redact_credential(&acc, key)),
        None => "[REDACTED]".to_string(),
    };
    let mut reply = json!({"ok": true, "setting": CONVERSATION_ROUTE.key});
    reply["firstRoute"] = json!(HARNESS_FIRST_ROUTE.read(cfg).as_str().map(shown));
    let chain = match crate::harness::routes::chain(cfg) {
        Ok(chain) => chain,
        Err(error) => {
            reply["error"] = json!(error);
            Vec::new()
        }
    };
    let health = crate::harness::route_health::report(context, cfg);
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
                    "label": shown(&rates.label), "currency": shown(&rates.currency), "symbol": shown(&rates.symbol), "unit": rates.unit,
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
    if let Some(error) = &models_snap.store_error {
        reply["storeError"] = json!(error);
    }
    match &models_snap.endpoint_snapshot {
        Some(snapshot) => {
            reply["effectiveModelEndpoint"] = json!(snapshot.effective_endpoint);
            reply["effectiveModelEndpointSource"] =
                json!(snapshot.effective_source.map(crate::settings::ModelEndpointSource::as_str));
        }
        None => {
            reply["effectiveModelEndpoint"] = Value::Null;
            reply["effectiveModelEndpointSource"] = Value::Null;
        }
    }
    let conversations: Vec<Value> = models_snap.conversations["conversations"]
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
                        "stored": entry["stored"].as_str().map(shown),
                        "route": route.map(shown),
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
