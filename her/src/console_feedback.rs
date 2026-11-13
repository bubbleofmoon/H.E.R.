//! Feedback routes of the console (ticket 23): `GET|POST /api/socialV2/feedback` and
//! `POST /api/socialV2/feedback-clear`, plus the owner push that `notifyOwnerOnError` enables.
//!
//! Child of `console.rs` (`#[path]`): it uses that file's private helpers. The store itself is
//! `feedback.rs`; this file is the HTTP policy in the order of the frozen JS handler
//! (`bridge.js`, around 6474-6520).

use super::*;

fn scope_of(context: &RuntimeContext) -> String {
    context
        .runtime_root()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "legacy".to_string())
}

fn failure((status, message): (u16, String)) -> (u16, Value) {
    (status, json!({ "ok": false, "error": message }))
}

/// `GET /api/socialV2/feedback` (the global agent gate already refuses agent callers).
pub(super) fn get(context: &RuntimeContext) -> (u16, Value) {
    match crate::feedback::read(context) {
        Ok(entries) => (200, json!({ "ok": true, "entries": entries })),
        Err(error) => failure(error),
    }
}

/// `POST /api/socialV2/feedback-clear`: console only.
pub(super) fn clear(context: &RuntimeContext, headers: &HashMap<String, String>) -> (u16, Value) {
    if headers.contains_key("x-agent-token") {
        return failure((403, "该接口仅控制台可用".to_string()));
    }
    match crate::feedback::clear(context) {
        Ok(()) => {
            crate::safe_println!("[console] 清空 AI 反馈");
            (200, json!({ "ok": true }))
        }
        Err(error) => failure(error),
    }
}

/// `POST /api/socialV2/feedback`.
pub(super) fn post(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> (u16, Value) {
    match post_inner(context, cfg, headers, body) {
        Ok(value) => (200, value),
        Err(error) => failure(error),
    }
}

fn post_inner(
    context: &RuntimeContext,
    cfg: &Value,
    headers: &HashMap<String, String>,
    body: &str,
) -> Result<Value, (u16, String)> {
    let input = parse_object_body(body)?;
    let key = agent::body_text(&input, "key");
    let level = match input.get("level").and_then(Value::as_str) {
        Some("warning") => "warning",
        Some("error") => "error",
        _ => "info",
    };
    let raw = agent::body_text(&input, "message");
    if key.is_empty() || raw.is_empty() {
        return Err((400, "key 和 message 不能为空".to_string()));
    }
    if let Some(token) = headers.get("x-agent-token") {
        if !agent_token_ok_for(context, &key, token)? {
            return Err((403, "agent token 无效".to_string()));
        }
        if !social_v2_projection_session_allowed(cfg, &key) {
            return Err((403, "目标不在当前模式允许范围内".to_string()));
        }
        if !tool_enabled(cfg, "feedback") {
            return Err((403, "工具未启用：qq_report_feedback".to_string()));
        }
    }
    if !crate::feedback::allow(&scope_of(context), &key, now_ms()) {
        return Err((429, "反馈过于频繁，请稍后再试".to_string()));
    }
    let max_length = agent::js_number_or(cfg.pointer("/socialV2/feedback/maxLength"), 500.0).max(1.0);
    let message = crate::social_mem::truncate_text(&raw, max_length as i64);
    // Keep the response's JS text contract, but mask the complete source before
    // cutting the copy retained in feedback, diagnostics or owner notices.
    let safe_message = crate::social_mem::truncate_text(&crate::activity::redact(context, &raw), max_length as i64);
    crate::feedback::append(
        context,
        json!({
            "id": crate::feedback::new_id(now_ms()),
            "key": key,
            "level": level,
            "message": safe_message,
            "time": crate::sticker::now_iso(),
        }),
    )?;
    let head = crate::activity::slice(&safe_message, 80);
    crate::safe_println!("[reserved2] AI 反馈 ({key}) [{level}]: {}", crate::activity::redact(context, &head));
    crate::activity::append(context, &format!("{key} [reserved2] AI 反馈 [{level}]：{head}"));
    if level == "error" && cfg.pointer("/socialV2/feedback/notifyOwnerOnError").is_some_and(truthy) {
        crate::admin_alert::notify_owner(context, cfg, &format!("[{key}] AI 报错反馈：{safe_message}"), "agent-feedback");
    }
    Ok(json!({ "ok": true, "key": key, "level": level, "message": message }))
}

/// JS truthiness of a config value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_none_or(|x| x != 0.0 && !x.is_nan()),
        _ => true,
    }
}
