//! Persona library and persona wake-word console JSON routes (`/api/personas*`; UI served at `/persona` via `console_ui`).
//! Mounted behind the console front (Host, credential, JSON and Origin checks, audit); storage and validation live in `persona.rs`.

use crate::console_front::AdminCaller;
use serde_json::{json, Value};
use std::collections::HashMap;

const JSON_TYPE: &str = "application/json; charset=utf-8";

pub fn is_route(path: &str) -> bool {
    path == "/api/personas" || path.starts_with("/api/personas/")
}

pub fn route(
    caller: AdminCaller<'_>, method: &str, path: &str, query: &HashMap<String, String>, body: &str,
) -> (u16, &'static str, String) {
    let context = caller.context();
    let object = || -> Result<Value, (u16, String)> {
        match serde_json::from_str::<Value>(body) {
            Ok(value) if value.is_object() => Ok(value),
            _ => Err((400, "body must be a JSON object".to_string())),
        }
    };
    let result = match (method, path) {
        ("GET", "/api/personas") => crate::persona::list(context),
        ("GET", "/api/personas/item") => crate::persona::item(context, query.get("id").map(String::as_str).unwrap_or("")),
        ("POST", "/api/personas/create") => object().and_then(|b| caller.with_authority(|a| crate::persona::create(context, &b, a))),
        ("POST", "/api/personas/update") => object().and_then(|b| caller.with_authority(|a| crate::persona::update(context, &b, a))),
        ("POST", "/api/personas/duplicate") => object().and_then(|b| caller.with_authority(|a| crate::persona::duplicate(context, &b, a))),
        ("POST", "/api/personas/delete") => object().and_then(|b| caller.with_authority(|a| crate::persona::delete(context, &b, a))),
        ("POST", "/api/personas/approve") => object().and_then(|b| caller.with_authority(|a| crate::persona::approve(context, &b, a))),
        ("POST", "/api/personas/assign") => object().and_then(|b| caller.with_authority(|a| crate::persona::assign(context, &b, a))),
        ("POST", "/api/personas/import-roles") => caller.with_authority(|a| crate::persona::import_roles(context, a)),
        ("GET", "/api/personas/wake-words") => crate::wake_words::view(context, query.get("conversation").map(String::as_str).unwrap_or("")),
        ("POST", "/api/personas/wake-words/undo") => object().and_then(|b| caller.with_authority(|a| crate::console::undo_wake_words(context, &b, a))),
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
