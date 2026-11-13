//! The `qq_*` MCP tools (port of `qq-bridge/src/mcp-snowluma-safe.js`).
//!
//! Recorded tool definitions live in `spec/tools-snowluma-safe.json`, with current wait guidance
//! applied at registration. Tools are thin console clients; authorization belongs to the console
//! (`x-agent-token`). Wait deadlines cover batching and the reserved final observation. Registration
//! follows `TOOL_CONFIG_FLAGS` and the sticker, voice and image switches.

use serde_json::{json, Map, Value};

use crate::model_view::serialize_model_data;
use crate::runtime::{RuntimeArea, RuntimeContext};

const SPEC: &str = include_str!("../spec/tools-snowluma-safe.json");

/// Result of one tool call as the MCP `tools/call` result carries it.
pub struct ToolOutcome {
    pub content: Vec<Value>,
    pub is_error: bool,
    pub delivery_unknown: bool,
}

impl ToolOutcome {
    fn text(text: String) -> Self {
        Self { content: vec![json!({"type": "text", "text": text})], is_error: false, delivery_unknown: false }
    }
    fn error(text: String) -> Self {
        Self { content: vec![json!({"type": "text", "text": text})], is_error: true, delivery_unknown: false }
    }
}

pub fn spec() -> Value {
    serde_json::from_str(SPEC).unwrap_or_else(|_| json!({ "tools": [] }))
}

/// JS `TOOL_CONFIG_FLAGS`: tool -> `socialV2.tools.<flag>` (false removes the tool at registration).
fn tool_flag(name: &str) -> Option<&'static str> {
    Some(match name {
        "qq_get_prompt" => "getPrompt",
        "qq_get_unread_messages" => "getUnread",
        "qq_get_recent_messages" => "getRecent",
        "qq_social_state" => "socialState",
        "qq_mark_read" => "markRead",
        "qq_set_wake_config" => "setWakeConfig",
        "qq_send_burst" => "sendBurst",
        "qq_send_message" => "sendMessage",
        "qq_wait_for_messages" => "waitMessages",
        "qq_report_feedback" => "feedback",
        "qq_get_my_recent_messages" => "getMyRecent",
        "qq_get_message_detail" => "getMessageDetail",
        "qq_get_active_members" => "getActiveMembers",
        "qq_memory_append" | "qq_memory_query" | "qq_memory_remove" | "qq_memory_clear" => "memory",
        "qq_slang_query" => "slangQuery",
        "qq_slang_submit" => "slangSubmit",
        "qq_send_voice" | "qq_list_voices" => "sendVoice",
        "qq_send_poke" => "sendPoke",
        "qq_get_message_images" => "getImages",
        "qq_get_forward_msg" => "getForwardMsg",
        "qq_list_stickers" => "listStickers",
        "qq_get_sticker_image" => "getStickerImage",
        "qq_send_sticker" => "sendSticker",
        "qq_collect_sticker" => "collectSticker",
        "qq_get_self_image" => "getSelfImage",
        "qq_sticker_note" => "stickerNote",
        "qq_set_sticker_remark" => "setStickerRemark",
        _ => return None,
    })
}

/// Tools that need `socialV2.sticker.enabled !== false` / `socialV2.voice.enabled !== false` as well.
fn needs_sticker_system(name: &str) -> bool {
    matches!(
        name,
        "qq_list_stickers" | "qq_get_sticker_image" | "qq_send_sticker" | "qq_collect_sticker" | "qq_sticker_note" | "qq_set_sticker_remark"
    )
}

fn needs_voice_system(name: &str) -> bool {
    matches!(name, "qq_send_voice" | "qq_list_voices")
}

/// Is the tool registered under this config (JS: evaluated once at server start)?
pub fn registered(name: &str, cfg: &Value) -> bool {
    let social = cfg.get("socialV2");
    let enabled = |path: &[&str]| -> bool {
        let mut cur = social;
        for seg in path {
            cur = cur.and_then(|v| v.get(*seg));
        }
        cur.and_then(Value::as_bool) != Some(false)
    };
    if let Some(flag) = tool_flag(name) {
        if !enabled(&["tools", flag]) {
            return false;
        }
    }
    if needs_sticker_system(name) && !enabled(&["sticker", "enabled"]) {
        return false;
    }
    if needs_voice_system(name) && !enabled(&["voice", "enabled"]) {
        return false;
    }
    true
}

/// The registered tool definitions in registration order.
pub fn registered_tools(cfg: &Value) -> Vec<Value> {
    spec()
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter(|t| t.get("name").and_then(Value::as_str).map(|n| registered(n, cfg)).unwrap_or(false))
                .map(|tool| {
                    let mut tool = tool.clone();
                    if matches!(tool["name"].as_str(), Some("qq_send_message" | "qq_send_burst" | "qq_reply" | "qq_send_group_message" | "qq_send_private_message")) {
                        let description = tool["description"].as_str().unwrap_or("");
                        tool["description"] = json!(format!("{description} {} 重复消息或本轮发送上限会拒绝整个批次，请遵循返回的停止指令。", crate::outbound_message::GUIDANCE));
                    }
                    if tool["name"] == "qq_wait_for_messages" {
                        let policy = crate::console::wait_policy(cfg);
                        tool["inputSchema"]["properties"]["purpose"]["description"] = json!("messages：等待对方下文/回复或沉睡前观察；reply：可选的短静默观察。发送工具自动控制回复节奏，首答无需先调用等待工具");
                        tool["description"] = json!(format!("发送工具自动控制回复节奏，首答无需先等待；本工具留给确实要等对方下文或沉睡观察时。等待分两种用途：reply 按最后来信补足 quietMs 静默，已安静足够就返回，不能代替沉睡观察；messages 等到新消息后合批，每条来信重置 {}ms 窗口，从首条起最多 {}ms；你成功发送后的互动窗口（preSleepWaitMs，只有你自己的发送重新计时）内则只在有给你的消息（forYou=true）时立即返回、不合批，只有闲聊时等到互动窗口结束再一起返回。@机器人或管理员来信立即返回。每条 newMessages 带桥接提示 forYou（true/false）和 forYouReason；它只是提示，是否回复由你决定。返回了给你的消息的等待不占本轮次数（每轮最多与发送上限 send.maxPerTurn 相同的次数）。互动窗口结束时返回 engagementEnded=true 和退出指令，桥接让你干净地沉睡并保留唤醒配置。直接读取 newMessages 和 readThroughSeq，无需重复查询。本轮两种用途合计最多 {} 次，最后一次在需要时自动转为完整沉睡观察。waitLimitReached=true 后唯一退出指令：end this turn now: produce your final answer and make no further tool calls。不要为退出而发送消息。若仍调用 mark_read/set_wake_config 收尾，观察不足时桥接强制 30 秒后再唤醒。未达上限且需要沉睡观察时使用 messages 和完整观察 timeoutMs；preSleepWaitSatisfied/Observed 表示观察资格，短 reply 不授予。", policy.window_ms, policy.max_ms, policy.max_per_turn));
                        tool["inputSchema"]["properties"]["quietMs"]["description"] = json!("reply 用途的静默窗口；messages 的合批窗口由后台 batchWindowMs/batchMaxMs 决定");
                        tool["inputSchema"]["properties"]["minNewMessages"]["description"] = json!("messages 用途期望的最少新消息数，默认 1；从首条起的合批硬上限到时仍返回已收集消息。reply 和保留的沉睡观察忽略此参数");
                    }
                    tool
                })
                .collect()
        })
        .unwrap_or_default()
}

// ----- HTTP plumbing --------------------------------------------------------------------------------------

pub(crate) struct HttpReply {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn wake_snapshot(context: &RuntimeContext, cfg: &Value, key: &str, token: &str) -> Result<String, String> {
    let port = context.console_port().or_else(|| cfg["consolePort"].as_u64().and_then(|p| u16::try_from(p).ok())).ok_or("wake snapshot console port unavailable")?;
    let body = serde_json::json!({"key":key,"token":token}).to_string();
    let reply = http_request(&format!("127.0.0.1:{port}"), "POST", "/api/socialV2/wake-snapshot",
        &[("x-console-token".into(), context.effective_console_token(cfg)), ("Content-Type".into(), "application/json".into())],
        Some(&body), std::time::Duration::from_secs(5))?;
    if reply.status != 200 { return Err(format!("wake snapshot refused: {}", reply.status)); }
    let packet: Value = serde_json::from_slice(&reply.body).map_err(|e| e.to_string())?;
    if packet.is_null() { return Ok(String::new()); }
    let packet = crate::model_view::strip_chat_with_runtime(context, &packet, cfg);
    Ok(format!("\n\n【本次消息快照】\n{packet}\n以上消息已提供，无需重复查询；partial=true 时用 qq_get_unread_messages(afterSeq=readThroughSeq) 继续读取。先处理当前消息；想回复时直接调用发送工具，桥接自动控制静默与打字节奏；reviseRequired=true 时先看 newMessages 再修改或重新发送。收尾只确认你已处理的 readThroughSeq（本地 seq，非 QQ messageId），传给 qq_mark_read 或 qq_set_wake_config 的 throughSeq；新到消息留待处理。"))
}

/// Query only positive urgent-message delivery evidence; never grant new receipts.
pub(crate) fn wake_delivered_urgent(context: &RuntimeContext, cfg: &Value, key: &str, token: &str, turn: &str) -> Result<Vec<i64>, String> {
    let port = context.console_port().or_else(|| cfg["consolePort"].as_u64().and_then(|p|u16::try_from(p).ok())).ok_or("wake evidence console port unavailable")?;
    let body = serde_json::json!({"key":key,"token":token,"turnId":turn,"readOnly":true}).to_string();
    let reply = http_request(&format!("127.0.0.1:{port}"), "POST", "/api/socialV2/wake-snapshot",
        &[("x-console-token".into(),context.effective_console_token(cfg)),("Content-Type".into(),"application/json".into())],
        Some(&body),std::time::Duration::from_secs(5))?;
    if reply.status != 200 { return Err(format!("wake evidence refused: {}",reply.status)); }
    let packet: Value = serde_json::from_slice(&reply.body).map_err(|e|e.to_string())?;
    serde_json::from_value(packet["urgentDeliveredSeqs"].clone()).map_err(|e|e.to_string())
}

pub(crate) fn http_request(
    host_port: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    timeout: std::time::Duration,
) -> Result<HttpReply, String> {
    http_request_with_connect_timeout(host_port, method, path, headers, body, timeout,
        std::time::Duration::from_secs(10))
}

fn http_request_with_connect_timeout(
    host_port: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    timeout: std::time::Duration,
    connect_timeout: std::time::Duration,
) -> Result<HttpReply, String> {
    use std::io::{Read, Write};
    let deadline = std::time::Instant::now() + timeout;
    // Connection and transmission have their own short budget; a connected console may hold
    // the response for reply timing, so its response budget must remain independent.
    let transmission_deadline = std::time::Instant::now() + timeout.min(connect_timeout);
    let timeout_error = || "The operation was aborted due to timeout".to_string();
    let addr = host_port
        .parse::<std::net::SocketAddr>()
        .ok()
        .or_else(|| std::net::ToSocketAddrs::to_socket_addrs(host_port).ok().and_then(|mut a| a.next()))
        .ok_or_else(|| "fetch failed".to_string())?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr,
        transmission_deadline.saturating_duration_since(std::time::Instant::now()))
        .map_err(|_| "fetch failed".to_string())?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    let mut pending = req.as_bytes();
    while !pending.is_empty() {
        let left = transmission_deadline.checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero()).ok_or_else(timeout_error)?;
        stream.set_write_timeout(Some(left)).map_err(|_| "fetch failed".to_string())?;
        match stream.write(pending) {
            Ok(0) => return Err("fetch failed".to_string()),
            Ok(count) => pending = &pending[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("fetch failed".to_string()),
        }
    }
    let mut raw: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(timeout_error());
        }
        stream.set_read_timeout(Some(left.min(std::time::Duration::from_secs(3600)))).ok();
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > 64 * 1024 * 1024 {
                    return Err("fetch failed".to_string());
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Err(timeout_error()),
            Err(_) => break,
        }
    }
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| "fetch failed".to_string())?;
    let head = String::from_utf8_lossy(&raw[..sep]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "fetch failed".to_string())?;
    let mut payload = raw[sep + 4..].to_vec();
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < payload.len() {
            let Some(le) = payload[i..].windows(2).position(|w| w == b"\r\n") else { break };
            let line = String::from_utf8_lossy(&payload[i..i + le]).to_string();
            let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16) else { break };
            i += le + 2;
            if size == 0 {
                break;
            }
            let end = (i + size).min(payload.len());
            out.extend_from_slice(&payload[i..end]);
            i = end + 2;
        }
        payload = out;
    }
    Ok(HttpReply { status, body: payload })
}

/// Bridge console (host, port). The console serves `/api/*` on `consolePort`, not on the OneBot port.
fn console_base(context: &RuntimeContext, cfg: &Value) -> Result<(String, u16), String> {
    let configured_port = cfg
        .get("consolePort")
        .and_then(|p| p.as_u64())
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0);
    let port = if context.is_isolated() {
        context
            .console_port()
            .filter(|port| *port != 0)
            .or(configured_port)
            .ok_or_else(|| "isolated runtime requires an explicit console port".to_string())?
    } else {
        configured_port.unwrap_or(3100)
    };
    Ok(("127.0.0.1".to_string(), port))
}

/// (host:port, onebot token). Isolated mode accepts only the selected runtime's dependency URL.
fn onebot_base(context: &RuntimeContext, cfg: &Value) -> Result<(String, String), String> {
    let url = if context.is_isolated() {
        context
            .dependency_url("onebot")
            .ok_or_else(|| "isolated runtime OneBot dependency is unavailable".to_string())?
    } else {
        cfg.get("snowluma").and_then(|s| s.get("httpUrl")).and_then(|u| u.as_str()).unwrap_or("http://127.0.0.1:3000")
    };
    let host_port = if context.is_isolated() {
        url.strip_prefix("http://")
            .filter(|authority| !authority.is_empty() && !authority.contains('/'))
            .ok_or_else(|| "isolated runtime OneBot dependency is invalid".to_string())?
            .to_string()
    } else {
        let host_port = url.trim_start_matches("http://").trim_end_matches('/').to_string();
        let (host, port) = host_port
            .split_once(':')
            .map(|(host, port)| (host.to_string(), port.parse().unwrap_or(3000)))
            .unwrap_or((host_port.clone(), 3000));
        format!("{host}:{port}")
    };
    let token = cfg.get("snowluma").and_then(|s| s.get("accessToken")).and_then(|t| t.as_str()).unwrap_or("").to_string();
    Ok((host_port, token))
}

/// Selected-runtime config; legacy contexts tolerate a missing/invalid file (JS `loadConfig` returns `{}`).
fn config(context: &RuntimeContext) -> Result<Value, String> {
    let text = match context.read_text(RuntimeArea::Config, "config.json") {
        Ok(text) => text,
        Err(_) if !context.is_isolated() => return Ok(json!({})),
        Err(_) => return Err("selected runtime config is unavailable".to_string()),
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(_) if !context.is_isolated() => Ok(json!({})),
        Err(_) => Err("selected runtime config is invalid".to_string()),
    }
}

/// JS `readConsoleToken`: a configured token only counts when the bridge would accept it.
fn console_token(context: &RuntimeContext, cfg: &Value) -> String {
    let configured = cfg.get("consoleToken").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).unwrap_or_default();
    let configured = configured.trim();
    let valid = (16..=128).contains(&configured.len()) && configured.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        return configured.to_string();
    }
    if context.is_isolated() {
        // The credential the selected console accepts (its generated token in `data/console-token`
        // when `consoleToken` is "").
        return context.effective_console_token(cfg);
    }
    context.read_text(RuntimeArea::Config, "state/console-token").map(|t| t.trim().to_string()).unwrap_or_default()
}

/// JS `errorText(value, fallback)`.
fn error_text(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => return s.trim().to_string(),
        Some(v @ Value::Object(_)) | Some(v @ Value::Array(_)) => {
            let text = v.to_string();
            if text != "{}" {
                return text.chars().take(500).collect();
            }
            if let Some(obj) = v.as_object() {
                for key in ["message", "msg", "error"] {
                    if let Some(Value::String(m)) = obj.get(key) {
                        if !m.trim().is_empty() {
                            return m.trim().chars().take(500).collect();
                        }
                    }
                }
            }
        }
        Some(Value::Number(n)) => return n.to_string(),
        Some(Value::Bool(b)) => return b.to_string(),
        _ => {}
    }
    fallback.to_string()
}

struct Api<'a> {
    context: &'a RuntimeContext,
    filter_reads: bool,
}

struct ApiCall {
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout_ms: u64,
    connect_timeout_ms: u64,
    long_poll: bool,
}

impl<'a> ApiCall {
    fn get(path: String) -> Self {
        Self { method: "GET", path, headers: Vec::new(), body: None, timeout_ms: 15_000, connect_timeout_ms: 10_000, long_poll: false }
    }
    fn post(path: &str, body: Value) -> Self {
        Self { method: "POST", path: path.to_string(), headers: Vec::new(), body: Some(body.to_string()), timeout_ms: 15_000, connect_timeout_ms: 10_000, long_poll: false }
    }
    fn token(mut self, token: &str) -> Self {
        self.headers.push(("x-agent-token".to_string(), token.to_string()));
        self
    }
    fn timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }
    fn connect_timeout(mut self, ms: u64) -> Self {
        self.connect_timeout_ms = ms;
        self
    }
}

struct ApiFailure {
    message: String,
    delivery_unknown: bool,
}
impl From<String> for ApiFailure {
    fn from(message: String) -> Self { Self { message, delivery_unknown: false } }
}

impl Api<'_> {
    /// JS `agentApi` / `agentApiLongPoll`: Ok(parsed body, `null` when it is not JSON), Err(message).
    fn call(&self, call: ApiCall) -> Result<Value, String> {
        self.call_classified(call, false).map_err(|e| e.message)
    }

    fn call_classified(&self, call: ApiCall, send: bool) -> Result<Value, ApiFailure> {
        let reply = self.request(call, send)?;
        let parsed: Option<Value> = serde_json::from_slice(&reply.body).ok();
        // Only a parsed refusal or complete outbox progress proves non-delivery. A lost,
        // malformed or truncated response cannot give the model permission to send again.
        let progress = parsed.as_ref().and_then(|b| b["unknown"].as_array());
        let unknown = send && (progress.is_some_and(|items| !items.is_empty())
            || (progress.is_some() && parsed.as_ref().is_some_and(|b|
                b["sent"].as_u64().unwrap_or(0) > 0 && b["pending"].as_array().is_some_and(|p| !p.is_empty())))
            || (progress.is_none() && (parsed.is_none() || reply.status == 408 || reply.status >= 500)));
        if !(200..300).contains(&reply.status) {
            return Err(ApiFailure { message: error_text(parsed.as_ref().and_then(|b| b.get("error")), &format!("桥接 API HTTP {}", reply.status)), delivery_unknown: unknown });
        }
        if let Some(body) = &parsed {
            if body.get("ok") == Some(&Value::Bool(false)) {
                return Err(ApiFailure { message: error_text(body.get("error"), "桥接 API 返回 ok:false（未说明原因）"), delivery_unknown: unknown });
            }
        }
        if send && parsed.as_ref().is_none_or(|b| b["ok"] != true) {
            return Err(ApiFailure { message: "发送响应无法确认".into(), delivery_unknown: true });
        }
        Ok(parsed.unwrap_or(Value::Null))
    }

    fn request(&self, call: ApiCall, send: bool) -> Result<HttpReply, ApiFailure> {
        let cfg = config(self.context)?;
        let (host, port) = console_base(self.context, &cfg)?;
        let token = console_token(self.context, &cfg);
        if self.context.is_isolated() && token.is_empty() {
            return Err("isolated runtime console token is unavailable".to_string().into());
        }
        let mut headers: Vec<(String, String)> = vec![("content-type".to_string(), "application/json".to_string())];
        if !token.is_empty() {
            headers.push(("x-console-token".to_string(), token));
        }
        headers.extend(call.headers.iter().cloned());
        headers.push(("x-agent-call".to_string(), "1".to_string()));
        let timeout = std::time::Duration::from_millis(call.timeout_ms.max(if call.long_poll { 1000 } else { 1 }));
        let reply = http_request_with_connect_timeout(&format!("{host}:{port}"), call.method, &call.path, &headers,
            call.body.as_deref(), timeout, std::time::Duration::from_millis(call.connect_timeout_ms)).map_err(|e| {
            if call.long_poll && e.contains("timeout") {
                ApiFailure { message: format!("等待超时（{}s）", (call.timeout_ms as f64 / 1000.0).round() as i64), delivery_unknown: send }
            } else {
                ApiFailure { message: e, delivery_unknown: send }
            }
        })?;
        Ok(reply)
    }

    fn response(&self, call: ApiCall) -> Result<(u16, Value), String> {
        let reply = self.request(call, false).map_err(|error| error.message)?;
        let body = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        Ok((reply.status, body))
    }

    /// JS `onebot(action, params)`: Ok(data) or Err(message).
    fn onebot(&self, action: &str, params: Value) -> Result<Value, String> {
        let cfg = config(self.context)?;
        let (host_port, token) = onebot_base(self.context, &cfg)?;
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if !token.is_empty() {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        let reply = http_request(&host_port, "POST", &format!("/{action}"), &headers, Some(&params.to_string()), std::time::Duration::from_secs(15))?;
        if !(200..300).contains(&reply.status) {
            let hint = if reply.status == 426 {
                "；HTTP 426 通常表示 httpUrl 指向了 WebSocket 端口，请检查 config.json 的 snowluma.httpUrl 是否为 OneBot HTTP API 地址"
            } else {
                ""
            };
            return Err(format!("HTTP {}{hint}", reply.status));
        }
        // Accepted deviation 18: a lone surrogate escape becomes U+FFFD instead of failing the call.
        let body: Value = std::str::from_utf8(&reply.body)
            .ok()
            .and_then(|text| crate::aux_text::parse_json_replacing_lone_surrogates::<Value>(text).ok())
            .map(|(value, _)| value)
            .ok_or_else(|| "Unexpected end of JSON input".to_string())?;
        let status_ok = body.get("status").and_then(Value::as_str) == Some("ok");
        let retcode_ok = body.get("retcode").and_then(Value::as_i64) == Some(0);
        if !status_ok || !retcode_ok {
            let retcode = body.get("retcode").map(js_text).unwrap_or_else(|| "undefined".to_string());
            let wording = body.get("wording").and_then(Value::as_str).unwrap_or("");
            return Err(format!("OneBot {action} 失败: retcode={retcode} {wording}"));
        }
        Ok(body.get("data").cloned().unwrap_or(Value::Null))
    }
}

// ----- JS value helpers -----------------------------------------------------------------------------------

/// JS `String(value)` for JSON values.
fn js_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => js_number(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(items) => items.iter().map(|v| if v.is_null() { String::new() } else { js_text(v) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS `Number.prototype.toString` for finite doubles.
pub fn js_number(n: f64) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    let abs = n.abs();
    if (1e-6..1e21).contains(&abs) {
        return format!("{n}");
    }
    let formatted = format!("{n:e}");
    match formatted.split_once('e') {
        Some((mantissa, exp)) => {
            let sign = if exp.starts_with('-') { "" } else { "+" };
            format!("{mantissa}e{sign}{exp}")
        }
        None => formatted,
    }
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `URLSearchParams` serialisation (space becomes `+`).
fn encode_form_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn query_string(pairs: &[(&str, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", encode_form_component(k), encode_form_component(v))).collect::<Vec<_>>().join("&")
}

/// `x ?? default` for an optional number rendered into a URL.
fn num_or(args: &Value, name: &str, default: &str) -> String {
    args.get(name).filter(|v| !v.is_null()).map(js_text).unwrap_or_else(|| default.to_string())
}

fn arg_str<'a>(args: &'a Value, name: &str) -> &'a str {
    args.get(name).and_then(Value::as_str).unwrap_or("")
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Some(_) => true,
    }
}

/// JS `unquoteJsonString`: a model that serialised one message as a JSON string ("\"hi\"") gets "hi".
fn unquote_json_string(value: &str) -> String {
    let t = value.trim();
    if t.starts_with('"') {
        if let Ok(Value::String(parsed)) = serde_json::from_str::<Value>(t) {
            return parsed;
        }
    }
    value.to_string()
}

/// The `messages` normalisation shared by `qq_send_burst` and `qq_send_message`.
fn normalize_messages(messages: &Value) -> Value {
    let Some(raw) = messages.as_str() else { return messages.clone() };
    let trimmed = raw.trim();
    if trimmed.starts_with('[') {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(trimmed) {
            return Value::Array(items.iter().map(|v| Value::String(js_text(v))).collect());
        }
    } else if trimmed.starts_with('"') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::String(s)) => return Value::String(s),
            Ok(Value::Array(items)) => return Value::Array(items.iter().map(|v| Value::String(js_text(v))).collect()),
            _ => {}
        }
    }
    messages.clone()
}

fn put(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        map.insert(key.to_string(), v.clone());
    }
}

/// Reserved2 CLI drafts share the authenticated QQ send endpoint.
pub(crate) fn send_reserved2_reply(
    context: &RuntimeContext, key: &str, token: &str, text: &str,
) -> Result<(u16, Value), String> {
    Api { context, filter_reads: false }.response(ApiCall::post("/api/socialV2/send-message",
        json!({"key":key,"messages":[text]})).token(token).timeout(300_000).connect_timeout(1000))
}

// ----- tool execution -------------------------------------------------------------------------------------

fn data_text(api: &Api<'_>, data: Value) -> ToolOutcome {
    if !api.filter_reads {
        return ToolOutcome::text(serialize_model_data(&data));
    }
    match config(api.context) {
        Ok(cfg) => ToolOutcome::text(serialize_model_data(&crate::model_view::strip_chat_with_runtime(api.context, &data, &cfg))),
        Err(error) => ToolOutcome::error(error),
    }
}

fn fail(prefix: &str, error: String) -> ToolOutcome {
    ToolOutcome::error(format!("{prefix}{error}"))
}

fn is_allowed(allow: &[String], deny: &[String], id: &str, allow_all_when_empty: bool) -> bool {
    if deny.iter().any(|d| d == id) {
        return false;
    }
    if !allow.is_empty() {
        return allow.iter().any(|a| a == id);
    }
    allow_all_when_empty
}

fn id_list(cfg: &Value, a: &str, b: &str) -> Vec<String> {
    cfg.get(a)
        .and_then(|x| x.get(b))
        .and_then(Value::as_array)
        .map(|l| l.iter().map(js_text).collect())
        .unwrap_or_default()
}

/// Legacy read tools have no session token: they are only for the closed agent (administrator private chat).
fn legacy_read_allowed(api: &Api<'_>) -> Result<bool, String> {
    let status = api.call(ApiCall::get("/api/status".to_string()))?;
    Ok(status.get("mode").map(js_text).unwrap_or_default() == "closed-agent")
}

const LEGACY_DENIED: &str = "旧只读工具仅在封闭 agent（管理员私聊）模式可用；其它模式请使用带会话令牌的 v2 读工具";

/// `args` is the validated, stripped input; session defaults from the environment are already applied.
pub fn call_tool(name: &str, args: &Value, context: &RuntimeContext) -> ToolOutcome {
    call_tool_inner(name, args, context, true)
}

/// Harness persists original packets and filters each model request, including replay.
pub(crate) fn call_tool_recorded(name: &str, args: &Value, context: &RuntimeContext) -> ToolOutcome {
    call_tool_inner(name, args, context, false)
}

fn call_tool_inner(name: &str, args: &Value, context: &RuntimeContext, filter_reads: bool) -> ToolOutcome {
    let api = Api { context, filter_reads };
    let token = arg_str(args, "token");
    let key = arg_str(args, "key");
    let with_token = |c: ApiCall| c.token(token);
    match name {
        "qq_status" => {
            let login = match api.onebot("get_login_info", json!({})) {
                Ok(l) => l,
                Err(e) => return fail("查询失败：", e),
            };
            let status = api.onebot("get_status", json!({})).unwrap_or(json!({}));
            let mut out = login.as_object().cloned().unwrap_or_default();
            if let Some(v) = status.get("online") {
                out.insert("online".to_string(), v.clone());
            }
            if let Some(v) = status.get("good") {
                out.insert("good".to_string(), v.clone());
            }
            data_text(&api, Value::Object(out))
        }
        "qq_list_groups" => {
            match legacy_read_allowed(&api) {
                Ok(true) => {}
                Ok(false) => return ToolOutcome::error(LEGACY_DENIED.to_string()),
                Err(e) => return fail("无法确认当前模式，拒绝执行：", e),
            }
            let cfg = config(context).unwrap_or(json!({}));
            let data = match api.onebot("get_group_list", json!({})) {
                Ok(d) => d,
                Err(e) => return fail("查询失败：", e),
            };
            let (allow, deny) = (id_list(&cfg, "allow", "groups"), id_list(&cfg, "deny", "groups"));
            let all_when_empty = cfg.get("allowAllWhenEmpty") == Some(&Value::Bool(true));
            let list = onebot_list(&data);
            let out: Vec<Value> = list
                .iter()
                .filter(|g| is_allowed(&allow, &deny, &g.get("group_id").map(js_text).unwrap_or_default(), all_when_empty))
                .map(|g| json!({"group_id": g.get("group_id"), "group_name": g.get("group_name")}))
                .map(strip_null_members)
                .collect();
            data_text(&api, Value::Array(out))
        }
        "qq_get_group_members" | "qq_get_group_history" => {
            let group = args.get("groupId").map(js_text).unwrap_or_default();
            match legacy_read_allowed(&api) {
                Ok(true) => {}
                Ok(false) => return ToolOutcome::error(LEGACY_DENIED.to_string()),
                Err(e) => return fail("无法确认当前模式，拒绝读取：", e),
            }
            let cfg = config(context).unwrap_or(json!({}));
            let (allow, deny) = (id_list(&cfg, "allow", "groups"), id_list(&cfg, "deny", "groups"));
            if !is_allowed(&allow, &deny, &group, cfg.get("allowAllWhenEmpty") == Some(&Value::Bool(true))) {
                return ToolOutcome::error(format!("拒绝：群 {group} 不在只读白名单中"));
            }
            let group_id = group.trim().parse::<f64>().map(|n| json!(n as i64)).unwrap_or(Value::Null);
            if name == "qq_get_group_members" {
                match api.onebot("get_group_member_list", json!({"group_id": group_id})) {
                    Ok(data) => {
                        let list: Vec<Value> = onebot_list(&data)
                            .iter()
                            .map(|m| strip_null_members(json!({"user_id": m.get("user_id"), "nickname": m.get("nickname"), "card": m.get("card")})))
                            .collect();
                        data_text(&api, Value::Array(list))
                    }
                    Err(e) => fail("查询失败：", e),
                }
            } else {
                let mut params = json!({"group_id": group_id});
                if let Some(seq) = args.get("messageSeq") {
                    params["message_seq"] = seq.clone();
                }
                match api.onebot("get_group_msg_history", params) {
                    Ok(data) => data_text(&api, data),
                    Err(e) => fail("查询失败：", e),
                }
            }
        }
        "qq_send_group_message" | "qq_reply" | "qq_send_private_message" => {
            let (path, target_key, target) = match name {
                "qq_send_group_message" => ("/api/send/group", "groupId", args.get("groupId")),
                "qq_reply" => ("/api/send/reply", "groupId", args.get("groupId")),
                _ => ("/api/send/private", "userId", args.get("userId")),
            };
            let message = unquote_json_string(arg_str(args, "message"));
            let mut body = Map::new();
            body.insert(target_key.to_string(), Value::String(target.map(js_text).unwrap_or_default()));
            if name == "qq_reply" {
                put(&mut body, "replyToMessageId", args.get("replyToMessageId"));
                body.insert("message".to_string(), Value::String(message));
            } else {
                body.insert("message".to_string(), Value::String(message));
                put(&mut body, "replyToMessageId", args.get("replyToMessageId"));
            }
            if truthy(args.get("token")) {
                body.insert("token".to_string(), args["token"].clone());
            }
            let mut call = ApiCall::post(path, Value::Object(body)).timeout(300_000);
            if truthy(args.get("token")) {
                call = call.token(token);
            }
            result(&api, call, "发送失败：")
        }
        "qq_get_prompt" => result(&api, with_token(ApiCall::get(format!("/api/socialV2/prompt?key={}", encode_uri_component(key)))), "获取提示词失败："),
        "qq_get_unread_messages" => {
            let mut path = format!("/api/socialV2/unread?key={}&limit={}", encode_uri_component(key), num_or(args, "limit", "30"));
            if let Some(after) = args.get("afterSeq") {
                path.push_str(&format!("&afterSeq={}", js_text(after)));
            }
            result(&api, with_token(ApiCall::get(path)), "获取未读消息失败：")
        }
        "qq_get_recent_messages" => result(
            &api,
            with_token(ApiCall::get(format!(
                "/api/socialV2/recent?key={}&limit={}&offset={}",
                encode_uri_component(key),
                num_or(args, "limit", "20"),
                num_or(args, "offset", "0")
            ))),
            "获取最近消息失败：",
        ),
        "qq_social_state" => result(&api, with_token(ApiCall::get(format!("/api/socialV2/state?key={}", encode_uri_component(key)))), "获取状态失败："),
        "qq_mark_read" => {
            let mut body = Map::new();
            body.insert("key".to_string(), json!(key));
            put(&mut body, "throughSeq", args.get("throughSeq"));
            result(&api, with_token(ApiCall::post("/api/socialV2/mark-read", Value::Object(body))), "标记已读失败：")
        }
        "qq_set_wake_config" => {
            let mut body = Map::new();
            body.insert("key".to_string(), json!(key));
            put(&mut body, "config", args.get("config"));
            put(&mut body, "throughSeq", args.get("throughSeq"));
            result(&api, with_token(ApiCall::post("/api/socialV2/wake-config", Value::Object(body))), "设置唤醒配置失败：")
        }
        "qq_send_burst" => {
            let group_key = format!("group:{}", args.get("groupId").map(js_text).unwrap_or_default());
            let messages = normalize_messages(args.get("messages").unwrap_or(&Value::Null));
            result(
                &api,
                with_token(ApiCall::post("/api/socialV2/send-burst", json!({"key": group_key, "messages": messages})).timeout(300_000)),
                "分条发送失败：",
            )
        }
        "qq_send_message" => {
            let mut body = Map::new();
            body.insert("key".to_string(), json!(key));
            body.insert("messages".to_string(), normalize_messages(args.get("messages").unwrap_or(&Value::Null)));
            put(&mut body, "replyToMessageId", args.get("replyToMessageId"));
            body.insert("atUserId".to_string(), args.get("atUserId").cloned().unwrap_or(Value::Null));
            put(&mut body, "gapMode", args.get("gapMode"));
            put(&mut body, "gapMs", args.get("gapMs"));
            put(&mut body, "gaps", args.get("gaps"));
            result(&api, with_token(ApiCall::post("/api/socialV2/send-message", Value::Object(body)).timeout(300_000)), "发送失败：")
        }
        "qq_send_poke" => {
            let target = args.get("targetUserId").filter(|v| !v.is_null()).map(js_text).unwrap_or_default();
            result(
                &api,
                with_token(ApiCall::post("/api/socialV2/send-poke", json!({"key": key, "targetUserId": target})).timeout(60_000)),
                "拍一拍失败：",
            )
        }
        "qq_wait_for_messages" => {
            let mut body = Map::new();
            body.insert("key".to_string(), json!(key));
            for field in ["purpose", "timeoutMs", "minNewMessages", "quietMs"] {
                put(&mut body, field, args.get(field));
            }
            let cfg = match config(context) { Ok(cfg) => cfg, Err(e) => return fail("等待失败：", e) };
            let mut call = with_token(ApiCall::post("/api/socialV2/wait", Value::Object(body)));
            call.long_poll = true;
            call.timeout_ms = crate::console::wait_client_timeout_ms(&cfg, args);
            result(&api, call, "等待失败：")
        }
        "qq_report_feedback" => {
            let mut body = Map::new();
            body.insert("key".to_string(), json!(key));
            put(&mut body, "level", args.get("level"));
            put(&mut body, "message", args.get("message"));
            result(&api, with_token(ApiCall::post("/api/socialV2/feedback", Value::Object(body))), "反馈失败：")
        }
        "qq_get_my_recent_messages" => result(
            &api,
            with_token(ApiCall::get(format!("/api/socialV2/my-recent?key={}&limit={}", encode_uri_component(key), num_or(args, "limit", "10")))),
            "获取自己消息失败：",
        ),
        "qq_get_message_detail" => result(
            &api,
            with_token(ApiCall::get(format!(
                "/api/socialV2/message-detail?key={}&messageId={}",
                encode_uri_component(key),
                encode_uri_component(&args.get("messageId").map(js_text).unwrap_or_default())
            ))),
            "获取消息详情失败：",
        ),
        "qq_get_active_members" => result(
            &api,
            with_token(ApiCall::get(format!("/api/socialV2/active-members?key={}&limit={}", encode_uri_component(key), num_or(args, "limit", "10")))),
            "获取活跃成员失败：",
        ),
        "qq_memory_append" => {
            let extra = if truthy(args.get("extra")) { args["extra"].clone() } else { json!({}) };
            result(
                &api,
                with_token(ApiCall::post("/api/socialV2/memory-append", json!({"key": key, "category": args.get("category"), "content": args.get("content"), "extra": extra}))),
                "记忆写入失败：",
            )
        }
        "qq_memory_query" => {
            let mut pairs = vec![("key", key.to_string())];
            if truthy(args.get("category")) {
                pairs.push(("category", arg_str(args, "category").to_string()));
            }
            result(&api, with_token(ApiCall::get(format!("/api/socialV2/memory?{}", query_string(&pairs)))), "记忆读取失败：")
        }
        "qq_memory_remove" => result(
            &api,
            with_token(ApiCall::post(
                "/api/socialV2/memory-remove",
                json!({"key": key, "category": args.get("category"), "content": arg_str(args, "content"), "target": arg_str(args, "target")}),
            )),
            "记忆删除失败：",
        ),
        "qq_memory_clear" => result(
            &api,
            with_token(ApiCall::post("/api/socialV2/memory-clear", json!({"key": key, "category": arg_str(args, "category")}))),
            "记忆清空失败：",
        ),
        "qq_slang_query" => {
            let q = arg_str(args, "q");
            let query = if q.is_empty() { String::new() } else { format!("&q={}", encode_uri_component(q)) };
            result(&api, with_token(ApiCall::get(format!("/api/socialV2/slang/query?key={}{}", encode_uri_component(key), query))), "查询黑话失败：")
        }
        "qq_slang_submit" => result(
            &api,
            with_token(ApiCall::post("/api/socialV2/slang/submit", json!({"key": key, "content": args.get("content"), "context": arg_str(args, "context")}))),
            "提交黑话失败：",
        ),
        "qq_get_message_images" => {
            let message_id = args.get("messageId").map(js_text).unwrap_or_default();
            let q = query_string(&[("key", key.to_string()), ("messageId", message_id.clone())]);
            let mut call = ApiCall::get(format!("/api/images/message?{q}")).timeout(180_000);
            if truthy(args.get("token")) {
                call = call.token(token);
            }
            match api.call(call) {
                Ok(data) => images_message_outcome(&message_id, &data),
                Err(e) => fail("获取图片失败：", e),
            }
        }
        "qq_list_stickers" => {
            let query = if truthy(args.get("query")) { js_text(&args["query"]) } else { String::new() };
            let mut body = Map::new();
            body.insert("key".to_string(), json!(key));
            body.insert("query".to_string(), json!(query));
            put(&mut body, "count", args.get("count"));
            body.insert("refresh".to_string(), json!(args.get("refresh") == Some(&Value::Bool(true))));
            body.insert("token".to_string(), json!(token));
            result(&api, with_token(ApiCall::post("/api/socialV2/sticker-list", Value::Object(body))), "获取表情列表失败：")
        }
        "qq_get_sticker_image" => {
            let sticker_id = arg_str(args, "stickerId");
            let q = query_string(&[("key", key.to_string()), ("stickerId", sticker_id.to_string())]);
            match api.call(with_token(ApiCall::get(format!("/api/socialV2/sticker-image?{q}")).timeout(180_000))) {
                Ok(data) => {
                    let image_content = image_content(&data);
                    if image_content.is_empty() {
                        return ToolOutcome::error(format!("表情没有可返回的图片：{}", error_text(data.get("error"), "未知")));
                    };
                    let sticker = data.get("sticker");
                    let id = sticker.and_then(|s| s.get("id")).filter(|v| truthy(Some(v))).map(js_text).unwrap_or_else(|| sticker_id.to_string());
                    let desc = sticker.and_then(|s| s.get("desc")).filter(|v| truthy(Some(v))).map(js_text);
                    let note = desc.map(|d| format!("（备注：{d}）")).unwrap_or_default();
                    ToolOutcome {
                        content: std::iter::once(json!({"type": "text", "text": format!("表情 {id}{note} 的图片内容：")})).chain(image_content).collect(),
                        is_error: false,
                        delivery_unknown: false,
                    }
                }
                Err(e) => fail("获取表情图片失败：", e),
            }
        }
        "qq_send_sticker" => {
            let mut body = Map::new();
            body.insert("key".to_string(), json!(key));
            body.insert("stickerId".to_string(), json!(arg_str(args, "stickerId")));
            put(&mut body, "replyToMessageId", args.get("replyToMessageId"));
            body.insert("atUserId".to_string(), args.get("atUserId").cloned().unwrap_or(Value::Null));
            result(&api, with_token(ApiCall::post("/api/socialV2/send-sticker", Value::Object(body)).timeout(300_000)), "发送表情失败：")
        }
        "qq_collect_sticker" => result(
            &api,
            with_token(ApiCall::post("/api/socialV2/collect-sticker", json!({"key": key, "messageId": arg_str(args, "messageId"), "remark": arg_str(args, "remark")})).timeout(60_000)),
            "收藏表情失败：",
        ),
        "qq_send_voice" => result(
            &api,
            with_token(
                ApiCall::post(
                    "/api/socialV2/send-voice",
                    json!({
                        "key": key, "voice": arg_str(args, "voice"), "label": arg_str(args, "label"),
                        "verify": args.get("verify") != Some(&Value::Bool(false)),
                        "readback": args.get("readback") == Some(&Value::Bool(true)), "token": token
                    }),
                )
                .timeout(300_000),
            ),
            "发送语音失败：",
        ),
        "qq_list_voices" => {
            let mut pairs = vec![("key", key.to_string())];
            if args.get("probe") == Some(&Value::Bool(false)) {
                pairs.push(("probe", "0".to_string()));
            }
            result(&api, with_token(ApiCall::get(format!("/api/socialV2/voices?{}", query_string(&pairs)))), "获取语音库失败：")
        }
        "qq_get_self_image" => {
            let q = query_string(&[("key", key.to_string())]);
            match api.call(with_token(ApiCall::get(format!("/api/socialV2/self-image?{q}")))) {
                Ok(data) => {
                    let image_content = image_content(&data);
                    if image_content.is_empty() {
                        return ToolOutcome::error(format!("没有可返回的形象图片：{}", error_text(data.get("error"), "未知")));
                    };
                    ToolOutcome {
                        content: std::iter::once(json!({"type": "text", "text": "这是你的默认 Q 版形象："})).chain(image_content).collect(),
                        is_error: false,
                        delivery_unknown: false,
                    }
                }
                Err(e) => fail("获取形象图片失败：", e),
            }
        }
        "qq_sticker_note" => {
            let mut payload = Map::new();
            payload.insert("key".to_string(), json!(key));
            payload.insert("stickerId".to_string(), json!(arg_str(args, "stickerId")));
            if let Some(note) = args.get("note").filter(|v| !v.is_null()) {
                payload.insert("note".to_string(), json!(js_text(note)));
            }
            if let Some(tags) = args.get("tags").filter(|v| !v.is_null()) {
                let list: Vec<Value> = tags.as_array().map(|a| a.iter().map(|t| json!(js_text(t))).collect()).unwrap_or_default();
                payload.insert("tags".to_string(), Value::Array(list));
            }
            if let Some(usage) = args.get("usage").filter(|v| !v.is_null()) {
                payload.insert("usage".to_string(), json!(js_text(usage)));
            }
            result(&api, with_token(ApiCall::post("/api/socialV2/sticker-note", Value::Object(payload))), "记录表情备注失败：")
        }
        "qq_set_sticker_remark" => result(
            &api,
            with_token(ApiCall::post("/api/socialV2/sticker-remark", json!({"key": key, "stickerId": arg_str(args, "stickerId"), "remark": arg_str(args, "remark")}))),
            "修改表情备注失败：",
        ),
        "qq_get_forward_msg" => forward_msg(&api, key, token, arg_str(args, "id")),
        other => ToolOutcome::error(format!("MCP error -32602: Tool {other} not found")),
    }
}

fn result(api: &Api<'_>, call: ApiCall, prefix: &str) -> ToolOutcome {
    let model_read = call.method == "GET" || call.path == "/api/socialV2/wait" || call.path == "/api/socialV2/sticker-list"
        || matches!(call.path.as_str(), "/api/send/group" | "/api/send/private" | "/api/send/reply");
    let send = matches!(call.path.as_str(), "/api/send/group" | "/api/send/private" | "/api/send/reply"
        | "/api/socialV2/send-message" | "/api/socialV2/send-burst" | "/api/socialV2/send-poke"
        | "/api/socialV2/send-sticker" | "/api/socialV2/send-voice");
    match api.call_classified(call, send) {
        Ok(data) if model_read || data["reviseRequired"] == true => data_text(api, data),
        Ok(data) => ToolOutcome::text(serialize_model_data(&data)),
        Err(e) => {
            let mut outcome = fail(prefix, e.message);
            outcome.delivery_unknown = e.delivery_unknown;
            outcome
        }
    }
}

fn onebot_list(data: &Value) -> Vec<Value> {
    match data {
        Value::Array(a) => a.clone(),
        other => other.get("data").and_then(Value::as_array).cloned().unwrap_or_default(),
    }
}

/// JSON.stringify drops `undefined` members; a missing OneBot field must not become `null`.
fn strip_null_members(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        other => other,
    }
}

fn image_content(data: &Value) -> Vec<Value> {
    let images = data.get("images").and_then(Value::as_array).cloned()
        .unwrap_or_else(|| data.get("image").cloned().into_iter().collect());
    let mut content = Vec::new();
    for img in images {
        if let Some(text) = img.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            content.push(json!({"type": "text", "text": text}));
        }
        if img.get("data").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
            && img.get("mimeType").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
            content.push(json!({"type": "image", "mimeType": img["mimeType"], "data": img["data"]}));
        }
    }
    content
}

fn images_message_outcome(message_id: &str, data: &Value) -> ToolOutcome {
    let images = data.get("images").and_then(Value::as_array).cloned().unwrap_or_default();
    if images.is_empty() {
        let note = data.get("note").filter(|v| truthy(Some(v))).map(js_text).unwrap_or_else(|| "未找到".to_string());
        return ToolOutcome::text(format!("消息 {message_id} 没有可返回的图片/表情：{note}"));
    }
    let mut content = Vec::new();
    let mut parts = Vec::new();
    for img in &images {
        let label = if img.get("kind").and_then(Value::as_str) == Some("face") { "表情" } else { "图片" };
        let index = img.get("index").filter(|v| !v.is_null()).map(js_text).unwrap_or_default();
        let text = img.get("text").filter(|v| truthy(Some(v))).map(js_text);
        let data_ok = truthy(img.get("data")) && truthy(img.get("mimeType"));
        if data_ok {
            let extra = text.map(|t| format!(" {t}")).unwrap_or_default();
            parts.push(format!("[{label}{index}{extra}]"));
            content.push(json!({"type": "image", "mimeType": img["mimeType"], "data": img["data"]}));
        } else {
            let extra = text.map(|t| format!(" {t}")).unwrap_or_else(|| "（获取失败）".to_string());
            parts.push(format!("[{label}{index}{extra}]"));
        }
    }
    if !parts.is_empty() {
        content.insert(0, json!({"type": "text", "text": format!("消息 {message_id} 的媒体内容（{} 项）：\n{}", images.len(), parts.join("\n"))}));
    }
    ToolOutcome { content, is_error: false, delivery_unknown: false }
}

fn forward_msg(api: &Api<'_>, key: &str, token: &str, id: &str) -> ToolOutcome {
    let q = query_string(&[("key", key.to_string()), ("id", id.to_string())]);
    let data = match api.call(ApiCall::get(format!("/api/socialV2/forward-message?{q}")).token(token).timeout(120_000)) {
        Ok(d) => d,
        Err(e) => return fail("查看合并转发失败：", e),
    };
    let outcome = data_text(api, data.clone());
    if outcome.is_error { return outcome; }
    let mut content = outcome.content;
    let mut images: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut collect = |msgs: Option<&Value>| {
        let Some(list) = msgs.and_then(Value::as_array) else { return };
        for m in list {
            let Some(media) = m.get("media").and_then(Value::as_array) else { continue };
            for item in media.iter().filter(|i| i.is_object()) {
                let key_id = ["url", "file", "faceId"]
                    .iter()
                    .find_map(|k| item.get(*k).filter(|v| truthy(Some(v))).map(js_text))
                    .unwrap_or_default();
                if key_id.is_empty() || seen.contains(&key_id) {
                    continue;
                }
                seen.push(key_id);
                images.push(item.clone());
            }
        }
    };
    collect(data.get("messages"));
    if let Some(previews) = data.get("nestedPreviews").and_then(Value::as_array) {
        for p in previews {
            collect(p.get("messages"));
        }
    }
    let mut image_texts: Vec<String> = Vec::new();
    if !images.is_empty() {
        let batch: Vec<Value> = images.iter().take(5).cloned().collect();
        match api.call(ApiCall::post("/api/socialV2/forward-media", json!({"key": key, "media": batch})).token(token).timeout(180_000)) {
            Ok(media) => {
                for img in media.get("images").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let index = img.get("index").filter(|v| !v.is_null()).map(|v| format!(" {}", js_text(v))).unwrap_or_default();
                    let text = img.get("text").filter(|v| truthy(Some(v))).map(js_text);
                    if truthy(img.get("data")) && truthy(img.get("mimeType")) {
                        content.push(json!({"type": "image", "mimeType": img["mimeType"], "data": img["data"]}));
                        image_texts.push(format!("[转发内图片{index}{}]", text.map(|t| format!(" {t}")).unwrap_or_default()));
                    } else {
                        image_texts.push(format!("[转发内图片{index}（{}）]", text.unwrap_or_else(|| "获取失败".to_string())));
                    }
                }
            }
            Err(e) => image_texts.push(format!("[转发内图片（批量获取失败：{e}）]")),
        }
    }
    if !image_texts.is_empty() {
        content.insert(0, json!({"type": "text", "text": format!("合并转发 {id} 的图片内容（{} 项）：\n{}", image_texts.len(), image_texts.join("\n"))}));
    }
    ToolOutcome { content, is_error: false, delivery_unknown: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_every_js_tool_and_gating_follows_the_js_flags() {
        let tools = spec()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), 38);
        let all = registered_tools(&json!({}));
        assert_eq!(all.len(), 38);
        let off = json!({"socialV2": {"sticker": {"enabled": false}, "voice": {"enabled": false}, "tools": {"sendPoke": false, "memory": false, "getImages": false}}});
        let names: Vec<String> = registered_tools(&off).iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        let gone_all = [
            "qq_list_stickers", "qq_get_sticker_image", "qq_send_sticker", "qq_collect_sticker", "qq_sticker_note", "qq_set_sticker_remark",
            "qq_send_voice", "qq_list_voices", "qq_send_poke", "qq_memory_append", "qq_memory_query", "qq_memory_remove", "qq_memory_clear", "qq_get_message_images",
        ];
        for gone in gone_all {
            assert!(!names.contains(&gone.to_string()), "{gone} must be unregistered");
        }
        assert!(names.contains(&"qq_get_self_image".to_string()), "self image only needs getSelfImage");
        assert_eq!(names.len(), 38 - gone_all.len());
    }

    #[test]
    fn js_number_formatting_matches_template_literals() {
        assert_eq!(js_number(30.0), "30");
        assert_eq!(js_number(1.5), "1.5");
        assert_eq!(js_number(-1.0), "-1");
        assert_eq!(js_number(1e21), "1e+21");
        assert_eq!(js_number(1.5e-7), "1.5e-7");
        assert_eq!(js_number(0.000001), "0.000001");
    }

    #[test]
    fn encoders_differ_like_the_js_apis() {
        assert_eq!(encode_uri_component("a b!c"), "a%20b!c");
        assert_eq!(encode_form_component("a b!c"), "a+b%21c");
        assert_eq!(encode_uri_component("群:1"), "%E7%BE%A4%3A1");
    }

    #[test]
    fn message_string_forms_are_unwrapped_like_the_js_tools() {
        assert_eq!(normalize_messages(&json!("[\"a\",5,null]")), json!(["a", "5", "null"]));
        assert_eq!(normalize_messages(&json!("\"hi\"")), json!("hi"));
        assert_eq!(normalize_messages(&json!("plain")), json!("plain"));
        assert_eq!(normalize_messages(&json!(["x"])), json!(["x"]));
        assert_eq!(unquote_json_string("\"你好\""), "你好");
    }

    #[test]
    fn error_text_matches_the_js_helper() {
        assert_eq!(error_text(Some(&json!(" boom ")), "f"), "boom");
        assert_eq!(error_text(Some(&json!({"code": 1})), "f"), "{\"code\":1}");
        assert_eq!(error_text(Some(&json!({})), "f"), "f");
        assert_eq!(error_text(Some(&json!(7)), "f"), "7");
        assert_eq!(error_text(None, "f"), "f");
    }

    #[test]
    fn gap_ledger_a_lone_surrogate_in_a_onebot_reply_is_replaced_not_an_error() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"status":"ok","retcode":0,"data":{"messages":[{"raw_message":"hi \ud83d there"}]}}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id("her-onebot-surrogate-")));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), format!(r#"{{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:{port}"}}"#)).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let data = Api { context: &context, filter_reads: true }.onebot("get_group_msg_history", json!({"group_id": 1})).expect("a lone surrogate must not fail the call");
        assert_eq!(data["messages"][0]["raw_message"], "hi \u{FFFD} there");
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
