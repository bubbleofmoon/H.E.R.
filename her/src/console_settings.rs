//! Settings console routes (`GET /api/settings`, `POST /api/settings/set`). Mounted behind the console front (Host,
//! credential, JSON and Origin checks, request audit); the registry, validation and the store live in `settings.rs`.
//! Bodies name the setting as `setting`, never `key`, which the console's conversation-key preflight would refuse.

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};

const JSON_TYPE: &str = "application/json; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/api/settings" || path.starts_with("/api/settings/")
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
