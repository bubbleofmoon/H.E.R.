//! Ticket 24 (US-40, US-41, US-42): access, security and console-token administration, the
//! second-generation whole reset and manual wake, the console's host and cross-site checks, and
//! the immediate invalidation that follows an access change.
//!
//! Invalidation rule: an administrator change that removes a conversation's authority (allow/deny
//! lists, closed-agent owner, `POST /api/socialV2/reset`, an audit undo that restores a narrower
//! configuration) acts before the HTTP response: the conversation's agent token is rotated or
//! removed, its DSH mapping is retired, its open ledger turns are cancelled and a ledger `reset`
//! record makes every queued job of that key stale. Re-granting access later (another write or an
//! undo) creates a new epoch; nothing queued under the old one runs again.

use super::*;

// ── host and cross-site checks (frozen JS console front) ─────────────────────────────────────────

fn allowed_hosts(port: u16) -> [String; 3] {
    [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]
}

/// JS: the Host header must name the loopback console with its bound port (DNS rebinding).
/// Checked before credentials; the icon no-ops and the Rust-only health probe precede it.
pub(super) fn host_denial(req: &Request, port: u16) -> Option<(u16, Value)> {
    if req.method == "GET" && matches!(req.path.as_str(), "/favicon.ico" | "/apple-touch-icon.png" | "/api/health") {
        return None;
    }
    let host = req.headers.get("host").map(String::as_str).unwrap_or("");
    if allowed_hosts(port).iter().any(|allowed| allowed == &host.to_lowercase()) {
        return None;
    }
    let shown = if host.is_empty() { "缺失" } else { host };
    Some((403, json!({"ok": false, "error": format!("Host 头不被允许（{shown}）：控制台只接受本机回环访问")})))
}

/// `new URL(origin).host` for the cases a browser sends: scheme://authority, lowercased, with the
/// scheme's default port dropped. Anything else (for example the opaque origin `null`) is empty.
fn origin_host(origin: &str) -> String {
    let Some((scheme, rest)) = origin.split_once("://") else { return String::new() };
    let scheme = scheme.to_ascii_lowercase();
    if scheme.is_empty() || !scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return String::new();
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map(|(_, host)| host).unwrap_or(authority).to_lowercase();
    let default_port = match scheme.as_str() {
        "http" | "ws" => Some("80"),
        "https" | "wss" => Some("443"),
        _ => None,
    };
    match (authority.rsplit_once(':'), default_port) {
        (Some((host, port)), Some(default)) if port == default && !host.is_empty() => host.to_string(),
        _ => authority,
    }
}

/// JS CSRF guard for every non-GET request that passed the console credential check: the body
/// must be declared `application/json` (415) and an Origin, when present, must be the loopback
/// console page (403).
pub(super) fn cross_site_denial(req: &Request, port: u16) -> Option<(u16, Value)> {
    if req.method == "GET" {
        return None;
    }
    let content_type = req.headers.get("content-type").map(|v| v.to_lowercase()).unwrap_or_default();
    if !content_type.contains("application/json") {
        return Some((415, json!({"ok": false, "error": "请求必须是 application/json"})));
    }
    if let Some(origin) = req.headers.get("origin").filter(|o| !o.is_empty()) {
        let host = origin_host(origin);
        if !allowed_hosts(port).contains(&host) {
            return Some((403, json!({"ok": false, "error": "跨站请求被拒绝"})));
        }
    }
    None
}

// ── JS value helpers ─────────────────────────────────────────────────────────────────────────────

/// `String(value ?? fallback)`.
fn js_string_or(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None | Some(Value::Null) => fallback.to_string(),
        Some(value) => social_policy_js_string(value),
    }
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(social_policy_js_trim_char)
}

/// `Number(String(value).trim())` restricted to what `Number.isFinite` accepts.
fn js_finite_number(value: &Value) -> Option<f64> {
    if let Value::Number(n) = value {
        return n.as_f64().filter(|f| f.is_finite());
    }
    let text = social_policy_js_string(value);
    let text = js_trim(&text);
    if text.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).ok().map(|n| n as f64);
        }
    }
    let unsigned = text.trim_start_matches(['+', '-']);
    if unsigned.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None; // Infinity, NaN and Rust-only spellings such as "inf"
    }
    if !text.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E')) {
        return None;
    }
    text.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// A JSON number the way `JSON.stringify` renders a JS number.
fn js_number_json(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

/// `[...new Set(arr.map((x) => Number(String(x).trim())).filter(Number.isFinite))]`.
fn js_id_list(value: Option<&Value>) -> Option<Value> {
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

/// JS `normalizeOwnerQQ`.
fn normalize_owner(value: Option<&Value>) -> Result<Value, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else { return Ok(Value::Null) };
    let text = social_policy_js_string(value);
    let text = js_trim(&text);
    if text.is_empty() {
        return Ok(Value::Null);
    }
    let invalid = || "ownerQQ 必须是 QQ 号（正整数）".to_string();
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let number: f64 = text.parse().map_err(|_| invalid())?;
    if number <= 0.0 || number > 9_007_199_254_740_991.0 {
        return Err(invalid());
    }
    Ok(json!(number as i64))
}

fn read_config_object(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    let text = context
        .read_text(RuntimeArea::Config, "config.json")
        .map_err(|_| (503, "runtime config is unavailable".to_string()))?;
    let value: Value = serde_json::from_str(&text).map_err(|_| (503, "runtime config is invalid".to_string()))?;
    if !value.is_object() {
        return Err((503, "runtime config is invalid".to_string()));
    }
    Ok(value)
}

fn write_config_object(context: &RuntimeContext, value: &Value) -> Result<(), (u16, String)> {
    write_json_atomic_for(context, CONFIG_PATH, value).map_err(|e| config_write_error(context, e))
}

// ── GET/POST /api/whitelist ──────────────────────────────────────────────────────────────────────────

/// `config.json` as the console found it at start (modification time and size), per runtime.
static CONFIG_START_STAMP: std::sync::OnceLock<std::sync::Mutex<HashMap<std::path::PathBuf, Option<(std::time::SystemTime, u64)>>>> =
    std::sync::OnceLock::new();

fn config_stamp(context: &RuntimeContext) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(context.config_file()).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Remember the start stamp (JS `cfgStamp` baseline taken at start).
pub(super) fn note_config_start(context: &RuntimeContext) {
    if context.is_isolated() {
        let stamps = CONFIG_START_STAMP.get_or_init(Default::default);
        if let Ok(mut stamps) = stamps.lock() {
            stamps.entry(context.config_file().to_path_buf()).or_insert_with(|| config_stamp(context));
        }
    }
}

/// Whether `config.json` differs from what the console found at start: from then on JS
/// `maybeReloadConfig` has shallow-assigned the raw file members over its in-memory `cfg`.
pub(super) fn config_changed_since_start(context: &RuntimeContext) -> bool {
    CONFIG_START_STAMP
        .get()
        .and_then(|stamps| stamps.lock().ok().and_then(|s| s.get(context.config_file()).cloned()))
        .is_some_and(|start| start != config_stamp(context))
}

/// JS `GET /api/whitelist` returns its in-memory `cfg`: the lists as `loadConfig` normalized them
/// at start (`normalizeIdList`, alias precedence, normalized owner or `null`) until `config.json`
/// changes; after that `maybeReloadConfig` has shallow-assigned the raw file values.
pub(super) fn whitelist_get(context: &RuntimeContext, cfg: &Value) -> Value {
    let list = |root: &str, kind: &str| json!(social_policy_list(cfg, root, kind).unwrap_or_default());
    let mut view = json!({
        "allow": {"private": list("allow", "private"), "groups": list("allow", "group")},
        "deny": {"private": list("deny", "private"), "groups": list("deny", "group")},
        "ownerQQ": owner_of(cfg),
    });
    let start = CONFIG_START_STAMP
        .get()
        .and_then(|stamps| stamps.lock().ok().and_then(|s| s.get(context.config_file()).cloned()));
    if let Some(start) = start {
        if start != config_stamp(context) {
            if let Ok(raw) = read_config_object(context) {
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

/// JS `POST /api/whitelist` in the selected runtime: allow/deny lists and the owner, then the
/// immediate revocation of every conversation that lost access (JS `reconcileSessionPolicies`).
pub(super) fn whitelist_post(context: &RuntimeContext, body: &str) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    // Access changes are serialized with closed-agent receipts, mode changes and resets: the gate
    // is taken before anything is written, so its failure leaves the configuration unchanged.
    let interaction = crate::closed_agent::lock(context).map_err(|e| (503, e))?;
    // The revocation compares the policy the runtime authorizes with (canonical lists and the
    // closed-agent owner overlay), read under the same gate as the write.
    let (before, after, file) = {
        let _gate = acquire_social_state_gate(context)?;
        let effective_before = context.load_config().map_err(|e| (503, e.to_string()))?;
        let mut file = read_config_object(context)?;
        let before = file.clone();
        let list = |root: &str, kind: &str| -> Value {
            js_id_list(body.get(root).and_then(|v| v.get(kind))).unwrap_or_else(|| {
                match before.get(root).and_then(|v| v.get(kind)) {
                    None | Some(Value::Null) => json!([]),
                    Some(value) => value.clone(),
                }
            })
        };
        // Accepted deviation 20 (defect fix): a request that names no list of a root leaves the
        // configured root exactly as it is, alias spellings (`allow.group`, `privates`) included;
        // JS rewrites it as `{private, groups}` and so drops the aliased entries (`bridge.js`
        // 4170-4172). A request that does name a list rewrites that root as JS does.
        let root_value = |root: &str| -> Value {
            let named = ["private", "groups"].iter().any(|kind| js_id_list(body.get(root).and_then(|v| v.get(*kind))).is_some());
            match before.get(root) {
                Some(configured) if !named && !configured.is_null() => configured.clone(),
                _ => json!({"private": list(root, "private"), "groups": list(root, "groups")}),
            }
        };
        let allow = root_value("allow");
        let deny = root_value("deny");
        // JS keeps its in-memory owner (`cfg.ownerQQ ?? null`), which carries the DSH overlay.
        let owner = match body.get("ownerQQ") {
            Some(value) => normalize_owner(Some(value)).map_err(|e| (400, e))?,
            None => owner_of(&effective_before),
        };
        let object = file.as_object_mut().expect("config checked above");
        object.insert("allow".into(), allow);
        object.insert("deny".into(), deny);
        object.insert("ownerQQ".into(), owner);
        write_config_object(context, &file)?;
        // A written configuration the runtime cannot load revokes everything the old one allowed.
        let effective_after = context.load_config().unwrap_or_else(|_| json!({}));
        (effective_before, effective_after, file)
    };
    let (retired, failures) = reconcile_access_change(context, &interaction, &before, &after, "access-revoked");
    drop(interaction);
    if !failures.is_empty() {
        return Err((500, format!("白名单已保存，但立即撤销未完成：{}", failures.join("；"))));
    }
    let ids = |kind: &str| social_policy_list(&file, "allow", kind).unwrap_or_default().join(",");
    let groups = ids("group");
    let private = ids("private");
    crate::safe_println!(
        "[console] 控制台：白名单已更新（群: {}，私聊: {}，管理员: {}）{}",
        if groups.is_empty() { "无" } else { &groups },
        if private.is_empty() { "无" } else { &private },
        if file["ownerQQ"].is_null() { "未设置".to_string() } else { social_policy_js_string(&file["ownerQQ"]) },
        if retired.is_empty() { String::new() } else { format!("；已立即撤销 {}", retired.join(",")) }
    );
    Ok(json!({"ok": true, "allow": file["allow"], "deny": file["deny"], "ownerQQ": file["ownerQQ"]}))
}

// ── access-change invalidation ───────────────────────────────────────────────────────────────────

fn owner_of(cfg: &Value) -> Value {
    normalize_owner(cfg.get("ownerQQ")).unwrap_or(Value::Null)
}

/// Every conversation the change can concern: DSH mappings, second-generation and first-generation
/// records, keys with open or administrator-held ledger turns (a chat or closed-agent turn before
/// its session binds), the ids the old allow lists named, newly denied ids and the old owner.
/// Failures to read a source are returned: the caller reports them instead of revoking less.
fn candidate_keys(context: &RuntimeContext, before: &Value, after: &Value) -> (Vec<String>, Vec<String>) {
    let mut keys: Vec<String> = Vec::new();
    let mut failures = Vec::new();
    match crate::session_map::all(context) {
        Ok(mappings) => keys.extend(mappings.into_iter().map(|(key, _)| key)),
        Err(error) => failures.push(format!("读取会话映射失败：{error}")),
    }
    match load_social_state_for(context) {
        Ok(state) => {
            if let Some(conversations) = state.get("conversations").and_then(Value::as_object) {
                keys.extend(conversations.keys().cloned());
            }
        }
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
            for id in social_policy_list(cfg, root, kind).unwrap_or_default() {
                keys.push(format!("{prefix}:{id}"));
            }
        }
    }
    if let Some(owner) = owner_of(before).as_i64() {
        keys.push(format!("private:{owner}"));
    }
    keys.sort();
    keys.dedup();
    (keys, failures)
}

/// Revoke, before the administrator's request returns, every conversation whose authority the
/// configuration change from `before` to `after` removed in the current mode: a key that left the
/// allowed set, and in closed-agent mode every mapping when the owner changed (its session policy
/// carries the owner, JS `sessionPolicy`). The caller holds the closed-agent interaction gate.
/// Returns the revoked keys and every step that failed; a failure never stops the other steps.
pub(super) fn reconcile_access_change(
    context: &RuntimeContext,
    _interaction_gate: &std::fs::File,
    before: &Value,
    after: &Value,
    why: &str,
) -> (Vec<String>, Vec<String>) {
    if !context.is_isolated() {
        return (Vec::new(), Vec::new());
    }
    let mode = runtime_mode(context);
    let owner_changed = mode == "closed-agent" && owner_of(before) != owner_of(after);
    let (keys, mut failures) = candidate_keys(context, before, after);
    // Under `allowAllWhenEmpty` a conversation can be allowed without any record (a first contact
    // whose job is still queued): no key list can name it. A policy record ends the epoch of every
    // conversation the new policy does not admit (`turn_ledger::reset_count`).
    if admits_unlisted(before, &mode) && access_policy(before) != access_policy(after) {
        if let Err(error) = crate::turn_ledger::record_policy_reset(context, &access_policy(after), &mode, why) {
            failures.push(format!("策略重置记录写入失败：{error}"));
        }
    }
    let mapped: Vec<String> = crate::session_map::all(context).unwrap_or_default().into_iter().map(|(k, _)| k).collect();
    let revoked: Vec<String> = keys
        .into_iter()
        .filter(|key| {
            (agent::mode_allowed(before, &mode, key) && !agent::mode_allowed(after, &mode, key))
                || (owner_changed && mapped.contains(key))
        })
        .collect();
    for key in &revoked {
        if let Err(error) = revoke_key(context, after, key, why) {
            crate::safe_println!("[console] ⚠️ 撤销 {key} 失败：{error}");
            failures.push(format!("{key}：{error}"));
        }
    }
    (revoked, failures)
}

/// The fields `agent::mode_allowed` decides with.
fn access_policy(cfg: &Value) -> Value {
    let mut policy = serde_json::Map::new();
    for field in ["allow", "deny", "allowAllWhenEmpty", "ownerQQ"] {
        if let Some(value) = cfg.get(field) {
            policy.insert(field.into(), value.clone());
        }
    }
    Value::Object(policy)
}

/// The policy admits conversations no list names (an empty allow list of a kind with
/// `allowAllWhenEmpty`, outside closed-agent mode).
fn admits_unlisted(cfg: &Value, mode: &str) -> bool {
    mode != "closed-agent"
        && cfg.get("allowAllWhenEmpty").and_then(Value::as_bool) == Some(true)
        && ["group", "private"].iter().any(|kind| social_policy_list(cfg, "allow", kind).unwrap_or_default().is_empty())
}

/// JS `retireSession` plus the token rotation the ticket requires for unmapped conversations: a
/// later re-grant must not let anything queued under this epoch run. Every step runs even when an
/// earlier one failed (fail closed); the failures are returned together.
fn revoke_key(context: &RuntimeContext, cfg: &Value, key: &str, why: &str) -> Result<(), String> {
    let mut failures = Vec::new();
    // Recorded first: queued chat/closed-agent jobs of this key compare the reset count.
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
    if let Some(session) = &session {
        if let Err(error) = crate::session_map::retire_if(context, key, session) {
            failures.push(format!("退役会话映射失败：{error}"));
        }
    }
    if let Err((_, error)) = mutate_social_state_for(context, |state| {
        if let Some(conversation) = state
            .get_mut("conversations")
            .and_then(|c| c.get_mut(key))
            .and_then(Value::as_object_mut)
        {
            conversation.insert("agentToken".into(), json!(fresh_agent_token()));
            conversation.insert("bootstrapSent".into(), json!(false));
        }
        Ok(())
    }) {
        failures.push(format!("轮换会话令牌失败：{error}"));
    }
    if let Err(error) = cancel_ledger_turns(context, key, why) {
        failures.push(error);
    }
    if let Some(session) = session {
        stop_retired_session_work(context, cfg, key, &session);
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("；")) }
}

/// Open and administrator-held turns of `key` end as cancelled: their late results are refused.
fn cancel_ledger_turns(context: &RuntimeContext, key: &str, why: &str) -> Result<(), String> {
    let records = crate::turn_ledger::read_records(context).map_err(|e| format!("读取回合账本失败：{e}"))?;
    let (turns, _) = crate::turn_ledger::fold(&records);
    let mut failures = Vec::new();
    for turn in turns.iter().filter(|t| t.key == key && (t.state.is_open() || t.state == crate::turn_ledger::State::Paused)) {
        if let Err(error) = crate::turn_ledger::Recorder::attach(context, &turn.id, key).without_retry().end("cancelled", why) {
            failures.push(format!("取消回合 {} 失败：{error}", turn.id));
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("；")) }
}

/// JS `stopRetiredSessionWork`: clear the DSH queue, cancel, archive, in the background.
fn stop_retired_session_work(context: &RuntimeContext, cfg: &Value, key: &str, session: &str) {
    let (context, cfg, key, session) = (context.clone(), cfg.clone(), key.to_string(), session.to_string());
    std::thread::spawn(move || {
        let _ = stop_and_archive_session(&context, &cfg, &key, &session);
    });
}

// ── GET/POST /api/security ───────────────────────────────────────────────────────────────────────

fn state_dir(context: &RuntimeContext) -> Result<String, String> {
    crate::subprocess::external_path(context.data_dir())
}

#[cfg(windows)]
fn state_dir_hardened(dir: &str) -> bool {
    is_hardened(dir)
}
#[cfg(not(windows))]
fn state_dir_hardened(dir: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(dir).map(|m| m.permissions().mode() & 0o077 == 0).unwrap_or(false)
}
#[cfg(windows)]
fn harden_state_dir(dir: &str) -> (bool, String) {
    harden_dir(dir)
}
#[cfg(not(windows))]
fn harden_state_dir(dir: &str) -> (bool, String) {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        Ok(()) => (true, "chmod 700".to_string()),
        Err(error) => (false, error.to_string()),
    }
}
#[cfg(windows)]
fn state_dir_command(dir: &str) -> String {
    manual_dir_command(dir)
}
#[cfg(not(windows))]
fn state_dir_command(dir: &str) -> String {
    format!("chmod -R go-rwx \"{dir}\"")
}

/// JS `hardenStateDirAcl` at console start: the selected data area holds the console token,
/// agent tokens and chat history.
pub(super) fn harden_selected_state_dir(context: &RuntimeContext) {
    if !context.is_isolated() {
        return;
    }
    let dir = match state_dir(context) {
        Ok(dir) => dir,
        Err(error) => { crate::safe_eprintln!("[console] {error}"); return; }
    };
    let (ok, detail) = harden_state_dir(&dir);
    if !ok {
        crate::safe_println!("[console] ⚠️ 收紧 state/ 权限失败：{detail}");
    }
}

/// JS `cfg.security`: `{ interceptNotify: true, ...file.security }`.
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
        Err(error) => return json!({"security":security_view(cfg),"stateDir":{"hardened":false,"error":error}}),
    };
    json!({
        "security": security_view(cfg),
        "stateDir": {"hardened": state_dir_hardened(&dir), "path": dir, "manualCommand": state_dir_command(&dir)},
    })
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

pub(super) fn security_post(context: &RuntimeContext, body: &str) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    if body.get("rehardenStateDir") == Some(&json!(true)) {
        let dir = state_dir(context).map_err(|error| (503, error))?;
        let (ok, detail) = harden_state_dir(&dir);
        if !ok {
            crate::safe_println!("[console] ⚠️ 收紧 state/ 权限失败：{detail}");
        }
        return Ok(json!({"ok": ok, "hardened": ok, "path": dir}));
    }
    let _gate = acquire_social_state_gate(context)?;
    let mut file = read_config_object(context)?;
    let previous_view = security_view(&file);
    let mut next = file.get("security").and_then(Value::as_object).cloned().unwrap_or_default();
    next.extend(body.as_object().cloned().unwrap_or_default());
    if let Some(value) = next.get("interceptNotify").cloned() {
        if !value.is_boolean() {
            next.insert("interceptNotify".into(), json!(js_truthy(&value)));
        }
    }
    file["security"] = Value::Object(next.clone());
    write_config_object(context, &file)?;
    let mut view = previous_view.as_object().cloned().unwrap_or_default();
    view.extend(next);
    crate::safe_println!("[console] 控制台：安全拦截通知已更新（interceptNotify={}）", view.get("interceptNotify").map(social_policy_js_string).unwrap_or_default());
    Ok(json!({"ok": true, "security": Value::Object(view)}))
}

// ── POST /api/authorize/read ─────────────────────────────────────────────────────────────────────

fn key_parts(key: &str) -> Option<(&str, &str)> {
    let (kind, id) = key.split_once(':')?;
    (matches!(kind, "group" | "private") && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some((kind, id))
}

fn positive_id(id: &str) -> bool {
    id.parse::<f64>().is_ok_and(|n| n.is_finite() && n > 0.0)
}

pub(super) fn authorize_read(context: &RuntimeContext, cfg: &Value, body: &str) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    let key = js_trim(&js_string_or(body.get("key"), "")).to_string();
    let token = js_trim(&js_string_or(body.get("token"), "")).to_string();
    let Some((_, id)) = key_parts(&key) else {
        return Err((400, agent::KEY_FORMAT_ERROR.to_string()));
    };
    if token.is_empty() || !agent_token_ok_for(context, &key, &token)? {
        return Err((
            403,
            if token.is_empty() {
                "缺少会话令牌；旧只读工具在需要令牌的模式下不可用，请改用带会话令牌的 v2 读工具".to_string()
            } else {
                "agent token 无效".to_string()
            },
        ));
    }
    let mode = current_mode_for(context)?;
    if !positive_id(id) || !agent::mode_allowed(cfg, &mode, &key) {
        return Err((403, "当前模式不允许读取该会话".to_string()));
    }
    Ok(json!({"ok": true, "key": key}))
}

// ── POST /api/console/token ──────────────────────────────────────────────────────────────────────

/// The token the request asked for (`String(body.token ?? '').trim()`), after the JS body checks.
pub(super) fn requested_console_token(body: &str) -> Result<String, (u16, String)> {
    let body = parse_object_body(body)?;
    Ok(js_trim(&js_string_or(body.get("token"), "")).to_string())
}

// ── POST /api/socialV2/reset ─────────────────────────────────────────────────────────────────────

/// JS `POST /api/socialV2/reset`: a new session epoch, every queued prompt dropped, timers and
/// queues cleared, every second-generation conversation's DSH work stopped, its mapping retired
/// and token voided, `social-v2.json` emptied and `paused=false`. The live scheduler observes the
/// new `resetEpoch` and drops everything it still holds; queued chat jobs see the ledger reset.
pub(super) fn social_v2_reset(context: &RuntimeContext) -> Result<Value, (u16, String)> {
    let _interaction_gate = crate::closed_agent::lock(context).map_err(|e| (503, e))?;
    let cfg = context.load_config().map_err(|e| (503, e.to_string()))?;
    // Recorded before anything changes: if it cannot be written, nothing is reset.
    crate::turn_ledger::record_reset_why(context, "*", "socialV2-reset").map_err(|e| (503, e))?;
    let state = load_social_state_for(context)?;
    let keys: Vec<String> = state
        .get("conversations")
        .and_then(Value::as_object)
        .map(|c| c.keys().cloned().collect())
        .unwrap_or_default();
    // Queued ingress of these conversations is discarded with them (JS drops the timers and
    // queues), also across a restart before the scheduler's next tick. A failure is reported: the
    // events would be replayed after a restart.
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
        if let Some(session) = crate::session_map::lookup(context, key).map_err(|e| (503, e))? {
            if crate::session_map::retire_if(context, key, &session).map_err(|e| (503, e))? {
                retired.push((key.clone(), session));
            }
        }
    }
    mutate_social_state_for(context, |state| {
        let previous = state.get("conversations").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut kept = serde_json::Map::new();
        for (key, conversation) in previous {
            // Pending sticker liabilities keep their (revoked) identity so quota stays counted,
            // as in the single-conversation reset; everything else goes.
            if ["stickerSendProtected", "stickerSendPending", "stickerCollectPending"]
                .iter()
                .any(|f| conversation.get(*f).and_then(Value::as_array).is_some_and(|a| !a.is_empty()))
            {
                let mut replacement = new_conversation(context);
                preserve_reset_sticker_liabilities(&conversation, &mut replacement);
                kept.insert(key, replacement);
            }
        }
        *state = json!({"conversations": kept, "resetEpoch": crate::turn_ledger::mint_id("reset-")});
        Ok(())
    })?;
    for key in &keys {
        if let Err(error) = cancel_ledger_turns(context, key, "socialV2-reset") {
            failures.push(format!("{key}：{error}"));
        }
    }
    for (key, session) in retired {
        stop_retired_session_work(context, &cfg, &key, &session);
    }
    if !failures.is_empty() {
        return Err((500, format!("二代状态已重置，但作废排队与在途工作未完成：{}", failures.join("；"))));
    }
    crate::safe_println!("[console] 控制台：二代 AI 状态已重置（会话、定时器、唤醒配置已清空，工具日志保留）");
    Ok(json!({"ok": true}))
}

// ── POST /api/socialV2/wake ──────────────────────────────────────────────────────────────────────

/// Bound on queued manual wakes per conversation (JS bounds its busy queue at 20).
const ADMIN_WAKE_QUEUE: usize = 20;

/// JS `POST /api/socialV2/wake`. The first wake of a conversation is a `bootstrap` wake. The
/// request is handed to the live scheduler through the conversation record (`adminWakes`) with the
/// token epoch it was made under; the scheduler drops it if that epoch is gone.
pub(super) fn social_v2_wake(context: &RuntimeContext, cfg: &Value, body: &str) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    let key = js_trim(&js_string_or(body.get("key"), "")).to_string();
    let requested = js_trim(&js_string_or(body.get("reason"), "admin")).to_string();
    let reason = if requested.is_empty() { "admin".to_string() } else { requested };
    if key.is_empty() {
        return Err((400, "key 不能为空".to_string()));
    }
    let mode = current_mode_for(context)?;
    if mode != "reserved2" {
        return Err((403, "该接口仅 reserved2 模式可用".to_string()));
    }
    let Some((_, id)) = key_parts(&key) else {
        return Err((400, agent::KEY_FORMAT_ERROR.to_string()));
    };
    if !positive_id(id) || !agent::mode_allowed(cfg, &mode, &key) {
        return Err((403, "目标不在当前模式允许范围内".to_string()));
    }
    let canonical = canonical_v2_key(&key).ok_or((400, format!("无效的会话 key：{key}")))?;
    let at = now_ms();
    let (first, effective) = mutate_social_state_for(context, |state| {
        // Checked again under the gate: a mode change, revocation or reset may have completed
        // while this request waited for it.
        let mode = current_mode_for(context)?;
        if mode != "reserved2" {
            return Err((403, "该接口仅 reserved2 模式可用".to_string()));
        }
        let fresh = context.load_config().map_err(|e| (503, e.to_string()))?;
        if !agent::mode_allowed(&fresh, &mode, &key) {
            return Err((403, "目标不在当前模式允许范围内".to_string()));
        }
        if !state["conversations"].is_object() {
            state["conversations"] = json!({});
        }
        if state["conversations"].get(&canonical).is_none() {
            state["conversations"][&canonical] = new_conversation(context);
        }
        let conversation = &mut state["conversations"][&canonical];
        let first = conversation.get("bootstrapSent").and_then(Value::as_bool) != Some(true);
        if first {
            conversation["bootstrapSent"] = json!(true);
        }
        let effective = if first { "bootstrap".to_string() } else { reason.clone() };
        let epoch = conversation
            .get("agentToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(crate::send_outbox::token_epoch)
            .unwrap_or_default();
        if !conversation["adminWakes"].is_array() {
            conversation["adminWakes"] = json!([]);
        }
        let queue = conversation["adminWakes"].as_array_mut().expect("array set above");
        queue.push(json!({"reason": effective, "epoch": epoch, "at": at}));
        if queue.len() > ADMIN_WAKE_QUEUE {
            queue.drain(..queue.len() - ADMIN_WAKE_QUEUE);
        }
        Ok((first, effective))
    })?;
    crate::safe_println!(
        "[console] 控制台：手动唤醒 {key}（{effective}{}）",
        if first && reason != effective { format!("，首次唤醒，请求原因 {reason} 已升级为 bootstrap") } else { String::new() }
    );
    Ok(json!({"ok": true, "key": key, "reason": effective, "requestedReason": reason, "firstWake": first}))
}

/// The live scheduler's side of the manual wake: take the oldest queued request of `key` (one per
/// tick, so a second request meets the first one's turn as JS's busy queue does). Returns it only
/// when it was made under the conversation's current token epoch.
pub(crate) fn take_admin_wake(context: &RuntimeContext, key: &str) -> Result<Option<String>, String> {
    mutate_social_state_for(context, |state| {
        let Some(conversation) = state.get_mut("conversations").and_then(|c| c.get_mut(key)) else {
            return Ok(None);
        };
        let Some(queue) = conversation.get_mut("adminWakes").and_then(Value::as_array_mut) else {
            return Ok(None);
        };
        if queue.is_empty() {
            return Ok(None);
        }
        let request = queue.remove(0);
        if queue.is_empty() {
            conversation.as_object_mut().map(|c| c.remove("adminWakes"));
        }
        let current = conversation
            .get("agentToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(crate::send_outbox::token_epoch)
            .unwrap_or_default();
        if request.get("epoch").and_then(Value::as_str).unwrap_or("") != current {
            return Ok(None);
        }
        Ok(request.get("reason").and_then(Value::as_str).map(str::to_string))
    })
    .map_err(|(_, e)| e)
}
