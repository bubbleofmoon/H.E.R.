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

/// `config.json`'s loader stamp and the settings store's mark (`settings::store_mark`, #154: the allow and deny lists
/// and the owner are settings, so a stored change is a configuration change too).
type StartStamp = (Option<crate::runtime::ConfigStamp>, Option<i64>);

/// The configuration as the console found it at start (`StartStamp`), per runtime.
static CONFIG_START_STAMP: std::sync::OnceLock<std::sync::Mutex<HashMap<std::path::PathBuf, StartStamp>>> =
    std::sync::OnceLock::new();

fn config_stamp(context: &RuntimeContext) -> StartStamp {
    (context.config_stamp().ok(), crate::settings::store_mark(context))
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

/// JS `POST /api/whitelist` in the selected runtime: allow/deny lists and the owner, then the
/// immediate revocation of every conversation that lost access (JS `reconcileSessionPolicies`).
/// #154: the lists and the owner are security settings. A change of one needs `"confirm": true` (409 otherwise, nothing
/// written; `confirm` other than a boolean is 400), is validated by the registry (400, nothing written) and is stored
/// through the settings store after its revocation is recorded; the file keeps its registry members as the store wrote
/// them back and gets the rest of the request. A request that does not name the owner leaves it as it is.
pub(super) fn whitelist_post(context: &RuntimeContext, body: &str) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    let confirm = crate::settings::confirm_field(&body)?;
    // Access changes are serialized with closed-agent receipts, mode changes and resets: the gate
    // is taken before anything is written, so its failure leaves the configuration unchanged.
    let interaction = crate::closed_agent::lock(context).map_err(|e| (503, e))?;
    finish_pending_before_change(context, &interaction)?;
    // The revocation compares the policy the runtime authorizes with (canonical lists and the
    // closed-agent owner overlay), read under the same gate as the write.
    let (before, after, file, pending) = {
        let _gate = acquire_social_state_gate(context)?;
        let effective_before = context.load_config().map_err(|e| (503, e.to_string()))?;
        // #192: the file as read, to tell a write that failed before publication from one that published.
        let unchanged = read_config_object(context)?;
        // #154: with the stored settings in place, so a list the request does not name is the one in effect.
        let mut file = unchanged.clone();
        crate::settings::mirror_into(context, &mut file).map_err(|e| (503, e))?;
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
        let object = file.as_object_mut().expect("config checked above");
        object.insert("allow".into(), allow);
        object.insert("deny".into(), deny);
        // #154: the owner is a setting the admin agent mode overrides while it is on, so a request that does not name
        // it never copies that mode's owner into it.
        if let Some(value) = body.get("ownerQQ") {
            object.insert("ownerQQ".into(), normalize_owner(Some(value)).map_err(|e| (400, e))?);
        }
        let changes = crate::settings::console_changes(context, &unchanged, &file)?;
        let changed: Vec<&crate::settings::Setting> = changes.iter().map(|(setting, _)| *setting).collect();
        crate::settings::check_confirmation(confirm, &changed)?;
        let previewed: Vec<_> = changes.iter().map(|(setting, value)| (*setting, Some(value.clone()))).collect();
        // A written configuration the runtime cannot load revokes everything the old one allowed.
        let effective_after = context.effective_config_with(&file, &previewed).unwrap_or_else(|_| json!({}));
        let stored = !changes.is_empty();
        if stored {
            // The store's write-back refuses a file the runtime cannot load: refuse it before anything is recorded.
            raw_config_for_write(context)?;
            note_config_before_request(context);
        }
        let pending = begin_access_change(context, &effective_before, &effective_after, "access-revoked")?;
        store_access_changes(context, changes, &previewed, &pending, "console")?;
        keep_stored_members(context, &mut file)?;
        // Once the store holds the change, a failed file write keeps the pending revocation (`Null`: always kept).
        write_config_or_forget(context, &file, if stored { &Value::Null } else { &unchanged }, &pending)?;
        let shown = context.load_config_document().ok().filter(Value::is_object).unwrap_or(file);
        (effective_before, effective_after, shown, pending)
    };
    let (retired, failures) = finish_access_change(context, &interaction, &before, &after, "access-revoked", pending);
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

// ── POST /api/conversations/scope ────────────────────────────────────────────────────────────────

/// The reply's reason when the running live owner serves another list than `rollout.rustGroups`.
const LIVE_RESTART_REASON: &str = "the live process serves the --live-groups list it was started with; the supervisor's watchdog restarts it with rollout.rustGroups on its next run (every two minutes), or run the watchdog task now (Schedule page, POST /api/tasks/watchdog/run)";

/// #186 test seam: runs inside `conversation_scope` between its fresh read of `config.json` and its write, with the
/// conversation key (so a test can hold one change there while another starts).
#[cfg(test)]
pub(super) static SCOPE_AFTER_READ: std::sync::Mutex<Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>> =
    std::sync::Mutex::new(None);

/// #186: admit (`"action":"admit"`) or remove (`"action":"remove"`) one conversation (`"conversation"`: `group:<id>`
/// or `private:<id>`) in both keys that decide whether it is answered: its allow list (`allow.groups` or
/// `allow.private`) and `rollout.rustGroups`. One read-modify-write of `config.json` under the gates of
/// `POST /api/whitelist`, then the same immediate revocation. The running live process keeps the `--live-groups`
/// list it was started with; `liveRestart` says whether it must restart. The body never names `key`: the console's
/// key preflight would refuse a conversation that is not admitted yet.
/// #154: the allow lists are security settings: a change of one needs `"confirm": true` (409 otherwise, nothing
/// written; `her-cli conversation` confirms with its `--confirm`) and is stored through the settings store after the
/// revocation is recorded; `rollout.rustGroups` and the alias removal are written to the file, which keeps its registry
/// members as the store wrote them back.
pub(super) fn conversation_scope(context: &RuntimeContext, body: &str) -> Result<Value, (u16, String)> {
    let body = parse_object_body(body)?;
    let confirm = crate::settings::confirm_field(&body)?;
    let key = canonical_v2_key(&js_string_or(body.get("conversation"), ""))
        .ok_or((400, "conversation must be group:<id> or private:<id>".to_string()))?;
    let admit = match body.get("action").and_then(Value::as_str) {
        Some("admit") => true,
        Some("remove") => false,
        _ => return Err((400, "action must be admit or remove".to_string())),
    };
    let (kind, id) = key.split_once(':').expect("canonical keys carry a colon");
    let (field, alias) = if kind == "group" { ("groups", "group") } else { ("private", "privates") };
    // Listed ids and served keys are compared in canonical form, whatever spelling the file uses.
    let listed_same = |value: &Value| {
        social_policy_normalized_id(value)
            .and_then(|listed| canonical_v2_key(&format!("{kind}:{listed}")))
            .is_some_and(|listed| listed == key)
    };
    let served_same = |value: &Value| value.as_str().and_then(canonical_v2_key).is_some_and(|served| served == key);
    let interaction = crate::closed_agent::lock(context).map_err(|e| (503, e))?;
    finish_pending_before_change(context, &interaction)?;
    let (before, after, changed, pending) = {
        let _gate = acquire_social_state_gate(context)?;
        let effective_before = context.load_config().map_err(|e| (503, e.to_string()))?;
        // #192: the file as read, to tell a write that failed before publication from one that published.
        let unchanged = read_config_object(context)?;
        // #154: with the stored settings in place, so the allow list changed is the one in effect.
        let mut file = unchanged.clone();
        crate::settings::mirror_into(context, &mut file).map_err(|e| (503, e))?;
        #[cfg(test)]
        {
            // Bound first, so the hook never runs while the seam's own lock is held.
            let hook = SCOPE_AFTER_READ.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(hook) = hook {
                hook(&key);
            }
        }
        let Some(mut served) = file.pointer("/rollout/rustGroups").and_then(Value::as_array).cloned() else {
            return Err((409, "rollout.rustGroups is not configured: the supervisor serves only that explicit list".to_string()));
        };
        if admit
            && social_policy_list(&file, "deny", kind)
                .unwrap_or_default()
                .iter()
                .any(|denied| canonical_v2_key(&format!("{kind}:{denied}")).as_deref() == Some(key.as_str()))
        {
            return Err((409, format!("{key} is in deny.{field}; remove it from the deny list first")));
        }
        // JS precedence: `groups ?? group`, `private ?? privates`.
        let preferred = file["allow"][field].clone();
        let chosen = if preferred.is_null() { file["allow"][alias].clone() } else { preferred };
        let mut listed = chosen.as_array().cloned().unwrap_or_default();
        let mut changed: Vec<String> = Vec::new();
        if admit != listed.iter().any(|value| listed_same(value)) {
            listed.retain(|value| !listed_same(value));
            if admit {
                listed.push(json!(id.parse::<i64>().expect("canonical ids are safe integers")));
            }
            if !file["allow"].is_object() {
                file["allow"] = json!({});
            }
            let allow = file["allow"].as_object_mut().expect("allow is an object");
            allow.remove(alias);
            allow.insert(field.to_string(), Value::Array(listed));
            changed.push(format!("allow.{field}"));
        }
        if admit != served.iter().any(|value| served_same(value)) {
            served.retain(|value| !served_same(value));
            if admit {
                served.push(json!(key));
            }
            file["rollout"]["rustGroups"] = Value::Array(served);
            changed.push("rollout.rustGroups".to_string());
        }
        if changed.is_empty() {
            (effective_before.clone(), effective_before, changed, None)
        } else {
            let changes = crate::settings::console_changes(context, &unchanged, &file)?;
            let settings: Vec<&crate::settings::Setting> = changes.iter().map(|(setting, _)| *setting).collect();
            crate::settings::check_confirmation(confirm, &settings)?;
            let previewed: Vec<_> = changes.iter().map(|(setting, value)| (*setting, Some(value.clone()))).collect();
            // A written configuration the runtime cannot load revokes everything the old one allowed.
            let effective_after = context.effective_config_with(&file, &previewed).unwrap_or_else(|_| json!({}));
            let stored = !changes.is_empty();
            if stored {
                // The store's write-back refuses a file the runtime cannot load: refuse it before anything is recorded.
                raw_config_for_write(context)?;
                note_config_before_request(context);
            }
            let pending = begin_access_change(context, &effective_before, &effective_after, "access-revoked")?;
            store_access_changes(context, changes, &previewed, &pending, "console")?;
            keep_stored_members(context, &mut file)?;
            // Once the store holds the change, a failed file write keeps the pending revocation (`Null`: always kept).
            write_config_or_forget(context, &file, if stored { &Value::Null } else { &unchanged }, &pending)?;
            (effective_before, effective_after, changed, pending)
        }
    };
    let (retired, failures) = if changed.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        finish_access_change(context, &interaction, &before, &after, "access-revoked", pending)
    };
    drop(interaction);
    if !failures.is_empty() {
        return Err((500, format!("conversation scope saved, but the immediate revocation did not finish: {}", failures.join("; "))));
    }
    let target: Vec<String> = after
        .pointer("/rollout/rustGroups")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let restart = live_restart(context, &target);
    let gaps = crate::watchdog::rollout_scope_gaps(&after);
    Ok(json!({
        "ok": true,
        "conversation": key,
        "action": if admit { "admit" } else { "remove" },
        "changed": changed,
        "revoked": retired,
        "liveRestart": restart,
        "scope": {"notAllowed": gaps.not_allowed, "notServed": gaps.not_served},
    }))
}

/// #186: whether the running live owner must restart to serve `target` (`rollout.rustGroups`). It serves the
/// `--live-groups` list its owner marker records, fixed at launch: the per-conversation source gates it holds
/// (`lifecycle::acquire_message_source`) were taken for that list, so it cannot widen it in place.
fn live_restart(context: &RuntimeContext, target: &[String]) -> Value {
    let gate = match context.resolve_path(RuntimeArea::Data, &format!("logs/{}", crate::lifecycle::LIVE_LOCK), PathIntent::WriteFile) {
        Ok(path) => crate::lifecycle::OwnerGate::new(path),
        Err(_) => return json!({"state": "unknown", "reason": "the live owner gate is unavailable"}),
    };
    match gate.probe() {
        crate::lifecycle::OwnerProbe::Free { .. } => {
            json!({"state": "none", "reason": "no live process runs; the next start serves rollout.rustGroups"})
        }
        crate::lifecycle::OwnerProbe::Held { marker: Some(marker) } if marker.role == "live" => {
            let serving = marker.groups.clone().unwrap_or_default();
            // Compared as sets of canonical keys, whatever spelling either list uses.
            let canonical = |list: &[String]| {
                let mut keys: Vec<String> = list.iter().filter_map(|entry| canonical_v2_key(entry)).collect();
                keys.sort();
                keys.dedup();
                keys
            };
            if canonical(serving.as_slice()) == canonical(target) {
                json!({"state": "none", "serving": serving, "reason": "the live process already serves rollout.rustGroups"})
            } else {
                json!({"state": "required", "serving": serving, "reason": LIVE_RESTART_REASON})
            }
        }
        _ => json!({"state": "unknown", "reason": "the live owner cannot be identified"}),
    }
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
    // #192: a retired session's recorded stop still names its conversation after the mapping is gone.
    match recorded_stops(context) {
        Ok(stops) => keys.extend(stops.into_iter().map(|(key, _)| key)),
        Err(error) => failures.push(error),
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
    entry: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    if !context.is_isolated() {
        return (Vec::new(), Vec::new());
    }
    reconcile_policies(context, before, after, &runtime_mode(context), why, after, entry, &[])
}

/// `reconcile_access_change` for the policies `before` and `after` in `mode`; retired DSH sessions are stopped with
/// the DSH settings of `dsh_cfg`. #192: a pending revocation is finished by this with its recorded policies and mode.
/// The conversations it selects are recorded in its pending entry `entry` (`targets`) before any step runs, and
/// `targets` (recorded by an earlier attempt) are selected again, so a retry revokes them whatever an interrupted
/// attempt already changed (a retired mapping, a finished stop); the entry, and with it the targets, is removed only
/// once every step succeeded.
#[allow(clippy::too_many_arguments)]
fn reconcile_policies(
    context: &RuntimeContext,
    before: &Value,
    after: &Value,
    mode: &str,
    why: &str,
    dsh_cfg: &Value,
    entry: Option<&str>,
    targets: &[String],
) -> (Vec<String>, Vec<String>) {
    let owner_changed = mode == "closed-agent" && owner_of(before) != owner_of(after);
    let (mut keys, mut failures) = candidate_keys(context, before, after);
    keys.extend(targets.iter().cloned());
    keys.sort();
    keys.dedup();
    // Under `allowAllWhenEmpty` a conversation can be allowed without any record (a first contact
    // whose job is still queued): no key list can name it. A policy record ends the epoch of every
    // conversation the new policy does not admit (`turn_ledger::reset_count`).
    if admits_unlisted(before, mode) && access_policy(before) != access_policy(after) {
        if let Err(error) = crate::turn_ledger::record_policy_reset(context, &access_policy(after), mode, why) {
            failures.push(format!("策略重置记录写入失败：{error}"));
        }
    }
    let mapped: Vec<String> = crate::session_map::all(context).unwrap_or_default().into_iter().map(|(k, _)| k).collect();
    // #192: a retry after a crash between a mapping's retirement and its token rotation finds the conversation through
    // the retired session's recorded stop (`record_stop` runs before `retire_if`), not only through current mappings.
    let stopped: Vec<String> = recorded_stops(context).unwrap_or_default().into_iter().map(|(key, _)| key).collect();
    let revoked: Vec<String> = keys
        .into_iter()
        .filter(|key| {
            targets.contains(key)
                || (agent::mode_allowed(before, mode, key) && !agent::mode_allowed(after, mode, key))
                || (owner_changed && (mapped.contains(key) || stopped.contains(key)))
        })
        .collect();
    if let Some(id) = entry {
        if let Err(error) = record_targets(context, id, &revoked) {
            failures.push(format!("the revocation targets were not recorded, nothing was revoked: {error}"));
            return (Vec::new(), failures);
        }
    }
    for key in &revoked {
        if let Err(error) = revoke_key(context, dsh_cfg, key, why) {
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
    // #192: the session's stop is recorded before its mapping is retired, so a crash before the stop finished leaves it
    // (and the conversation it belongs to) for the next console start or access change.
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
    if let Some(session) = stop {
        stop_recorded_session(context, cfg, key, &session);
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

/// #186: background session stops still running in this process; `her-cli conversation` waits for them before exit.
static BACKGROUND_STOPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// JS `stopRetiredSessionWork`: clear the DSH queue, cancel, archive, in the background.
fn stop_retired_session_work(context: &RuntimeContext, cfg: &Value, key: &str, session: &str) {
    let (context, cfg, key, session) = (context.clone(), cfg.clone(), key.to_string(), session.to_string());
    BACKGROUND_STOPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::thread::spawn(move || {
        let _ = stop_and_archive_session(&context, &cfg, &key, &session);
        BACKGROUND_STOPS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    });
}

/// #192: the DSH sessions whose recorded stop runs in this process now.
static RUNNING_STOPS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// #192 test seam: given a session id, `Some(result)` replaces that session's DSH stop.
#[cfg(test)]
pub(super) static STOP_SEAM: std::sync::Mutex<Option<std::sync::Arc<dyn Fn(&str) -> Option<Result<(), String>> + Send + Sync>>> =
    std::sync::Mutex::new(None);

fn run_recorded_stop(context: &RuntimeContext, cfg: &Value, key: &str, session: &str) -> Result<(), String> {
    #[cfg(test)]
    {
        let seam = STOP_SEAM.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(result) = seam.and_then(|seam| seam(session)) {
            return result;
        }
    }
    stop_and_archive_session(context, cfg, key, session)
}

/// #192: `stop_retired_session_work` for a stop recorded in the pending-revocation record (`record_stop`). The record
/// is removed, under the closed-agent interaction gate, only once the stop succeeded; a failure or a crash leaves it
/// for the next console start or access change. A session whose stop already runs in this process is not started
/// again.
fn stop_recorded_session(context: &RuntimeContext, cfg: &Value, key: &str, session: &str) {
    {
        let mut running = RUNNING_STOPS.lock().unwrap_or_else(|p| p.into_inner());
        if running.iter().any(|running| running == session) {
            return;
        }
        running.push(session.to_string());
    }
    let (context, cfg, key, session) = (context.clone(), cfg.clone(), key.to_string(), session.to_string());
    BACKGROUND_STOPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::thread::spawn(move || {
        if run_recorded_stop(&context, &cfg, &key, &session).is_ok() {
            match crate::closed_agent::lock(&context) {
                Ok(_gate) => {
                    if let Err(error) = forget_stop(&context, &key, &session) {
                        crate::safe_println!("[console] ⚠️ the finished DSH stop of {key} stays recorded: {error}");
                    }
                }
                Err(error) => crate::safe_println!("[console] ⚠️ the finished DSH stop of {key} stays recorded: {error}"),
            }
        }
        RUNNING_STOPS.lock().unwrap_or_else(|p| p.into_inner()).retain(|running| running != &session);
        BACKGROUND_STOPS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    });
}

/// #186 test seam: counts as one background session stop until dropped.
#[cfg(test)]
pub(super) struct BackgroundStopForTest;

#[cfg(test)]
impl BackgroundStopForTest {
    pub(super) fn start() -> Self {
        BACKGROUND_STOPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for BackgroundStopForTest {
    fn drop(&mut self) {
        BACKGROUND_STOPS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// #186: wait at most `limit` for this process's background session stops (each is bounded by
/// `stop_and_archive_session`'s own eight seconds). True when none is left.
pub(super) fn wait_background_stops(limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while BACKGROUND_STOPS.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    true
}

// ── #192: access changes whose revocation has not finished ───────────────────────────────────────

/// Data-area record (`{"pending": [...]}`) of the access changes whose revocation has not finished. An access change
/// adds its entry before it writes `config.json` and removes it once its revocation fully succeeded, so a crash in
/// between leaves the entry for `finish_pending_revocations`. Read and written only under the closed-agent
/// interaction gate.
pub(super) const PENDING_REVOCATIONS: &str = "access-revocations-pending.json";

/// #192 test seam: runs in every access change after its configuration write and before its revocation, with the
/// runtime's `config.json` path (so a test can stop only its own runtime there, as a crash would).
#[cfg(test)]
pub(super) static AFTER_ACCESS_WRITE: std::sync::Mutex<Option<std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync>>> =
    std::sync::Mutex::new(None);

/// The pending entries; none when the file does not exist (a runtime that never recorded one). An unreadable or
/// malformed file, or an entry without usable policies, is an error: its entries cannot be finished, and nothing may
/// be added to it.
fn pending_revocations(context: &RuntimeContext) -> Result<Vec<Value>, String> {
    let pending = read_record(context)?["pending"].as_array().cloned().unwrap_or_default();
    if let Some(index) = pending.iter().position(|entry| !usable_entry(entry)) {
        return Err(format!("data/{PENDING_REVOCATIONS} entry {index} has no usable before/after policy; repair or remove it"));
    }
    // #192: entries are told apart by `id` (the stop and target bookkeeping names them); two entries with one id
    // would let one entry's removal drop the other, so the record is refused instead.
    let mut ids: Vec<String> = Vec::new();
    for id in pending.iter().filter_map(|entry| entry["id"].as_str()) {
        if ids.iter().any(|seen| seen == id) {
            return Err(format!("data/{PENDING_REVOCATIONS} has the pending id {id} twice; repair or remove it"));
        }
        ids.push(id.to_string());
    }
    Ok(pending)
}

/// An entry carries both policies (`before`, `after`) as objects whose `allow` and `deny`, when present, are objects,
/// as `begin_access_change` writes them, and `targets`, when present, as an array of canonical conversation keys (the
/// keys `canonical_v2_key` returns unchanged). `mode` and `why` may be
/// missing: the current mode and `access-revoked` are used.
fn usable_entry(entry: &Value) -> bool {
    ["before", "after"].iter().all(|side| {
        entry
            .get(*side)
            .and_then(Value::as_object)
            .is_some_and(|policy| ["allow", "deny"].iter().all(|list| policy.get(*list).is_none_or(Value::is_object)))
    }) && entry.get("targets").is_none_or(|targets| {
        targets.as_array().is_some_and(|keys| {
            keys.iter().all(|key| key.as_str().is_some_and(|key| canonical_v2_key(key).as_deref() == Some(key)))
        })
    })
}

/// #192: the conversations an earlier attempt selected for `entry` (`targets`).
fn entry_targets(entry: &Value) -> Vec<String> {
    entry["targets"].as_array().map(|keys| keys.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

/// #192: add `targets` to the pending entry `id` before any revocation step runs. The caller holds the closed-agent
/// interaction gate.
fn record_targets(context: &RuntimeContext, id: &str, targets: &[String]) -> Result<(), String> {
    let mut pending = pending_revocations(context)?;
    let Some(entry) = pending.iter_mut().find(|entry| entry["id"].as_str() == Some(id)) else {
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

/// The whole record (`pending` and, #192 run 6, `stops`); an empty one when the file does not exist. A file that is not
/// JSON with a `pending` array is an error.
fn read_record(context: &RuntimeContext) -> Result<Value, String> {
    let Some(text) = context
        .read_optional_text(RuntimeArea::Data, PENDING_REVOCATIONS)
        .map_err(|e| format!("data/{PENDING_REVOCATIONS} is unreadable: {e}"))?
    else {
        return Ok(json!({"pending": []}));
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .filter(|record| record.get("pending").is_some_and(Value::is_array))
        .ok_or_else(|| format!("data/{PENDING_REVOCATIONS} is not a pending-revocation record; repair or remove it"))
}

/// Replace one field of the record, keeping the others.
fn write_record_field(context: &RuntimeContext, field: &str, value: Value) -> Result<(), String> {
    let mut record = read_record(context)?;
    record[field] = value;
    let text = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
    context.write_atomic(RuntimeArea::Data, PENDING_REVOCATIONS, text.as_bytes())
}

fn write_pending_revocations(context: &RuntimeContext, pending: &[Value]) -> Result<(), String> {
    write_record_field(context, "pending", Value::Array(pending.to_vec()))
}

/// #192: the DSH session stops (`stops`: `{"key", "session"}`) recorded by `record_stop` that have not finished. An
/// absent field is no stop; a field that is not an array, or an item without a string `key` and `session`, is an
/// error (so access changes are refused, as for an unusable pending entry).
fn recorded_stops(context: &RuntimeContext) -> Result<Vec<(String, String)>, String> {
    let record = read_record(context)?;
    let Some(stops) = record.get("stops") else { return Ok(Vec::new()) };
    let malformed = || format!("data/{PENDING_REVOCATIONS} has a malformed stop record; repair or remove it");
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

/// #192: record a retired DSH session's stop as its own pending step, before its mapping is retired. The caller holds
/// the closed-agent interaction gate.
fn record_stop(context: &RuntimeContext, key: &str, session: &str) -> Result<(), String> {
    recorded_stops(context)?;
    let mut stops = read_record(context)?["stops"].as_array().cloned().unwrap_or_default();
    let stop = json!({"key": key, "session": session});
    if !stops.contains(&stop) {
        stops.push(stop);
    }
    write_record_field(context, "stops", Value::Array(stops))
}

/// #192: remove a finished stop from the record. The caller holds the closed-agent interaction gate.
fn forget_stop(context: &RuntimeContext, key: &str, session: &str) -> Result<(), String> {
    recorded_stops(context)?;
    let mut stops = read_record(context)?["stops"].as_array().cloned().unwrap_or_default();
    let stop = json!({"key": key, "session": session});
    stops.retain(|recorded| recorded != &stop);
    write_record_field(context, "stops", Value::Array(stops))
}

/// Record the access change from `before` to `after` (effective configurations, the policies only) as pending, before
/// its configuration write. Returns the entry id (none in a legacy runtime, which revokes nothing). Refused with 503,
/// and so nothing written, when the record cannot be kept.
pub(super) fn begin_access_change(
    context: &RuntimeContext,
    before: &Value,
    after: &Value,
    why: &str,
) -> Result<Option<String>, (u16, String)> {
    if !context.is_isolated() {
        return Ok(None);
    }
    let mut pending = pending_revocations(context).map_err(|e| (503, e))?;
    let id = crate::turn_ledger::mint_id("revoke-");
    pending.push(json!({
        "id": id,
        "why": why,
        "mode": runtime_mode(context),
        "before": access_policy(before),
        "after": access_policy(after),
        "at": now_ms(),
    }));
    write_pending_revocations(context, &pending)
        .map_err(|e| (503, format!("the access change was not recorded, nothing was written: {e}")))?;
    Ok(Some(id))
}

/// Remove the pending entry `id` (its revocation finished, or its configuration write failed).
pub(super) fn forget_access_change(context: &RuntimeContext, id: &Option<String>) -> Result<(), String> {
    let Some(id) = id else { return Ok(()) };
    let mut pending = pending_revocations(context)?;
    pending.retain(|entry| entry["id"].as_str() != Some(id.as_str()));
    write_pending_revocations(context, &pending)
}

/// Write `config.json` for an access change recorded as `pending`; `unchanged` is the file as the change read it. A
/// failed write is settled by `settle_failed_write`.
fn write_config_or_forget(
    context: &RuntimeContext,
    file: &Value,
    unchanged: &Value,
    pending: &Option<String>,
) -> Result<(), (u16, String)> {
    let Err((status, error)) = write_config_object(context, file) else { return Ok(()) };
    Err((status, format!("{error}{}", settle_failed_write(context, unchanged, pending))))
}

/// #154: stores an access change's registry settings (`settings::apply_changes`, actor `administrator`, `source`) after
/// its revocation was recorded. A failed store write is settled by `settle_failed_store_write`, whose note the error
/// carries.
pub(super) fn store_access_changes(
    context: &RuntimeContext,
    changes: Vec<(&'static crate::settings::Setting, Value)>,
    previewed: &[(&'static crate::settings::Setting, Option<Value>)],
    pending: &Option<String>,
    source: &str,
) -> Result<(), (u16, String)> {
    crate::settings::apply_changes(context, changes, "administrator", source)
        .map(|_| ())
        .map_err(|(status, error)| (status, format!("{error}{}", settle_failed_store_write(context, previewed, pending))))
}

/// #154: after a failed settings store write of an access change: the pending entry is removed only when the store
/// holds none of the change (`settings::holds_any`: the failure came before the commit); a store that holds it, or
/// cannot be read, keeps the entry for the next console start or access change. Returns the note added to the error.
fn settle_failed_store_write(
    context: &RuntimeContext,
    previewed: &[(&'static crate::settings::Setting, Option<Value>)],
    pending: &Option<String>,
) -> String {
    if pending.is_none() {
        return String::new();
    }
    if crate::settings::holds_any(context, previewed).unwrap_or(true) {
        return "; the change may have been stored, so its revocation stays pending and is finished at the next console start or access change".to_string();
    }
    match forget_access_change(context, pending) {
        Ok(()) => String::new(),
        Err(cleanup) => format!("; the recorded revocation stays pending: {cleanup}"),
    }
}

/// #154: keeps `file`'s registry members as the stored `config.json` now holds them (`settings::keep_registry_values`),
/// after a writer stored its registry changes and the store wrote them back, so the writer's own write changes no
/// registry member. Stored changes stand, so a file that cannot hold them is 500 (an access change's revocation then
/// stays pending for the next console start or access change).
pub(super) fn keep_stored_members(context: &RuntimeContext, file: &mut Value) -> Result<(), (u16, String)> {
    let current = read_config_object(context)?;
    crate::settings::keep_registry_values(&current, file).map(|_| ()).map_err(|key| {
        (500, format!("config.json cannot hold {key}: a member on its path is not an object; stored settings stand"))
    })
}

/// #154: a settings-route change (`settings::Change`) of an access setting (`settings::Setting::access`), with a
/// whitelist write's side effects. Under the closed-agent interaction gate, earlier pending revocations are finished
/// first; under the configuration gate (`authority`, which re-checks the console credential) the change is previewed
/// (`settings::preview`), the policy after it is recorded as a pending revocation, and `commit` (the route's own set,
/// reset or undo) writes the store; then, with the configuration gate released, every conversation that lost access
/// is revoked (tokens rotated, mappings retired, turns cancelled) before the reply, which gets `revoked`. A failed
/// commit is settled by `settle_failed_store_write`; a revocation that does not finish is 500 (the change stands and its
/// entry stays pending).
pub(crate) fn access_setting_change(
    context: &RuntimeContext,
    authority: crate::persona::Authority<'_>,
    change: &crate::settings::Change<'_>,
    commit: impl FnOnce() -> Result<Value, (u16, String)>,
) -> Result<Value, (u16, String)> {
    let interaction = crate::closed_agent::lock(context).map_err(|e| (503, e))?;
    finish_pending_before_change(context, &interaction)?;
    let (before, after, mut reply, pending) = {
        let _gate = authority()?;
        let effective_before = context.load_config().map_err(|e| (503, e.to_string()))?;
        let raw = read_config_object(context)?;
        let previewed = crate::settings::preview(context, change)?;
        // A configuration the runtime cannot load revokes everything the old one allowed.
        let effective_after = context.effective_config_with(&raw, &previewed).unwrap_or_else(|_| json!({}));
        let pending = begin_access_change(context, &effective_before, &effective_after, "access-revoked")?;
        let reply = commit().map_err(|(status, error)| {
            (status, format!("{error}{}", settle_failed_store_write(context, &previewed, &pending)))
        })?;
        (effective_before, effective_after, reply, pending)
    };
    let (revoked, failures) = finish_access_change(context, &interaction, &before, &after, "access-revoked", pending);
    drop(interaction);
    if !failures.is_empty() {
        return Err((500, format!("the setting was saved, but the immediate revocation did not finish: {}", failures.join("; "))));
    }
    reply["revoked"] = json!(revoked);
    Ok(reply)
}

/// The raw `config.json` as an access change finds it before its write, for `settle_failed_write` (`Null` when it
/// cannot be read: a failed write then always keeps its entry).
pub(super) fn config_snapshot(context: &RuntimeContext) -> Value {
    read_config_object(context).unwrap_or(Value::Null)
}

/// After a failed configuration write: the pending entry is removed only when `config.json` still holds `unchanged`
/// (the failure came before the atomic replace published anything). A file that changed, or cannot be read, may hold
/// the new policy (a failure after publication, such as the folder sync): the entry stays, and the next console start
/// or access change finishes its revocation. Returns the note added to the write error.
pub(super) fn settle_failed_write(context: &RuntimeContext, unchanged: &Value, pending: &Option<String>) -> String {
    if pending.is_none() {
        return String::new();
    }
    let published = read_config_object(context).map_or(true, |current| &current != unchanged);
    if published {
        return "; the configuration may have been written, so its revocation stays pending and is finished at the next console start or access change".to_string();
    }
    match forget_access_change(context, pending) {
        Ok(()) => String::new(),
        Err(cleanup) => format!("; the recorded revocation stays pending: {cleanup}"),
    }
}

/// The revocation that follows an access change's configuration write. Its pending entry is removed only when every
/// step succeeded; otherwise it stays for the next console start or access change.
pub(super) fn finish_access_change(
    context: &RuntimeContext,
    interaction_gate: &std::fs::File,
    before: &Value,
    after: &Value,
    why: &str,
    pending: Option<String>,
) -> (Vec<String>, Vec<String>) {
    #[cfg(test)]
    {
        // Bound first, so the hook never runs while the seam's own lock is held.
        let hook = AFTER_ACCESS_WRITE.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(hook) = hook {
            hook(context.config_file());
        }
    }
    let (revoked, mut failures) = reconcile_access_change(context, interaction_gate, before, after, why, pending.as_deref());
    if failures.is_empty() {
        if let Err(error) = forget_access_change(context, &pending) {
            failures.push(format!("the finished revocation is still recorded as pending: {error}"));
        }
    }
    (revoked, failures)
}

/// #192: finish every recorded access change whose revocation did not complete: a crash between its configuration
/// write and its revocation, or a step that failed. Each entry revokes by its own recorded policies and mode, so a
/// later re-grant cannot hide the conversations it removed: they get a fresh epoch even when admitted again. An entry
/// is removed only when its revocation fully succeeded. Returns the revoked keys and the failures. The caller holds
/// the closed-agent interaction gate.
fn finish_pending_revocations_locked(
    context: &RuntimeContext,
    _interaction_gate: &std::fs::File,
) -> Result<(Vec<String>, Vec<String>), String> {
    if !context.is_isolated() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut pending = pending_revocations(context)?;
    if pending.is_empty() && recorded_stops(context)?.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    // #192: every entry carries an id (a hand-written one may not), so its targets can be recorded against it.
    if pending.iter().any(|entry| !entry["id"].is_string()) {
        for entry in pending.iter_mut().filter(|entry| !entry["id"].is_string()) {
            entry["id"] = json!(crate::turn_ledger::mint_id("revoke-"));
        }
        write_pending_revocations(context, &pending)?;
    }
    // Retired DSH sessions are stopped with the DSH settings in force now.
    let current = context.load_config().unwrap_or_else(|_| json!({}));
    let mut revoked = Vec::new();
    let mut failures = Vec::new();
    let mut finished: Vec<String> = Vec::new();
    for entry in &pending {
        let id = entry["id"].as_str().unwrap_or_default();
        let why = entry["why"].as_str().unwrap_or("access-revoked").to_string();
        let mode = entry["mode"].as_str().map(str::to_string).unwrap_or_else(|| runtime_mode(context));
        let targets = entry_targets(entry);
        let (keys, entry_failures) =
            reconcile_policies(context, &entry["before"], &entry["after"], &mode, &why, &current, Some(id), &targets);
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
    // The record is read again: the attempts above recorded their targets (and stops) in it.
    if !finished.is_empty() {
        let mut left = pending_revocations(context)?;
        left.retain(|entry| !entry["id"].as_str().is_some_and(|id| finished.iter().any(|done| done == id)));
        write_pending_revocations(context, &left).map_err(|e| format!("the finished revocations are still recorded as pending: {e}"))?;
    }
    // #192: every recorded DSH stop that has not finished runs again in the background (one already running in this
    // process, such as those the loop above just started, is skipped).
    for (key, session) in recorded_stops(context)? {
        stop_recorded_session(context, &current, &key, &session);
    }
    Ok((revoked, failures))
}

/// Before an access change, under its gate: finish the earlier changes' revocations. The change is refused with 503,
/// and nothing written, while one of them cannot finish, so a re-grant never revives an epoch that was due to end.
pub(super) fn finish_pending_before_change(context: &RuntimeContext, interaction_gate: &std::fs::File) -> Result<(), (u16, String)> {
    let (_, failures) = finish_pending_revocations_locked(context, interaction_gate).map_err(|e| (503, e))?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err((503, format!("an earlier access change's revocation did not finish, nothing was written: {}", failures.join("; "))))
    }
}

/// #192: console start finishes what an access change cut off between its configuration write and its revocation
/// left (also one made by `her-cli conversation`). Takes the closed-agent interaction gate (five seconds at most) only
/// when an entry is recorded: an entry written meanwhile belongs to a change that holds the gate and finishes it itself.
pub(super) fn finish_pending_revocations(context: &RuntimeContext) -> Result<(Vec<String>, Vec<String>), String> {
    if !context.is_isolated()
        || (pending_revocations(context).is_ok_and(|pending| pending.is_empty())
            && recorded_stops(context).is_ok_and(|stops| stops.is_empty()))
    {
        return Ok((Vec::new(), Vec::new()));
    }
    let interaction = crate::closed_agent::lock(context)?;
    finish_pending_revocations_locked(context, &interaction)
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
    // #154: `security.interceptNotify` and `security.outboundGuard` are security settings: a change of one needs
    // `"confirm": true` (409 otherwise, nothing written), is validated by the registry (400) and is stored through the
    // settings store; the file keeps its registry members as the store wrote them back and gets the other members.
    let confirm = crate::settings::confirm_field(&body)?;
    let _gate = acquire_social_state_gate(context)?;
    let unchanged = read_config_object(context)?;
    let mut file = unchanged.clone();
    crate::settings::mirror_into(context, &mut file).map_err(|e| (503, e))?;
    let previous_view = security_view(&file);
    let mut next = file.get("security").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut fields = body.as_object().cloned().unwrap_or_default();
    fields.remove("confirm");
    next.extend(fields);
    if let Some(value) = next.get("interceptNotify").cloned() {
        if !value.is_boolean() {
            next.insert("interceptNotify".into(), json!(js_truthy(&value)));
        }
    }
    file["security"] = Value::Object(next.clone());
    let changes = crate::settings::console_changes(context, &unchanged, &file)?;
    let changed: Vec<&crate::settings::Setting> = changes.iter().map(|(setting, _)| *setting).collect();
    crate::settings::check_confirmation(confirm, &changed)?;
    if !changes.is_empty() {
        // The store's write-back refuses a file the runtime cannot load: refuse it before the store takes a change.
        raw_config_for_write(context)?;
        note_config_before_request(context);
    }
    crate::settings::apply_changes(context, changes, "administrator", "console")?;
    keep_stored_members(context, &mut file)?;
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
    social_v2_wake_with(context, cfg, body, &|_| true)
}

/// `social_v2_wake` plus an authority check run under the social-state gate against the current configuration
/// (ticket 33: a Schedule page run now cannot commit after the console credential rotated).
pub(super) fn social_v2_wake_with(
    context: &RuntimeContext,
    cfg: &Value,
    body: &str,
    still_authorized: &dyn Fn(&Value) -> bool,
) -> Result<Value, (u16, String)> {
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
        if !still_authorized(&fresh) {
            return Err((401, "console credential was revoked".to_string()));
        }
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
