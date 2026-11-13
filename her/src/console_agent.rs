//! Agent-facing console surface (ticket 14): the global agent-call gate, the in-memory read
//! receipts, the group-member check and the group poke.
//!
//! This module is a child of `console.rs` (`#[path]`), so it uses that file's private helpers. It
//! ports the JS bridge's central agent middleware (`bridge.js`, around 3405-3500) and the
//! handlers whose JS code has no equivalent in the older Rust console.

use super::*;
use std::collections::{BTreeSet, HashMap as StdHashMap};
use std::sync::Mutex;

// ----- read receipts (JS `modelSeenSeqs`) -----------------------------------------------------------------

/// Receipts of one conversation, valid only for the session token (epoch) they were given under.
type ReceiptMap = StdHashMap<(String, String), (String, BTreeSet<i64>)>;

fn receipt_store() -> &'static Mutex<ReceiptMap> {
    static STORE: std::sync::OnceLock<Mutex<ReceiptMap>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn receipt_scope(context: &RuntimeContext) -> String {
    if context.is_isolated() {
        context.data_dir().to_string_lossy().to_string()
    } else {
        "legacy".to_string()
    }
}

/// Token epoch of a conversation record ("" when it has no token).
fn epoch_of(conv: &Value) -> String {
    conv.get("agentToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(crate::send_outbox::token_epoch)
        .unwrap_or_default()
}

/// Seqs the model has been offered in this process under the conversation's current token. They are
/// deliberately not stored in `social-v2.json`: after a restart the model must fetch unread messages
/// again (bridge.js `readThroughSeqV2` comment), so a persisted receipt could authorise skipping unseen
/// messages. A token rotation (cancelled, replaced or reset turn) voids them as well.
pub(super) fn receipts_of(context: &RuntimeContext, key: &str, epoch: &str) -> Vec<i64> {
    let store = receipt_store().lock().unwrap_or_else(|p| p.into_inner());
    match store.get(&(receipt_scope(context), key.to_string())) {
        Some((stored_epoch, set)) if stored_epoch == epoch => set.iter().copied().collect(),
        _ => Vec::new(),
    }
}

fn set_receipts(context: &RuntimeContext, key: &str, epoch: &str, seqs: Vec<i64>) {
    let mut store = receipt_store().lock().unwrap_or_else(|p| p.into_inner());
    let slot = (receipt_scope(context), key.to_string());
    if seqs.is_empty() {
        store.remove(&slot);
    } else {
        store.insert(slot, (epoch.to_string(), seqs.into_iter().collect()));
    }
}

/// Exact urgent receipts belong to one turn; a new turn replaces the small seq set.
fn urgent_delivery_store() -> &'static Mutex<ReceiptMap> {
    static STORE: std::sync::OnceLock<Mutex<ReceiptMap>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn receipt_turn(context: &RuntimeContext, key: &str, conv: &Value) -> Option<String> {
    let records = crate::turn_ledger::read_records(context).ok()?;
    let after_reset = records.iter().rposition(|r| crate::turn_ledger::is_reset_of(r,key)).map_or(0,|i|i+1);
    // A late old-turn read cannot acquire the replacement conversation's reused seqs.
    let (turns, _) = crate::turn_ledger::fold(&records[after_reset..]);
    let epoch = epoch_of(conv);
    turns.iter().rev().find(|t| t.key == key && t.state.is_open() && t.epochs.contains(&epoch)).map(|t| t.id.clone())
}

fn note_urgent_delivery(context: &RuntimeContext, key: &str, conv: &Value, offered: &[Value]) {
    let urgent: BTreeSet<_> = offered.iter()
        .filter(|m| m["atSelf"] == true || m["isOwner"] == true)
        .filter_map(|m| js_safe_integer(&m["seq"]).filter(|seq| *seq > 0)).collect();
    if urgent.is_empty() { return; }
    let Some(turn) = receipt_turn(context, key, conv) else { return };
    let mut store = urgent_delivery_store().lock().unwrap_or_else(|p| p.into_inner());
    let slot = store.entry((receipt_scope(context), key.to_string())).or_default();
    if slot.0 != turn { *slot = (turn, BTreeSet::new()); }
    slot.1.extend(urgent);
}

/// The live bridge and console share state, but delivery receipts belong to the console process.
pub(super) fn wake_snapshot(context: &RuntimeContext, cfg: &Value, body: &str) -> Result<Value, (u16, String)> {
    let input = parse_object_body(body)?;
    let key = body_text(&input, "key");
    let token = body_text(&input, "token");
    let state = load_social_state_for(context)?;
    if current_mode_for(context)? != "reserved2" || state["paused"] == true
        || cfg.pointer("/socialV2/enabled") == Some(&json!(false)) || !key_allowed(cfg, &key)
        || token.is_empty() || !agent_token_ok_for(context, &key, &token)? {
        return Err((403, "wake snapshot invalidated".into()));
    }
    let conv = &state["conversations"][&key];
    if input.get("readOnly") == Some(&json!(true)) {
        let turn = body_text(&input, "turnId");
        let store = urgent_delivery_store().lock().unwrap_or_else(|p|p.into_inner());
        let delivered: Vec<_> = store.get(&(receipt_scope(context), key.clone()))
            .filter(|(id, _)| !turn.is_empty() && id == &turn)
            .map(|(_, seqs)| seqs.iter().copied().collect()).unwrap_or_default();
        return Ok(json!({"urgentDeliveredSeqs":delivered}));
    }
    let Some(mut packet) = build_wake_snapshot(context, cfg, &key, conv) else {
        return Ok(Value::Null);
    };
    let unread = conv["unread"].as_array().cloned().unwrap_or_default();
    note_offered_receipts(context, &key, conv, &unread, packet["messages"].as_array().unwrap());
    packet["readThroughSeq"] = json!(through_of(context, &key, conv));
    Ok(packet)
}

/// JS buildWakeSnapshotV2 selection, shared with the tool-free CLI renderer.
/// Computing a packet does not grant tool read receipts; the authenticated route does that.
pub(crate) fn build_wake_snapshot(context: &RuntimeContext, cfg: &Value, key: &str, conv: &Value) -> Option<Value> {
    if cfg.pointer("/socialV2/context/inlineWakeMessages") == Some(&json!(false))
        || (!tool_enabled(cfg, "getUnread") && !tool_enabled(cfg, "getRecent")) {
        return None;
    }
    let unread = conv["unread"].as_array().cloned().unwrap_or_default();
    let bounded = |name: &str, fallback: f64, min: f64, max: f64| {
        // JS Number(context[name] ?? fallback); retain coerced zero until clamping.
        cfg.pointer(&format!("/socialV2/context/{name}")).filter(|v| !v.is_null()).map(social_v1_number)
            .filter(|n| n.is_finite()).unwrap_or(fallback).round().clamp(min, max) as usize
    };
    let max_chars = bounded("wakeMaxChars", 6000.0, 500.0, 20000.0);
    let max_unread = bounded("wakeMessageLimit", 12.0, 1.0, 100.0);
    let max_recent = bounded("wakeRecentLimit", 4.0, 0.0, 20.0);
    let mut packet = json!({"messages":[],"recent":[],"unreadCount":unread.len(),"includedUnreadCount":0,"readThroughSeq":through_of(context,&key,conv),"partial":!unread.is_empty()});
    let candidate_through = |packet: &Value| {
        let mut offered = conv.clone();
        let mut seen = receipts_of(context, &key, &epoch_of(conv));
        for field in ["messages", "recent"] {
            seen.extend(packet[field].as_array().unwrap().iter().filter_map(|m| js_safe_integer(&m["seq"])));
        }
        offered["modelSeenSeqs"] = json!(seen);
        read_through_seq(&offered)
    };
    let add = |field: &str, message: &Value, packet: &mut Value| {
        let previous = packet["readThroughSeq"].clone();
        packet[field].as_array_mut().unwrap().push(crate::model_view::compact_model_message(message));
        packet["includedUnreadCount"] = json!(packet["messages"].as_array().unwrap().len());
        packet["partial"] = json!(packet["messages"].as_array().unwrap().len() < unread.len());
        packet["readThroughSeq"] = json!(candidate_through(packet));
        if packet.to_string().encode_utf16().count() <= max_chars { return true; }
        packet[field].as_array_mut().unwrap().pop();
        packet["includedUnreadCount"] = json!(packet["messages"].as_array().unwrap().len());
        packet["partial"] = json!(packet["messages"].as_array().unwrap().len() < unread.len());
        packet["readThroughSeq"] = previous;
        false
    };
    if tool_enabled(cfg, "getUnread") {
        for message in unread.iter().take(max_unread) { if !add("messages", message, &mut packet) { break; } }
    }
    if tool_enabled(cfg, "getRecent") {
        let recent: Vec<_> = conv["recentMessages"].as_array().into_iter().flatten()
            .filter(|m| js_safe_integer(&m["seq"]).is_none() || !unread.iter().any(|u| u["seq"] == m["seq"]))
            .collect();
        for message in recent.iter().rev().take(max_recent) { if !add("recent", message, &mut packet) { break; } }
        packet["recent"].as_array_mut().unwrap().reverse();
    }
    Some(packet)
}

/// Put this process's receipts into a conversation value (replacing anything the file carried).
pub(super) fn attach_receipts(context: &RuntimeContext, key: &str, conv: &mut Value) {
    let epoch = epoch_of(conv);
    if let Some(obj) = conv.as_object_mut() {
        obj.insert("modelSeenSeqs".to_string(), json!(receipts_of(context, key, &epoch)));
    }
}

/// `attach_receipts` for the conversation `key` inside a whole state.
pub(super) fn attach_state_receipts(context: &RuntimeContext, state: &mut Value, key: &str) {
    if let Some(conv) = state.get_mut("conversations").and_then(|c| c.get_mut(key)) {
        attach_receipts(context, key, conv);
    }
}

/// Remove stored receipts from a state that was read or is about to be written (older builds persisted them).
pub(super) fn strip_receipts(state: &mut Value) {
    if let Some(conversations) = state.get_mut("conversations").and_then(Value::as_object_mut) {
        for conv in conversations.values_mut() {
            if let Some(obj) = conv.as_object_mut() {
                obj.remove("modelSeenSeqs");
            }
        }
    }
}

/// JS `noteModelMessagesV2` bookkeeping: receipts are bounded by the unread buffer, only seqs that
/// are in it count. `conv` is the conversation record (its token decides which session the receipts
/// belong to).
pub(super) fn note_receipts(context: &RuntimeContext, key: &str, conv: &Value, unread: &[Value], offered: &[Value]) {
    note_urgent_delivery(context, key, conv, offered);
    note_offered_receipts(context, key, conv, unread, offered);
}

fn note_offered_receipts(context: &RuntimeContext, key: &str, conv: &Value, unread: &[Value], offered: &[Value]) {
    let epoch = epoch_of(conv);
    // JS bounds existing receipts by raw unread membership, even for a legacy unsafe i64.
    // Safe-integer validation applies only when adding a newly offered receipt below.
    let unread_seqs: BTreeSet<i64> = unread.iter().filter_map(|m| {
        m.get("seq").and_then(|seq| seq.as_i64().or_else(|| js_safe_integer(seq)))
    }).collect();
    let mut seen: BTreeSet<i64> = receipts_of(context, key, &epoch).into_iter().filter(|s| unread_seqs.contains(s)).collect();
    for message in offered {
        if let Some(seq) = message.get("seq").and_then(js_safe_integer) {
            if seq > 0 && unread_seqs.contains(&seq) {
                seen.insert(seq);
            }
        }
    }
    set_receipts(context, key, &epoch, seen.into_iter().collect());
}

// ----- the central agent gate -------------------------------------------------------------------------------

fn is_live_agent_token(context: &RuntimeContext, token: &str) -> Result<bool, (u16, String)> {
    let state = load_social_state_for(context)?;
    let Some(conversations) = state.get("conversations").and_then(Value::as_object) else {
        return Ok(false);
    };
    Ok(conversations.values().any(|conv| {
        let expected = conv.get("agentToken").and_then(Value::as_str).unwrap_or("");
        !expected.is_empty()
            && expected.len() == token.len()
            && expected.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }))
}

const ADMIN_ONLY_V2_PATHS: [&str; 6] = [
    "/api/socialV2/config",
    "/api/socialV2/activity",
    "/api/socialV2/reset",
    "/api/socialV2/states",
    "/api/socialV2/wake",
    "/api/socialV2/initial-prompt",
];

/// Routes an agent session token may reach (bridge.js `agentAllowed`).
fn agent_surface(path: &str) -> bool {
    path == "/api/status"
        || path.starts_with("/api/socialV2/")
        || path.starts_with("/api/memory/")
        || path.starts_with("/api/send/")
        || path.starts_with("/api/images/")
        || path == "/api/authorize/read"
}

/// Everything the JS server checks, in its order, for a request that declares itself an agent call
/// (`x-agent-call`) or carries an `x-agent-token`. Runs after the console token check.
pub(super) fn agent_global_denial(req: &Request, cfg: &Value, context: &RuntimeContext) -> Option<(u16, String)> {
    let path = req.path.as_str();
    let token = req.headers.get("x-agent-token");
    let token_present = token.map(|t| !t.is_empty()).unwrap_or(false);
    if req.headers.contains_key("x-agent-call") {
        if token_present {
            match is_live_agent_token(context, token.unwrap()) {
                Ok(true) => {}
                Ok(false) => return Some((403, "未授权：智能体调用携带的会话令牌无效".to_string())),
                Err(error) => return Some(error),
            }
        } else if !(path == "/api/status" || path.starts_with("/api/send/") || path.starts_with("/api/images/")) {
            return Some((403, "未授权：该接口的智能体调用必须携带有效的会话令牌".to_string()));
        }
    }
    let token = token?;
    if token.is_empty() {
        return Some((403, "agent token 不能为空".to_string()));
    }
    let v2_surface = path.starts_with("/api/socialV2/") || path.starts_with("/api/send/") || path.starts_with("/api/images/");
    if v2_surface {
        match load_social_state_for(context) {
            Ok(state) => {
                if state.get("paused").and_then(Value::as_bool) == Some(true) {
                    return Some((403, "AI 已暂停，当前不允许执行 v2 工具".to_string()));
                }
            }
            Err(error) => return Some(error),
        }
        if cfg.get("socialV2").and_then(|s| s.get("enabled")).and_then(Value::as_bool) == Some(false) {
            return Some((403, "二代模式已关闭，当前不允许执行 v2 工具".to_string()));
        }
    }
    if path.starts_with("/api/socialV2/") {
        match current_mode_for(context) {
            Ok(mode) if mode == "reserved2" => {}
            Ok(_) => return Some((403, "该接口仅 reserved2 模式可用".to_string())),
            Err(error) => return Some(error),
        }
    }
    let admin_only = ADMIN_ONLY_V2_PATHS.contains(&path)
        || (path == "/api/socialV2/feedback" && req.method == "GET")
        || (path == "/api/socialV2/tool-log" && req.method == "GET")
        || path == "/api/stickers"
        || path.starts_with("/api/stickers/");
    if admin_only || !agent_surface(path) {
        return Some((403, "该接口仅控制台可用".to_string()));
    }
    None
}

/// Routes that follow the standard social-v2 policy for agent tokens (handlers do the per-key checks).
pub(super) fn is_agent_policy_route(req: &Request) -> bool {
    matches!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", "/api/socialV2/mark-read")
            | ("POST", "/api/socialV2/wake-config")
            | ("POST", "/api/socialV2/wait")
            | ("POST", "/api/socialV2/send-burst")
            | ("POST", "/api/socialV2/send-poke")
            | ("POST", "/api/socialV2/feedback")
            | ("GET", "/api/socialV2/message-detail")
    )
}

pub(super) fn is_send_route(req: &Request) -> bool {
    req.method == "POST" && matches!(req.path.as_str(), "/api/send/group" | "/api/send/private" | "/api/send/reply")
}

// ----- JS helpers ------------------------------------------------------------------------------------------

/// `String(value ?? '').trim()` for an optional body member.
pub(super) fn body_text(body: &Value, name: &str) -> String {
    match body.get(name) {
        None | Some(Value::Null) => String::new(),
        Some(v) => social_policy_js_string(v).trim_matches(social_policy_js_trim_char).to_string(),
    }
}

/// A `(status, message)` error as a JSON reply; messages that already are JSON objects pass through.
pub(super) fn error_value(error: (u16, String)) -> (u16, Value) {
    let (status, message) = error;
    if message.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(&message) {
            return (status, value);
        }
    }
    (status, json!({ "ok": false, "error": message }))
}

/// bridge.js `/^(group|private):(\d+)$/` then `Number(id)`: (kind, id text, id) when valid and positive.
pub(super) fn parse_key(key: &str) -> Option<(&str, &str, i64)> {
    let (kind, id_s) = key.split_once(':')?;
    if (kind != "group" && kind != "private") || id_s.is_empty() || !id_s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = id_s.parse::<f64>().ok().filter(|n| n.is_finite() && *n > 0.0)? as i64;
    Some((kind, id_s, id))
}

pub(super) const KEY_FORMAT_ERROR: &str = "key 格式应为 group:群号 或 private:QQ号";

/// JS `modeAllowed`: closed-agent serves only the owner's private chat, the other modes the allow-lists.
pub(super) fn mode_allowed(cfg: &Value, mode: &str, key: &str) -> bool {
    if mode == "closed-agent" {
        let owner = cfg.get("ownerQQ").map(social_policy_js_string).unwrap_or_default();
        return key == format!("private:{owner}");
    }
    social_v2_session_allowed(cfg, key)
}

// ----- group poke -----------------------------------------------------------------------------------------

/// JS `assertGroupMember`: Some(reason) when the target is not provably a member (fail closed).
fn assert_group_member(ob: &crate::onebot::OneBot, group_id: i64, user_id: &str) -> Option<String> {
    let reply = match ob.request("get_group_member_list", json!({ "group_id": group_id })) {
        Some(r) => r,
        None => return Some(format!("无法获取群 {group_id} 的成员列表：OneBot get_group_member_list 请求失败")),
    };
    if reply.get("status").and_then(Value::as_str) != Some("ok") || reply.get("retcode").and_then(Value::as_i64) != Some(0) {
        let retcode = reply.get("retcode").map(social_policy_js_string).unwrap_or_else(|| "undefined".to_string());
        let wording = reply.get("wording").and_then(Value::as_str).unwrap_or("");
        return Some(format!("无法获取群 {group_id} 的成员列表：OneBot get_group_member_list 失败: retcode={retcode} {wording}"));
    }
    let data = reply.get("data").cloned().unwrap_or(Value::Null);
    let members: Vec<Value> = match &data {
        Value::Array(a) => a.clone(),
        other => other.get("data").and_then(Value::as_array).cloned().unwrap_or_default(),
    };
    let ids: Vec<String> = members
        .iter()
        .map(|m| m.get("user_id").filter(|v| !v.is_null()).map(social_policy_js_string).unwrap_or_default())
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() {
        return Some(format!("群 {group_id} 的成员列表为空，无法校验拍一拍目标"));
    }
    if ids.iter().any(|id| id == user_id) {
        None
    } else {
        Some(format!("QQ {user_id} 不是群 {group_id} 的成员，已拒绝拍一拍"))
    }
}

/// `POST /api/socialV2/send-poke` (bridge.js 5529). The group member check, the quota reservation and the
/// pre-send validity check run before the poke leaves; a refused or failed poke gives its quota back.
pub(super) fn send_poke(
    context: &RuntimeContext,
    cfg: &Value,
    headers: &HashMap<String, String>,
    body: &str,
) -> (u16, Value) {
    match send_poke_inner(context, cfg, headers, body) {
        Ok(reply) => reply,
        Err(error) => error_value(error),
    }
}

fn send_poke_inner(
    context: &RuntimeContext,
    cfg: &Value,
    headers: &HashMap<String, String>,
    body: &str,
) -> Result<(u16, Value), (u16, String)> {
    let b = parse_object_body(body)?;
    let key = body_text(&b, "key");
    let target = {
        let t = body_text(&b, "targetUserId");
        if b.get("targetUserId").map(|v| !v.is_null()).unwrap_or(false) { t } else { body_text(&b, "userId") }
    };
    if key.is_empty() {
        return Err((400, "key 不能为空".to_string()));
    }
    if current_mode_for(context)? != "reserved2" {
        return Err((403, "该接口仅 reserved2 模式可用".to_string()));
    }
    let token = headers.get("x-agent-token").cloned().unwrap_or_default();
    if token.is_empty() {
        return Err((403, "reserved2 模式发送拍一拍必须携带 agent token".to_string()));
    }
    if !agent_token_ok_for(context, &key, &token)? {
        return Err((403, "agent token 无效".to_string()));
    }
    if !social_v2_projection_session_allowed(cfg, &key) {
        return Err((403, "目标不在当前模式允许范围内".to_string()));
    }
    if !tool_enabled(cfg, "sendPoke") {
        return Err((403, "工具未启用：qq_send_poke".to_string()));
    }
    let state = load_social_state_for(context)?;
    if state.get("paused").and_then(Value::as_bool) == Some(true) {
        return Err((403, "二代 AI 已暂停，不能发送拍一拍".to_string()));
    }
    let Some((kind, id_s, id)) = parse_key(&key) else {
        return Err((400, KEY_FORMAT_ERROR.to_string()));
    };
    if !mode_allowed(cfg, "reserved2", &key) {
        return Err((403, "当前模式不允许操作该会话".to_string()));
    }
    if silent_blocks_send(context, cfg, &key)? {
        return Err((403, "静默模式已开启，当前不允许拍一拍".to_string()));
    }
    if kind == "group" && target.is_empty() {
        return Err((400, "群聊拍一拍必须指定 targetUserId（要拍的群友 QQ 号）".to_string()));
    }
    let target_ok = target.as_bytes().first().map(|b| (b'1'..=b'9').contains(b)).unwrap_or(false) && target.bytes().all(|b| b.is_ascii_digit());
    if !target.is_empty() && !target_ok {
        return Err((400, "targetUserId 必须是正整数 QQ 号".to_string()));
    }
    let ob = onebot_for_runtime(context, cfg).map_err(|error| (503, error))?;
    if kind == "group" {
        if let Some(reason) = assert_group_member(&ob, id, &target) {
            return Err((403, reason));
        }
    }
    let storage_key = canonical_v2_key(&key).ok_or((400, format!("无效的会话 key：{key}")))?;
    let epoch = crate::send_outbox::token_epoch(&token);
    // The token was accepted before the blocking member lookup. A revocation during
    // that lookup cancels this send before reserving quota, just like a later guard.
    send_guard_for(context, cfg, &storage_key, &epoch, GuardPolicy::Reserved2, "sendPoke")
        .map_err(|(status, message)| (status, format!("拍一拍失败：{message}")))?;
    let now = now_ms();
    reserve_send_quota(context, cfg, &storage_key, 1, now, &epoch)?;
    // The member check was asynchronous in JS and the operator may have changed mode, allow-list or silence
    // meanwhile: confirm once more right before the request.
    let outcome: Result<(), (u16, String)> = (|| {
        send_guard_for(context, cfg, &storage_key, &epoch, GuardPolicy::Reserved2, "sendPoke").map_err(|(_, m)| (500, m))?;
        if silent_blocks_send(context, cfg, &key)? {
            return Err((403, "静默模式已开启，当前不允许拍一拍".to_string()));
        }
        let (action, params) = if kind == "group" {
            ("group_poke", json!({ "group_id": id, "user_id": target.parse::<f64>().unwrap_or(0.0) as i64 }))
        } else {
            ("send_poke", json!({ "user_id": id }))
        };
        let reply = ob.request(action, params).ok_or((500, format!("OneBot {action} 连接失败")))?;
        if reply.get("status").and_then(Value::as_str) != Some("ok") || reply.get("retcode").and_then(Value::as_i64) != Some(0) {
            let retcode = reply.get("retcode").map(social_policy_js_string).unwrap_or_else(|| "undefined".to_string());
            let wording = reply.get("wording").and_then(Value::as_str).unwrap_or("");
            return Err((500, format!("OneBot {action} 失败: retcode={retcode} {wording}")));
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            let poke_text = if kind == "group" { format!("[拍一拍] 我拍了拍 {target}") } else { "[拍一拍] 我拍了拍你".to_string() };
            let recent_limit = cfg
                .get("socialV2")
                .and_then(|s| s.get("context"))
                .and_then(|c| c.get("recentLimit"))
                .and_then(Value::as_f64)
                .filter(|n| *n != 0.0)
                .unwrap_or(100.0) as usize;
            let target_id = if target.is_empty() { id_s.to_string() } else { target.clone() };
            mutate_social_state_for(context, |state| {
                let conv = state
                    .get_mut("conversations")
                    .and_then(|c| c.get_mut(&storage_key))
                    .and_then(Value::as_object_mut)
                    .ok_or((404, format!("会话不存在：{storage_key}")))?;
                conv.insert("lastActionAt".to_string(), json!(now));
                conv.insert("lastAiReplyAt".to_string(), json!(now));
                if let Some(wake) = conv.get_mut("wakeConfig").and_then(Value::as_object_mut) {
                    wake.insert("noActionCount".to_string(), json!(0));
                }
                let record = json!({
                    "messageId": Value::Null, "sender": "我", "text": poke_text, "plain": poke_text, "tail": poke_text,
                    "kind": "poke", "quoteTargetIsSelf": false, "isOwner": true, "ownerLabel": "我", "isSelf": true,
                    "media": [], "hasMedia": false, "forwardIds": [], "hasForward": false,
                    "poke": { "targetId": target_id, "targetIsSelf": false, "groupId": if kind == "group" { json!(id_s) } else { Value::Null } },
                    "time": now_ms()
                });
                let recent = conv.entry("recentMessages".to_string()).or_insert_with(|| json!([]));
                if let Some(list) = recent.as_array_mut() {
                    list.push(record);
                    if list.len() > recent_limit {
                        let excess = list.len() - recent_limit;
                        list.drain(0..excess);
                    }
                }
                conv.insert("preSleepWaitSatisfiedAt".to_string(), json!(0));
                conv.insert("preSleepWaitObservedAt".to_string(), json!(0));
                conv.insert("preSleepWaitAccumMs".to_string(), json!(0));
                conv.insert("engagedAt".to_string(), json!(now_ms()));
                Ok(())
            })?;
            crate::safe_println!("[console] send-poke {key} 成功");
            crate::activity::append(
                context,
                &format!("{key} [reserved2] 工具拍一拍{}", if kind == "group" { format!(" -> {target}") } else { String::new() }),
            );
            Ok((200, json!({ "ok": true, "key": key, "kind": kind, "targetUserId": if target.is_empty() { id_s.to_string() } else { target } })))
        }
        Err((status, message)) => {
            release_send_quota(context, &storage_key, now, 1);
            let status = if (400..600).contains(&status) { status } else { 500 };
            Err((status, format!("拍一拍失败：{message}")))
        }
    }
}

// ----- sending: one execution core for send-message, send-burst and /api/send/* -----------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SendFlavor {
    /// `POST /api/socialV2/send-message`
    Message,
    /// `POST /api/socialV2/send-burst`
    Burst,
    /// `POST /api/send/{group,private,reply}`
    Direct,
}

pub(super) struct SendCall<'a> {
    pub key: &'a str,
    pub kind: &'a str,
    pub id_s: &'a str,
    pub token: &'a str,
    pub messages: Vec<String>,
    pub reply_ref: String,
    pub at_user: Option<i64>,
    pub gap_mode: &'a str,
    pub body: &'a Value,
    pub flavor: SendFlavor,
    pub policy: GuardPolicy,
    /// Quote already resolved by the caller (JS resolves it before the quota for `/api/send/*`).
    pub resolved_quote: Option<(Value, Option<i64>)>,
}

fn last_tool_send() -> &'static Mutex<StdHashMap<(String, String), i64>> {
    static LAST: std::sync::OnceLock<Mutex<StdHashMap<(String, String), i64>>> = std::sync::OnceLock::new();
    LAST.get_or_init(|| Mutex::new(StdHashMap::new()))
}

/// JS `dupSendBlock`: with `socialV2.send.duplicateWindowMs` > 0 a conversation takes one tool send per window.
pub(super) fn dup_send_block(context: &RuntimeContext, cfg: &Value, key: &str) -> Option<String> {
    duplicate_window(context, cfg, key, None)
}

fn claim_tool_send(context: &RuntimeContext, cfg: &Value, key: &str, at: i64) -> Option<String> {
    duplicate_window(context, cfg, key, Some(at))
}

fn release_unsent_tool_claim(context: &RuntimeContext, key: &str, at: i64) {
    let mut sends=last_tool_send().lock().unwrap_or_else(|p|p.into_inner());
    let scope=(receipt_scope(context),key.to_string());
    if sends.get(&scope)==Some(&at) {sends.remove(&scope);}
}

fn duplicate_window(context: &RuntimeContext, cfg: &Value, key: &str, mark: Option<i64>) -> Option<String> {
    let window = cfg
        .get("socialV2")
        .and_then(|s| s.get("send"))
        .and_then(|s| s.get("duplicateWindowMs"))
        .map(|v| match v {
            Value::Number(n) => n.as_f64().unwrap_or(0.0),
            Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        })
        .unwrap_or(0.0)
        .max(0.0);
    let mut sends = last_tool_send().lock().unwrap_or_else(|p| p.into_inner());
    let scope = (receipt_scope(context), key.to_string());
    let now = mark.unwrap_or_else(now_ms);
    let gap = now - sends.get(&scope).copied().unwrap_or(0);
    if window > 0.0 && (gap as f64) < window {
        return Some(format!("刚说过话了（{} 秒前），本轮别重复。要补充就合并进上一条。", ((gap as f64) / 1000.0).round() as i64));
    }
    if mark.is_some() {
        sends.insert(scope, now);
    }
    None
}

fn js_slice_200(text: &str) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(200).collect::<Vec<u16>>())
}

fn truthy_json(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        _ => true,
    }
}

/// JS `resolveReplyInfo`: the quoted message must demonstrably belong to this conversation.
pub(super) fn resolve_reply_info_js(ob: &crate::onebot::OneBot, kind: &str, conv_id: &str, message_id: &str) -> Option<Value> {
    reply_info_js(ob, kind, conv_id, message_id, crate::inbound_text::PREVIEW_UNITS as usize).map(|(info, _)| info)
}

/// `resolve_reply_info_js` with a caller-chosen text width; also returns the message's real length in UTF-16 units.
fn reply_info_js(ob: &crate::onebot::OneBot, kind: &str, conv_id: &str, message_id: &str, max_units: usize) -> Option<(Value, usize)> {
    let numeric = message_id
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0)? as i64;
    let reply = ob.request("get_msg", json!({ "message_id": numeric }))?;
    if reply.get("status").and_then(Value::as_str) != Some("ok") || reply.get("retcode").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let raw = reply.get("data").filter(|d| d.is_object())?.clone();
    let text_of = |v: Option<&Value>| v.filter(|x| !x.is_null()).map(social_policy_js_string);
    let raw_group = text_of(raw.get("group_id")).or_else(|| text_of(raw.get("groupId")));
    if kind == "group" {
        if raw_group? != conv_id {
            return None;
        }
    } else {
        if raw_group.is_some() {
            return None;
        }
        let raw_user = text_of(raw.get("user_id"))
            .or_else(|| text_of(raw.get("userId")))
            .or_else(|| text_of(raw.get("sender").and_then(|s| s.get("user_id"))))?;
        if raw_user != conv_id {
            return None;
        }
    }
    let sender_obj = raw.get("sender");
    let non_empty = |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let sender = non_empty(sender_obj.and_then(|s| s.get("card")))
        .or_else(|| non_empty(sender_obj.and_then(|s| s.get("nickname"))))
        .or_else(|| text_of(sender_obj.and_then(|s| s.get("user_id"))))
        .or_else(|| text_of(raw.get("user_id")))
        .unwrap_or_else(|| "未知".to_string());
    let text = reply_text_of(ob.clone(), kind, conv_id, &raw.get("message").cloned().unwrap_or(json!([])));
    let user_id = text_of(sender_obj.and_then(|s| s.get("user_id"))).or_else(|| text_of(raw.get("user_id")));
    let units: Vec<u16> = text.encode_utf16().collect();
    let shown = String::from_utf16_lossy(&units[..units.len().min(max_units)]);
    Some((json!({ "sender": sender, "text": shown, "userId": user_id }), units.len()))
}

/// JS `resolveReplyTargetV2`: a reference may be a QQ message id or a local `seq` of `recentMessages`.
pub(super) fn resolve_quote_js(
    context: &RuntimeContext,
    ob: &crate::onebot::OneBot,
    storage_key: &str,
    kind: &str,
    id_s: &str,
    reference: &str,
) -> Option<(Value, String)> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    let state = load_social_state_for(context).ok()?;
    let found = state
        .get("conversations")
        .and_then(|c| c.get(storage_key))
        .and_then(|c| c.get("recentMessages"))
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find(|m| m.get("seq").map(social_policy_js_string).as_deref() == Some(reference)).cloned());
    let from_local = |found: &Value, real: &str| {
        let text = found
            .get("text")
            .filter(|v| truthy_json(v))
            .or_else(|| found.get("plain"))
            .map(social_policy_js_string)
            .unwrap_or_default();
        json!({
            "sender": found.get("sender").filter(|v| truthy_json(v)).map(social_policy_js_string).unwrap_or_default(),
            "text": js_slice_200(&text),
            "userId": Value::Null,
            "messageId": real,
            "seq": found.get("seq").cloned().unwrap_or(Value::Null),
        })
    };
    let local_id = found.as_ref().and_then(|f| f.get("messageId")).filter(|v| truthy_json(v)).map(social_policy_js_string);
    if let (Some(f), Some(real)) = (&found, &local_id) {
        if real != reference {
            let info = resolve_reply_info_js(ob, kind, id_s, real).unwrap_or_else(|| from_local(f, real));
            return Some((info, real.clone()));
        }
    }
    if let Some(info) = resolve_reply_info_js(ob, kind, id_s, reference) {
        return Some((info, reference.to_string()));
    }
    if let (Some(f), Some(real)) = (&found, &local_id) {
        let info = resolve_reply_info_js(ob, kind, id_s, real).unwrap_or_else(|| from_local(f, real));
        return Some((info, real.clone()));
    }
    None
}

pub(super) const UNRESOLVED_QUOTE: &str = "无法解析被引用消息，请确认 message id 正确且属于当前会话（可用 qq_get_message_detail 查看）";

/// The process-wide send order (JS `sendChain`), owned by the recorded send.
pub(super) use crate::recorded_send::send_chain;

/// Everything after the request-specific validation: quota reservation (persisted before any request),
/// quote resolution, the durable intent record, item-by-item sending with the validity check before each
/// item, and settlement of the conversation state.
pub(super) fn execute_send(context: &RuntimeContext, cfg: &Value, call: &SendCall<'_>) -> Result<(u16, Value), (u16, String)> {
    use crate::send_outbox::token_epoch;
    let (key, kind, id_s, token) = (call.key, call.kind, call.id_s, call.token);
    let normalized: Vec<String> = call.messages.iter().map(|m| crate::outbound_message::normalize(m))
        .filter(|m| !m.is_empty()).collect();
    if normalized.is_empty() { return Err((400, "归一化后没有可发送的消息".into())); }
    let (messages, at_user, flavor, policy) = (&normalized, call.at_user, call.flavor, call.policy);
    let send_cfg = cfg.get("socialV2").and_then(|s| s.get("send")).cloned().unwrap_or_else(|| json!({}));
    let ob = onebot_for_runtime(context, cfg).map_err(|error| (503, error))?;
    let initial = load_social_state_for(context)?["conversations"][key].clone();
    let chain = send_chain();
    let turn_scope = if token.is_empty() { String::new() } else {
        repeat_guard::admit(context, key, messages)?
    };
    let clamp_gap = |ms: i64| -> i64 {
        let min = send_cfg.get("burstIntervalMinMs").and_then(|v| v.as_i64()).unwrap_or(1000);
        let max = send_cfg.get("maxGapMs").and_then(|v| v.as_i64()).unwrap_or(10000);
        ms.clamp(min.min(max), max.max(min))
    };
    let count = messages.len();
    let mut delays: Vec<i64> = Vec::new();
    if count > 1 {
        match call.gap_mode {
            "fixed" => {
                let gaps: Vec<i64> = call
                    .body
                    .get("gaps")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                    .unwrap_or_default();
                if gaps.len() >= count - 1 {
                    for g in gaps.iter().take(count - 1) {
                        delays.push(clamp_gap(*g));
                    }
                } else {
                    let g = clamp_gap(
                        call.body
                            .get("gapMs")
                            .and_then(|v| v.as_i64())
                            .or_else(|| send_cfg.get("burstIntervalMinMs").and_then(|v| v.as_i64()))
                            .unwrap_or(1000),
                    );
                    for _ in 0..count - 1 {
                        delays.push(g);
                    }
                }
            }
            "byLength" => {
                let base = send_cfg.get("gapBaseMs").and_then(|v| v.as_i64()).unwrap_or(800);
                let per = send_cfg.get("gapPerCharMs").and_then(|v| v.as_i64()).unwrap_or(20);
                for m in messages.iter().take(count - 1) {
                    let chars = m.chars().count().max(1) as i64;
                    delays.push(clamp_gap(base + chars * per));
                }
            }
            _ => {
                for _ in 0..count - 1 {
                    delays.push(clamp_gap(1000 + (now_ms() % 1000)));
                }
            }
        }
    }
    let now = now_ms();
    let epoch = if token.is_empty() { String::new() } else { token_epoch(token) };
    if flavor == SendFlavor::Message {
        // All checks passed and the send is about to start: the duplicate window opens now (JS markToolSend).
        if let Some(blocked) = claim_tool_send(context, cfg, key, now) {
            return Err((429, blocked));
        }
    }
    // 1. Persist the quota reservation before anything else can fail or send.
    reserve_send_quota(context, cfg, key, count, now, &epoch)?;
    // 2. Resolve the quote target. A refusal gives the reservation back; nothing was sent.
    let mut quoted = Value::Null;
    let mut actual_reply: Option<i64> = None;
    if let Some((info, id)) = call.resolved_quote.clone() {
        quoted = info;
        actual_reply = id;
    } else if !call.reply_ref.is_empty() {
        match resolve_quote_js(context, &ob, key, kind, id_s, &call.reply_ref) {
            Some((info, message_id)) => {
                actual_reply = message_id.trim().parse::<i64>().ok();
                quoted = info;
            }
            None => {
                release_send_quota(context, key, now, count);
                return Err((400, UNRESOLVED_QUOTE.to_string()));
            }
        }
    }
    // 3. Claim the batch and record the intent before the first OneBot request. The claim is
    // held until the batch settles, so a resume cannot run it alongside this send.
    let fields = json!({
        "key": key, "epoch": epoch, "messages": messages, "delays": delays,
        "replyTo": actual_reply, "atUser": at_user, "turnScope": turn_scope
    });
    let recorded = match crate::recorded_send::Recorded::begin(context, now_ms, now, fields) {
        Ok(recorded) => recorded,
        Err(error) => {
            release_send_quota(context, key, now, count);
            return Err(match error {
                crate::recorded_send::BeginError::Busy => (409, crate::recorded_send::BATCH_BUSY.to_string()),
                crate::recorded_send::BeginError::Intent(error) => (503, format!("发送意图未能持久记录，未发送：{error}")),
            });
        }
    };
    let batch_id = recorded.batch_id().to_string();
    // 4. Send item by item, with durable attempt and outcome records.
    let indices: Vec<usize> = (0..count).collect();
    let plan = plan_for(messages, actual_reply, at_user, &indices);
    let tool_flag = match flavor {
        SendFlavor::Message => "sendMessage",
        SendFlavor::Burst => "sendBurst",
        SendFlavor::Direct if kind == "private" => "sendPrivate",
        SendFlavor::Direct if !call.reply_ref.is_empty() => "reply",
        SendFlavor::Direct => "sendGroup",
    };
    // JS `sendChain`: every outbound bubble (text and sticker) is serialized in enqueue order, and a
    // burst occupies the chain through its typing wait and gaps. Held until the items are sent;
    // quota settlement and state updates run after it is released.
    let report = run_send_items(context, cfg, &ob, &recorded, key, kind, id_s, &epoch, policy, tool_flag, &plan, &delays, &[], &initial, &turn_scope);
    drop(chain);
    // 5. Settle the reservation and the conversation state.
    let states = ordered_states(count, &report);
    let sent_texts = sent_texts_of(messages, &report);
    let sent = sent_texts.len();
    let release = report
        .states
        .iter()
        .filter(|(_, s)| matches!(s, crate::send_outbox::ItemState::NotCommitted | crate::send_outbox::ItemState::Cancelled | crate::send_outbox::ItemState::Superseded))
        .count();
    let full = sent == count;
    let targeting = json!({"replyToMessageId":actual_reply,"atUserId":at_user,"replyToUserId":quoted["userId"]});
    let targeted_sent = report.states.iter().any(|(idx,state)| *idx == 0 && matches!(state,crate::send_outbox::ItemState::Confirmed {..}));
    finish_send_batch(context, cfg, key, now, release, &sent_texts, full, targeted_sent.then_some(&targeting))
        .map_err(|(status, error)| (status, format!("已发送 {sent} 条消息，但运行时状态更新未能确认：{error}")))?;
    if let Some(error) = &report.discard_error {
        return Err((503, format!("批次已停止（已发 {sent} 条），{error}")));
    }
    if let Some(mut revision) = report.revision.clone() {
        if sent==0 && flavor==SendFlavor::Message {release_unsent_tool_claim(context,key,now);}
        revision["key"] = json!(key);
        revision["batchId"] = json!(batch_id);
        revision["sent"] = json!(sent);
        revision["progress"] = crate::send_outbox::progress_json(&batch_id, &states);
        reply_timing::publish_revision(context, key, &epoch, &mut revision)?;
        return Ok((200,revision));
    }
    if !full {
        let mut out = crate::send_outbox::progress_json(&batch_id, &states);
        out["ok"] = json!(false);
        out["error"] = json!(partial_error_message(&report, sent));
        out["key"] = json!(key);
        if flavor != SendFlavor::Burst {
            out["quoted"] = quoted;
        }
        if flavor == SendFlavor::Message {
            out["delays"] = json!(delays);
        }
        return Ok((500, out));
    }
    let mut out = match flavor {
        SendFlavor::Message => json!({ "ok": true, "key": key, "batchId": batch_id, "sent": sent, "failed": count - sent, "delays": delays, "quoted": quoted }),
        SendFlavor::Burst => json!({ "ok": true, "key": key, "batchId": batch_id, "sent": sent, "failed": count - sent }),
        SendFlavor::Direct => json!({ "ok": true, "key": key, "batchId": batch_id, "sent": sent, "failed": if sent > 0 { 0 } else { 1 }, "quoted": quoted }),
    };
    if flavor != SendFlavor::Direct {
        if count >= 3 {
            out["hint"] = json!("你已经连发了多条，确认是必要的吗？真人很少一口气补完。");
        }
        if let Some(w) = find_cjk_space_warning(messages) {
            out["warn"] = json!(w);
        }
        if let Some(w) = find_split_boundary_warning(messages) {
            out["splitWarn"] = json!(w);
        }
    }
    crate::safe_println!("[console] send {key}: 成功 {sent}/{count} 条");
    crate::activity::append(
        context,
        &match flavor {
            SendFlavor::Message => format!("{key} [reserved2] 工具统一发送：成功 {sent}/{count} 条"),
            SendFlavor::Burst => format!("{key} [reserved2] 工具分条发送：成功 {sent}/{count} 条"),
            SendFlavor::Direct => format!("{key} [send] 成功 {sent}/1 条：{}", crate::activity::slice(&messages[0], 80)),
        },
    );
    Ok((200, out))
}

pub(super) fn first_typing_delay(cfg: &Value, text: &str, elapsed: i64) -> i64 {
    let base = js_number_or(cfg.get("firstGapBaseMs").filter(|v| !v.is_null()).or_else(|| cfg.get("gapBaseMs")),1000.0);
    let per = js_number_or(cfg.get("firstGapPerCharMs").filter(|v| !v.is_null()).or_else(|| cfg.get("gapPerCharMs")),60.0);
    let min = js_number_or(cfg.get("burstIntervalMinMs"),300.0).max(100.0);
    let max = js_number_or(cfg.get("maxGapMs"),10000.0).max(min);
    let budget = (base + text.encode_utf16().count() as f64 * per).round().clamp(min,max) as i64;
    if cfg.get("fillTypingBudget") == Some(&json!(false)) { budget } else { budget.saturating_sub(elapsed).max(0) }
}

/// Message count, per-message length (UTF-16 units, as JS) and the outbound sensitive-text check.
pub(super) fn check_limits_and_text(cfg: &Value, send_cfg: &Value, messages: &[String], what: &str) -> Result<(), (u16, String)> {
    let max_msgs = send_cfg.get("burstMaxMessages").and_then(Value::as_f64).filter(|n| *n != 0.0).unwrap_or(8.0).max(1.0) as i64;
    let max_chars = send_cfg.get("maxMessageChars").and_then(Value::as_f64).filter(|n| *n != 0.0).unwrap_or(500.0).max(1.0) as i64;
    if messages.len() as i64 > max_msgs {
        return Err((400, format!("最多发送 {max_msgs} 条")));
    }
    for m in messages {
        if m.encode_utf16().count() as i64 > max_chars {
            return Err((400, format!("单条消息不能超过 {max_chars} 字")));
        }
        if let Some(why) = crate::pipeline::configured_outbound_violation(cfg, m, &[]) {
            crate::safe_println!("[console] {what} 被哨兵拦下：{why}");
            return Err((403, "消息含敏感信息，已阻止发送".to_string()));
        }
    }
    Ok(())
}

fn burst_inner(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> Result<(u16, Value), (u16, String)> {
    let b = parse_object_body(body)?;
    let key = body_text(&b, "key");
    let messages = parse_messages(&b.get("messages").cloned().unwrap_or(Value::Null));
    let reply_given = b
        .get("replyToMessageId")
        .map(|v| !v.is_null() && !social_policy_js_string(v).trim_matches(social_policy_js_trim_char).is_empty())
        .unwrap_or(false);
    if reply_given {
        return Err((400, "qq_send_burst 暂不支持引用，请使用 qq_reply".to_string()));
    }
    if key.is_empty() || messages.is_empty() {
        return Err((400, "key 和 messages 不能为空".to_string()));
    }
    let send_cfg = cfg.get("socialV2").and_then(|s| s.get("send")).cloned().unwrap_or_else(|| json!({}));
    let merge_into_one = send_cfg.get("mergeMultiIntoOne").and_then(Value::as_bool) != Some(false);
    if messages.len() > 1 && !merge_into_one && send_cfg.get("burstEnabled").and_then(Value::as_bool) == Some(false) {
        return Err((403, "已禁用多条发送，请合并为一条消息".to_string()));
    }
    // The frozen JS assigns to a `const` here and answers 500 "Assignment to constant variable." for any
    // multi-message burst while merging is on (a defect); the documented behaviour is to merge.
    let messages = merge_multi(messages, merge_into_one);
    let token = headers.get("x-agent-token").cloned().unwrap_or_default();
    if !token.is_empty() {
        if !agent_token_ok_for(context, &key, &token)? {
            return Err((403, "agent token 无效".to_string()));
        }
        if !social_v2_projection_session_allowed(cfg, &key) {
            return Err((403, "目标不在当前模式允许范围内".to_string()));
        }
        if !tool_enabled(cfg, "sendBurst") {
            return Err((403, "工具未启用：qq_send_burst".to_string()));
        }
    }
    if current_mode_for(context)? != "reserved2" {
        return Err((403, "该接口仅 reserved2 模式可用".to_string()));
    }
    if token.is_empty() {
        return Err((403, "reserved2 模式发送必须携带 agent token".to_string()));
    }
    let Some((kind, id_s, _)) = parse_key(&key) else {
        return Err((400, KEY_FORMAT_ERROR.to_string()));
    };
    if !mode_allowed(cfg, "reserved2", &key) {
        return Err((403, "目标不在当前模式允许范围内".to_string()));
    }
    if silent_blocks_send(context, cfg, &key)? {
        return Err((403, "静默模式已开启，当前不允许发送".to_string()));
    }
    check_limits_and_text(cfg, &send_cfg, &messages, "send-burst")?;
    let storage_key = canonical_v2_key(&key).ok_or((400, format!("无效的会话 key：{key}")))?;
    let call = SendCall {
        key: &storage_key,
        kind,
        id_s,
        token: &token,
        messages,
        reply_ref: String::new(),
        at_user: None,
        gap_mode: "auto",
        body: &b,
        flavor: SendFlavor::Burst,
        policy: GuardPolicy::Reserved2,
        resolved_quote: None,
    };
    let (status, mut out) = execute_send(context, cfg, &call)?;
    out["key"] = json!(key);
    Ok((status, out))
}

pub(super) fn send_burst(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> (u16, Value) {
    match burst_inner(context, cfg, headers, body) {
        Ok(reply) => reply,
        Err(error) => error_value(error),
    }
}

/// `POST /api/send/{group,private,reply}` (bridge.js 7172). The session token is read from the body, as JS does.
pub(super) fn send_direct(context: &RuntimeContext, cfg: &Value, path: &str, body: &str) -> (u16, Value) {
    match direct_inner(context, cfg, path, body) {
        Ok(reply) => reply,
        Err(error) => error_value(error),
    }
}

fn direct_inner(context: &RuntimeContext, cfg: &Value, path: &str, body: &str) -> Result<(u16, Value), (u16, String)> {
    let b = parse_object_body(body)?;
    let token = body_text(&b, "token");
    let is_private = path == "/api/send/private";
    let is_reply = path == "/api/send/reply";
    let target_id = body_text(&b, if is_private { "userId" } else { "groupId" });
    let message = {
        let raw = body_text(&b, "message");
        match serde_json::from_str::<Value>(raw.trim()) {
            Ok(Value::String(s)) if raw.trim().starts_with('"') => s,
            _ => raw,
        }
    };
    let reply_present = b
        .get("replyToMessageId")
        .map(|v| !v.is_null() && !social_policy_js_string(v).trim().is_empty())
        .unwrap_or(false);
    let reply_ref = if reply_present { body_text(&b, "replyToMessageId") } else { String::new() };
    let at_present = b.get("atUserId").map(truthy_json).unwrap_or(false);
    let at_user = b
        .get("atUserId")
        .filter(|v| !v.is_null())
        .and_then(|v| social_policy_js_string(v).trim().parse::<f64>().ok())
        .map(|n| n as i64);
    let key = format!("{}:{}", if is_private { "private" } else { "group" }, target_id);
    let mode = current_mode_for(context)?;
    if mode == "chat" || mode == "reserved" {
        return Err((403, "发送工具仅限 closed-agent / reserved2 模式使用".to_string()));
    }
    let state = load_social_state_for(context)?;
    if state.get("paused").and_then(Value::as_bool) == Some(true) && !token.is_empty() {
        return Err((403, "AI 已暂停，当前不允许执行发送工具".to_string()));
    }
    if target_id.is_empty() || message.is_empty() {
        return Err((400, "目标 id 和 message 不能为空".to_string()));
    }
    if is_private && at_present {
        return Err((400, "私聊不需要 @".to_string()));
    }
    if is_reply && !reply_present {
        return Err((400, "replyToMessageId 不能为空".to_string()));
    }
    if mode == "reserved2" && token.is_empty() {
        return Err((403, "reserved2 模式发送必须携带 agent token".to_string()));
    }
    if !token.is_empty() && !agent_token_ok_for(context, &key, &token)? {
        return Err((403, "agent token 无效".to_string()));
    }
    let flag = if is_private { "sendPrivate" } else if is_reply { "reply" } else { "sendGroup" };
    if !token.is_empty() && !tool_enabled(cfg, flag) {
        return Err((403, format!("工具未启用：{flag}")));
    }
    if silent_blocks_send(context, cfg, &key)? {
        return Err((403, "静默模式已开启，当前不允许发送".to_string()));
    }
    let Some((kind, id_s, _)) = parse_key(&key) else {
        return Err((400, "key 格式无效".to_string()));
    };
    if !mode_allowed(cfg, &mode, &key) {
        return Err((403, "目标不在当前模式允许范围内".to_string()));
    }
    if reply_present {
        let digits = reply_ref.strip_prefix('-').unwrap_or(&reply_ref);
        if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err((400, "replyToMessageId 必须是非零整数（消息 id 可能为负数）".to_string()));
        }
    }
    let storage_key = canonical_v2_key(&key).ok_or((400, "key 格式无效".to_string()))?;
    let ob = onebot_for_runtime(context, cfg).map_err(|error| (503, error))?;
    let mut resolved_quote = None;
    if reply_present {
        let Some((info, real)) = resolve_quote_js(context, &ob, &storage_key, kind, id_s, &reply_ref) else {
            return Err((400, UNRESOLVED_QUOTE.to_string()));
        };
        resolved_quote = Some((info, real.trim().parse::<i64>().ok()));
    }
    let send_cfg = cfg.get("socialV2").and_then(|s| s.get("send")).cloned().unwrap_or_else(|| json!({}));
    let max_chars = send_cfg.get("maxMessageChars").and_then(Value::as_f64).filter(|n| *n != 0.0).unwrap_or(500.0).max(1.0) as i64;
    if message.encode_utf16().count() as i64 > max_chars {
        return Err((400, format!("单条消息不能超过 {max_chars} 字")));
    }
    if let Some(why) = crate::pipeline::configured_outbound_violation(cfg, &message, &[]) {
        crate::safe_println!("[console] send 被哨兵拦下：{why}");
        return Err((403, "消息含敏感信息，已阻止发送".to_string()));
    }
    if token.is_empty() {
        // Closed-agent (administrator chat) sends carry no session token; JS creates the conversation lazily.
        mutate_social_state_for(context, |state| {
            if state.get("conversations").and_then(|c| c.get(&storage_key)).is_none() {
                if !state.get("conversations").map(Value::is_object).unwrap_or(false) {
                    state["conversations"] = json!({});
                }
                state["conversations"][&storage_key] = new_conversation(context);
            }
            Ok(())
        })?;
    }
    let call = SendCall {
        key: &storage_key,
        kind,
        id_s,
        token: &token,
        messages: vec![message],
        reply_ref: String::new(),
        at_user,
        gap_mode: "auto",
        body: &b,
        flavor: SendFlavor::Direct,
        policy: GuardPolicy::Reserved2OrClosedAgent,
        resolved_quote,
    };
    let (status, mut out) = execute_send(context, cfg, &call)?;
    out["key"] = json!(key);
    Ok((status, out))
}

// ----- read marks and wake configuration -----------------------------------------------------------------

/// JS `readThroughSeqV2` for a conversation read from the file: the receipts of this process are put in
/// first (the file never carries them).
pub(super) fn through_of(context: &RuntimeContext, key: &str, conv: &Value) -> i64 {
    let mut with_receipts = conv.clone();
    attach_receipts(context, key, &mut with_receipts);
    read_through_seq(&with_receipts)
}

/// JS `getSocialV2State(key)`: the conversation record, created with defaults when the key is new.
fn ensure_conversation(context: &RuntimeContext, state: &mut Value, key: &str) {
    if !state.get("conversations").map(Value::is_object).unwrap_or(false) {
        state["conversations"] = json!({});
    }
    if state["conversations"].get(key).is_none() {
        state["conversations"][key] = new_conversation(context);
    }
}

fn force_short_rewake(wake: &mut Value, now: i64) {
    wake["infinite"] = json!(false);
    wake["sleepUntil"] = json!(crate::sticker::iso_at(now.saturating_add(30_000)));
}

fn pre_sleep_error(context: &RuntimeContext, key: &str, cfg: &Value, conv: &Value, now: i64, what: &str) -> Option<(u16, String)> {
    let remaining = crate::wake::pre_sleep_remaining(conv, cfg, now)?;
    // The observation the prompt asks for; the reserved2 turn budget covers it.
    let pre_sleep_ms = crate::wake::prompt_pre_sleep_wait_ms(cfg);
    let used = match wait_usage(context, key, conv) {
        Ok((_, used)) => used,
        Err(error) => return Some(error),
    };
    // Exhausted turns may close; the handlers install a short finite re-wake below.
    if used >= wait_policy(cfg).max_per_turn { return None; }
    let remain_min = (remaining as f64 / 60_000.0).ceil() as i64;
    let error = if what == "mark_read" {
        format!("还不能通过 qq_mark_read 直接回到潜水：还需等待约 {remain_min} 分钟无新消息，或调用 qq_wait_for_messages(timeoutMs={pre_sleep_ms}) 完成一次沉睡前观察。如果等待期间有人发新消息，请先查看返回的 newMessages；判断不需要你参与就可以直接收尾沉睡，若你参与了则需下次再等观察窗口。")
    } else {
        format!("还不能立刻设置潜水/下一次唤醒：还需等待约 {remain_min} 分钟无新消息，或调用 qq_wait_for_messages(timeoutMs={pre_sleep_ms}) 完成一次沉睡前观察。如果等待期间有人发新消息，请先查看返回的 newMessages；判断不需要你参与就可以直接设置并沉睡，若你参与了则需下次再等观察窗口。")
    };
    Some((
        400,
        json!({ "ok": false, "error": error, "preSleepWaitMs": pre_sleep_ms, "preSleepWaitRemainingMs": remaining }).to_string(),
    ))
}

/// JS `requestedReadThroughSeqV2`: Ok(None) when the request has no `throughSeq` member at all.
fn requested_through(body: &Value, conv: &Value, through_now: i64, is_agent: bool) -> Result<Option<i64>, (u16, String)> {
    let Some(raw) = body.get("throughSeq") else { return Ok(None) };
    let last_unread = conv.get("lastUnreadSeq").and_then(js_safe_integer).unwrap_or(0);
    let max = if is_agent { through_now } else { last_unread };
    match js_safe_integer(raw) {
        Some(seq) if seq >= 0 && seq <= max && seq <= last_unread => Ok(Some(seq)),
        _ => Err((
            400,
            "throughSeq 必须是不超过已展示连续消息水位的非负安全整数；请先读取未读消息并使用返回的 readThroughSeq".to_string(),
        )),
    }
}

/// JS `acknowledgeMessagesV2`: drops the acknowledged unread messages and moves the persistent mark.
fn acknowledge(context: &RuntimeContext, key: &str, conv: &mut Value, through: Option<i64>) -> usize {
    let before = conv.get("unread").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let last_unread = conv.get("lastUnreadSeq").and_then(js_safe_integer).unwrap_or(0);
    match through {
        None => {
            conv["unread"] = json!([]);
            conv["lastReadThroughSeq"] = json!(last_unread);
        }
        Some(seq) => {
            let kept: Vec<Value> = conv
                .get("unread")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter(|m| m.get("seq").and_then(js_safe_integer).map(|s| s > seq).unwrap_or(true)).cloned().collect())
                .unwrap_or_default();
            conv["unread"] = json!(kept);
            let previous = conv.get("lastReadThroughSeq").and_then(js_safe_integer).unwrap_or(0);
            conv["lastReadThroughSeq"] = json!(previous.max(seq));
        }
    }
    let unread = conv.get("unread").and_then(Value::as_array).cloned().unwrap_or_default();
    note_receipts(context, key, conv, &unread, &[]);
    before - unread.len()
}

fn agent_header_token(headers: &HashMap<String, String>) -> Option<&str> {
    headers.get("x-agent-token").map(String::as_str)
}

fn note_acknowledged_urgent(context: &RuntimeContext, before: Option<&Value>, body: &str, reply: &Value) {
    // A configuration-only call or failed write is not an acknowledgement. Use the
    // requested prefix, never the response cursor (which can skip retained gaps).
    if reply["markedCount"].as_u64().unwrap_or(0) == 0 { return; }
    let Some(state) = before else { return };
    let key = reply["key"].as_str().unwrap_or("");
    let conv = &state["conversations"][key];
    let input: Value = serde_json::from_str(body).unwrap_or_default();
    let through = js_safe_integer(&input["throughSeq"])
        .unwrap_or_else(|| conv["lastUnreadSeq"].as_i64().unwrap_or(-1));
    let acknowledged: Vec<_> = conv["unread"].as_array().into_iter().flatten()
        .filter(|m| js_safe_integer(&m["seq"]).is_some_and(|seq| seq <= through)).cloned().collect();
    note_urgent_delivery(context, key, conv, &acknowledged);
}

pub(super) fn mark_read(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> (u16, Value) {
    let before = load_social_state_for(context).ok();
    match mark_read_inner(context, cfg, headers, body) {
        Ok(reply) => {
            if agent_header_token(headers).is_some_and(|token| !token.is_empty()) {
                note_acknowledged_urgent(context, before.as_ref(), body, &reply);
            }
            (200, reply)
        },
        Err(error) => error_value(error),
    }
}

fn mark_read_inner(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> Result<Value, (u16, String)> {
    let b = parse_object_body(body)?;
    let key = body_text(&b, "key");
    if key.is_empty() {
        return Err((400, "key 不能为空".to_string()));
    }
    let agent_token = agent_header_token(headers);
    let (_, is_agent) = social_projection_request_state(context, cfg, &key, agent_token, "markRead", "qq_mark_read")?;
    let storage_key = projection_storage_key(&key)?;
    let auth = agent_token.map(|token| SocialProjectionWriteAuth { token, tool_flag: "markRead", tool_name: "qq_mark_read" });
    mutate_social_state_for(context, |state| {
        // The token is checked again under the state gate: a token rotated while the request waited
        // (a cancelled or replaced turn) must not be able to acknowledge anything.
        if let Some(auth) = auth {
            social_projection_write_denial(context, state, &storage_key, auth)?;
        }
        ensure_conversation(context, state, &storage_key);
        let conv = &mut state["conversations"][&storage_key];
        let now = now_ms();
        let through_now = through_of(context, &storage_key, conv);
        let (_, wait_used) = wait_usage(context, &storage_key, conv)?;
        let wait_max = wait_policy(cfg).max_per_turn;
        let exhausted = wait_used >= wait_max;
        let short_rewake = exhausted && crate::wake::pre_sleep_remaining(conv, cfg, now).is_some();
        let through = requested_through(&b, conv, through_now, is_agent).map_err(|(status, error)| {
            (status, if exhausted { format!("{error} {END_TURN}") } else { error })
        })?;
        // Back to diving right after talking is refused until the pre-sleep observation was done
        // (private chats are exempt, as in JS).
        let wake = conv.get("wakeConfig").cloned().unwrap_or_else(|| json!({}));
        if !storage_key.starts_with("private:") && crate::wake::is_sleeping_config(&wake) {
            if let Some(error) = pre_sleep_error(context, &storage_key, cfg, conv, now, "mark_read") {
                return Err(error);
            }
        }
        let marked = acknowledge(context, &storage_key, conv, through);
        conv["lastActionAt"] = json!(now);
        let mut wc = conv.get("wakeConfig").cloned().unwrap_or_else(|| json!({}));
        wc["noActionCount"] = json!(0);
        if wc.get("infinite").and_then(Value::as_bool) != Some(true) && wc.get("sleepUntil").map(Value::is_null).unwrap_or(true) {
            wc["infinite"] = json!(true);
        }
        crate::wake::ensure_wakeable(&mut wc, cfg, now, rand01());
        wc["confirmedAt"] = json!(now);
        wc["confirmedBy"] = json!("mark_read");
        if short_rewake { force_short_rewake(&mut wc, now); }
        conv["wakeConfig"] = wc.clone();
        let unread_count = conv.get("unread").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        let read_through = through_of(context, &storage_key, conv);
        let safety = wake_safety(&wc, now);
        let mut out = json!({
            "ok": true, "key": key, "markedCount": marked, "unreadCount": unread_count, "readThroughSeq": read_through,
            "wakeGuaranteed": safety["guaranteed"].clone(), "wakeSafety": safety, "wakeConfig": wc,
        });
        if exhausted { note_wait_budget(&mut out, wait_used, wait_max); }
        if short_rewake { out["forcedRewakeMs"] = json!(30_000); }
        Ok(out)
    })
}

pub(super) fn wake_config(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> (u16, Value) {
    let before = load_social_state_for(context).ok();
    match wake_config_inner(context, cfg, headers, body) {
        Ok(reply) => {
            if agent_header_token(headers).is_some_and(|token| !token.is_empty()) {
                note_acknowledged_urgent(context, before.as_ref(), body, &reply);
            }
            (200, reply)
        },
        Err(error) => error_value(error),
    }
}

fn wake_config_inner(context: &RuntimeContext, cfg: &Value, headers: &HashMap<String, String>, body: &str) -> Result<Value, (u16, String)> {
    let b = parse_object_body(body)?;
    let key = body_text(&b, "key");
    if key.is_empty() {
        return Err((400, "key 不能为空".to_string()));
    }
    let agent_token = agent_header_token(headers);
    let (_, is_agent) = social_projection_request_state(context, cfg, &key, agent_token, "setWakeConfig", "qq_set_wake_config")?;
    let storage_key = projection_storage_key(&key)?;
    let auth = agent_token.map(|token| SocialProjectionWriteAuth { token, tool_flag: "setWakeConfig", tool_name: "qq_set_wake_config" });
    mutate_social_state_for(context, |state| {
        if let Some(auth) = auth {
            social_projection_write_denial(context, state, &storage_key, auth)?;
        }
        ensure_conversation(context, state, &storage_key);
        let conv = &mut state["conversations"][&storage_key];
        let now = now_ms();
        let through_now = through_of(context, &storage_key, conv);
        let (_, wait_used) = wait_usage(context, &storage_key, conv)?;
        let wait_max = wait_policy(cfg).max_per_turn;
        let exhausted = wait_used >= wait_max;
        let short_rewake = exhausted && crate::wake::pre_sleep_remaining(conv, cfg, now).is_some();
        let through = requested_through(&b, conv, through_now, is_agent).map_err(|(status, error)| {
            (status, if exhausted { format!("{error} {END_TURN}") } else { error })
        })?;
        // With the markRead tool switched off the mark is ignored and only the configuration is set.
        let acknowledge_on_close = through.is_some() && tool_enabled(cfg, "markRead");
        let current = conv.get("wakeConfig").cloned().unwrap_or_else(|| json!({}));
        let input = b.get("config").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        let next = crate::wake::normalize_wake_config(&current, &input, cfg, &storage_key, now, rand01())?;
        if !storage_key.starts_with("private:") && crate::wake::is_sleeping_config(&next) {
            if let Some(error) = pre_sleep_error(context, &storage_key, cfg, conv, now, "wake_config") {
                return Err(error);
            }
        }
        let marked = if acknowledge_on_close { acknowledge(context, &storage_key, conv, through) } else { 0 };
        let mut wc = next.clone();
        if let Some(obj) = wc.as_object_mut() {
            obj.insert("lastWakeAt".into(), obj.get("lastWakeAt").filter(|v| truthy_json(v)).cloned().unwrap_or(json!(0)));
            obj.insert("wakeCount".into(), obj.get("wakeCount").filter(|v| truthy_json(v)).cloned().unwrap_or(json!(0)));
            obj.insert("noActionCount".into(), json!(0));
            obj.insert("confirmedAt".into(), json!(now));
            obj.insert("confirmedBy".into(), json!("set_wake_config"));
        }
        if short_rewake { force_short_rewake(&mut wc, now); }
        conv["wakeConfig"] = wc.clone();
        conv["lastActionAt"] = json!(now);
        conv["preSleepWaitSatisfiedAt"] = json!(0);
        conv["preSleepWaitObservedAt"] = json!(0);
        conv["preSleepWaitAccumMs"] = json!(0);
        let unread_count = conv.get("unread").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        let read_through = through_of(context, &storage_key, conv);
        let mut out = json!({
            "ok": true, "key": key, "markedCount": marked, "unreadCount": unread_count, "readThroughSeq": read_through,
            "wakeConfig": wc.clone(), "wakeSafety": wake_safety(&wc, now),
        });
        if exhausted { note_wait_budget(&mut out, wait_used, wait_max); }
        if short_rewake { out["forcedRewakeMs"] = json!(30_000); }
        Ok(out)
    })
}

// ----- message detail ------------------------------------------------------------------------------------

/// `GET /api/socialV2/message-detail?key=&messageId=` (bridge.js 6534): one message by QQ message id or local seq.
pub(super) fn message_detail(
    context: &RuntimeContext,
    cfg: &Value,
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
) -> (u16, Value) {
    match message_detail_inner(context, cfg, headers, query) {
        Ok(reply) => (200, reply),
        Err(error) => error_value(error),
    }
}

fn message_detail_inner(
    context: &RuntimeContext,
    cfg: &Value,
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
) -> Result<Value, (u16, String)> {
    let key = query.get("key").map(|s| s.trim_matches(social_policy_js_trim_char).to_string()).unwrap_or_default();
    let message_id = query.get("messageId").map(|s| s.trim_matches(social_policy_js_trim_char).to_string()).unwrap_or_default();
    if key.is_empty() || message_id.is_empty() {
        return Err((400, "key 和 messageId 不能为空".to_string()));
    }
    let (state, _) = social_projection_request_state(context, cfg, &key, agent_header_token(headers), "getMessageDetail", "qq_get_message_detail")?;
    let Some((kind, id_s_raw, _)) = parse_key(&key) else {
        return Err((400, KEY_FORMAT_ERROR.to_string()));
    };
    let storage_key = projection_storage_key(&key)?;
    let id_s = id_s_raw.parse::<f64>().map(|n| (n as i64).to_string()).unwrap_or_else(|_| id_s_raw.to_string());
    let found = state
        .get("conversations")
        .and_then(|c| c.get(&storage_key))
        .and_then(|c| c.get("recentMessages"))
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find(|m| {
                    m.get("seq").map(social_policy_js_string).as_deref() == Some(message_id.as_str())
                        || (m.get("messageId").map(truthy_json).unwrap_or(false)
                            && m.get("messageId").map(social_policy_js_string).as_deref() == Some(message_id.as_str()))
                })
                .cloned()
        });
    // A message longer than its preview keeps `fullText` (bounded at store time); older or shorter
    // records fall back to the 200-unit preview.
    let preview_of = |m: &Value| js_slice_200(&m.get("text").filter(|v| truthy_json(v)).or_else(|| m.get("plain")).map(social_policy_js_string).unwrap_or_default());
    let local_full = |m: &Value| m.get("fullText").and_then(Value::as_str).map(str::to_string);
    let local_info = |m: &Value, message_id_value: Value| {
        json!({
            "sender": m.get("sender").filter(|v| truthy_json(v)).map(social_policy_js_string).unwrap_or_default(),
            "text": local_full(m).unwrap_or_else(|| preview_of(m)),
            "userId": m.get("userId").filter(|v| truthy_json(v)).map(|v| json!(social_policy_js_string(v))).unwrap_or(Value::Null),
            "messageId": message_id_value,
            "seq": m.get("seq").cloned().unwrap_or(Value::Null),
        })
    };
    let found_real_id = found.as_ref().and_then(|m| m.get("messageId")).filter(|v| truthy_json(v)).map(social_policy_js_string);
    let cap = crate::inbound_text::full_text_cap(cfg) as usize;
    let mut info: Option<Value> = None;
    // Real length of the message when known, for `textChars` / `textTruncated`.
    let mut total_chars: Option<usize> = None;
    match (&found, &found_real_id) {
        (Some(m), Some(real)) if *real != message_id => info = Some(local_info(m, json!(real))),
        (Some(m), _) if local_full(m).is_some() => {
            info = Some(local_info(m, found_real_id.clone().map(Value::from).unwrap_or(Value::Null)));
        }
        _ => {
            let ob = onebot_for_runtime(context, cfg).map_err(|error| (503, error))?;
            if let Some((remote, chars)) = reply_info_js(&ob, kind, &id_s, &message_id, cap) {
                info = Some(remote);
                total_chars = Some(chars);
            }
            if info.is_none() {
                if let Some(m) = &found {
                    info = Some(local_info(m, found_real_id.clone().map(Value::from).unwrap_or(Value::Null)));
                }
            }
        }
    }
    if let (Some(obj), Some(m)) = (info.as_mut(), found.as_ref()) {
        obj["forwardIds"] = if m.get("forwardIds").map(Value::is_array).unwrap_or(false) { m["forwardIds"].clone() } else { json!([]) };
        obj["hasForward"] = json!(m.get("hasForward").map(truthy_json).unwrap_or(false));
        if local_full(m).is_some() {
            total_chars = m.get("textChars").and_then(Value::as_u64).map(|n| n as usize);
        }
    }
    if let Some(obj) = info.as_mut() {
        let shown = obj.get("text").and_then(Value::as_str).map(|t| t.encode_utf16().count()).unwrap_or(0);
        // Without a known real length the 200-unit preview rule applies, as before.
        let truncated = match total_chars {
            Some(total) => total > shown,
            None => shown >= crate::inbound_text::PREVIEW_UNITS as usize,
        };
        obj["textTruncated"] = json!(truncated);
        if let Some(total) = total_chars {
            obj["textChars"] = json!(total);
        }
    }
    Ok(json!({ "ok": true, "key": key, "messageId": message_id, "info": info }))
}

// ----- dispatch -------------------------------------------------------------------------------------------

/// JS `Number(value)` of an optional member (`undefined` is NaN).
pub(super) fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => parse_js_number(s.trim_matches(social_policy_js_trim_char)),
        Some(_) => f64::NAN,
    }
}

/// JS `Number(value) || fallback`.
pub(super) fn js_number_or(value: Option<&Value>, fallback: f64) -> f64 {
    let n = match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => parse_js_number(s.trim_matches(social_policy_js_trim_char)),
        Some(_) => f64::NAN,
    };
    if n.is_nan() || n == 0.0 { fallback } else { n }
}

/// The agent routes of this module; `None` for every other request.
pub(super) fn route(req: &Request, cfg: &Value, context: &RuntimeContext) -> Option<(u16, Value)> {
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/socialV2/mark-read") => Some(mark_read(context, cfg, &req.headers, &req.body)),
        ("POST", "/api/socialV2/wake-config") => Some(wake_config(context, cfg, &req.headers, &req.body)),
        ("POST", "/api/socialV2/wait") => Some(match handle_wait_post(context, cfg, &req.headers, &req.body) {
            Ok(value) => (200, value),
            Err(error) => error_value(error),
        }),
        ("GET", "/api/socialV2/message-detail") => Some(message_detail(context, cfg, &req.headers, &req.query)),
        ("POST", "/api/socialV2/send-burst") => Some(send_burst(context, cfg, &req.headers, &req.body)),
        ("POST", "/api/socialV2/send-poke") => Some(send_poke(context, cfg, &req.headers, &req.body)),
        ("POST", path @ ("/api/send/group" | "/api/send/private" | "/api/send/reply")) => Some(send_direct(context, cfg, path, &req.body)),
        _ => None,
    }
}

/// `GET /api/status`: the real mode (JS `currentMode`); an agent call only gets the minimal object, never the
/// administrator fields (owner, allow-lists, activity).
pub(super) fn finish_status(req: &Request, context: &RuntimeContext, cfg: &Value, mut payload: Value) -> Value {
    let mode = current_mode_for(context).unwrap_or_else(|_| "reserved2".to_string());
    let role = read_role_state(context).unwrap_or_else(|_| json!({}));
    let role_name = role.get("role").cloned().unwrap_or(Value::Null);
    let role_mode = role.get("mode").cloned().unwrap_or_else(|| json!("active"));
    if req.headers.contains_key("x-agent-token") || req.headers.contains_key("x-agent-call") {
        // JS checkDsh uses settings.describe success, not socket reachability or credentials alone.
        let dsh_ready = dsh_rpc_for(context, cfg)
            .and_then(|rpc| rpc.call("settings/describe", json!({})))
            .is_ok();
        return json!({
            "mode": mode, "role": role_name, "roleMode": role_mode,
            "dshReady": dsh_ready,
            "socialV2Paused": payload.get("socialV2Paused").cloned().unwrap_or(json!(false)),
        });
    }
    payload["mode"] = json!(mode);
    payload["role"] = role_name;
    payload["roleMode"] = role_mode;
    // Administrator-only: the summary carries other conversations' text.
    payload["activity"] = json!(crate::activity::tail(context, 100));
    // JS `dshReady` and `snowlumaHealthSnapshot()`: the live bridge's own view, published beside its heartbeat. Without a
    // fresh snapshot (no live bridge is running) nothing is known: not ready (JS starts `dshReady` false until the first
    // check passes) and an unknown gateway. The administrator status never calls DSH itself.
    let (dsh_ready, snowluma) = crate::dsh_health::published_status(context)
        .unwrap_or_else(|| (false, crate::gateway_health::unknown_json()));
    payload["dshReady"] = json!(dsh_ready);
    payload["snowluma"] = snowluma;
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hf129_fix3_urgent_tool_evidence_is_scoped_to_one_turn() {
        let root = std::env::temp_dir().join(crate::turn_ledger::mint_id("rbridge-urgent-turn-"));
        for dir in ["config","data","resources","user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let cfg = json!({"allow":{"groups":[1]},"consoleToken":"synthetic-turn-evidence"});
        std::fs::write(root.join("config/config.json"),cfg.to_string()).unwrap();
        std::fs::write(root.join("data/mode.json"),r#"{"mode":"reserved2"}"#).unwrap();
        let key = "group:1";
        let conv = json!({"agentToken":"same-token","unread":[{"seq":1,"atSelf":true}]});
        std::fs::write(root.join("data/social-v2.json"),json!({"conversations":{key:conv}}).to_string()).unwrap();
        std::fs::write(root.join("dependencies.json"),r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let begin = |id, at| crate::turn_ledger::append_record(&context,&json!({"k":"begun","turn":id,"key":key,
            "epoch":crate::send_outbox::token_epoch("same-token"),"inbound":[],"reason":"atMention","t":at})).unwrap();
        begin("one",1);
        note_receipts(&context,key,&conv,conv["unread"].as_array().unwrap(),conv["unread"].as_array().unwrap());
        let query = |id| wake_snapshot(&context,&cfg,&json!({"key":key,"token":"same-token","turnId":id,"readOnly":true}).to_string()).unwrap();
        assert_eq!(query("one")["urgentDeliveredSeqs"],json!([1]));
        crate::turn_ledger::append_record(&context,&json!({"k":"ended","turn":"one","key":key,"outcome":"completed","t":2})).unwrap();
        begin("two",3);
        assert_eq!(query("two")["urgentDeliveredSeqs"],json!([]),"prior tool evidence cannot cover a later turn");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wake_snapshot_count_limits_follow_js_number_coercion() {
        // Pure selection: the legacy context supplies only the in-memory receipt scope.
        let context = RuntimeContext::legacy();
        let conv = json!({
            "agentToken": "wake-number-count-test",
            "unread": (1..=105).map(|seq| json!({"seq":seq})).collect::<Vec<_>>(),
            "recentMessages": (201..=225).map(|seq| json!({"seq":seq})).collect::<Vec<_>>(),
            "lastUnreadSeq":105, "lastReadThroughSeq":0
        });
        // Expected counts from bridge.js buildWakeSnapshotV2's bounded/Number expression.
        for (value, unread_count, recent_count) in [
            (json!("2"), 2, 2),
            (json!("0"), 1, 0),
            (json!(""), 1, 0),
            (json!(" 12 "), 12, 12),
            (json!("abc"), 12, 4),
            (json!("NaN"), 12, 4),
            (json!("Infinity"), 12, 4),
            (json!("-Infinity"), 12, 4),
            (json!("1e309"), 12, 4),
            (json!(null), 12, 4),
            (json!(true), 1, 1),
            (json!(false), 1, 0),
            (json!("1.5"), 2, 2),
            (json!("-2"), 1, 0),
            (json!("250"), 100, 20),
            (json!("0x10"), 16, 16),
            (json!("0b10"), 2, 2),
            (json!("0o10"), 8, 8),
            (json!("1.2e1"), 12, 12),
            (json!("\u{feff}\u{2028}12\u{a0}"), 12, 12),
            (json!([]), 1, 0),
            (json!(["2"]), 2, 2),
            (json!({}), 12, 4),
        ] {
            let cfg = json!({"socialV2":{"context":{
                "wakeMessageLimit":value, "wakeRecentLimit":value, "wakeMaxChars":20000
            }}});
            let packet = build_wake_snapshot(&context, &cfg, "private:wake-number-count", &conv).unwrap();
            assert_eq!(packet["messages"].as_array().unwrap().len(), unread_count, "unread limit {value}");
            assert_eq!(packet["recent"].as_array().unwrap().len(), recent_count, "recent limit {value}");
            assert_eq!(packet["messages"][0]["seq"], 1);
            assert_eq!(packet["readThroughSeq"], unread_count);
            assert_eq!(packet["partial"], true);
            if recent_count > 0 {
                assert_eq!(packet["recent"][0]["seq"], 226 - recent_count);
                assert_eq!(packet["recent"][recent_count - 1]["seq"], 225);
            }
        }
        let missing = build_wake_snapshot(&context, &json!({}), "private:wake-number-count", &conv).unwrap();
        assert_eq!(missing["includedUnreadCount"], 12);
        assert_eq!(missing["recent"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn wake_snapshot_character_limit_follows_js_number_coercion() {
        let context = RuntimeContext::legacy();
        let conv = json!({
            "agentToken":"wake-number-budget-test",
            "unread": (1..=3).map(|seq| json!({"seq":seq,"text":"x".repeat(200)})).collect::<Vec<_>>(),
            "recentMessages":[], "lastUnreadSeq":3, "lastReadThroughSeq":0
        });
        for (value, included, budget) in [
            (json!("500"), 1, 500),
            (json!("0"), 1, 500),
            (json!(""), 1, 500),
            (json!(" 12 "), 1, 500),
            (json!(" 700 "), 2, 700),
            (json!("abc"), 3, 6000),
            (json!("NaN"), 3, 6000),
            (json!("Infinity"), 3, 6000),
            (json!("-Infinity"), 3, 6000),
            (json!("1e309"), 3, 6000),
            (json!(null), 3, 6000),
            (json!(true), 1, 500),
            (json!(false), 1, 500),
        ] {
            let cfg = json!({"socialV2":{"context":{"wakeMaxChars":value,"wakeRecentLimit":"0"}}});
            let packet = build_wake_snapshot(&context, &cfg, "private:wake-number-budget", &conv).unwrap();
            assert_eq!(packet["includedUnreadCount"], included, "character limit {value}");
            assert_eq!(packet["readThroughSeq"], included, "character limit {value}");
            assert_eq!(packet["partial"], included < 3, "character limit {value}");
            assert!(packet.to_string().encode_utf16().count() <= budget, "character limit {value}");
        }
    }

    #[test]
    fn note_receipts_retains_buffered_legacy_unsafe_process_receipts() {
        // This exercises only the process map: no legacy config or data is read or written.
        let context = RuntimeContext::legacy();
        let key = "private:14";
        let unsafe_seq = 9_007_199_254_740_992_i64;
        let mut conv = json!({
            "agentToken": "ticket-14-in-memory-numeric-receipt-test",
            "unread": [{ "seq": 1.0 }, { "seq": unsafe_seq }, { "seq": 2 }],
            "lastUnreadSeq": 2,
            "lastReadThroughSeq": 0
        });
        let epoch = epoch_of(&conv);
        set_receipts(&context, key, &epoch, vec![1, unsafe_seq, 3]);
        let unread = conv["unread"].as_array().unwrap();
        note_receipts(&context, key, &conv, unread, &[
            json!({ "seq": unsafe_seq }), json!({ "seq": 1.5 }),
            json!({ "seq": 0 }), json!({ "seq": 4 }),
        ]);
        assert_eq!(receipts_for(&context, key, &conv), vec![1, unsafe_seq],
            "JS retains matching existing process receipts, including unsafe ones, and prunes evicted receipts");
        attach_receipts(&context, key, &mut conv);
        assert_eq!(read_through_seq(&conv), 1, "an unseen buffered safe seq stops the cursor");

        let unread = conv["unread"].as_array().unwrap();
        note_receipts(&context, key, &conv, unread, &[json!({ "seq": 2.0 })]);
        attach_receipts(&context, key, &mut conv);
        assert_eq!(receipts_for(&context, key, &conv), vec![1, 2, unsafe_seq]);
        assert_eq!(read_through_seq(&conv), 2);

        note_receipts(&context, key, &conv, &[json!({ "seq": 2 })], &[]);
        assert_eq!(receipts_for(&context, key, &conv), vec![2]);
        conv["agentToken"] = json!("ticket-14-rotated-numeric-receipt-test");
        assert_eq!(receipts_for(&context, key, &conv), Vec::<i64>::new());
        note_receipts(&context, key, &conv, &[json!({ "seq": unsafe_seq })], &[json!({ "seq": unsafe_seq })]);
        assert_eq!(receipts_for(&context, key, &conv), Vec::<i64>::new(),
            "an unsafe seq cannot create a new receipt, including after token rotation");
    }

    /// The outbound sensitive-text filter must give the verdict of the frozen JS `SENSITIVE_RE` on every case of
    /// `tests/fixtures/sensitive-corpus.json`, the recorded sensitive-text regression corpus.
    #[test]
    fn sensitive_filter_matches_the_js_corpus() {
        let corpus: Value = serde_json::from_str(include_str!("../tests/fixtures/sensitive-corpus.json")).expect("corpus is JSON");
        let cases = corpus["cases"].as_array().expect("cases");
        assert!(cases.len() > 2000);
        let mut wrong: Vec<String> = Vec::new();
        for case in cases {
            let text = case["text"].as_str().expect("text");
            let expected = case["sensitive"].as_bool().expect("verdict");
            let got = crate::pipeline::outbound_violation(text, &[]).is_some();
            if got != expected {
                wrong.push(format!("{text:?}: js={expected} rust={got}"));
            }
        }
        assert!(wrong.is_empty(), "{} of {} cases differ, first: {:#?}", wrong.len(), cases.len(), &wrong[..wrong.len().min(12)]);
    }

    #[test]
    fn js_number_or_follows_number_or_fallback() {
        assert_eq!(js_number_or(None, 7.0), 7.0);
        assert_eq!(js_number_or(Some(&json!(null)), 7.0), 7.0);
        assert_eq!(js_number_or(Some(&json!("12")), 7.0), 12.0);
        assert_eq!(js_number_or(Some(&json!("x")), 7.0), 7.0);
        assert_eq!(js_number_or(Some(&json!(0)), 7.0), 7.0);
        assert_eq!(js_number_or(Some(&json!(true)), 7.0), 1.0);
        assert_eq!(js_number_or(Some(&json!([])), 7.0), 7.0);
    }

    #[test]
    fn keys_parse_like_the_js_regexp() {
        assert_eq!(parse_key("group:123"), Some(("group", "123", 123)));
        assert_eq!(parse_key("private:0123"), Some(("private", "0123", 123)));
        assert_eq!(parse_key("group:0"), None);
        assert_eq!(parse_key("group:-1"), None);
        assert_eq!(parse_key("friend:1"), None);
        assert_eq!(parse_key("group:"), None);
    }
}

/// Receipts this process holds for a conversation record (token epoch taken from the record itself).
#[cfg(test)]
pub(super) fn receipts_for(context: &RuntimeContext, key: &str, conv: &Value) -> Vec<i64> {
    receipts_of(context, key, &epoch_of(conv))
}
