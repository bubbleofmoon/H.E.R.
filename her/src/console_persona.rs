//! Persona library console routes (`/personas`, `/api/personas*`). Mounted behind the console
//! front (Host, credential, JSON and Origin checks, audit); storage and validation live in `persona.rs`.

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};
use std::collections::HashMap;

const JSON_TYPE: &str = "application/json; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/personas" || path == "/api/personas" || path.starts_with("/api/personas/")
}

pub fn route(
    context: &RuntimeContext, method: &str, path: &str, query: &HashMap<String, String>, body: &str, agent_call: bool,
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
        ("GET", "/api/personas") => crate::persona::list(context),
        ("GET", "/api/personas/item") => crate::persona::item(context, query.get("id").map(String::as_str).unwrap_or("")),
        ("POST", "/api/personas/create") => object().and_then(|b| crate::persona::create(context, &b)),
        ("POST", "/api/personas/update") => object().and_then(|b| crate::persona::update(context, &b)),
        ("POST", "/api/personas/duplicate") => object().and_then(|b| crate::persona::duplicate(context, &b)),
        ("POST", "/api/personas/delete") => object().and_then(|b| crate::persona::delete(context, &b)),
        ("POST", "/api/personas/approve") => object().and_then(|b| crate::persona::approve(context, &b)),
        ("POST", "/api/personas/assign") => object().and_then(|b| crate::persona::assign(context, &b)),
        ("POST", "/api/personas/import-roles") => crate::persona::import_roles(context),
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
