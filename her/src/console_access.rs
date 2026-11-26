//! Thin HTTP console adapters for access, security, console-token, reset, and manual wake routes.
//! Domain authority, token rotation, mode transitions, and immediate revocation live in `social_state`.

use super::*;
use crate::config_tx::{CommitError, CommitOutcome, PageConfigWrite};
use crate::social_state::{
    self, js_string, js_string_or, js_trim, js_truthy, owner_of, policy_list, raw_config_object,
    SocialError,
};

type StartStamp = (Option<crate::runtime::ConfigStamp>, Option<i64>);

static CONFIG_START_STAMP: std::sync::OnceLock<
    std::sync::Mutex<HashMap<std::path::PathBuf, StartStamp>>,
> = std::sync::OnceLock::new();

fn config_stamp(context: &RuntimeContext) -> StartStamp {
    (context.config_stamp().ok(), crate::settings::store_mark(context))
}

/// Remember the start stamp (JS `cfgStamp` baseline taken at start).
pub(super) fn note_config_start(context: &RuntimeContext) {
    if context.is_isolated() {
        let stamps = CONFIG_START_STAMP.get_or_init(Default::default);
        if let Ok(mut stamps) = stamps.lock() {
            stamps
                .entry(context.config_file().to_path_buf())
                .or_insert_with(|| config_stamp(context));
        }
    }
}

/// `GET /api/whitelist` view.
pub(super) fn whitelist_get(context: &RuntimeContext, cfg: &Value) -> Value {
    let list = |root: &str, kind: &str| json!(policy_list(cfg, root, kind).unwrap_or_default());
    let mut view = json!({
        "allow": { "private": list("allow", "private"), "groups": list("allow", "group") },
        "deny": { "private": list("deny", "private"), "groups": list("deny", "group") },
        "ownerQQ": owner_of(cfg),
    });
    let start = CONFIG_START_STAMP.get().and_then(|stamps| {
        stamps
            .lock()
            .ok()
            .and_then(|s| s.get(context.config_file()).cloned())
    });
    if let Some(start) = start {
        if start != config_stamp(context) {
            if let Ok(raw) = context.load_config_document() {
                for field in ["allow", "deny", "ownerQQ"] {
                    if let Some(value) = raw.get(field) {
                        view[field] = value.clone();
                    }
                }
            }
        }
    }
    view
}

/// `POST /api/whitelist` HTTP adapter.
#[cfg(test)]
pub(super) fn whitelist_post(
    context: &RuntimeContext,
    body: &str,
) -> Result<Value, (u16, String)> {
    whitelist_post_tx(context, body)
        .map(|outcome| outcome.value)
        .map_err(CommitError::into_http)
}

/// `POST /api/whitelist` transactional HTTP adapter.
pub(super) fn whitelist_post_tx(
    context: &RuntimeContext,
    body: &str,
) -> Result<CommitOutcome<Value>, CommitError> {
    let body = parse_object_body(body)?;
    crate::config_tx::commit_whitelist(context, &body)
}

/// `POST /api/conversations/scope` transactional HTTP adapter.
pub(super) fn conversation_scope_tx(
    context: &RuntimeContext,
    body: &str,
) -> Result<CommitOutcome<Value>, CommitError> {
    let body = parse_object_body(body)?;
    crate::config_tx::commit_conversation_scope(context, &body)
}

// ── GET/POST /api/security ───────────────────────────────────────────────────────────────────────

fn state_dir(context: &RuntimeContext) -> Result<String, String> {
    crate::subprocess::external_path(context.data_dir())
}

fn state_dir_hardened(dir: &str) -> bool {
    crate::guarded_fs::is_directory_hardened(
        std::path::Path::new(dir),
        crate::guarded_fs::AclPolicy::OwnerSystemAdmins,
    )
}

fn harden_state_dir(dir: &str) -> (bool, String) {
    match crate::guarded_fs::harden_directory_tree(
        std::path::Path::new(dir),
        crate::guarded_fs::AclPolicy::OwnerSystemAdmins,
    ) {
        Ok(()) => (true, "native".to_string()),
        Err(error) => (false, error),
    }
}

fn state_dir_command(dir: &str) -> String {
    crate::guarded_fs::manual_harden_command(std::path::Path::new(dir))
}

pub(super) fn harden_selected_state_dir(context: &RuntimeContext) {
    if !context.is_isolated() {
        return;
    }
    let dir = match state_dir(context) {
        Ok(dir) => dir,
        Err(error) => {
            crate::safe_eprintln!("[console] {error}");
            return;
        }
    };
    let (ok, detail) = harden_state_dir(&dir);
    if !ok {
        crate::safe_println!("[console] ⚠️ 收紧 state/ 权限失败：{detail}");
    }
}

fn security_view(cfg: &Value) -> Value {
    let mut view = serde_json::Map::new();
    view.insert("interceptNotify".into(), json!(true));
    if let Some(stored) = cfg.get("security").and_then(Value::as_object) {
        view.extend(stored.clone());
    }
    Value::Object(view)
}

pub(super) fn security_get(context: &RuntimeContext, cfg: &Value) -> Value {
    let dir = match state_dir(context) {
        Ok(dir) => dir,
        Err(error) => {
            return json!({
                "security": security_view(cfg),
                "stateDir": { "hardened": false, "error": error },
            })
        }
    };
    json!({
        "security": security_view(cfg),
        "stateDir": {
            "hardened": state_dir_hardened(&dir),
            "path": dir,
            "manualCommand": state_dir_command(&dir),
        },
    })
}

pub(super) fn security_post_tx(
    context: &RuntimeContext,
    body: &str,
) -> Result<CommitOutcome<Value>, CommitError> {
    let body = parse_object_body(body)?;
    if body.get("rehardenStateDir") == Some(&json!(true)) {
        let _gate = social_state::acquire_gate(context)?;
        let dir = state_dir(context).map_err(|error| CommitError::plain(503, error))?;
        let (ok, detail) = harden_state_dir(&dir);
        if !ok {
            crate::safe_println!("[console] ⚠️ 收紧 state/ 权限失败：{detail}");
        }
        return Ok(CommitOutcome {
            value: json!({ "ok": ok, "hardened": ok, "path": dir }),
            config_diff: None,
        });
    }
    let confirm = crate::settings::confirm_field(&body)?;
    crate::config_tx::commit_page_config(context, |ctx| {
        let unchanged = raw_config_object(ctx).map_err(SocialError::into_http)?;
        let mut file = unchanged.clone();
        crate::settings::mirror_into(ctx, &mut file).map_err(|e| (503, e))?;
        let previous_view = security_view(&file);
        let mut next = file
            .get("security")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut fields = body.as_object().cloned().unwrap_or_default();
        fields.remove("confirm");
        next.extend(fields);
        if let Some(value) = next.get("interceptNotify").cloned() {
            if !value.is_boolean() {
                next.insert("interceptNotify".into(), json!(js_truthy(&value)));
            }
        }
        file["security"] = Value::Object(next.clone());
        let mut view = previous_view.as_object().cloned().unwrap_or_default();
        view.extend(next);
        Ok(PageConfigWrite {
            carried: file.clone(),
            file,
            base_for_changes: unchanged,
            confirm,
            check_canonical_before_store: true,
            keep_stored_always: true,
            prior_wake: None,
            keep_error_fmt: |key| {
                (
                    500,
                    format!(
                        "config.json cannot hold {key}: a member on its path is not an object; \
                         stored settings stand"
                    ),
                )
            },
            isolated_write_error_503: true,
            wake_error_prefix: "",
            finish: Box::new(move |_, _| {
                crate::safe_println!(
                    "[console] 控制台：安全拦截通知已更新（interceptNotify={}）",
                    view.get("interceptNotify")
                        .map(js_string)
                        .unwrap_or_default()
                );
                Ok(json!({ "ok": true, "security": Value::Object(view) }))
            }),
        })
    })
}

// ── POST /api/authorize/read ─────────────────────────────────────────────────────────────────────

pub(super) fn authorize_read(
    context: &RuntimeContext,
    cfg: &Value,
    body: &str,
) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    let key = js_string_or(body.get("key"), "");
    let token = js_string_or(body.get("token"), "");
    social_state::authorize_read(context, cfg, &key, &token)
        .map(|key| json!({ "ok": true, "key": key }))
        .map_err(SocialError::into_http)
}

// ── POST /api/console/token ──────────────────────────────────────────────────────────────────────

pub(super) fn requested_console_token(body: &str) -> Result<String, (u16, String)> {
    let body = parse_object_body(body)?;
    Ok(js_trim(&js_string_or(body.get("token"), "")).to_string())
}

// ── POST /api/socialV2/reset ─────────────────────────────────────────────────────────────────────

pub(super) fn social_v2_reset(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    social_state::reset_all_v2(context).map_err(SocialError::into_http)
}

// ── POST /api/socialV2/wake ──────────────────────────────────────────────────────────────────────

pub(super) fn social_v2_wake(
    context: &RuntimeContext,
    cfg: &Value,
    body: &str,
) -> Result<Value, (u16, String)> {
    social_v2_wake_with(context, cfg, body, &|_| true)
}

pub(super) fn social_v2_wake_with(
    context: &RuntimeContext,
    cfg: &Value,
    body: &str,
    still_authorized: &dyn Fn(&Value) -> bool,
) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    let key = js_string_or(body.get("key"), "");
    let reason = js_string_or(body.get("reason"), "admin");
    social_state::queue_admin_wake(context, cfg, &key, &reason, still_authorized)
        .map_err(SocialError::into_http)
}
