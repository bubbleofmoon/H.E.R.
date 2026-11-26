//! Canonical social-state ownership (`social-v2.json`), conversation authority predicates,
//! runtime mode transitions, token generation/revocation, and immediate access-change revocation.
//!
//! HTTP routes in `console.rs` and `console_access.rs` act as adapters over this module,
//! translating `SocialError` into HTTP status codes and messages at the transport boundary.

use crate::console_auth::credential_eq;
use crate::conversation_state::{
    acquire as gate_acquire, acquire_config_gate as config_gate_acquire,
    ensure_or_rotate_agent_token, read_agent_token, read_console_document,
    revoke_agent_token_if_current, token_epoch, write_console_document, Conversation,
    ConversationState, Gate, FILE as SOCIAL_V2_FILE,
};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const KEY_FORMAT_ERROR: &str = "key 格式应为 group:群号 或 private:QQ号";
pub(crate) const END_TURN: &str =
    "end this turn now: produce your final answer and make no further tool calls";
pub(crate) const PENDING_REVOCATIONS: &str = "access-revocations-pending.json";

pub(crate) const LEGACY_CONFIG_PATH: &str = crate::legacy_path!("qq-bridge/config.json");
const LEGACY_MODE_PATH: &str = crate::legacy_path!("qq-bridge/state/mode.json");
const LEGACY_ROLE_STATE_FILE: &str = crate::legacy_path!("qq-bridge/state/current-role.json");
const LEGACY_SESSIONS_FILE: &str = crate::legacy_path!("qq-bridge/state/sessions.json");
const LIVE_RESTART_REASON: &str = "the live process serves the --live-groups list it was started with; the supervisor's watchdog restarts it with rollout.rustGroups on its next run (every two minutes), or run the watchdog task now (Schedule page, POST /api/tasks/watchdog/run)";
const ADMIN_WAKE_QUEUE: usize = 20;

/// Domain error returned by social-state operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocialError {
    BadInput(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    Storage(String),
    Unavailable(String),
}

impl SocialError {
    pub(crate) fn http_status(&self) -> u16 {
        match self {
            Self::BadInput(_) => 400,
            Self::Unauthorized(_) => 401,
            Self::Forbidden(_) => 403,
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::Storage(_) => 500,
            Self::Unavailable(_) => 503,
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::BadInput(msg)
            | Self::Unauthorized(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::Conflict(msg)
            | Self::Storage(msg)
            | Self::Unavailable(msg) => msg,
        }
    }

    pub(crate) fn into_http(self) -> (u16, String) {
        let status = self.http_status();
        let message = match self {
            Self::BadInput(msg)
            | Self::Unauthorized(msg)
            | Self::Forbidden(msg)
            | Self::NotFound(msg)
            | Self::Conflict(msg)
            | Self::Unavailable(msg)
            | Self::Storage(msg) => msg,
        };
        (status, message)
    }

    pub(crate) fn from_http(error: (u16, String)) -> Self {
        match error.0 {
            400 => Self::BadInput(error.1),
            401 => Self::Unauthorized(error.1),
            403 => Self::Forbidden(error.1),
            404 => Self::NotFound(error.1),
            409 => Self::Conflict(error.1),
            500 => Self::Storage(error.1),
            _ => Self::Unavailable(error.1),
        }
    }
}

impl fmt::Display for SocialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for SocialError {}

impl From<SocialError> for String {
    fn from(error: SocialError) -> Self {
        error.to_string()
    }
}

// ── DSH session-stop seam & background stop tracker ──────────────────────────────────────────────

/// Coordinates DSH session retirement and mode write-through during access/mode changes.
pub(crate) trait SessionStopper: Send + Sync {
    fn stop_and_archive(
        &self,
        context: &RuntimeContext,
        cfg: &Value,
        key: &str,
        session_id: &str,
    ) -> Result<(), String>;

    fn sync_mode(&self, context: &RuntimeContext, cfg: &Value, mode: &str) -> bool;
}

pub(crate) struct DshSessionStopper;

impl SessionStopper for DshSessionStopper {
    fn stop_and_archive(
        &self,
        context: &RuntimeContext,
        cfg: &Value,
        key: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let rpc = crate::dsh::rpc_for(context, cfg)?;
        let session = session_id.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stopped = crate::dsh::stop_session_work(&rpc, &session);
            let archived = crate::dsh::archive_session(&rpc, &session);
            let _ = tx.send(stopped.map(|_| ()).and(archived));
        });
        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                crate::safe_println!(
                    "[console] ⚠️ 停止/归档旧会话失败 {key} -> {session_id}：{error}；请在 DSH 检查旧任务"
                );
                Err(error)
            }
            Err(_) => {
                crate::safe_println!(
                    "[console] ⚠️ 停止/归档旧会话超时 {key} -> {session_id}；请在 DSH 检查旧任务"
                );
                Err("停止旧会话超时".to_string())
            }
        }
    }

    fn sync_mode(&self, context: &RuntimeContext, cfg: &Value, mode: &str) -> bool {
        crate::dsh::rpc_for(context, cfg)
            .and_then(|rpc| {
                rpc.call(
                    "settings.update",
                    json!({ "ns": "qq-mode", "patch": { "mode": mode } }),
                )
            })
            .is_ok()
    }
}

pub(crate) fn default_stopper() -> &'static Arc<dyn SessionStopper> {
    static STOPPER: OnceLock<Arc<dyn SessionStopper>> = OnceLock::new();
    STOPPER.get_or_init(|| Arc::new(DshSessionStopper))
}

/// Tracks in-flight background DSH stop threads so short-lived CLI callers can wait cleanly.
pub(crate) struct StopTracker {
    count: Mutex<usize>,
    cond: Condvar,
    running_stops: Mutex<Vec<String>>,
}

impl StopTracker {
    pub(crate) fn new() -> Self {
        Self {
            count: Mutex::new(0),
            cond: Condvar::new(),
            running_stops: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn global() -> &'static Arc<Self> {
        static GLOBAL: OnceLock<Arc<StopTracker>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(Self::new()))
    }

    fn enter_background(&self) {
        let mut count = self.count.lock().unwrap_or_else(|p| p.into_inner());
        *count += 1;
    }

    fn finish_background(&self) {
        let mut count = self.count.lock().unwrap_or_else(|p| p.into_inner());
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.cond.notify_all();
        }
    }

    fn try_claim_session(&self, session: &str) -> bool {
        let mut running = self.running_stops.lock().unwrap_or_else(|p| p.into_inner());
        if running.iter().any(|existing| existing == session) {
            return false;
        }
        running.push(session.to_string());
        true
    }

    fn release_session(&self, session: &str) {
        self.running_stops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|existing| existing != session);
    }

    /// Wait at most `limit` for tracked background stops to finish.
    pub(crate) fn wait(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut count = self.count.lock().unwrap_or_else(|p| p.into_inner());
        while *count > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, timeout) = self
                .cond
                .wait_timeout(count, deadline.saturating_duration_since(now))
                .unwrap_or_else(|p| p.into_inner());
            count = guard;
            if timeout.timed_out() && *count > 0 {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn start_for_test(self: &Arc<Self>) -> TestStopGuard {
        self.enter_background();
        TestStopGuard(Arc::clone(self))
    }
}

#[cfg(test)]
pub(crate) struct TestStopGuard(Arc<StopTracker>);

#[cfg(test)]
impl Drop for TestStopGuard {
    fn drop(&mut self) {
        self.0.finish_background();
    }
}

pub(crate) fn stop_and_archive_session(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    session_id: &str,
) -> Result<(), String> {
    default_stopper().stop_and_archive(context, cfg, key, session_id)
}

#[cfg(test)]
pub(crate) static AFTER_ACCESS_WRITE: Mutex<Option<Arc<dyn Fn(&std::path::Path) + Send + Sync>>> =
    Mutex::new(None);

#[cfg(test)]
pub(crate) static SCOPE_AFTER_READ: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>> =
    Mutex::new(None);

// ── Clock & low-level storage helpers ────────────────────────────────────────────────────────────

pub(crate) fn now_ms() -> i64 {
    #[cfg(test)]
    if let Some(now) = crate::console::test_clock::now() {
        return now;
    }
    crate::parity_clock::now_ms() as i64
}

fn rand01() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    ((nanos ^ (nanos >> 17)) % 1_000_000) as f64 / 1_000_000.0
}

fn legacy_read_json(path: &str) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub(crate) fn legacy_write_error(error: crate::guarded_file::Failure) -> String {
    if error.class().starts_with("temp/") {
        format!("写临时文件失败：{error}")
    } else {
        format!("改名失败：{error}")
    }
}

static LEGACY_JSON_WRITE_GATE: Mutex<()> = Mutex::new(());

pub(crate) fn legacy_write_json_atomic(path: &str, value: &Value) -> Result<(), String> {
    let _gate = LEGACY_JSON_WRITE_GATE
        .lock()
        .map_err(|_| "JSON write gate is poisoned".to_string())?;
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("序列化失败：{e}"))?;
    crate::guarded_file::write_atomic(std::path::Path::new(path), text.as_bytes())
        .map_err(legacy_write_error)
}

pub(crate) fn config_write_error(context: &RuntimeContext, error: String) -> SocialError {
    if context.is_isolated() {
        SocialError::Unavailable("runtime config is unavailable".to_string())
    } else {
        SocialError::Storage(error)
    }
}

pub(crate) fn acquire_gate(context: &RuntimeContext) -> Result<Gate, SocialError> {
    gate_acquire(context).map_err(SocialError::from_http)
}

pub(crate) fn acquire_config_gate(context: &RuntimeContext) -> Result<Option<Gate>, SocialError> {
    config_gate_acquire(context).map_err(SocialError::from_http)
}

// ── JS scalar / key / policy normalization & authority predicates ────────────────────────────────

pub(crate) fn is_js_trim_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

pub(crate) fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_trim_char)
}

pub(crate) fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value
            .as_f64()
            .map(|number| {
                if number == 0.0 {
                    "0".to_string()
                } else if number.abs() >= 1.0e21 {
                    format!("{number:e}")
                } else {
                    number.to_string()
                }
            })
            .unwrap_or_else(|| value.to_string()),
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    js_string(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

pub(crate) fn js_string_or(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None | Some(Value::Null) => fallback.to_string(),
        Some(value) => js_string(value),
    }
}

pub(crate) fn js_finite_number(value: &Value) -> Option<f64> {
    if let Value::Number(n) = value {
        return n.as_f64().filter(|f| f.is_finite());
    }
    let text = js_string(value);
    let text = js_trim(&text);
    if text.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = text.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).ok().map(|n| n as f64);
        }
    }
    let unsigned = text.trim_start_matches(['+', '-']);
    if unsigned.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
    {
        return None;
    }
    text.parse::<f64>().ok().filter(|f| f.is_finite())
}

pub(crate) fn js_number_json(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

pub(crate) fn js_id_list(value: Option<&Value>) -> Option<Value> {
    let items = value?.as_array()?;
    let mut seen: Vec<f64> = Vec::new();
    for item in items {
        if let Some(n) = js_finite_number(item) {
            let n = if n == 0.0 { 0.0 } else { n };
            if !seen.contains(&n) {
                seen.push(n);
            }
        }
    }
    Some(Value::Array(seen.into_iter().map(js_number_json).collect()))
}

pub(crate) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

pub(crate) fn normalized_id(value: &Value) -> Option<String> {
    let value = js_string(value);
    let value = value.trim_matches(is_js_trim_char);
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.to_string())
}

/// Canonical social-v2 conversation key (`group:<id>` or `private:<id>` with positive JS safe integer).
pub(crate) fn canonical_key(key: &str) -> Option<String> {
    let (kind, id) = key.trim_matches(is_js_trim_char).split_once(':')?;
    if kind != "group" && kind != "private" {
        return None;
    }
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = id.parse().ok()?;
    if n <= 0 || n > 9_007_199_254_740_991 {
        return None;
    }
    Some(format!("{kind}:{n}"))
}

pub(crate) fn key_parts(key: &str) -> Option<(&str, &str)> {
    let (kind, id) = key.split_once(':')?;
    let ok = matches!(kind, "group" | "private")
        && !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_digit());
    ok.then_some((kind, id))
}

pub(crate) fn positive_id(id: &str) -> bool {
    id.parse::<f64>()
        .is_ok_and(|n| n.is_finite() && n > 0.0)
}

/// `bridge.js` `/^(group|private):(\d+)$/` then `Number(id)`: `(kind, id text, id)`.
pub(crate) fn parse_key(key: &str) -> Option<(&str, &str, i64)> {
    let (kind, id_s) = key.split_once(':')?;
    if (kind != "group" && kind != "private")
        || id_s.is_empty()
        || !id_s.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let id = id_s
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n > 0.0)? as i64;
    Some((kind, id_s, id))
}

pub(crate) fn normalize_owner(value: Option<&Value>) -> Result<Value, SocialError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(Value::Null);
    };
    let text = js_string(value);
    let text = js_trim(&text);
    if text.is_empty() {
        return Ok(Value::Null);
    }
    let invalid = || SocialError::BadInput("ownerQQ 必须是 QQ 号（正整数）".to_string());
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let number: f64 = text.parse().map_err(|_| invalid())?;
    if number <= 0.0 || number > 9_007_199_254_740_991.0 {
        return Err(invalid());
    }
    Ok(json!(number as i64))
}

pub(crate) fn owner_of(cfg: &Value) -> Value {
    normalize_owner(cfg.get("ownerQQ")).unwrap_or(Value::Null)
}

/// The configured owner's private conversation (`private:<ownerQQ>`), if `ownerQQ` is a positive integer.
pub(crate) fn owner_conversation(cfg: &Value) -> Option<String> {
    cfg.get("ownerQQ")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .map(|qq| format!("private:{qq}"))
}

pub(crate) fn policy_list(cfg: &Value, root: &str, kind: &str) -> Option<Vec<String>> {
    let policy = cfg.get(root)?;
    let (preferred, fallback) = match kind {
        "group" => ("groups", "group"),
        "private" => ("private", "privates"),
        _ => return None,
    };
    let values = policy
        .get(preferred)
        .filter(|value| !value.is_null())
        .or_else(|| policy.get(fallback))?
        .as_array()?;
    Some(values.iter().filter_map(normalized_id).collect())
}

pub(crate) fn session_allowed(cfg: &Value, key: &str) -> bool {
    let Some(canonical) = canonical_key(key) else {
        return false;
    };
    let Some((kind, id)) = canonical.split_once(':') else {
        return false;
    };
    let deny = policy_list(cfg, "deny", kind).unwrap_or_default();
    if deny.iter().any(|value| value == id) {
        return false;
    }
    let allow = policy_list(cfg, "allow", kind).unwrap_or_default();
    if allow.is_empty() {
        cfg.get("allowAllWhenEmpty").and_then(Value::as_bool) == Some(true)
    } else {
        allow.iter().any(|value| value == id)
    }
}

/// Frozen social-v2 projection middleware check before storage canonical safe-integer check.
pub(crate) fn projection_session_allowed(cfg: &Value, key: &str) -> bool {
    let Some((kind, raw_id)) = key.trim_matches(is_js_trim_char).split_once(':') else {
        return false;
    };
    if !matches!(kind, "group" | "private")
        || raw_id.is_empty()
        || !raw_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let Ok(number) = raw_id.parse::<f64>() else {
        return false;
    };
    let id = if number.is_infinite() {
        if number.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        }
    } else if let Some(number) = serde_json::Number::from_f64(number) {
        js_string(&Value::Number(number))
    } else {
        "NaN".to_string()
    };
    let deny = policy_list(cfg, "deny", kind).unwrap_or_default();
    if deny.iter().any(|value| value == &id) {
        return false;
    }
    let allow = policy_list(cfg, "allow", kind).unwrap_or_default();
    if allow.is_empty() {
        cfg.get("allowAllWhenEmpty").and_then(Value::as_bool) == Some(true)
    } else {
        allow.iter().any(|value| value == &id)
    }
}

pub(crate) fn key_in_allowlist(cfg: &Value, key: &str) -> bool {
    let Some(canonical) = canonical_key(key) else {
        return false;
    };
    let (kind, id) = canonical.split_once(':').expect("canonical key carries a colon");
    let field = if kind == "group" { "groups" } else { "private" };
    let Some(list) = cfg
        .get("allow")
        .and_then(|a| a.get(field))
        .and_then(|v| v.as_array())
    else {
        return false;
    };
    let want: i64 = id.parse().unwrap_or(0);
    list.iter().any(|x| match x {
        Value::String(s) => s.trim() == id,
        Value::Number(n) => n.as_i64() == Some(want),
        _ => false,
    })
}

/// Whether `key` may act in `mode` under `cfg` (`closed-agent`: owner only; others: allow/deny policy).
pub(crate) fn access_allowed(cfg: &Value, mode: &str, key: &str) -> bool {
    if mode == "closed-agent" {
        let owner = cfg.get("ownerQQ").map(js_string).unwrap_or_default();
        return key == format!("private:{owner}");
    }
    session_allowed(cfg, key)
}

/// #186 inbound gate: listed in allow and permitted by session policy (`allowAllWhenEmpty` admits nothing here).
pub(crate) fn inbound_admitted(cfg: &Value, key: &str) -> bool {
    key_in_allowlist(cfg, key) && session_allowed(cfg, key)
}

/// All IDs of `kind` (`"group"` or `"private"`) admitted by `inbound_admitted`, in allow-list order.
pub(crate) fn admitted_ids(cfg: &Value, kind: &str) -> Vec<String> {
    policy_list(cfg, "allow", kind)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| inbound_admitted(cfg, &format!("{kind}:{id}")))
        .collect()
}

pub(crate) fn approved_group_allowed(cfg: &Value, key: &str, message: &str) -> bool {
    let limit = crate::settings::send::MAX_CHARS.integer(cfg).min(500) as usize;
    key.starts_with("group:")
        && canonical_key(key).as_deref() == Some(key)
        && inbound_admitted(cfg, key)
        && crate::settings::tool_enabled(cfg, "sendGroup")
        && !message.is_empty()
        && message.encode_utf16().count() <= limit
        && crate::pipeline::configured_outbound_violation(cfg, message, &[]).is_none()
}

/// Additional menu gates imposed by the reserved2 runtime/console contract.
pub(crate) fn reserved2_menu_tool(cfg: &Value, key: &str, name: &str) -> bool {
    match name {
        "qq_send_private_message" => key.starts_with("private:"),
        "qq_send_group_message" | "qq_reply" | "qq_send_burst" => key.starts_with("group:"),
        "qq_send_voice" | "qq_list_voices" => {
            crate::settings::VOICE_ENABLED.switch(cfg)
                && crate::settings::tool_enabled(cfg, "sendVoice")
        }
        "qq_set_sticker_remark" => crate::settings::tool_enabled(cfg, "setStickerRemark"),
        _ => true,
    }
}

pub(crate) fn social_v1_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(value) => value.as_f64().unwrap_or(f64::NAN),
        Value::String(_) | Value::Array(_) => {
            let text = js_string(value);
            let text = text.trim_matches(is_js_trim_char);
            if text.is_empty() {
                return 0.0;
            }
            let (radix, digits) = if let Some(digits) =
                text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
            {
                (16_u32, Some(digits))
            } else if let Some(digits) =
                text.strip_prefix("0b").or_else(|| text.strip_prefix("0B"))
            {
                (2_u32, Some(digits))
            } else if let Some(digits) =
                text.strip_prefix("0o").or_else(|| text.strip_prefix("0O"))
            {
                (8_u32, Some(digits))
            } else {
                (10_u32, None)
            };
            if let Some(digits) = digits {
                if digits.is_empty() {
                    return f64::NAN;
                }
                let mut number = 0.0;
                for ch in digits.chars() {
                    let Some(digit) = ch.to_digit(radix) else {
                        return f64::NAN;
                    };
                    number = number * f64::from(radix) + f64::from(digit);
                }
                number
            } else {
                text.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Object(_) => f64::NAN,
    }
}

pub(crate) fn social_v1_spread(value: Option<&Value>) -> serde_json::Map<String, Value> {
    match value {
        Some(Value::Object(values)) => values.clone(),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value.clone()))
            .collect(),
        Some(Value::String(value)) => value
            .chars()
            .enumerate()
            .map(|(index, value)| (index.to_string(), json!(value.to_string())))
            .collect(),
        _ => serde_json::Map::new(),
    }
}

// ── Mode & role-state authority ──────────────────────────────────────────────────────────────────

pub(crate) fn normalize_mode(raw: &str) -> Option<&'static str> {
    crate::config::normalize_mode(raw)
}

/// Active mode (`chat`, `reserved`, `reserved2`, `closed-agent`), defaulting to `"reserved2"` when unreadable.
pub(crate) fn mode(context: &RuntimeContext) -> String {
    mode_checked(context).unwrap_or_else(|_| "reserved2".to_string())
}

/// Active mode, failing explicitly when the selected runtime's `mode.json` is unreadable or invalid.
pub(crate) fn mode_checked(context: &RuntimeContext) -> Result<String, SocialError> {
    if !context.is_isolated() {
        return Ok(legacy_read_json(LEGACY_MODE_PATH)
            .and_then(|v| v.get("mode").and_then(Value::as_str).map(str::to_string))
            .map(|s| normalize_mode(&s).map(str::to_string).unwrap_or(s))
            .unwrap_or_else(|| "reserved2".to_string()));
    }
    let text = context
        .read_optional_text(RuntimeArea::Data, "mode.json")
        .map_err(|_| SocialError::Storage("runtime mode is unavailable".to_string()))?;
    let Some(text) = text else {
        return Ok("reserved2".to_string());
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| SocialError::Storage("runtime mode is invalid".to_string()))?;
    let Some(object) = value.as_object() else {
        return Err(SocialError::Storage("runtime mode is invalid".to_string()));
    };
    match object.get("mode") {
        Some(Value::String(raw)) => Ok(normalize_mode(raw)
            .map(str::to_string)
            .unwrap_or_else(|| raw.clone())),
        None => Ok("reserved2".to_string()),
        Some(_) => Err(SocialError::Storage("runtime mode is invalid".to_string())),
    }
}

pub(crate) fn set_mode(
    context: &RuntimeContext,
    raw_mode: &str,
    preset: Option<&Value>,
    sync_dsh: bool,
) -> Result<Value, SocialError> {
    set_mode_with(
        context,
        raw_mode,
        preset,
        sync_dsh,
        default_stopper().as_ref(),
    )
}

pub(crate) fn set_mode_with(
    context: &RuntimeContext,
    raw_mode: &str,
    preset: Option<&Value>,
    sync_dsh: bool,
    stopper: &dyn SessionStopper,
) -> Result<Value, SocialError> {
    let Some(normalized) = normalize_mode(raw_mode.trim()) else {
        return Err(SocialError::BadInput(
            "mode 必须是 chat / reserved / reserved2 / closed-agent（simulation 为 reserved2 的规范名）"
                .to_string(),
        ));
    };
    if !context.is_isolated() {
        let existing = legacy_read_json(LEGACY_MODE_PATH).unwrap_or_else(|| json!({}));
        let preset_str = match preset {
            Some(v) => match v {
                Value::Null => String::new(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            },
            None => existing
                .get("closedAgentPreset")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        };
        let next = json!({ "mode": normalized, "closedAgentPreset": preset_str });
        legacy_write_json_atomic(LEGACY_MODE_PATH, &next).map_err(SocialError::Storage)?;
        let dsh_synced = if sync_dsh {
            write_legacy_mode_to_dsh(normalized).is_ok()
        } else {
            false
        };
        if sync_dsh && !dsh_synced {
            crate::safe_println!("[console] 模式写穿 DSH 失败；本次仅写本地，下次 DSH 轮询会覆盖回滚");
        }
        crate::safe_println!("[console] 模式已设置为 {normalized}");
        return Ok(json!({
            "ok": true,
            "mode": normalized,
            "closedAgentPreset": preset_str,
            "dshSynced": dsh_synced,
        }));
    }
    let _interaction_gate = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    set_mode_isolated_locked(context, raw_mode, preset, sync_dsh, stopper)
}

pub(crate) fn set_mode_isolated_locked(
    context: &RuntimeContext,
    raw_mode: &str,
    preset: Option<&Value>,
    sync_dsh: bool,
    stopper: &dyn SessionStopper,
) -> Result<Value, SocialError> {
    let Some(normalized) = normalize_mode(raw_mode.trim()) else {
        return Err(SocialError::BadInput(
            "mode 必须是 chat / reserved / reserved2 / closed-agent（simulation 为 reserved2 的规范名）"
                .to_string(),
        ));
    };
    let existing = match context
        .read_optional_text(RuntimeArea::Data, "mode.json")
        .map_err(|_| SocialError::Unavailable("runtime mode is unavailable".to_string()))?
    {
        Some(text) => serde_json::from_str::<Value>(&text)
            .map_err(|_| SocialError::Unavailable("runtime mode is invalid".to_string()))?,
        None => json!({}),
    };
    let preset_str = match preset {
        Some(v) => match v {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => String::new(),
        },
        None => existing
            .get("closedAgentPreset")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    };
    let changed = existing.get("mode").and_then(Value::as_str) != Some(normalized)
        || (normalized == "closed-agent"
            && existing
                .get("closedAgentPreset")
                .and_then(Value::as_str)
                .unwrap_or("")
                != preset_str);
    let revision = if changed {
        crate::turn_ledger::mint_id("mode-")
    } else {
        existing["revision"].as_str().unwrap_or("").to_string()
    };
    if changed {
        ConversationState::for_runtime(context)
            .update_document(|doc| {
                doc.rotate_all_tokens();
                Ok(())
            })
            .map_err(SocialError::from_http)?;
        let journal = crate::inbound::Journal::open(context).map_err(SocialError::Unavailable)?;
        let keys = journal
            .replayable()
            .map_err(SocialError::Unavailable)?
            .into_iter()
            .map(|e| e.event_key)
            .collect::<Vec<_>>();
        journal
            .mark_handled(&keys, now_ms())
            .map_err(SocialError::Unavailable)?;
        if existing["mode"] == "reserved" && normalized != "reserved" {
            crate::reserved::retire_mode_state(context).map_err(SocialError::Unavailable)?;
        }
    }
    let mut retired = Vec::new();
    if changed {
        for (key, session) in crate::session_map::all(context).map_err(SocialError::Unavailable)? {
            if crate::session_map::retire_if(context, &key, &session)
                .map_err(SocialError::Unavailable)?
            {
                retired.push((key, session));
            }
        }
    }
    let next = json!({
        "mode": normalized,
        "closedAgentPreset": preset_str,
        "revision": revision,
    });
    context
        .write_atomic(
            RuntimeArea::Data,
            "mode.json",
            serde_json::to_string_pretty(&next)
                .unwrap_or_default()
                .as_bytes(),
        )
        .map_err(|_| SocialError::Unavailable("runtime mode is unavailable".to_string()))?;
    let cfg = context
        .load_config()
        .map_err(|e| SocialError::Unavailable(e.to_string()))?;
    let dsh_synced = sync_dsh && stopper.sync_mode(context, &cfg, normalized);
    for (key, session) in retired {
        let _ = stopper.stop_and_archive(context, &cfg, &key, &session);
    }
    crate::safe_println!("[console] 选定运行环境的模式已设置为 {normalized}");
    Ok(json!({
        "ok": true,
        "mode": normalized,
        "closedAgentPreset": preset_str,
        "dshSynced": dsh_synced,
    }))
}

fn write_legacy_mode_to_dsh(mode_name: &str) -> Result<(), String> {
    let cfg = legacy_read_json(LEGACY_CONFIG_PATH).unwrap_or_else(|| json!({}));
    let base = cfg
        .get("dsh")
        .and_then(|d| d.get("baseUrl"))
        .and_then(Value::as_str)
        .unwrap_or("http://127.0.0.1:3080");
    let rpc = crate::dsh::Rpc::connect(base)?;
    let v = match rpc.call(
        "settings.update",
        json!({ "ns": "qq-mode", "patch": { "mode": mode_name } }),
    ) {
        Ok(v) => v,
        Err(e) if e.contains("401") => {
            std::thread::sleep(Duration::from_millis(120));
            rpc.call(
                "settings.update",
                json!({ "ns": "qq-mode", "patch": { "mode": mode_name } }),
            )?
        }
        Err(e) => return Err(e),
    };
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err("settings.update 返回 ok=false".to_string());
    }
    Ok(())
}

/// Apply a DSH mode read only if no console mode write raced the settings request.
pub(crate) fn apply_dsh_mode(
    context: &RuntimeContext,
    new_mode: &str,
    observed: Option<String>,
) -> Result<bool, SocialError> {
    apply_dsh_mode_with(context, new_mode, observed, default_stopper().as_ref())
}

pub(crate) fn apply_dsh_mode_with(
    context: &RuntimeContext,
    new_mode: &str,
    observed: Option<String>,
    stopper: &dyn SessionStopper,
) -> Result<bool, SocialError> {
    let _interaction_gate = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    let current = context
        .read_optional_text(RuntimeArea::Data, "mode.json")
        .map_err(|e| SocialError::Unavailable(e.to_string()))?;
    if current != observed {
        return Ok(false);
    }
    if mode(context) == new_mode {
        return Ok(true);
    }
    set_mode_isolated_locked(context, new_mode, None, false, stopper)?;
    Ok(true)
}

/// Publish the DSH owner override and retire sessions made under the previous policy.
pub(crate) fn apply_closed_agent_owner(
    context: &RuntimeContext,
    owner: &Value,
) -> Result<(), SocialError> {
    apply_closed_agent_owner_with(context, owner, default_stopper().as_ref())
}

pub(crate) fn apply_closed_agent_owner_with(
    context: &RuntimeContext,
    owner: &Value,
    stopper: &dyn SessionStopper,
) -> Result<(), SocialError> {
    let _interaction_gate = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    let cfg = context
        .load_config()
        .map_err(|e| SocialError::Unavailable(e.to_string()))?;
    let changed = mode(context) == "closed-agent" && cfg.get("ownerQQ") != Some(owner);
    let policy = json!({ "ownerQQ": owner });
    let existing = context
        .read_optional_text(RuntimeArea::Data, "closed-agent-owner.json")
        .map_err(|e| SocialError::Unavailable(e.to_string()))?
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    if existing.as_ref() == Some(&policy) {
        return Ok(());
    }
    context
        .write_atomic(
            RuntimeArea::Data,
            "closed-agent-owner.json",
            policy.to_string().as_bytes(),
        )
        .map_err(SocialError::Storage)?;
    if !changed {
        return Ok(());
    }
    ConversationState::for_runtime(context)
        .update_document(|doc| {
            doc.rotate_all_tokens();
            Ok(())
        })
        .map_err(SocialError::from_http)?;
    for (key, session) in crate::session_map::all(context).map_err(SocialError::Unavailable)? {
        if crate::session_map::retire_if(context, &key, &session)
            .map_err(SocialError::Unavailable)?
        {
            let _ = stopper.stop_and_archive(context, &cfg, &key, &session);
        }
    }
    Ok(())
}

fn default_role_state() -> Value {
    json!({ "role": null, "mode": "active" })
}

pub(crate) fn role_state(context: &RuntimeContext) -> Result<Value, SocialError> {
    if !context.is_isolated() {
        return Ok(legacy_read_json(LEGACY_ROLE_STATE_FILE).unwrap_or_else(default_role_state));
    }
    let Some(text) = context
        .read_optional_text(RuntimeArea::Data, "current-role.json")
        .map_err(|_| SocialError::Unavailable("runtime role state is unavailable".to_string()))?
    else {
        return Ok(default_role_state());
    };
    let state: Value = serde_json::from_str(&text)
        .map_err(|_| SocialError::Unavailable("runtime role state is invalid".to_string()))?;
    if !state.is_object() {
        return Err(SocialError::Unavailable(
            "runtime role state is invalid".to_string(),
        ));
    }
    Ok(state)
}

pub(crate) fn write_role_state(
    context: &RuntimeContext,
    role: Option<&str>,
    role_mode: &str,
) -> Result<(), SocialError> {
    let value = json!({ "role": role, "mode": role_mode });
    if !context.is_isolated() {
        return legacy_write_json_atomic(LEGACY_ROLE_STATE_FILE, &value)
            .map_err(SocialError::Storage);
    }
    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| SocialError::Storage(format!("序列化失败：{e}")))?;
    context
        .write_atomic(RuntimeArea::Data, "current-role.json", text.as_bytes())
        .map_err(SocialError::Storage)
}

/// Silent mode blocks every non-owner send (`shouldBlockSilentReply`).
pub(crate) fn silent_blocks_send(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
) -> Result<bool, SocialError> {
    let state = role_state(context)?;
    let owner_key = format!(
        "private:{}",
        cfg.get("ownerQQ").and_then(Value::as_i64).unwrap_or(0)
    );
    Ok(state.get("mode").and_then(Value::as_str) == Some("silent") && key != owner_key)
}

// ── Token & generation authority ─────────────────────────────────────────────────────────────────

pub(crate) fn token(context: &RuntimeContext, key: &str) -> Result<Option<String>, SocialError> {
    read_agent_token(context, key).map_err(SocialError::Storage)
}

pub(crate) fn generation(token_value: &str) -> String {
    token_epoch(token_value)
}

pub(crate) fn ensure_or_rotate(
    context: &RuntimeContext,
    key: &str,
    rotate: bool,
) -> Result<String, SocialError> {
    ensure_or_rotate_agent_token(context, key, rotate).map_err(SocialError::Storage)
}

pub(crate) fn revoke_if_current(
    context: &RuntimeContext,
    key: &str,
    current_token: &str,
) -> Result<Option<String>, SocialError> {
    revoke_agent_token_if_current(context, key, current_token).map_err(SocialError::Storage)
}

pub(crate) fn revoke(
    context: &RuntimeContext,
    key: &str,
    expected_token: &str,
) -> Result<Option<String>, SocialError> {
    revoke_if_current(context, key, expected_token)
        .map(|token| token.as_deref().map(generation))
}

pub(crate) fn holds_token(
    context: &RuntimeContext,
    key: &str,
    candidate: &str,
) -> Result<bool, SocialError> {
    let live = token(context, key)?;
    Ok(!candidate.is_empty() && matches!(live, Some(current) if credential_eq(&current, candidate)))
}

pub(crate) fn owner_holds_token(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    candidate: &str,
) -> Result<bool, SocialError> {
    let holds = holds_token(context, key, candidate)?;
    Ok(holds && owner_conversation(cfg).as_deref() == Some(key))
}

pub(crate) fn token_matches_doc(
    doc: &crate::conversation_state::SocialDocument,
    key: &str,
    candidate: &str,
) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let canonical = canonical_key(key);
    let lookup_key = canonical.as_deref().unwrap_or(key);
    doc.conversation(lookup_key)
        .is_some_and(|conv| conv.matches_token(candidate))
}

pub(crate) fn token_ok(
    context: &RuntimeContext,
    key: &str,
    candidate: &str,
) -> Result<bool, SocialError> {
    if candidate.is_empty() {
        return Ok(false);
    }
    let doc = ConversationState::for_runtime(context)
        .read_document()
        .map_err(SocialError::from_http)?;
    Ok(token_matches_doc(&doc, key, candidate))
}

pub(crate) fn authorize_read(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    agent_token: &str,
) -> Result<String, SocialError> {
    let key = js_trim(key).to_string();
    let agent_token = js_trim(agent_token).to_string();
    let Some((_, id)) = key_parts(&key) else {
        return Err(SocialError::BadInput(KEY_FORMAT_ERROR.to_string()));
    };
    if agent_token.is_empty() || !token_ok(context, &key, &agent_token)? {
        return Err(SocialError::Forbidden(if agent_token.is_empty() {
            "缺少会话令牌；旧只读工具在需要令牌的模式下不可用，请改用带会话令牌的 v2 读工具"
                .to_string()
        } else {
            "agent token 无效".to_string()
        }));
    }
    let current_mode = mode_checked(context)?;
    if !positive_id(id) || !access_allowed(cfg, &current_mode, &key) {
        return Err(SocialError::Forbidden(
            "当前模式不允许读取该会话".to_string(),
        ));
    }
    Ok(key)
}

// ── Selected config writes & engine-choice import ────────────────────────────────────────────────

pub(crate) fn raw_config_for_write(context: &RuntimeContext) -> Result<Value, SocialError> {
    let text = context
        .read_text(RuntimeArea::Config, "config.json")
        .map_err(|_| SocialError::Unavailable("runtime config is unavailable".to_string()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| SocialError::Unavailable("runtime config is invalid".to_string()))?;
    if !value.is_object() || crate::config::canonical_config(&value).is_err() {
        return Err(SocialError::Unavailable(
            "runtime config is invalid".to_string(),
        ));
    }
    Ok(value)
}

pub(crate) fn raw_config_object(context: &RuntimeContext) -> Result<Value, SocialError> {
    let text = context
        .read_text(RuntimeArea::Config, "config.json")
        .map_err(|_| SocialError::Unavailable("runtime config is unavailable".to_string()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| SocialError::Unavailable("runtime config is invalid".to_string()))?;
    if !value.is_object() {
        return Err(SocialError::Unavailable(
            "runtime config is invalid".to_string(),
        ));
    }
    Ok(value)
}

// ── Wait policy & turn observation helpers ───────────────────────────────────────────────────────

pub(crate) struct WaitPolicy {
    pub window_ms: i64,
    pub max_ms: i64,
    pub max_per_turn: u64,
}

fn wait_section(cfg: &Value) -> Value {
    cfg.get("socialV2")
        .and_then(|s| s.get("wait"))
        .cloned()
        .unwrap_or_else(|| json!({}))
}

pub(crate) fn wait_policy(cfg: &Value) -> WaitPolicy {
    let section = wait_section(cfg);
    let duration = |name: &str, default: i64| {
        section
            .get(name)
            .and_then(Value::as_i64)
            .filter(|n| (1..=120_000).contains(n))
            .unwrap_or(default)
    };
    WaitPolicy {
        window_ms: duration("batchWindowMs", 8_000),
        max_ms: crate::settings::wake_timing::WAIT_BATCH_MAX.integer(cfg),
        max_per_turn: section
            .get("maxPerTurn")
            .and_then(Value::as_u64)
            .filter(|n| (1..=9_007_199_254_740_991).contains(n))
            .unwrap_or(4),
    }
}

pub(crate) fn turn_send_cap(cfg: &Value) -> u64 {
    crate::settings::send::TURN_MAX.integer(cfg) as u64
}

pub(crate) fn reserved2_deadline(
    deadline: i64,
    cap: i64,
    ceiling: i64,
    start: i64,
    conv: &Value,
) -> i64 {
    conv["engagedAt"]
        .as_i64()
        .filter(|at| *at > 0 && *at >= start)
        .map_or(deadline, |at| {
            deadline.max(at.saturating_add(cap)).min(ceiling.max(deadline))
        })
}

fn wait_number(wait_cfg: &Value, key: &str, default: i64) -> i64 {
    wait_cfg
        .get(key)
        .and_then(Value::as_i64)
        .filter(|n| *n != 0)
        .unwrap_or(default)
}

fn wait_timeout_bounds(cfg: &Value) -> (i64, i64) {
    use crate::settings::wake_timing::{WAIT_MAX, WAIT_MIN};
    let min_ms = WAIT_MIN.integer(cfg).max(100);
    (min_ms, WAIT_MAX.integer(cfg).max(min_ms))
}

struct WaitBounds {
    min_quiet_after_new: i64,
    max_quiet_ms: i64,
}

fn wait_bounds(cfg: &Value) -> WaitBounds {
    let (_, max_ms) = wait_timeout_bounds(cfg);
    let min_quiet_after_new = crate::settings::wake_timing::WAIT_QUIET.integer(cfg);
    WaitBounds {
        min_quiet_after_new,
        max_quiet_ms: min_quiet_after_new.max((120_000i64).min(max_ms)),
    }
}

pub(crate) fn wait_timeout_range(cfg: &Value) -> (i64, i64) {
    wait_timeout_bounds(cfg)
}

pub(crate) fn wait_quiet_bounds(cfg: &Value) -> (i64, i64) {
    let bounds = wait_bounds(cfg);
    (bounds.min_quiet_after_new, bounds.max_quiet_ms)
}

pub(crate) fn wait_client_timeout_ms(cfg: &Value, args: &Value) -> u64 {
    let section = wait_section(cfg);
    let (min_ms, max_ms) = wait_timeout_bounds(cfg);
    let raw_timeout = match args.get("timeoutMs") {
        Some(v) => js_finite_number(v).unwrap_or(f64::NAN),
        None => f64::NAN,
    };
    let default_ms = wait_number(&section, "defaultMs", 30_000) as f64;
    let base = if raw_timeout.is_finite() {
        raw_timeout
    } else {
        default_ms
    };
    let timeout = if base.is_finite() {
        (base + 0.5).floor() as i64
    } else {
        0
    }
    .max(min_ms)
    .min(max_ms);
    let observation = crate::wake::pre_sleep_wait_ms(cfg);
    timeout
        .max(observation)
        .saturating_add(wait_policy(cfg).max_ms)
        .saturating_add(20_000)
        .clamp(1_000, 725_000) as u64
}

pub(crate) fn wait_max_quiet_ms(cfg: &Value) -> i64 {
    wait_bounds(cfg).max_quiet_ms.max(wait_policy(cfg).max_ms)
}

pub(crate) fn wait_console_generation() -> &'static str {
    static GENERATION: OnceLock<String> = OnceLock::new();
    GENERATION
        .get_or_init(|| crate::turn_ledger::mint_id("console-"))
        .as_str()
}

pub(crate) fn wait_live_path(console: &str, request: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{console}:{request}").as_bytes());
    format!("locks/observation-wait-{digest:x}.lock")
}

#[cfg(test)]
pub(crate) fn hold_test_observation_wait(
    context: &RuntimeContext,
    key: &str,
    agent_token: &str,
    turn: &str,
) -> std::fs::File {
    let console = wait_console_generation();
    let request = crate::turn_ledger::mint_id("wait-");
    let lock = context
        .open_locked_file(RuntimeArea::Data, &wait_live_path(console, &request))
        .unwrap();
    ConversationState::for_runtime(context)
        .update(key, |conv| {
            conv.set_field(
                "activeWait",
                json!({
                    "id": request,
                    "console": console,
                    "turn": turn,
                    "purpose": "messages",
                    "epoch": token_epoch(agent_token),
                }),
            );
            Ok(())
        })
        .unwrap();
    lock
}

pub(crate) fn observation_wait_turn(
    context: &RuntimeContext,
    key: &str,
) -> Result<Option<String>, SocialError> {
    let conv = ConversationState::for_runtime(context)
        .read(key)
        .map_err(SocialError::from_http)?
        .unwrap_or_default();
    let Some(pending) = conv.field("activeWait") else {
        return Ok(None);
    };
    let epoch = conv
        .token()
        .map(token_epoch)
        .unwrap_or_else(|| token_epoch(""));
    if pending["purpose"] != "messages" || pending["epoch"] != epoch {
        return Ok(None);
    }
    let (Some(console), Some(request), Some(turn)) = (
        pending["console"].as_str(),
        pending["id"].as_str(),
        pending["turn"].as_str(),
    ) else {
        return Ok(None);
    };
    let relative = wait_live_path(console, request);
    let path = context
        .resolve_path(RuntimeArea::Data, &relative, PathIntent::ReadOptionalFile)
        .map_err(|e| SocialError::Unavailable(e.to_string()))?;
    if !path.is_file() {
        return Ok(None);
    }
    match context.open_locked_file(RuntimeArea::Data, &relative) {
        Err(error) if error == "runtime file is already locked" => Ok(Some(turn.to_string())),
        Ok(_) => Ok(None),
        Err(error) => Err(SocialError::Unavailable(error)),
    }
}

pub(crate) fn turn_budget_ended(context: &RuntimeContext, key: &str, turn: &str) -> bool {
    ConversationState::for_runtime(context)
        .read(key)
        .ok()
        .flatten()
        .and_then(|c| c.field("turnBudgetEnded").cloned())
        .is_some_and(|v| v == turn)
}

pub(crate) fn turn_wait_limit_reached(
    context: &RuntimeContext,
    key: &str,
    turn: &str,
    token: &str,
) -> bool {
    let scope = format!("{}:{turn}", token_epoch(token));
    ConversationState::for_runtime(context)
        .read(key)
        .ok()
        .flatten()
        .is_some_and(|conv| {
            let budget = conv.field("waitBudget");
            let stored_max = budget.and_then(|b| b.get("max")).and_then(Value::as_u64);
            let live_max = context
                .load_config()
                .ok()
                .map(|cfg| wait_policy(&cfg).max_per_turn);
            let effective_max = match (stored_max, live_max) {
                (Some(s), Some(l)) => Some(s.min(l)),
                (a, b) => a.or(b),
            };
            conv.token() == Some(token)
                && budget.and_then(|b| b.get("scope")).and_then(Value::as_str)
                    == Some(scope.as_str())
                && effective_max.is_some_and(|max| {
                    budget
                        .and_then(|b| b.get("used"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        >= max
                })
        })
}

/// Replace a consumed finite deadline using the configured sleep range.
pub(crate) fn rearm_observation_sleep(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    token: &str,
    now: i64,
) -> Result<(), String> {
    ConversationState::for_runtime(context)
        .update(key, |conv| {
            if conv.token() == Some(token) {
                let mut wake = conv.field("wakeConfig").cloned().unwrap_or_else(|| json!({}));
                if wake["infinite"] == false && wake["sleepUntil"].is_null() {
                    crate::wake::draw_default_sleep(&mut wake, cfg, now, rand01());
                    conv.set_field("wakeConfig", wake);
                }
            }
            Ok(())
        })
        .map_err(|(_, error)| error)
}

// ── Conversation lifecycle, wake bookkeeping & resets ────────────────────────────────────────────

pub(crate) fn begin_legacy_wait_turn(
    context: &RuntimeContext,
    key: &str,
) -> Result<(), SocialError> {
    ConversationState::for_runtime(context)
        .update(key, |conv| {
            conv.set_field("waitTurnId", json!(crate::turn_ledger::mint_id("wait-")));
            Ok(())
        })
        .map_err(SocialError::from_http)
}

pub(crate) fn set_legacy_wait_deadline(
    context: &RuntimeContext,
    key: &str,
    deadline: i64,
) -> Result<(), SocialError> {
    ConversationState::for_runtime(context)
        .update(key, |conv| {
            conv.set_field("waitTurnDeadlineAt", json!(deadline));
            Ok(())
        })
        .map_err(SocialError::from_http)
}

pub(crate) fn record_reserved2_incoming(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    event_key: &str,
    mut message: Value,
) -> Result<i64, SocialError> {
    ConversationState::for_runtime(context)
        .update_or_init(key, |conv| {
            if let Some(seq) = conv
                .field("recentMessages")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find(|m| m["inboundKey"] == event_key))
                .and_then(|m| m["seq"].as_i64())
            {
                return Ok(seq);
            }
            let seq = conv.field("lastUnreadSeq").and_then(Value::as_i64).unwrap_or(0) + 1;
            message["seq"] = json!(seq);
            message["inboundKey"] = json!(event_key);
            if message["isSelf"] != true {
                let name = if key.starts_with("private:") {
                    None
                } else {
                    message["sender"].as_str()
                };
                crate::social_mem::note_member_seen(
                    conv,
                    message["userId"].as_str(),
                    name,
                    message["time"].as_i64().unwrap_or(0),
                );
            }
            conv.set_field("lastUnreadSeq", json!(seq));
            if message["quietNeutral"] != true {
                conv.set_field("lastReplyIncomingAt", message["time"].clone());
            } else if conv.field("lastReplyIncomingAt").is_none() {
                let anchor = conv
                    .field("lastIncomingAt")
                    .filter(|v| v.as_i64().is_some_and(|t| t > 0))
                    .cloned()
                    .unwrap_or_else(|| message["time"].clone());
                conv.set_field("lastReplyIncomingAt", anchor);
            }
            let urgent = message["isOwner"] == true || message["atSelf"] == true;
            if message["quietNeutral"] != true || urgent {
                conv.set_field("lastReplyUrgent", json!(urgent));
            }
            conv.set_field("lastIncomingAt", message["time"].clone());
            for field in [
                "preSleepWaitSatisfiedAt",
                "preSleepWaitObservedAt",
                "preSleepWaitAccumMs",
            ] {
                conv.set_field(field, json!(0));
            }
            let recent_limit = cfg
                .pointer("/socialV2/context/recentLimit")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .unwrap_or(100) as usize;
            let mut recent = conv
                .field("recentMessages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            recent.push(message.clone());
            if recent.len() > recent_limit {
                recent.drain(..recent.len() - recent_limit);
            }
            conv.set_field("recentMessages", Value::Array(recent));
            let unread_limit = cfg
                .pointer("/socialV2/context/unreadLimit")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .unwrap_or(30) as usize;
            let unread_msg =
                crate::conversation_state::UnreadMessage::from_value(&message).unwrap_or_default();
            conv.push_unread(unread_msg, unread_limit);
            Ok(seq)
        })
        .map_err(SocialError::from_http)
}

pub(crate) const WAKE_ACCESS_REFUSED: &str = "wake refused by current access settings";
pub(crate) const WAKE_SUSPENDED: &str = "wake suspended by current settings";

pub(crate) fn claim_reserved2_wake(
    context: &RuntimeContext,
    key: &str,
    reason: &str,
    now: i64,
    expected: Option<&str>,
) -> Result<(Value, String), SocialError> {
    ConversationState::for_runtime(context)
        .update_document(|doc| {
            let cfg = context.load_config().map_err(|e| (503, e.to_string()))?;
            if !inbound_admitted(&cfg, key) {
                return Err((403, WAKE_ACCESS_REFUSED.to_string()));
            }
            if mode(context) != "reserved2"
                || doc.paused()
                || cfg.pointer("/socialV2/enabled") == Some(&json!(false))
            {
                return Err((403, WAKE_SUSPENDED.to_string()));
            }
            let Some(mut conv) = doc.conversation(key) else {
                return Err((503, "wake has no agent token".to_string()));
            };
            let previous = conv.to_value();
            let current_token = conv
                .token()
                .ok_or((503, "wake has no agent token".to_string()))?
                .to_string();
            if expected.is_some_and(|exp| !credential_eq(exp, &current_token)) {
                return Err((409, "queued wake token invalidated".to_string()));
            }
            let mut wc = conv.field("wakeConfig").cloned().unwrap_or_else(|| json!({}));
            wc["lastWakeAt"] = json!(now);
            wc["wakeCount"] = json!(previous["wakeConfig"]["wakeCount"].as_i64().unwrap_or(0) + 1);
            conv.set_field("wakeConfig", wc);
            conv.set_field("lastWakeReason", json!(reason));
            for field in [
                "preSleepWaitSatisfiedAt",
                "preSleepWaitObservedAt",
                "preSleepWaitAccumMs",
                "engagedAt",
            ] {
                conv.set_field(field, json!(0));
            }
            let mut times = conv
                .field("wakeTimes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            times.push(json!(now));
            times.retain(|v| v.as_i64().is_some_and(|at| now - at < 3_600_000));
            if times.len() > 200 {
                times.drain(..times.len() - 200);
            }
            conv.set_field("wakeTimes", Value::Array(times));
            doc.set_conversation(key, conv);
            Ok((previous, current_token))
        })
        .map_err(SocialError::from_http)
}

pub(crate) fn consume_reserved2_deadline(
    context: &RuntimeContext,
    key: &str,
    expected_token: &str,
    at: i64,
    previous: &Value,
) -> Result<(), SocialError> {
    ConversationState::for_runtime(context)
        .update(key, |conv| {
            if !credential_eq(conv.token().unwrap_or(""), expected_token) {
                return Err((403, "queued wake token invalidated".into()));
            }
            if let Some(mut wc) = conv.field("wakeConfig").cloned() {
                if wc["lastWakeAt"].as_i64() == Some(at)
                    && wc["confirmedAt"] == previous["wakeConfig"]["confirmedAt"]
                    && wc["sleepUntil"] == previous["wakeConfig"]["sleepUntil"]
                {
                    wc["sleepUntil"] = Value::Null;
                    conv.set_field("wakeConfig", wc);
                }
            }
            Ok(())
        })
        .map_err(SocialError::from_http)
}

pub(crate) fn rollback_reserved2_wake(
    context: &RuntimeContext,
    key: &str,
    at: i64,
    previous: &Value,
) -> Result<(), SocialError> {
    ConversationState::for_runtime(context)
        .update(key, |conv| {
            if let Some(mut times) = conv.field("wakeTimes").and_then(Value::as_array).cloned() {
                if let Some(i) = times.iter().rposition(|v| v.as_i64() == Some(at)) {
                    times.remove(i);
                }
                conv.set_field("wakeTimes", Value::Array(times));
            }
            if let Some(mut wc) = conv.field("wakeConfig").cloned() {
                if wc["lastWakeAt"].as_i64() == Some(at) {
                    for field in ["lastWakeAt", "wakeCount"] {
                        wc[field] = previous["wakeConfig"][field].clone();
                    }
                    conv.set_field("lastWakeReason", previous["lastWakeReason"].clone());
                    if wc["confirmedAt"] == previous["wakeConfig"]["confirmedAt"] {
                        wc["sleepUntil"] = previous["wakeConfig"]["sleepUntil"].clone();
                    }
                    conv.set_field("wakeConfig", wc);
                }
            }
            Ok(())
        })
        .map_err(SocialError::from_http)
}

pub(crate) fn complete_reserved2_turn(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    expected_token: &str,
    started: Option<i64>,
    closing_seen: &Value,
    miss: u32,
) -> Result<(bool, Value), SocialError> {
    ConversationState::for_runtime(context)
        .update(key, |conv| {
            if !credential_eq(conv.token().unwrap_or(""), expected_token) {
                return Err((403, "completed turn token invalidated".into()));
            }
            let mut wc = conv.field("wakeConfig").cloned().unwrap_or_else(|| json!({}));
            let stamp = wc["confirmedAt"].clone();
            let closed = stamp != *closing_seen
                && matches!(
                    wc["confirmedBy"].as_str(),
                    Some("mark_read" | "set_wake_config")
                );
            let now = crate::parity_clock::now_ms() as i64;
            if let Some(started) = started {
                let acted = conv
                    .field("lastActionAt")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    >= started
                    || conv
                        .field("lastAiReplyAt")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        >= started;
                let no_action = if acted {
                    0
                } else {
                    wc["noActionCount"].as_u64().unwrap_or(0) + 1
                };
                wc["noActionCount"] = json!(no_action);
                let limit = crate::settings::wake_timing::NO_ACTION.integer(cfg) as u64;
                if no_action >= limit {
                    let keywords = wc["triggers"]["keywords"].clone();
                    let dropped = wc["triggers"]["dropNames"].clone();
                    wc = crate::wake::default_wake_config(cfg, now, rand01());
                    if keywords.as_array().is_some_and(|a| !a.is_empty())
                        && keywords != wc["triggers"]["keywords"]
                    {
                        if let Some(fields) = wc.as_object_mut() {
                            fields.remove("inheritsKeywords");
                        }
                        wc["customKeywordsFor"] = wc["triggers"]["keywords"].clone();
                        wc["triggers"]["keywords"] = keywords;
                    }
                    if dropped.as_array().is_some_and(|a| !a.is_empty()) {
                        wc["triggers"]["dropNames"] = dropped;
                    }
                    conv.set_field("bootstrapSent", json!(true));
                }
            }
            let max = crate::settings::wake_timing::REMINDERS.integer(cfg) as u64;
            if closed {
                crate::wake::ensure_wakeable(&mut wc, cfg, now, rand01());
            } else if u64::from(miss) >= max {
                let dropped = wc["triggers"]["dropNames"].clone();
                wc = crate::wake::default_wake_config(cfg, now, rand01());
                if dropped.as_array().is_some_and(|a| !a.is_empty()) {
                    wc["triggers"]["dropNames"] = dropped;
                }
            }
            conv.set_field("wakeConfig", wc);
            Ok((!closed && u64::from(miss) < max, stamp))
        })
        .map_err(SocialError::from_http)
}

pub(crate) fn mark_unbootstrapped(context: &RuntimeContext, key: &str) -> Result<(), SocialError> {
    ConversationState::for_runtime(context)
        .update_existing(key, |conversation| {
            conversation.set_field("bootstrapSent", json!(false));
            Ok(())
        })
        .map(|_| ())
        .map_err(SocialError::from_http)
}

pub(crate) fn claim_first_wake(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
) -> Result<bool, SocialError> {
    if cfg
        .get("socialV2")
        .and_then(|s| s.get("enabled"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return Ok(false);
    }
    if mode_checked(context)? != "reserved2" {
        return Ok(false);
    }
    let pending = |doc: &crate::conversation_state::SocialDocument| -> bool {
        !doc.paused()
            && doc
                .conversation(key)
                .is_some_and(|c| c.field("bootstrapSent").and_then(Value::as_bool) != Some(true))
    };
    if !pending(
        &ConversationState::for_runtime(context)
            .read_document()
            .map_err(SocialError::from_http)?,
    ) {
        return Ok(false);
    }
    ConversationState::for_runtime(context)
        .update_document(|doc| {
            if !pending(doc) {
                return Ok(false);
            }
            if let Some(mut conv) = doc.conversation(key) {
                conv.set_field("bootstrapSent", json!(true));
                doc.set_conversation(key, conv);
            }
            Ok(true)
        })
        .map_err(SocialError::from_http)
}

pub(crate) fn queue_admin_wake(
    context: &RuntimeContext,
    cfg: &Value,
    key_raw: &str,
    reason_raw: &str,
    still_authorized: &dyn Fn(&Value) -> bool,
) -> Result<Value, SocialError> {
    let key = js_trim(key_raw).to_string();
    let requested = js_trim(reason_raw).to_string();
    let reason = if requested.is_empty() {
        "admin".to_string()
    } else {
        requested
    };
    if key.is_empty() {
        return Err(SocialError::BadInput("key 不能为空".to_string()));
    }
    let current_mode = mode_checked(context)?;
    if current_mode != "reserved2" {
        return Err(SocialError::Forbidden(
            "该接口仅 reserved2 模式可用".to_string(),
        ));
    }
    let Some((_, id)) = key_parts(&key) else {
        return Err(SocialError::BadInput(KEY_FORMAT_ERROR.to_string()));
    };
    if !positive_id(id) || !access_allowed(cfg, &current_mode, &key) {
        return Err(SocialError::Forbidden(
            "目标不在当前模式允许范围内".to_string(),
        ));
    }
    let canonical = canonical_key(&key)
        .ok_or_else(|| SocialError::BadInput(format!("无效的会话 key：{key}")))?;
    let at = now_ms();
    let (first, effective) = ConversationState::for_runtime(context)
        .update_or_init(&canonical, |conversation| {
            let current_mode = mode_checked(context).map_err(SocialError::into_http)?;
            if current_mode != "reserved2" {
                return Err((403, "该接口仅 reserved2 模式可用".to_string()));
            }
            let fresh = context.load_config().map_err(|e| (503, e.to_string()))?;
            if !still_authorized(&fresh) {
                return Err((401, "console credential was revoked".to_string()));
            }
            if !access_allowed(&fresh, &current_mode, &key) {
                return Err((403, "目标不在当前模式允许范围内".to_string()));
            }
            let first = conversation
                .field("bootstrapSent")
                .and_then(Value::as_bool)
                != Some(true);
            if first {
                conversation.set_field("bootstrapSent", json!(true));
            }
            let effective = if first {
                "bootstrap".to_string()
            } else {
                reason.clone()
            };
            let epoch = conversation.generation().unwrap_or_default();
            let mut queue = conversation
                .field("adminWakes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            queue.push(json!({ "reason": effective, "epoch": epoch, "at": at }));
            if queue.len() > ADMIN_WAKE_QUEUE {
                queue.drain(..queue.len() - ADMIN_WAKE_QUEUE);
            }
            conversation.set_field("adminWakes", Value::Array(queue));
            Ok((first, effective))
        })
        .map_err(SocialError::from_http)?;
    crate::safe_println!(
        "[console] 控制台：手动唤醒 {key}（{effective}{}）",
        if first && reason != effective {
            format!("，首次唤醒，请求原因 {reason} 已升级为 bootstrap")
        } else {
            String::new()
        }
    );
    Ok(json!({
        "ok": true,
        "key": key,
        "reason": effective,
        "requestedReason": reason,
        "firstWake": first,
    }))
}

pub(crate) fn take_admin_wake(
    context: &RuntimeContext,
    key: &str,
) -> Result<Option<String>, SocialError> {
    ConversationState::for_runtime(context)
        .update_existing(key, |conversation| {
            let Some(mut queue) = conversation
                .field("adminWakes")
                .and_then(Value::as_array)
                .cloned()
            else {
                return Ok(None);
            };
            if queue.is_empty() {
                return Ok(None);
            }
            let request = queue.remove(0);
            if queue.is_empty() {
                conversation.remove_field("adminWakes");
            } else {
                conversation.set_field("adminWakes", Value::Array(queue));
            }
            let current = conversation.generation().unwrap_or_default();
            if request.get("epoch").and_then(Value::as_str).unwrap_or("") != current {
                return Ok(None);
            }
            Ok(request
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string))
        })
        .map(Option::flatten)
        .map_err(SocialError::from_http)
}

pub(crate) fn preserve_reset_sticker_liabilities(previous: &Value, replacement: &mut Value) {
    for field in [
        "stickerSendProtected",
        "stickerSendPending",
        "stickerCollectPending",
    ] {
        if let Some(value) = previous.get(field) {
            replacement[field] = value.clone();
        }
    }
    for field in ["stickerSendPending", "stickerCollectPending"] {
        if let Some(entries) = replacement.get_mut(field).and_then(Value::as_array_mut) {
            for entry in entries {
                if let Some(object) = entry.as_object_mut() {
                    object.insert("revoked".to_string(), json!(true));
                }
            }
        }
    }
}

fn reset_selected_session_record(
    context: &RuntimeContext,
    key: &str,
    old: Option<&str>,
    guard: Option<&Value>,
) -> Result<Option<String>, String> {
    ConversationState::for_runtime(context)
        .update_document(|doc| {
            let prev_conv = doc.conversation(key);
            let previous = prev_conv
                .as_ref()
                .map(Conversation::to_value)
                .unwrap_or(Value::Null);
            if let Some(expected) = guard {
                let cfg = context.load_config().map_err(|e| (503, e.to_string()))?;
                if mode(context) != "reserved2"
                    || doc.paused()
                    || !inbound_admitted(&cfg, key)
                    || cfg.pointer("/socialV2/enabled") == Some(&json!(false))
                    || [
                        "agentToken",
                        "lastIncomingAt",
                        "lastUnreadSeq",
                        "wakeConfig",
                        "adminWakes",
                    ]
                    .iter()
                    .any(|field| previous[*field] != expected[*field])
                {
                    return Ok(None);
                }
                let (turns, _) = crate::turn_ledger::fold(
                    &crate::turn_ledger::read_records(context).map_err(|e| (503, e))?,
                );
                if crate::turn_ledger::blocking_turn(&turns, key).is_some() {
                    return Ok(None);
                }
            }
            let mapped = crate::session_map::lookup(context, key).map_err(|e| (503, e))?;
            let prev_token = prev_conv.as_ref().and_then(|c| c.token()).unwrap_or("");
            let partial_rotation = guard.is_some()
                && old.is_some()
                && mapped.is_none()
                && crate::session_map::unrevoked_rotation(context, key, prev_token)
                    .map_err(|e| (503, e))?
                    .as_deref()
                    == old;
            if mapped.as_deref() != old && !partial_rotation {
                return Ok(None);
            }
            crate::turn_ledger::record_reset(context, key).map_err(|e| (503, e))?;
            crate::reserved::reset_conversation(context, key).map_err(|e| (503, e))?;
            if !partial_rotation {
                if let Some(old) = old {
                    if !crate::session_map::retire_if(context, key, old).map_err(|e| (503, e))? {
                        return Ok(None);
                    }
                }
            }
            let replacement = if let Some(mut existing) = prev_conv.filter(|_| guard.is_some()) {
                let token = existing.rotate_token();
                existing.set_field("bootstrapSent", json!(true));
                doc.set_conversation(key, existing);
                return Ok(Some(token));
            } else {
                let mut replacement_val = Conversation::new_default(context).to_value();
                preserve_reset_sticker_liabilities(&previous, &mut replacement_val);
                Conversation::from_value(&replacement_val)
                    .unwrap_or_else(|| Conversation::new_default(context))
            };
            let token = replacement.token().unwrap_or("").to_string();
            doc.set_conversation(key, replacement);
            Ok(Some(token))
        })
        .map_err(|(_, e)| e)
}

pub(crate) fn rotate_idle_session(
    context: &RuntimeContext,
    key: &str,
    sid: &str,
    guard: &Value,
) -> Result<Option<String>, SocialError> {
    let _interaction_gate = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    reset_selected_session_record(context, key, Some(sid), Some(guard))
        .map_err(SocialError::Storage)
}

pub(crate) fn reset_conversation(
    context: &RuntimeContext,
    key: &str,
) -> Result<Value, SocialError> {
    reset_conversation_with(context, key, default_stopper().as_ref())
}

pub(crate) fn reset_conversation_with(
    context: &RuntimeContext,
    key: &str,
    stopper: &dyn SessionStopper,
) -> Result<Value, SocialError> {
    if !context.is_isolated() {
        let mut store = legacy_read_json(LEGACY_SESSIONS_FILE).unwrap_or_else(|| json!({}));
        let sessions = store.get("sessions").cloned().unwrap_or_else(|| json!({}));
        if key.is_empty() || sessions.get(key).is_none() {
            return Err(SocialError::NotFound("会话不存在".to_string()));
        }
        let old_session = match sessions.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(other) => other.to_string(),
        };
        if let Some(obj) = store.get_mut("sessions").and_then(Value::as_object_mut) {
            obj.remove(key);
        }
        if let Some(obj) = store.get_mut("sessionPolicies").and_then(Value::as_object_mut) {
            obj.remove(key);
        }
        legacy_write_json_atomic(LEGACY_SESSIONS_FILE, &store).map_err(SocialError::Storage)?;
        ConversationState::for_runtime(&RuntimeContext::legacy())
            .update_document(|doc| {
                doc.remove_conversation(key);
                Ok(())
            })
            .map_err(SocialError::from_http)?;
        crate::safe_println!(
            "[console] 已清除会话上下文 {key}（旧会话 {old_session}；注意：未调用 DSH 归档/停止）"
        );
        return Ok(json!({ "ok": true, "key": key, "archived": old_session }));
    }

    let _interaction_gate = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    let sessions_text = context
        .read_optional_text(RuntimeArea::Data, "sessions.json")
        .map_err(|_| SocialError::Unavailable("runtime session data is unavailable".to_string()))?;
    let store = match sessions_text {
        Some(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                SocialError::Unavailable("runtime session data is invalid".to_string())
            })?;
            if !value.is_object() {
                return Err(SocialError::Unavailable(
                    "runtime session data is invalid".to_string(),
                ));
            }
            value
        }
        None => json!({}),
    };
    let cfg = context
        .load_config()
        .map_err(|_| SocialError::Unavailable("runtime configuration is unavailable".to_string()))?;
    let current_mode = mode(context);
    let harness = !crate::turn_engine::Engine::read_choice(context, &cfg, key, &current_mode)
        .map_err(SocialError::Unavailable)?
        .uses_dsh();
    let mapped = match store.get("sessions") {
        None => false,
        Some(Value::Object(sessions)) => sessions.contains_key(key),
        Some(_) => {
            return Err(SocialError::Unavailable(
                "runtime session data is invalid".to_string(),
            ))
        }
    };
    if key.is_empty() || (!mapped && !harness) {
        return Err(SocialError::NotFound("会话不存在".to_string()));
    }
    if store
        .get("sessionPolicies")
        .is_some_and(|policies| !policies.is_object())
    {
        return Err(SocialError::Unavailable(
            "runtime session data is invalid".to_string(),
        ));
    }
    let social_text = context
        .read_optional_text(RuntimeArea::Data, SOCIAL_V2_FILE)
        .map_err(|_| SocialError::Unavailable("runtime data is unavailable".to_string()))?
        .ok_or_else(|| SocialError::Unavailable("runtime data is unavailable".to_string()))?;
    let social: Value = serde_json::from_str(&social_text)
        .map_err(|_| SocialError::Unavailable("runtime data is invalid".to_string()))?;
    if !social.is_object()
        || social
            .get("conversations")
            .is_some_and(|conversations| !conversations.is_object())
    {
        return Err(SocialError::Unavailable(
            "runtime data is invalid".to_string(),
        ));
    }
    context
        .resolve_path(RuntimeArea::Data, "sessions.json", PathIntent::WriteFile)
        .map_err(|_| SocialError::Unavailable("runtime session data is unavailable".to_string()))?;
    context
        .resolve_path(RuntimeArea::Data, SOCIAL_V2_FILE, PathIntent::WriteFile)
        .map_err(|_| SocialError::Unavailable("runtime data is unavailable".to_string()))?;

    let old = crate::session_map::lookup(context, key).map_err(SocialError::Unavailable)?;
    if old.is_none() && !harness {
        return Err(SocialError::NotFound("会话不存在".to_string()));
    }
    reset_selected_session_record(context, key, old.as_deref(), None)
        .map_err(SocialError::Storage)?
        .ok_or_else(|| {
            SocialError::Conflict("会话已变化，重置未应用，请重试".to_string())
        })?;
    let old_session = old.unwrap_or_default();

    let mut deferred: Option<SocialError> = None;
    if let Ok(records) = crate::turn_ledger::read_records(context) {
        let (turns, _) = crate::turn_ledger::fold(&records);
        for turn in turns.iter().filter(|t| {
            t.key == key
                && (t.state.is_open() || t.state == crate::turn_ledger::State::Paused)
        }) {
            if turn.backend == "harness"
                && !crate::harness::resolve_reset(context, &turn.id).map_err(|error| {
                    SocialError::Storage(format!("harness reset partially failed: {error}"))
                })?
            {
                continue;
            }
            if let Err(error) = crate::turn_ledger::Recorder::attach(context, &turn.id, key)
                .end("cancelled", "session-reset")
            {
                crate::safe_println!("[console] ⚠️ 重置时未能结算回合 {}：{error}", turn.id);
                deferred.get_or_insert(SocialError::Storage(format!(
                    "turn {} was not settled: {error}",
                    turn.id
                )));
            }
        }
    }
    if mapped {
        let _ = stopper.stop_and_archive(context, &cfg, key, &old_session);
    }
    if let Some(error) = deferred {
        return Err(error);
    }
    crate::safe_println!("[console] 已清除选定运行环境中的会话上下文 {key}（旧会话 {old_session}）");
    Ok(json!({ "ok": true, "key": key, "archived": old_session }))
}

pub(crate) fn reset_all_v2(context: &RuntimeContext) -> Result<Value, SocialError> {
    reset_all_v2_with(context, StopTracker::global(), default_stopper())
}

pub(crate) fn reset_all_v2_with(
    context: &RuntimeContext,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<Value, SocialError> {
    let _interaction_gate = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    let cfg = context
        .load_config()
        .map_err(|e| SocialError::Unavailable(e.to_string()))?;
    crate::turn_ledger::record_reset_why(context, "*", "socialV2-reset")
        .map_err(SocialError::Unavailable)?;
    let doc = ConversationState::for_runtime(context)
        .read_document()
        .map_err(SocialError::from_http)?;
    let keys = doc.conversation_keys();
    let mut failures = Vec::new();
    let settled = crate::inbound::Journal::open(context).and_then(|journal| {
        let stale: Vec<String> = journal
            .replayable()?
            .into_iter()
            .filter(|e| keys.iter().any(|k| e.event_key.starts_with(&format!("{k}|"))))
            .map(|e| e.event_key)
            .collect();
        journal.mark_handled(&stale, now_ms())
    });
    if let Err(error) = settled {
        failures.push(format!("入站事件未能作废（重启后可能重放）：{error}"));
    }
    let mut retired = Vec::new();
    for key in &keys {
        if let Some(session) =
            crate::session_map::lookup(context, key).map_err(SocialError::Unavailable)?
        {
            if crate::session_map::retire_if(context, key, &session)
                .map_err(SocialError::Unavailable)?
            {
                retired.push((key.clone(), session));
            }
        }
    }
    ConversationState::for_runtime(context)
        .update_document(|doc| {
            let mut kept = serde_json::Map::new();
            for (key, conversation) in doc.conversations() {
                let conv_val = conversation.to_value();
                if [
                    "stickerSendProtected",
                    "stickerSendPending",
                    "stickerCollectPending",
                ]
                .iter()
                .any(|f| {
                    conv_val
                        .get(*f)
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty())
                }) {
                    let mut replacement = Conversation::new_default(context).to_value();
                    preserve_reset_sticker_liabilities(&conv_val, &mut replacement);
                    kept.insert(key.to_string(), replacement);
                }
            }
            doc.replace_all_with_reset(kept, crate::turn_ledger::mint_id("reset-"));
            Ok(())
        })
        .map_err(SocialError::from_http)?;
    for key in &keys {
        if let Err(error) = cancel_ledger_turns(context, key, "socialV2-reset") {
            failures.push(format!("{key}：{error}"));
        }
    }
    for (key, session) in retired {
        stop_retired_session_work_with(context, &cfg, &key, &session, tracker, stopper);
    }
    if !failures.is_empty() {
        return Err(SocialError::Storage(format!(
            "二代状态已重置，但作废排队与在途工作未完成：{}",
            failures.join("；")
        )));
    }
    crate::safe_println!("[console] 控制台：二代 AI 状态已重置（会话、定时器、唤醒配置已清空，工具日志保留）");
    Ok(json!({ "ok": true }))
}

pub(crate) fn reconcile_default_wake_configs(
    context: &RuntimeContext,
    before: Option<&Value>,
) -> Result<(), SocialError> {
    let cfg = context
        .load_config_requiring_settings()
        .map_err(SocialError::Unavailable)?;
    let mut state = read_console_document(context).map_err(SocialError::from_http)?;
    let Some(conversations) = state.get_mut("conversations").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let defaults = crate::wake::default_wake_plan(&cfg);
    let target = crate::session::wake_config_from_json(Some(&defaults));
    let batch = crate::settings::wake_timing::BATCH.integer(&cfg);
    let range = crate::wake::default_sleep_range(&cfg);
    let previous_range = before.map(crate::wake::default_sleep_range);
    let mut changed = false;
    for conversation in conversations.values_mut() {
        let Some(plan) = conversation
            .get_mut("wakeConfig")
            .filter(|w| w["confirmedBy"] == "default")
        else {
            continue;
        };
        let current = crate::session::wake_config_from_json(Some(plan));
        let range_changed = plan
            .get("sleepRangeMs")
            .or(previous_range.as_ref())
            .is_some_and(|old| old != &range);
        if target.infinite && !current.infinite {
            if let Some(fields) = plan.as_object_mut() {
                fields.remove("sleepUntil");
                fields.remove("sleepRangeMs");
            }
        } else if !target.infinite && (current.infinite || range_changed) {
            crate::wake::draw_default_sleep(plan, &cfg, now_ms(), rand01());
            changed = true;
        }
        for (field, stale) in [
            ("mode", current.mode_active != target.mode_active),
            ("infinite", current.infinite != target.infinite),
            (
                "batchWindowMs",
                current.batch_window_ms.unwrap_or(batch).max(1000) != batch,
            ),
        ] {
            if stale {
                plan[field] = defaults[field].clone();
                changed = true;
            }
        }
        for (field, stale) in [
            ("atMention", current.at_mention != target.at_mention),
            ("nameMention", current.name_mention != target.name_mention),
            ("question", current.question_boolean != target.question_boolean),
            ("poke", current.poke != target.poke),
            ("anyMessage", current.any_message != target.any_message),
            ("probability", current.probability != target.probability),
            (
                "speakerIds",
                !current.speaker_ids.is_empty()
                    || !crate::wake::normalize_speaker_ids(plan["triggers"].get("speakerIds"))
                        .is_empty(),
            ),
        ] {
            if stale {
                if !plan["triggers"].is_object() {
                    plan["triggers"] = json!({});
                }
                plan["triggers"][field] = defaults["triggers"][field].clone();
                changed = true;
            }
        }
        changed |= reconcile_default_keywords(
            plan,
            &defaults["triggers"]["keywords"],
            before,
            current.keywords != target.keywords,
        );
    }
    if changed {
        write_console_document(context, &state)
            .map_err(|error| SocialError::Storage(format!("social-state/write: {error}")))?;
    }
    Ok(())
}

fn reconcile_default_keywords(
    plan: &mut Value,
    recommended: &Value,
    before: Option<&Value>,
    stale: bool,
) -> bool {
    let words = &plan["triggers"]["keywords"];
    let copied_for = plan.get("customKeywordsFor");
    let empty = recommended.as_array().is_none_or(Vec::is_empty);
    let inherited = plan["inheritsKeywords"] == true
        || before.and_then(|cfg| cfg.pointer("/socialV2/wake/recommendedKeywords")) == Some(words);
    if copied_for == Some(recommended) {
        return false;
    }
    if empty && copied_for.is_some() {
        plan["customKeywordsFor"] = recommended.clone();
        return true;
    }
    if (empty && !inherited) || (!stale && copied_for.is_none()) {
        return false;
    }
    if stale {
        if !plan["triggers"].is_object() {
            plan["triggers"] = json!({});
        }
        plan["triggers"]["keywords"] = recommended.clone();
    }
    if let Some(fields) = plan.as_object_mut() {
        fields.remove("customKeywordsFor");
        if empty {
            fields.remove("inheritsKeywords");
        } else {
            plan["inheritsKeywords"] = json!(true);
        }
    }
    true
}

pub(crate) fn undo_wake_words_locked(
    context: &RuntimeContext,
    body: &Value,
    authority: crate::persona::Authority<'_>,
) -> Result<Value, SocialError> {
    let key = body["conversation"]
        .as_str()
        .and_then(crate::initial_prompts::canonical_key)
        .ok_or_else(|| {
            SocialError::BadInput("conversation must be group:<id> or private:<id>".to_string())
        })?;
    let id = body["id"]
        .as_u64()
        .ok_or_else(|| SocialError::BadInput("id is required".to_string()))?;
    let _authority = authority().map_err(SocialError::from_http)?;
    let mut state = read_console_document(context).map_err(SocialError::from_http)?;
    let result =
        crate::wake_words::undo(&mut state, &key, id, now_ms()).map_err(SocialError::from_http)?;
    write_console_document(context, &state)
        .map_err(|error| SocialError::Storage(format!("social-state/write: {error}")))?;
    Ok(result)
}

// ── Immediate access revocation & configuration changes ──────────────────────────────────────────

fn candidate_keys(
    context: &RuntimeContext,
    before: &Value,
    after: &Value,
) -> (Vec<String>, Vec<String>) {
    let mut keys: Vec<String> = Vec::new();
    let mut failures = Vec::new();
    match crate::session_map::all(context) {
        Ok(mappings) => keys.extend(mappings.into_iter().map(|(key, _)| key)),
        Err(error) => failures.push(format!("读取会话映射失败：{error}")),
    }
    match ConversationState::for_runtime(context).read_document() {
        Ok(doc) => keys.extend(doc.conversation_keys()),
        Err((_, error)) => failures.push(format!("读取会话状态失败：{error}")),
    }
    match crate::reserved::known_keys(context) {
        Ok(known) => keys.extend(known),
        Err(error) => failures.push(format!("读取第一代会话状态失败：{error}")),
    }
    match crate::turn_ledger::read_records(context) {
        Ok(records) => {
            let (turns, _) = crate::turn_ledger::fold(&records);
            keys.extend(
                turns
                    .into_iter()
                    .filter(|t| t.state.is_open() || t.state == crate::turn_ledger::State::Paused)
                    .map(|t| t.key),
            );
        }
        Err(error) => failures.push(format!("读取回合账本失败：{error}")),
    }
    for (cfg, root) in [(before, "allow"), (after, "deny")] {
        for (kind, prefix) in [("group", "group"), ("private", "private")] {
            for id in policy_list(cfg, root, kind).unwrap_or_default() {
                keys.push(format!("{prefix}:{id}"));
            }
        }
    }
    if let Some(owner) = owner_of(before).as_i64() {
        keys.push(format!("private:{owner}"));
    }
    match recorded_stops(context) {
        Ok(stops) => keys.extend(stops.into_iter().map(|(key, _)| key)),
        Err(error) => failures.push(error),
    }
    keys.sort();
    keys.dedup();
    (keys, failures)
}

fn access_policy(cfg: &Value) -> Value {
    let mut policy = serde_json::Map::new();
    for field in ["allow", "deny", "allowAllWhenEmpty", "ownerQQ"] {
        if let Some(value) = cfg.get(field) {
            policy.insert(field.into(), value.clone());
        }
    }
    Value::Object(policy)
}

fn admits_unlisted(cfg: &Value, current_mode: &str) -> bool {
    current_mode != "closed-agent"
        && cfg.get("allowAllWhenEmpty").and_then(Value::as_bool) == Some(true)
        && ["group", "private"]
            .iter()
            .any(|kind| policy_list(cfg, "allow", kind).unwrap_or_default().is_empty())
}

fn cancel_ledger_turns(context: &RuntimeContext, key: &str, why: &str) -> Result<(), String> {
    let records =
        crate::turn_ledger::read_records(context).map_err(|e| format!("读取回合账本失败：{e}"))?;
    let (turns, _) = crate::turn_ledger::fold(&records);
    let mut failures = Vec::new();
    for turn in turns.iter().filter(|t| {
        t.key == key && (t.state.is_open() || t.state == crate::turn_ledger::State::Paused)
    }) {
        if let Err(error) = crate::turn_ledger::Recorder::attach(context, &turn.id, key)
            .without_retry()
            .end("cancelled", why)
        {
            failures.push(format!("取消回合 {} 失败：{error}", turn.id));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

fn stop_retired_session_work_with(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    session: &str,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) {
    let (context, cfg, key, session) = (
        context.clone(),
        cfg.clone(),
        key.to_string(),
        session.to_string(),
    );
    let tracker_bg = Arc::clone(tracker);
    let stopper_bg = Arc::clone(stopper);
    tracker.enter_background();
    std::thread::spawn(move || {
        let _ = stopper_bg.stop_and_archive(&context, &cfg, &key, &session);
        tracker_bg.finish_background();
    });
}

fn stop_recorded_session_with(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    session: &str,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) {
    if !tracker.try_claim_session(session) {
        return;
    }
    let (context, cfg, key, session) = (
        context.clone(),
        cfg.clone(),
        key.to_string(),
        session.to_string(),
    );
    let tracker_bg = Arc::clone(tracker);
    let stopper_bg = Arc::clone(stopper);
    tracker.enter_background();
    std::thread::spawn(move || {
        if stopper_bg
            .stop_and_archive(&context, &cfg, &key, &session)
            .is_ok()
        {
            match crate::closed_agent::lock(&context) {
                Ok(_gate) => {
                    if let Err(error) = forget_stop(&context, &key, &session) {
                        crate::safe_println!(
                            "[console] ⚠️ the finished DSH stop of {key} stays recorded: {error}"
                        );
                    }
                }
                Err(error) => {
                    crate::safe_println!(
                        "[console] ⚠️ the finished DSH stop of {key} stays recorded: {error}"
                    );
                }
            }
        }
        tracker_bg.release_session(&session);
        tracker_bg.finish_background();
    });
}

fn revoke_key_with(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    why: &str,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    if let Err(error) = crate::turn_ledger::record_reset_why(context, key, why) {
        failures.push(format!("账本重置记录写入失败：{error}"));
    }
    let session = match crate::session_map::lookup(context, key) {
        Ok(session) => session,
        Err(error) => {
            failures.push(format!("读取会话映射失败：{error}"));
            None
        }
    };
    let mut stop = None;
    if let Some(session) = &session {
        if let Err(error) = record_stop(context, key, session) {
            failures.push(format!("the DSH session stop was not recorded: {error}"));
        } else {
            if let Err(error) = crate::session_map::retire_if(context, key, session) {
                failures.push(format!("退役会话映射失败：{error}"));
            }
            stop = Some(session.clone());
        }
    }
    if let Err((_, error)) =
        ConversationState::for_runtime(context).update_existing(key, |conversation| {
            conversation.rotate_token();
            conversation.set_field("bootstrapSent", json!(false));
            conversation.remove_field("engineHandover");
            Ok(())
        })
    {
        failures.push(format!("轮换会话令牌失败：{error}"));
    }
    if let Err(error) = cancel_ledger_turns(context, key, why) {
        failures.push(error);
    }
    if let Some(session) = stop {
        stop_recorded_session_with(context, cfg, key, &session, tracker, stopper);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

struct ReconcileRequest<'a> {
    before: &'a Value,
    after: &'a Value,
    mode: &'a str,
    why: &'a str,
    dsh_cfg: &'a Value,
    entry: Option<&'a str>,
    targets: &'a [String],
}

fn reconcile_policies(
    context: &RuntimeContext,
    req: &ReconcileRequest<'_>,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> (Vec<String>, Vec<String>) {
    let owner_changed = req.mode == "closed-agent" && owner_of(req.before) != owner_of(req.after);
    let (mut keys, mut failures) = candidate_keys(context, req.before, req.after);
    keys.extend(req.targets.iter().cloned());
    keys.sort();
    keys.dedup();
    if admits_unlisted(req.before, req.mode)
        && access_policy(req.before) != access_policy(req.after)
    {
        if let Err(error) = crate::turn_ledger::record_policy_reset(
            context,
            &access_policy(req.after),
            req.mode,
            req.why,
        ) {
            failures.push(format!("策略重置记录写入失败：{error}"));
        }
    }
    let mapped: Vec<String> = crate::session_map::all(context)
        .unwrap_or_default()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let stopped: Vec<String> = recorded_stops(context)
        .unwrap_or_default()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    let revoked: Vec<String> = keys
        .into_iter()
        .filter(|key| {
            req.targets.contains(key)
                || (access_allowed(req.before, req.mode, key)
                    && !access_allowed(req.after, req.mode, key))
                || (owner_changed && (mapped.contains(key) || stopped.contains(key)))
        })
        .collect();
    if let Some(id) = req.entry {
        if let Err(error) = record_targets(context, id, &revoked) {
            failures.push(format!(
                "the revocation targets were not recorded, nothing was revoked: {error}"
            ));
            return (Vec::new(), failures);
        }
    }
    for key in &revoked {
        if let Err(error) = revoke_key_with(context, req.dsh_cfg, key, req.why, tracker, stopper) {
            crate::safe_println!("[console] ⚠️ 撤销 {key} 失败：{error}");
            failures.push(format!("{key}：{error}"));
        }
    }
    (revoked, failures)
}

fn read_record(context: &RuntimeContext) -> Result<Value, String> {
    let Some(text) = context
        .read_optional_text(RuntimeArea::Data, PENDING_REVOCATIONS)
        .map_err(|e| format!("data/{PENDING_REVOCATIONS} is unreadable: {e}"))?
    else {
        return Ok(json!({ "pending": [] }));
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .filter(|record| record.get("pending").is_some_and(Value::is_array))
        .ok_or_else(|| {
            format!(
                "data/{PENDING_REVOCATIONS} is not a pending-revocation record; repair or remove it"
            )
        })
}

fn write_record_field(context: &RuntimeContext, field: &str, value: Value) -> Result<(), String> {
    let mut record = read_record(context)?;
    record[field] = value;
    let text = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
    context.write_atomic(RuntimeArea::Data, PENDING_REVOCATIONS, text.as_bytes())
}

fn write_pending_revocations(context: &RuntimeContext, pending: &[Value]) -> Result<(), String> {
    write_record_field(context, "pending", Value::Array(pending.to_vec()))
}

fn usable_entry(entry: &Value) -> bool {
    ["before", "after"].iter().all(|side| {
        entry
            .get(*side)
            .and_then(Value::as_object)
            .is_some_and(|policy| {
                ["allow", "deny"]
                    .iter()
                    .all(|list| policy.get(*list).is_none_or(Value::is_object))
            })
    }) && entry.get("targets").is_none_or(|targets| {
        targets.as_array().is_some_and(|keys| {
            keys.iter().all(|key| {
                key.as_str()
                    .is_some_and(|key| canonical_key(key).as_deref() == Some(key))
            })
        })
    })
}

fn entry_targets(entry: &Value) -> Vec<String> {
    entry["targets"]
        .as_array()
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn pending_revocations(context: &RuntimeContext) -> Result<Vec<Value>, String> {
    let pending = read_record(context)?["pending"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(index) = pending.iter().position(|entry| !usable_entry(entry)) {
        return Err(format!(
            concat!(
                "data/{} entry {} has no usable before/after policy; ",
                "repair or remove it"
            ),
            PENDING_REVOCATIONS, index
        ));
    }
    let mut ids: Vec<String> = Vec::new();
    for id in pending.iter().filter_map(|entry| entry["id"].as_str()) {
        if ids.iter().any(|seen| seen == id) {
            return Err(format!(
                "data/{PENDING_REVOCATIONS} has the pending id {id} twice; repair or remove it"
            ));
        }
        ids.push(id.to_string());
    }
    Ok(pending)
}

fn record_targets(context: &RuntimeContext, id: &str, targets: &[String]) -> Result<(), String> {
    let mut pending = pending_revocations(context)?;
    let Some(entry) = pending
        .iter_mut()
        .find(|entry| entry["id"].as_str() == Some(id))
    else {
        return Err(format!("pending entry {id} is not recorded"));
    };
    let mut recorded = entry_targets(entry);
    for key in targets {
        if !recorded.contains(key) {
            recorded.push(key.clone());
        }
    }
    entry["targets"] = json!(recorded);
    write_pending_revocations(context, &pending)
}

fn recorded_stops(context: &RuntimeContext) -> Result<Vec<(String, String)>, String> {
    let record = read_record(context)?;
    let Some(stops) = record.get("stops") else {
        return Ok(Vec::new());
    };
    let malformed = || {
        format!("data/{PENDING_REVOCATIONS} has a malformed stop record; repair or remove it")
    };
    stops
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .map(|stop| match (stop["key"].as_str(), stop["session"].as_str()) {
            (Some(key), Some(session)) => Ok((key.to_string(), session.to_string())),
            _ => Err(malformed()),
        })
        .collect()
}

fn record_stop(context: &RuntimeContext, key: &str, session: &str) -> Result<(), String> {
    recorded_stops(context)?;
    let mut stops = read_record(context)?["stops"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let stop = json!({ "key": key, "session": session });
    if !stops.contains(&stop) {
        stops.push(stop);
    }
    write_record_field(context, "stops", Value::Array(stops))
}

fn forget_stop(context: &RuntimeContext, key: &str, session: &str) -> Result<(), String> {
    recorded_stops(context)?;
    let mut stops = read_record(context)?["stops"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let stop = json!({ "key": key, "session": session });
    stops.retain(|recorded| recorded != &stop);
    write_record_field(context, "stops", Value::Array(stops))
}

pub(crate) fn begin_access_change(
    context: &RuntimeContext,
    before: &Value,
    after: &Value,
    why: &str,
) -> Result<Option<String>, SocialError> {
    if !context.is_isolated() {
        return Ok(None);
    }
    let mut pending = pending_revocations(context).map_err(SocialError::Unavailable)?;
    let id = crate::turn_ledger::mint_id("revoke-");
    pending.push(json!({
        "id": id,
        "why": why,
        "mode": mode(context),
        "before": access_policy(before),
        "after": access_policy(after),
        "at": now_ms(),
    }));
    write_pending_revocations(context, &pending).map_err(|e| {
        SocialError::Unavailable(format!(
            "the access change was not recorded, nothing was written: {e}"
        ))
    })?;
    Ok(Some(id))
}

pub(crate) fn forget_access_change(
    context: &RuntimeContext,
    id: &Option<String>,
) -> Result<(), String> {
    let Some(id) = id else { return Ok(()) };
    let mut pending = pending_revocations(context)?;
    pending.retain(|entry| entry["id"].as_str() != Some(id.as_str()));
    write_pending_revocations(context, &pending)
}

pub(crate) fn finish_access_change(
    context: &RuntimeContext,
    interaction_gate: &std::fs::File,
    before: &Value,
    after: &Value,
    why: &str,
    pending: Option<String>,
) -> (Vec<String>, Vec<String>) {
    finish_access_change_with(
        context,
        interaction_gate,
        before,
        after,
        why,
        pending,
        StopTracker::global(),
        default_stopper(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_access_change_with(
    context: &RuntimeContext,
    _interaction_gate: &std::fs::File,
    before: &Value,
    after: &Value,
    why: &str,
    pending: Option<String>,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> (Vec<String>, Vec<String>) {
    #[cfg(test)]
    {
        let hook = AFTER_ACCESS_WRITE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(context.config_file());
        }
    }
    let (revoked, mut failures) = if !context.is_isolated() {
        (Vec::new(), Vec::new())
    } else {
        let current_mode = mode(context);
        reconcile_policies(
            context,
            &ReconcileRequest {
                before,
                after,
                mode: &current_mode,
                why,
                dsh_cfg: after,
                entry: pending.as_deref(),
                targets: &[],
            },
            tracker,
            stopper,
        )
    };
    if failures.is_empty() {
        if let Err(error) = forget_access_change(context, &pending) {
            failures.push(format!(
                "the finished revocation is still recorded as pending: {error}"
            ));
        }
    }
    (revoked, failures)
}

fn finish_pending_revocations_locked(
    context: &RuntimeContext,
    _interaction_gate: &std::fs::File,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<(Vec<String>, Vec<String>), String> {
    if !context.is_isolated() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut pending = pending_revocations(context)?;
    if pending.is_empty() && recorded_stops(context)?.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if pending.iter().any(|entry| !entry["id"].is_string()) {
        for entry in pending.iter_mut().filter(|entry| !entry["id"].is_string()) {
            entry["id"] = json!(crate::turn_ledger::mint_id("revoke-"));
        }
        write_pending_revocations(context, &pending)?;
    }
    let current = context.load_config().unwrap_or_else(|_| json!({}));
    let mut revoked = Vec::new();
    let mut failures = Vec::new();
    let mut finished: Vec<String> = Vec::new();
    for entry in &pending {
        let id = entry["id"].as_str().unwrap_or_default();
        let why = entry["why"].as_str().unwrap_or("access-revoked").to_string();
        let entry_mode = entry["mode"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| mode(context));
        let targets = entry_targets(entry);
        let (keys, entry_failures) = reconcile_policies(
            context,
            &ReconcileRequest {
                before: &entry["before"],
                after: &entry["after"],
                mode: &entry_mode,
                why: &why,
                dsh_cfg: &current,
                entry: Some(id),
                targets: &targets,
            },
            tracker,
            stopper,
        );
        for key in keys {
            if !revoked.contains(&key) {
                revoked.push(key);
            }
        }
        if entry_failures.is_empty() {
            finished.push(id.to_string());
        } else {
            failures.extend(entry_failures);
        }
    }
    if !finished.is_empty() {
        let mut left = pending_revocations(context)?;
        left.retain(|entry| {
            !entry["id"]
                .as_str()
                .is_some_and(|id| finished.iter().any(|done| done == id))
        });
        write_pending_revocations(context, &left).map_err(|e| {
            format!("the finished revocations are still recorded as pending: {e}")
        })?;
    }
    for (key, session) in recorded_stops(context)? {
        stop_recorded_session_with(context, &current, &key, &session, tracker, stopper);
    }
    Ok((revoked, failures))
}

pub(crate) fn finish_pending_before_change(
    context: &RuntimeContext,
    interaction_gate: &std::fs::File,
) -> Result<(), SocialError> {
    finish_pending_before_change_with(
        context,
        interaction_gate,
        StopTracker::global(),
        default_stopper(),
    )
}

pub(crate) fn finish_pending_before_change_with(
    context: &RuntimeContext,
    interaction_gate: &std::fs::File,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<(), SocialError> {
    let (_, failures) =
        finish_pending_revocations_locked(context, interaction_gate, tracker, stopper)
            .map_err(SocialError::Unavailable)?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(SocialError::Unavailable(format!(
            "an earlier access change's revocation did not finish, nothing was written: {}",
            failures.join("; ")
        )))
    }
}

pub(crate) fn finish_pending_revocations(
    context: &RuntimeContext,
) -> Result<(Vec<String>, Vec<String>), SocialError> {
    finish_pending_revocations_with(context, StopTracker::global(), default_stopper())
}

pub(crate) fn finish_pending_revocations_with(
    context: &RuntimeContext,
    tracker: &Arc<StopTracker>,
    stopper: &Arc<dyn SessionStopper>,
) -> Result<(Vec<String>, Vec<String>), SocialError> {
    if !context.is_isolated()
        || (pending_revocations(context).is_ok_and(|pending| pending.is_empty())
            && recorded_stops(context).is_ok_and(|stops| stops.is_empty()))
    {
        return Ok((Vec::new(), Vec::new()));
    }
    let interaction = crate::closed_agent::lock(context).map_err(SocialError::Unavailable)?;
    finish_pending_revocations_locked(context, &interaction, tracker, stopper)
        .map_err(SocialError::Unavailable)
}

pub(crate) fn live_restart(context: &RuntimeContext, target: &[String]) -> Value {
    let Ok(path) = context.resolve_path(
        RuntimeArea::Data,
        &format!("logs/{}", crate::lifecycle::OwnerRole::Live.lock_file()),
        PathIntent::ReadOptionalFile,
    ) else {
        return json!({
            "state": "unknown",
            "reason": "the live owner gate is unavailable",
        });
    };
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json!({
                "state": "none",
                "reason": "no live process runs; the next start serves rollout.rustGroups",
            });
        }
        Ok(meta) if meta.is_file() && !crate::guarded_file::is_link(&meta) => {}
        _ => {
            return json!({
                "state": "unknown",
                "reason": "the live owner gate is unavailable",
            });
        }
    }
    let mut backend = crate::lifecycle::LiveLifecycle;
    match crate::lifecycle::LifecycleBackend::probe_owner(
        &mut backend,
        context.data_dir(),
        crate::lifecycle::OwnerRole::Live,
        crate::lifecycle::ProbeMode::Existing,
    ) {
        crate::lifecycle::OwnerProbe::Free { .. } => {
            json!({
                "state": "none",
                "reason": "no live process runs; the next start serves rollout.rustGroups",
            })
        }
        crate::lifecycle::OwnerProbe::Held {
            marker: Some(marker),
        } if marker.role == crate::lifecycle::OwnerRole::Live.as_str() => {
            let serving = marker.groups.clone().unwrap_or_default();
            let canonical = |list: &[String]| {
                let mut keys: Vec<String> =
                    list.iter().filter_map(|entry| canonical_key(entry)).collect();
                keys.sort();
                keys.dedup();
                keys
            };
            if canonical(serving.as_slice()) == canonical(target) {
                json!({
                    "state": "none",
                    "serving": serving,
                    "reason": "the live process already serves rollout.rustGroups",
                })
            } else {
                json!({
                    "state": "required",
                    "serving": serving,
                    "reason": LIVE_RESTART_REASON,
                })
            }
        }
        _ => json!({
            "state": "unknown",
            "reason": "the live owner cannot be identified",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_state::update_raw;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let safe: String = label
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-social-state-{}-{}-{safe}",
                std::process::id(),
                crate::parity_clock::now_ms()
            )));
            let _ = std::fs::remove_dir_all(&dir);
            for sub in ["config", "data", "resources", "logs", "user/dsh"] {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_context(root: &TestDir, mut cfg: Value) -> RuntimeContext {
        if cfg
            .get("consoleToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            cfg["consoleToken"] = json!("synthetic-social-state-test-token");
        }
        std::fs::write(
            root.0.join("config/config.json"),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.0.join("data/social-v2.json"),
            r#"{"paused":false,"conversations":{}}"#,
        )
        .unwrap();
        std::fs::write(
            root.0.join("dependencies.json"),
            json!({
                "dsh": "http://127.0.0.1:1",
                "model": "http://127.0.0.1:1",
                "onebot": "http://127.0.0.1:1"
            })
            .to_string(),
        )
        .unwrap();
        RuntimeContext::from_root(&root.0).unwrap()
    }

    struct FakeStopper {
        allow_stop: AtomicBool,
        mode_synced: AtomicBool,
        stopped: Mutex<Vec<(String, String)>>,
        synced_modes: Mutex<Vec<String>>,
    }

    impl FakeStopper {
        fn new(allow_stop: bool) -> Arc<Self> {
            Arc::new(Self {
                allow_stop: AtomicBool::new(allow_stop),
                mode_synced: AtomicBool::new(true),
                stopped: Mutex::new(Vec::new()),
                synced_modes: Mutex::new(Vec::new()),
            })
        }

        fn recorded_stops(&self) -> Vec<(String, String)> {
            self.stopped.lock().unwrap().clone()
        }

        fn recorded_modes(&self) -> Vec<String> {
            self.synced_modes.lock().unwrap().clone()
        }
    }

    impl SessionStopper for FakeStopper {
        fn stop_and_archive(
            &self,
            _context: &RuntimeContext,
            _cfg: &Value,
            key: &str,
            session_id: &str,
        ) -> Result<(), String> {
            self.stopped
                .lock()
                .unwrap()
                .push((key.to_string(), session_id.to_string()));
            if self.allow_stop.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("fake DSH stop failed".to_string())
            }
        }

        fn sync_mode(&self, _context: &RuntimeContext, _cfg: &Value, mode: &str) -> bool {
            self.synced_modes.lock().unwrap().push(mode.to_string());
            self.mode_synced.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn authority_predicates_enforce_owner_tokens_and_canonical_keys() {
        assert_eq!(
            owner_conversation(&json!({ "ownerQQ": 10001 })).as_deref(),
            Some("private:10001")
        );
        assert_eq!(owner_conversation(&json!({ "ownerQQ": "1" })), None);
        assert_eq!(owner_conversation(&json!({ "ownerQQ": 1.5 })), None);
        assert_eq!(owner_conversation(&json!({ "ownerQQ": 0 })), None);
        assert_eq!(owner_conversation(&json!({ "ownerQQ": "bad" })), None);

        assert_eq!(canonical_key(" group:0042 ").as_deref(), Some("group:42"));
        assert_eq!(canonical_key("private:0"), None);
        let beyond_js_safe = format!("group:{}", 1u64 << 53);
        assert_eq!(canonical_key(&beyond_js_safe), None);

        let cfg = json!({
            "ownerQQ": 7001,
            "allow": { "groups": [42, "100"], "private": ["7001"] },
            "deny": { "groups": [100] },
            "socialV2": { "tools": { "sendGroup": true, "setStickerRemark": false } }
        });
        assert!(inbound_admitted(&cfg, "group:42"));
        assert!(!inbound_admitted(&cfg, "group:100"));
        assert!(!inbound_admitted(&cfg, "group:43"));
        assert_eq!(admitted_ids(&cfg, "group"), vec!["42".to_string()]);
        assert!(access_allowed(&cfg, "closed-agent", "private:7001"));
        assert!(!access_allowed(&cfg, "closed-agent", "group:42"));
        assert!(approved_group_allowed(&cfg, "group:42", "hello"));
        assert!(!approved_group_allowed(&cfg, "private:7001", "hello"));
        assert!(!approved_group_allowed(&cfg, "group:042", "hello"));
        assert!(reserved2_menu_tool(&cfg, "group:42", "qq_send_group_message"));
        assert!(!reserved2_menu_tool(&cfg, "private:7001", "qq_send_group_message"));
        assert!(!reserved2_menu_tool(&cfg, "group:42", "qq_set_sticker_remark"));

        let root = TestDir::new("authority");
        let context = test_context(&root, cfg.clone());
        let first = ensure_or_rotate(&context, "private:7001", false).unwrap();
        assert!(owner_holds_token(&context, &cfg, "private:7001", &first).unwrap());
        assert!(!owner_holds_token(&context, &cfg, "group:42", &first).unwrap());
        assert!(!holds_token(&context, "private:7001", "wrong-token").unwrap());
        let rotated = ensure_or_rotate(&context, "private:7001", true).unwrap();
        assert_ne!(rotated, first);
        assert_ne!(generation(&rotated), generation(&first));
        assert!(!holds_token(&context, "private:7001", &first).unwrap());
        assert!(holds_token(&context, "private:7001", &rotated).unwrap());
        let after = revoke_if_current(&context, "private:7001", &rotated)
            .unwrap()
            .expect("matching token rotates");
        assert_ne!(after, rotated);
        assert!(!holds_token(&context, "private:7001", &rotated).unwrap());
        assert_eq!(token(&context, "private:7001").unwrap(), Some(after));

        // Corrupt social-v2.json must fail closed even when presented token is empty or non-owner.
        std::fs::write(root.0.join("data/social-v2.json"), "{corrupt").unwrap();
        assert!(matches!(
            holds_token(&context, "private:7001", ""),
            Err(SocialError::Storage(_))
        ));
        assert!(matches!(
            owner_holds_token(&context, &cfg, "group:42", ""),
            Err(SocialError::Storage(_))
        ));
    }

    #[test]
    fn mode_transitions_normalize_aliases_reject_invalid_files_and_retire_sessions_via_stopper() {
        let root = TestDir::new("mode");
        let cfg = json!({
            "ownerQQ": 7001,
            "allow": { "groups": [42], "private": [7001] }
        });
        let context = test_context(&root, cfg);
        let tok = ensure_or_rotate(&context, "group:42", false).unwrap();
        std::fs::write(
            root.0.join("data/sessions.json"),
            json!({
                "sessions": { "group:42": "dsh-mode-sess" },
                "sessionPolicies": { "group:42": "[\"reserved2\",\"\",null]" }
            })
            .to_string(),
        )
        .unwrap();

        let bad = set_mode_with(&context, "bogus", None, false, &*FakeStopper::new(true))
            .unwrap_err();
        assert_eq!(bad.http_status(), 400);

        let stopper = FakeStopper::new(true);
        let reply = set_mode_with(&context, "simulation", None, true, &*stopper).unwrap();
        assert_eq!(reply["mode"], "reserved2");
        assert_eq!(mode_checked(&context).unwrap(), "reserved2");

        let reply = set_mode_with(&context, "closed-agent", Some(&json!("p1")), true, &*stopper)
            .unwrap();
        assert_eq!(reply["mode"], "closed-agent");
        assert_eq!(reply["dshSynced"], true);
        assert_eq!(stopper.recorded_modes(), vec!["reserved2", "closed-agent"]);
        assert_eq!(
            stopper.recorded_stops(),
            vec![("group:42".to_string(), "dsh-mode-sess".to_string())]
        );
        assert_eq!(crate::session_map::lookup(&context, "group:42").unwrap(), None);
        assert_ne!(token(&context, "group:42").unwrap().as_deref(), Some(tok.as_str()));

        // A stale `apply_dsh_mode` observation does not overwrite a newer mode write.
        assert!(!apply_dsh_mode_with(&context, "chat", Some("stale".into()), &*stopper).unwrap());
        let observed = context
            .read_optional_text(RuntimeArea::Data, "mode.json")
            .unwrap();
        assert!(apply_dsh_mode_with(&context, "chat", observed, &*stopper).unwrap());
        assert_eq!(mode(&context), "chat");

        std::fs::write(root.0.join("data/mode.json"), "not json").unwrap();
        let err = mode_checked(&context).unwrap_err();
        assert_eq!(err.http_status(), 500);
        assert_eq!(err.message(), "runtime mode is invalid");
    }

    #[test]
    fn access_revocation_rotates_tokens_and_retries_failed_dsh_stops_via_stopper() {
        let root = TestDir::new("revocation");
        let cfg = json!({
            "ownerQQ": 7001,
            "allow": { "groups": [42], "private": [7001] },
            "rollout": { "enabled": true, "rustGroups": ["group:42"] }
        });
        let context = test_context(&root, cfg);
        let old_tok = ensure_or_rotate(&context, "group:42", false).unwrap();
        std::fs::write(
            root.0.join("data/sessions.json"),
            json!({
                "sessions": { "group:42": "dsh-revoke-sess" },
                "sessionPolicies": { "group:42": "[\"reserved2\",\"\",null]" }
            })
            .to_string(),
        )
        .unwrap();

        let tracker = Arc::new(StopTracker::new());
        let stopper: Arc<FakeStopper> = FakeStopper::new(false);
        let stopper_dyn: Arc<dyn SessionStopper> = stopper.clone();
        let reply = crate::config_tx::change_conversation_scope_waiting_with(
            &context,
            "group:42",
            "remove",
            Duration::from_secs(5),
            &tracker,
            &stopper_dyn,
        )
        .unwrap();
        assert_eq!(reply["revoked"], json!(["group:42"]));
        assert_eq!(crate::session_map::lookup(&context, "group:42").unwrap(), None);
        assert_ne!(
            token(&context, "group:42").unwrap().as_deref(),
            Some(old_tok.as_str())
        );
        let pending_raw = context
            .read_optional_text(RuntimeArea::Data, PENDING_REVOCATIONS)
            .unwrap()
            .unwrap();
        let pending_json: Value = serde_json::from_str(&pending_raw).unwrap();
        assert_eq!(
            pending_json["stops"],
            json!([{ "key": "group:42", "session": "dsh-revoke-sess" }])
        );

        stopper.allow_stop.store(true, Ordering::SeqCst);
        let (revoked, failures) =
            finish_pending_revocations_with(&context, &tracker, &stopper_dyn).unwrap();
        assert!(revoked.is_empty());
        assert!(failures.is_empty());
        assert!(tracker.wait(Duration::from_secs(5)));
        let after_raw = context
            .read_optional_text(RuntimeArea::Data, PENDING_REVOCATIONS)
            .unwrap()
            .unwrap();
        let after_json: Value = serde_json::from_str(&after_raw).unwrap();
        assert_eq!(after_json["stops"], json!([]));

        // Guarded test stop timeout reports unfinished without blocking indefinitely.
        let guard = tracker.start_for_test();
        let timeout_reply = crate::config_tx::change_conversation_scope_waiting_with(
            &context,
            "group:42",
            "admit",
            Duration::from_millis(50),
            &tracker,
            &stopper_dyn,
        )
        .unwrap();
        assert_eq!(
            timeout_reply["sessionStops"],
            "unfinished: check the retired sessions' tasks in DSH"
        );
        drop(guard);
        assert!(tracker.wait(Duration::from_secs(5)));
    }

    #[test]
    fn reserved2_wake_and_reset_lifecycle_preserve_sticker_liabilities_and_fence_wakes() {
        let root = TestDir::new("wake-reset");
        let cfg = json!({
            "ownerQQ": 7001,
            "allow": { "groups": [42], "private": [7001] }
        });
        let context = test_context(&root, cfg.clone());
        let seq = record_reserved2_incoming(
            &context,
            &cfg,
            "group:42",
            "evt-1",
            json!({
                "sender": "Alice",
                "userId": "2001",
                "text": "hi",
                "time": 1000,
                "isSelf": false
            }),
        )
        .unwrap();
        assert_eq!(seq, 1);
        assert!(claim_first_wake(&context, &cfg, "group:42").unwrap());
        assert!(!claim_first_wake(&context, &cfg, "group:42").unwrap());

        let old_tok = ensure_or_rotate(&context, "group:42", false).unwrap();
        let wake = queue_admin_wake(&context, &cfg, "group:42", "manual", &|_| true).unwrap();
        assert_eq!(wake["reason"], "manual");
        assert_eq!(take_admin_wake(&context, "group:42").unwrap().as_deref(), Some("manual"));
        assert_eq!(take_admin_wake(&context, "group:42").unwrap(), None);

        // Stale token epoch queued wake is dropped cleanly.
        let _ = queue_admin_wake(&context, &cfg, "group:42", "stale", &|_| true).unwrap();
        let new_tok = ensure_or_rotate(&context, "group:42", true).unwrap();
        assert_ne!(old_tok, new_tok);
        assert_eq!(take_admin_wake(&context, "group:42").unwrap(), None);

        let (prev, claimed_tok) =
            claim_reserved2_wake(&context, "group:42", "manual", 2000, Some(&new_tok)).unwrap();
        assert_eq!(claimed_tok, new_tok);
        consume_reserved2_deadline(&context, "group:42", &new_tok, 2000, &prev).unwrap();
        rollback_reserved2_wake(&context, "group:42", 2000, &prev).unwrap();

        // Preserve sticker liabilities across reset_all_v2_with and retire DSH mapping via stopper.
        update_raw(&context, |doc| {
            doc["paused"] = json!(true);
            doc["conversations"]["group:42"]["stickerSendPending"] =
                json!([{ "id": "stk-1", "time": 2000 }]);
            Ok(())
        })
        .unwrap();
        std::fs::write(
            root.0.join("data/sessions.json"),
            json!({ "sessions": { "group:42": "dsh-reset-all" } }).to_string(),
        )
        .unwrap();
        let tracker = Arc::new(StopTracker::new());
        let stopper: Arc<FakeStopper> = FakeStopper::new(true);
        let stopper_dyn: Arc<dyn SessionStopper> = stopper.clone();
        let reset_reply = reset_all_v2_with(&context, &tracker, &stopper_dyn).unwrap();
        assert_eq!(reset_reply["ok"], true);
        assert!(tracker.wait(Duration::from_secs(5)));
        assert_eq!(
            stopper.recorded_stops(),
            vec![("group:42".to_string(), "dsh-reset-all".to_string())]
        );
        let doc = read_console_document(&context).unwrap();
        assert!(!doc["paused"].as_bool().unwrap_or(false));
        assert_eq!(
            doc["conversations"]["group:42"]["stickerSendPending"][0]["revoked"],
            true
        );

        write_role_state(&context, Some("alice"), "silent").unwrap();
        assert!(silent_blocks_send(&context, &cfg, "group:42").unwrap());
        assert!(!silent_blocks_send(&context, &cfg, "private:7001").unwrap());
    }

    #[test]
    fn turn_wait_limit_reached_honors_lowered_max_per_turn() {
        let root = TestDir::new("wait-limit-lowered");
        let cfg = json!({
            "allow": { "groups": [42] },
            "socialV2": { "wait": { "maxPerTurn": 4 } }
        });
        let context = test_context(&root, cfg);
        let token = ensure_or_rotate(&context, "group:42", false).unwrap();
        let turn = "turn-42";
        let scope = format!("{}:{turn}", token_epoch(&token));
        update_raw(&context, |doc| {
            doc["conversations"]["group:42"]["waitBudget"] =
                json!({ "scope": scope, "used": 2, "max": 4 });
            Ok(())
        })
        .unwrap();
        assert!(!turn_wait_limit_reached(&context, "group:42", turn, &token));

        let mut lowered = context.load_config().unwrap();
        lowered["socialV2"]["wait"]["maxPerTurn"] = json!(2);
        context
            .write_atomic(
                RuntimeArea::Config,
                "config.json",
                lowered.to_string().as_bytes(),
            )
            .unwrap();
        assert!(turn_wait_limit_reached(&context, "group:42", turn, &token));
        assert!(!turn_wait_limit_reached(
            &context,
            "group:42",
            "other-turn",
            &token
        ));
        assert!(!turn_wait_limit_reached(
            &context,
            "group:42",
            turn,
            "wrong-token"
        ));
    }
}
