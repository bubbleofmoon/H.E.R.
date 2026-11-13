//! DSH 客户端（P2-A）：RPC 调用、会话创建与 prompt 投递。登录 Cookie 归 `dsh_credential`。
//!
//! 与 JS 版 `src/dsh-client.js` 逐项对齐：
//!   信封  {type:"client-request", rpcId, method:"session/list", payload:{args:{...}}}`
//!   路径  POST /api/<method 点号换斜杠>`
//!   包装  session/list -> args._request；多数 session/* -> args.request；其余 -> args 平铺
//!   特例  session/prompt 必须带 requestId`

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use crate::dsh_credential::Credential;
use crate::runtime::RuntimeContext;

fn now_ms() -> u128 {
    crate::parity_clock::now_ms() as u128
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

/// 极简 HTTP/1.1 POST，返回 (状态码, 正文)。只连本机，不需要 TLS。
/// 必须能处理 chunked 编码——DSH 的响应是分块传输的。
fn http_post(host_port: &str, path: &str, cookie: Option<&str>, body: &str) -> Option<(u16, String)> {
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(c) = cookie {
        req.push_str(&format!("Cookie: {c}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);

    let mut s = std::net::TcpStream::connect(host_port).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok()?;
    s.write_all(req.as_bytes()).ok()?;

    let mut raw: Vec<u8> = Vec::new();
    s.read_to_end(&mut raw).ok()?;

    let sep = find_sub(&raw, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..sep]).to_string();
    let mut payload = raw[sep + 4..].to_vec();

    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;

    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    if chunked {
        payload = decode_chunked(&payload);
    }

    Some((status, String::from_utf8_lossy(&payload).to_string()))
}

fn bounded_loopback_endpoint(base: &str) -> Result<(String, std::net::SocketAddr), String> {
    let normalized = base.trim_end_matches('/');
    let authority = normalized
        .strip_prefix("http://")
        .ok_or_else(|| "bounded DSH calls require an http:// loopback SocketAddr".to_string())?;
    if authority.is_empty()
        || authority
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b'@'))
    {
        return Err("bounded DSH calls require a bare literal loopback authority".into());
    }
    let address = authority
        .parse::<std::net::SocketAddr>()
        .map_err(|_| "bounded DSH calls require a literal loopback SocketAddr with a port".to_string())?;
    if address.port() == 0 || !address.ip().is_loopback() {
        return Err("bounded DSH calls require a nonzero-port loopback SocketAddr".into());
    }
    Ok((authority.to_string(), address))
}

fn deadline_remaining(
    deadline: std::time::Instant,
    stage: &str,
) -> Result<std::time::Duration, String> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        Err(format!("DSH deadline exceeded during {stage}"))
    } else {
        Ok(remaining)
    }
}

fn deadline_io_error(stage: &str, error: std::io::Error) -> String {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        format!("DSH deadline exceeded during {stage}: {error}")
    } else {
        format!("DSH {stage} failed: {error}")
    }
}

const CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

fn io_remaining(
    deadline: std::time::Instant,
    stage: &str,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<std::time::Duration, String> {
    if cancelled.is_some_and(|check| check()) {
        return Err("DSH call cancelled".into());
    }
    let remaining = deadline_remaining(deadline, stage)?;
    Ok(if cancelled.is_some() {
        remaining.min(CANCEL_POLL)
    } else {
        remaining
    })
}

fn poll_timeout(error: &std::io::Error, cancelled: Option<&dyn Fn() -> bool>) -> bool {
    cancelled.is_some()
        && matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        )
}

fn write_all_until(
    stream: &mut std::net::TcpStream,
    mut bytes: &[u8],
    deadline: std::time::Instant,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(), String> {
    while !bytes.is_empty() {
        let remaining = io_remaining(deadline, "request write", cancelled)?;
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|error| format!("set DSH request write deadline: {error}"))?;
        match stream.write(bytes) {
            Ok(0) => return Err("DSH request write failed: connection wrote zero bytes".into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if poll_timeout(&error, cancelled) => continue,
            Err(error) => return Err(deadline_io_error("request write", error)),
        }
    }
    io_remaining(deadline, "request write", cancelled).map(|_| ())
}

fn read_to_end_until(
    stream: &mut std::net::TcpStream,
    deadline: std::time::Instant,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let remaining = io_remaining(deadline, "response read", cancelled)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("set DSH response read deadline: {error}"))?;
        match stream.read(&mut buffer) {
            Ok(0) => {
                io_remaining(deadline, "response read", cancelled)?;
                return Ok(raw);
            }
            Ok(read) => {
                io_remaining(deadline, "response read", cancelled)?;
                raw.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if poll_timeout(&error, cancelled) => continue,
            Err(error) => return Err(deadline_io_error("response read", error)),
        }
    }
}

fn http_post_with_deadline(
    host_port: &str,
    address: std::net::SocketAddr,
    path: &str,
    cookie: Option<&str>,
    body: &str,
    deadline: std::time::Instant,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(u16, String), String> {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(cookie) = cookie {
        request.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    let mut stream = loop {
        let timeout = io_remaining(deadline, "connect", cancelled)?;
        match std::net::TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted
                    || poll_timeout(&error, cancelled) => continue,
            Err(error) => return Err(deadline_io_error("connect", error)),
        }
    };
    io_remaining(deadline, "connect", cancelled)?;
    write_all_until(&mut stream, request.as_bytes(), deadline, cancelled)?;
    let raw = read_to_end_until(&mut stream, deadline, cancelled)?;

    let separator = find_sub(&raw, b"\r\n\r\n")
        .ok_or_else(|| "DSH response read failed: HTTP header terminator is missing".to_string())?;
    let head = String::from_utf8_lossy(&raw[..separator]).to_string();
    let mut payload = raw[separator + 4..].to_vec();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "DSH response read failed: HTTP status is invalid".to_string())?;
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        payload = decode_chunked(&payload);
    }
    Ok((status, String::from_utf8_lossy(&payload).to_string()))
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 解开 chunked 传输编码。
fn decode_chunked(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < input.len() {
        // 读十六进制长度行
        let Some(line_end) = find_sub(&input[i..], b"\r\n") else { break };
        let line = String::from_utf8_lossy(&input[i..i + line_end]).to_string();
        let size_str = line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_str, 16) else { break };
        i += line_end + 2;
        if size == 0 {
            break;
        }
        let end = (i + size).min(input.len());
        out.extend_from_slice(&input[i..end]);
        i = end + 2; // 跳过块尾 CRLF
    }
    out
}

// ── RPC ──────────────────────────────────────────────────────────────────────

/// args 包装方式（对应 JS 的 METHOD_ARG_WRAPPERS）。
fn arg_wrapper(endpoint: &str) -> Option<&'static str> {
    match endpoint {
        "session/list" => Some("_request"),
        "agentPresets/read" | "session/create" | "session/prompt" | "session/cancel" | "session/selectModel"
        | "session/rename" | "session/fork" | "session/updateQueue" | "session/page"
        | "session/search" | "session/follow" | "session/projections" | "workspace/create"
        | "workspace/rename" | "workspace/delete" | "workspace/archiveSession"
        | "workspace/insertBefore" | "workspace/insertSessionBefore" => Some("request"),
        _ => None,
    }
}

/// 不需要参数的端点（JS 里映射成 null 的那些）。
fn takes_no_args(endpoint: &str) -> bool {
    matches!(endpoint, "agentPresets/list" | "settings/describe")
}

/// 生成一个 UUID v4 形状的 requestId（不引 uuid 依赖）。
pub(crate) fn mint_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut h = Sha256::new();
    h.update(now_ms().to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(n.to_le_bytes());
    let d = h.finalize();
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

pub struct Rpc {
    pub base: String,
    /// The login in force. A request refused with 401 refreshes it (`dsh_credential`), so it is shared state behind
    /// `&self`.
    credential: Credential,
}

impl Rpc {
    /// The session cookie currently in force (after any 401 refresh).
    pub fn cookie(&self) -> String {
        self.credential.cookie()
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    pub fn connect(base: &str) -> Result<Self, String> {
        Self::connect_with_runtime(&RuntimeContext::legacy(), base)
    }

    pub fn connect_with_runtime(context: &RuntimeContext, base: &str) -> Result<Self, String> {
        let base = base.trim_end_matches('/').to_string();
        let credential = Credential::mint(context, &base)?;
        Ok(Self { base, credential })
    }

    /// Compatibility constructor for callers which already extracted a credentials-file secret.
    /// Isolated handlers must use `connect_with_runtime` so a later 401 refresh stays bound to the
    /// selected runtime credentials rather than consulting the ambient DSH home.
    #[cfg(test)]
    pub fn connect_with_secret(base: &str, secret: &str) -> Result<Self, String> {
        let base = base.trim_end_matches('/').to_string();
        let credential = Credential::from_secret(&base, secret)?;
        Ok(Self { base, credential })
    }

    /// Re-read the credentials, mint a new cookie and verify it with the side-effect-free `agentPresets/list`
    /// (`Credential::relogin`).
    pub fn relogin(&mut self) -> Result<(), String> {
        let this = &*self;
        this.credential
            .relogin(|| this.call("agentPresets/list", serde_json::json!({})).map(|_| ()))
    }

    /// 调一个 RPC 方法，返回 result.value。
    pub fn call(&self, method: &str, payload: serde_json::Value) -> Result<serde_json::Value, String> {
        self.call_with_rpc_id(method, payload, &mint_request_id())
    }

    /// Same as `call` with a caller-chosen JSON-RPC id. For `session/prompt`, default its
    /// `requestId` to this tracked id: DSH echoes the prompt requestId in durable
    /// `data.source.rpcId`, allowing briefing acknowledgement and exact restart recovery.
    pub fn call_with_rpc_id(
        &self,
        method: &str,
        payload: serde_json::Value,
        rpc_id: &str,
    ) -> Result<serde_json::Value, String> {
        self.call_http(method, payload, rpc_id, true)
    }

    /// One request with the cookie in force and no refresh: a 401 is returned to the caller, which classifies the
    /// login (`dsh_credential::check_login`).
    pub fn call_without_refresh(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.call_http(method, payload, &mint_request_id(), false)
    }

    fn call_http(
        &self,
        method: &str,
        payload: serde_json::Value,
        rpc_id: &str,
        refresh: bool,
    ) -> Result<serde_json::Value, String> {
        let endpoint = method.replace('.', "/");
        let mut body = payload;
        if endpoint == "session/prompt" && body.get("requestId").is_none() {
            body["requestId"] = serde_json::json!(rpc_id);
        }
        let args = if takes_no_args(&endpoint) {
            serde_json::json!({})
        } else if let Some(w) = arg_wrapper(&endpoint) {
            serde_json::json!({ w: body })
        } else {
            body
        };
        let envelope = serde_json::json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": endpoint,
            "payload": { "args": args },
        });

        let host_port = self
            .base
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let path = format!("/api/{endpoint}");
        let body_text = envelope.to_string();
        let (mut status, mut text) = http_post(&host_port, &path, Some(&self.cookie()), &body_text)
            .ok_or_else(|| format!("HTTP 连接失败：{endpoint}"))?;
        if status == 401 && refresh {
            // A 401 is refused before the request is processed, so one re-authentication and one repeat is safe
            // for every method, `session/prompt` included (same rpc id). Never more than once per request.
            if let Some(fresh) = self.credential.refresh_after_rejection() {
                crate::recovery_log::record(
                    self.credential.context(),
                    crate::run_record::Trigger::Turn,
                    crate::run_record::Component::Dsh,
                    "reauth-401",
                    crate::run_record::Outcome::Ok,
                    &format!("{endpoint} refused with 401; credentials re-read, request repeated once"),
                );
                (status, text) = http_post(&host_port, &path, Some(&fresh), &body_text)
                    .ok_or_else(|| format!("HTTP 连接失败：{endpoint}"))?;
            }
        }

        if status == 401 {
            return Err(format!("401 未授权：{endpoint}"));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("响应不是 JSON（{endpoint}）：{e}；原文 {}", crate::activity::preview_chars(&text, 200)))?;

        if v.get("type").and_then(|t| t.as_str()) != Some("server-response") {
            return Err(format!("响应类型不对（{endpoint}）：{}", crate::activity::preview_chars(&text, 200)));
        }
        if v.get("rpcId").and_then(|id| id.as_str()) != Some(rpc_id) {
            return Err(format!("RPC response identity mismatch（{endpoint}）"));
        }
        let result = v.get("result").ok_or_else(|| format!("响应缺 result（{endpoint}）"))?;
        if result.get("ok").and_then(|o| o.as_bool()) != Some(true) {
            let code = result
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .unwrap_or("?");
            let msg = result
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            return Err(format!("{endpoint} 被拒：{code}: {msg}"));
        }
        Ok(result.get("value").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// Call one RPC using a shared absolute deadline. Intended for multi-step metadata probes:
    /// callers pass the same deadline to each fallback request so the total budget is bounded.
    /// Only explicit literal loopback HTTP SocketAddrs are accepted on this path.
    pub fn call_with_deadline(
        &self,
        method: &str,
        payload: serde_json::Value,
        deadline: std::time::Instant,
    ) -> Result<serde_json::Value, String> {
        self.call_bounded(method, payload, deadline, None)
    }

    /// Bounded task RPC with the bridge's single 401 re-authentication. Poll both I/O
    /// and authentication boundaries; cancellation never permits another request.
    pub fn call_with_cancellation(
        &self,
        method: &str,
        payload: serde_json::Value,
        deadline: std::time::Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<serde_json::Value, String> {
        self.call_bounded(method, payload, deadline, Some(cancelled))
    }

    /// `call_with_cancellation` with a caller-chosen JSON-RPC id, recorded before the call so a
    /// later run can ask DSH whether it holds that exact prompt (`probe_prompt_bounded`).
    pub fn call_with_cancellation_as(
        &self,
        method: &str,
        payload: serde_json::Value,
        rpc_id: &str,
        deadline: std::time::Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<serde_json::Value, String> {
        self.call_bounded_as(method, payload, rpc_id.to_string(), deadline, Some(cancelled))
    }

    fn call_bounded(
        &self,
        method: &str,
        payload: serde_json::Value,
        deadline: std::time::Instant,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<serde_json::Value, String> {
        self.call_bounded_as(method, payload, mint_request_id(), deadline, cancelled)
    }

    fn call_bounded_as(
        &self,
        method: &str,
        payload: serde_json::Value,
        rpc_id: String,
        deadline: std::time::Instant,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<serde_json::Value, String> {
        let endpoint = method.replace('.', "/");
        let mut body = payload;
        if endpoint == "session/prompt" && body.get("requestId").is_none() {
            body["requestId"] = serde_json::json!(rpc_id);
        }
        let args = if takes_no_args(&endpoint) {
            serde_json::json!({})
        } else if let Some(wrapper) = arg_wrapper(&endpoint) {
            serde_json::json!({ wrapper: body })
        } else {
            body
        };
        let envelope = serde_json::json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": endpoint,
            "payload": { "args": args },
        });
        let (host_port, address) = bounded_loopback_endpoint(&self.base)?;
        let body_text = envelope.to_string();
        let (mut status, mut text) = http_post_with_deadline(
            &host_port,
            address,
            &format!("/api/{endpoint}"),
            Some(&self.cookie()),
            &body_text,
            deadline,
            cancelled,
        )?;

        if status == 401 && cancelled.is_some() {
            io_remaining(deadline, "reauthentication", cancelled)?;
            if let Some(fresh) = self.credential.refresh_after_rejection() {
                io_remaining(deadline, "reauthentication", cancelled)?;
                (status, text) = http_post_with_deadline(
                    &host_port,
                    address,
                    &format!("/api/{endpoint}"),
                    Some(&fresh),
                    &body_text,
                    deadline,
                    cancelled,
                )?;
            }
        }

        if status == 401 {
            return Err(format!("401 未授权：{endpoint}"));
        }
        if status == 404 {
            let excerpt = crate::activity::preview_chars(&text, 200);
            return Err(format!("HTTP 404 Not Found（{endpoint}）：{excerpt}"));
        }
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}（{endpoint}）"));
        }
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("响应不是 JSON（{endpoint}）：{error}；原文 {}", crate::activity::preview_chars(&text, 200)))?;
        if value.get("type").and_then(|kind| kind.as_str()) != Some("server-response") {
            return Err(format!("响应类型不对（{endpoint}）：{}", crate::activity::preview_chars(&text, 200)));
        }
        if value.get("rpcId").and_then(|id| id.as_str()) != Some(rpc_id.as_str()) {
            return Err(format!("RPC response identity mismatch（{endpoint}）"));
        }
        let result = value
            .get("result")
            .ok_or_else(|| format!("响应缺 result（{endpoint}）"))?;
        if result.get("ok").and_then(|ok| ok.as_bool()) != Some(true) {
            let code = result
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(|code| code.as_str())
                .unwrap_or("?");
            let message = result
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .unwrap_or("");
            return Err(format!("{endpoint} 被拒：{code}: {message}"));
        }
        Ok(result
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }
}

// ── 演示：建会话 + 投 prompt（A2 验收）────────────────────────────────────────

/// 用 Rust 建一个会话并投一句 prompt，打印每步结果。
pub async fn demo_create_and_prompt(base: &str, preset: &str) -> Result<(), String> {
    let rpc = Rpc::connect(base)?;
    println!("[dsh] 1/5 Cookie 铸造成功");

    let list = rpc.call("session/list", serde_json::json!({}))?;
    let n = list.get("items").and_then(|i| i.as_array()).map(|a| a.len()).unwrap_or(0);
    println!("[dsh] 2/5 session/list ok：{n} 个会话");

    let presets = rpc.call("agentPresets/list", serde_json::Value::Null)?;
    let names: Vec<String> = presets
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    println!("[dsh] 3/5 可用预设：{}", names.join(", "));

    let ws_dir = "C:/qqbot/state/rust-demo";
    let _ = std::fs::create_dir_all(ws_dir);
    let ws = rpc.call("workspace/create", serde_json::json!({ "path": ws_dir }))?;
    let ws_id = ws
        .get("workspace")
        .and_then(|w| w.get("workspaceId"))
        .and_then(|i| i.as_str())
        .ok_or_else(|| format!("workspace/create 没返回 workspaceId：{ws}"))?
        .to_string();
    println!("[dsh] 4/5 workspace/create ok：{ws_id}");
    let _ = rpc.call(
        "workspace/rename",
        serde_json::json!({ "workspaceId": ws_id, "title": "Rust 移植验证" }),
    );

    let sess = rpc.call(
        "session/create",
        serde_json::json!({ "workspaceId": ws_id, "agentPreset": preset }),
    )?;
    let sid = sess
        .get("sessionId")
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("session/create 没返回 sessionId：{sess}"))?
        .to_string();
    println!("[dsh] 5/5 session/create ok：{sid}（预设 {preset}）");

    // A3：**先订阅再投 prompt**。反过来的话回合可能在订阅建立前就结束，
    // 实时流不会重放，只能读快照，那样分不清是这轮还是上一轮的回复。
    let mut follow = FollowStream::open(base, &rpc.cookie(), &sid).await?;
    println!("[dsh] 已订阅事件流（baseline seq={}）", follow.baseline_seq());

    let accepted = rpc.call(
        "session/prompt",
        serde_json::json!({
            "sessionId": sid,
            "mode": "queue",
            "content": [{ "type": "text", "text": "只回复两个字：收到" }]
        }),
    )?;
    println!("[dsh] prompt 已投递：{}", accepted);

    println!("[dsh] 等回复（实时事件流）…");
    let reply = follow.next_turn(std::time::Duration::from_secs(120)).await?;
    follow.close().await;
    println!("[dsh] ✅ 拿到回复：{reply}");
    println!("[dsh] ✅ A2+A3 通过（会话 {sid}）");
    Ok(())
}

// ── A3：session/follow 事件流 ────────────────────────────────────────────────

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// 从 assistant 消息的 content 块里提取纯文本。
fn blocks_to_text(content: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(arr) = content.as_array() {
        for b in arr {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                }
            }
        }
    }
    out
}

/// 一轮里的一条 provider usage 采样（`assistant/message` / `assistant/attempt`）。
///
/// 字段与 JS `token-ledger.js` 的 `ingestEvent` 输入一一对应：同一
/// (turn, step, retry) 只保留一条，`seq` 严格变大才允许覆盖。
#[derive(Debug, Clone)]
pub struct TurnUsage {
    pub turn: i64,
    pub step: i64,
    pub retry: i64,
    pub seq: i64,
    /// 事件自带的时间（Unix ms）；取不到时用收到事件的时间。
    pub t: i64,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// 原始 usage 桶（cacheMiss / cacheHit / cacheWrite / output / reasoning）。
    pub usage: serde_json::Value,
}

/// 从一条会话事件里取 provider usage，语义照抄 JS `usageOfEvent`：
///   · `assistant/message` 优先用 `data.usage`；
///   · 否则从 `assistant/message` / `assistant/attempt` 的 `data.stream` 尾部
///     找最后一个带 `usage` 的 chunk。
///
/// 与 JS 一样用「字段存在」判断（`!== undefined`）：`data.usage: null` 也算存在，不回落到 stream，
/// 归一化后得到 0 桶，于是不产生采样。
pub(crate) fn usage_of_event(event: &serde_json::Value) -> Option<serde_json::Value> {
    let etype = event.get("type")?.as_str()?;
    if etype == "assistant/message" {
        if let Some(usage) = event.get("data").and_then(|d| d.get("usage")) {
            return Some(usage.clone());
        }
    }
    if etype != "assistant/message" && etype != "assistant/attempt" {
        return None;
    }
    let stream = event.get("data")?.get("stream")?.as_array()?;
    for chunk in stream.iter().rev() {
        if !chunk.is_object() {
            continue;
        }
        if let Some(usage) = chunk.get("usage") {
            return Some(usage.clone());
        }
    }
    None
}

/// 同一 (turn, step, retry) 只留 seq 严格更大的采样（与 JS `samples` Map 的覆盖规则一致）。
fn push_usage_sample(samples: &mut Vec<TurnUsage>, sample: TurnUsage) {
    if let Some(existing) = samples
        .iter_mut()
        .find(|s| s.turn == sample.turn && s.step == sample.step && s.retry == sample.retry)
    {
        if sample.seq >= 0 && existing.seq >= sample.seq {
            return;
        }
        *existing = sample;
        return;
    }
    samples.push(sample);
}

/// `modelSelection.lastUsed ?? pending` 里的 (model, provider)，空字符串按缺失处理。
fn selection_of_snapshot(snapshot: &serde_json::Value) -> (Option<String>, Option<String>) {
    let selection = snapshot.get("projections").and_then(|p| p.get("values")).and_then(|v| v.get("modelSelection"));
    let last = selection.and_then(|s| {
        s.get("lastUsed")
            .filter(|v| !v.is_null())
            .or_else(|| s.get("pending"))
            .filter(|v| !v.is_null())
    });
    let text = |key: &str| {
        last.and_then(|x| x.get(key))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    (text("model"), text("provider"))
}

fn canonical_usage(buckets: &crate::pricing::Buckets) -> serde_json::Value {
    serde_json::json!({
        "cacheMiss": buckets.cache_miss,
        "cacheHit": buckets.cache_hit,
        "cacheWrite": buckets.cache_write,
        "output": buckets.output,
        "reasoning": buckets.reasoning,
    })
}

/// 快照里的账本事实（照抄 JS `ingestBaseline` / `ingestSnapshotRecords`）。
#[derive(Debug, Clone, Default)]
pub struct UsageHarvest {
    pub samples: Vec<TurnUsage>,
    pub base: Option<BaseFact>,
}

/// `projections.tokenUsage`：整条会话日志的权威累计，截至 `as_of_seq`。
#[derive(Debug, Clone)]
pub struct BaseFact {
    pub as_of_seq: i64,
    pub usage: serde_json::Value,
    pub model: Option<String>,
    pub provider: Option<String>,
}

/// 会话级的 usage 折叠状态（模型/候选后端、重试轴）。纯函数，离线可测。
#[derive(Debug, Clone)]
pub struct UsageFold {
    retry_axis: HashMap<(i64, i64), i64>,
    model: Option<String>,
    provider: Option<String>,
}

impl UsageFold {
    pub fn new(model: Option<String>, provider: Option<String>) -> Self {
        Self { retry_axis: HashMap::new(), model, provider }
    }

    /// 记住模型选择：模型非空才更新，provider 随之覆盖（与 JS `rememberModel` 一致）。
    pub fn seed_selection(&mut self, model: Option<String>, provider: Option<String>) {
        if let Some(m) = model.filter(|m| !m.is_empty()) {
            self.model = Some(m);
            self.provider = provider.filter(|p| !p.is_empty());
        }
    }

    /// 当前会话记住的模型与候选后端（`FollowStream.model/provider` 的来源）。
    pub fn model(&self) -> Option<String> {
        self.model.clone()
    }

    pub fn provider(&self) -> Option<String> {
        self.provider.clone()
    }

    /// 处理一条实时事件；产生 usage 采样时返回它。
    ///
    /// 口径照抄 bridge.js `recordSessionUsage` + `ingestEvent`：
    ///   · 模型 = `data.usage.model` ?? 会话记住的模型 ?? `data.model`；候选后端 = `data.provider` ?? 记住的 provider；
    ///   · 重试轴：`llm/retry-started` 只影响它所属的 (turn, step)；
    ///   · 时间 = 事件的 `time`，缺失时用 `now`；序号缺失记为 -1（不参与覆盖判断）。
    pub fn observe(&mut self, event: &serde_json::Value, now: i64) -> Option<TurnUsage> {
        let etype = event.get("type")?.as_str()?;
        let data = event.get("data");
        let seq = event.get("seq").and_then(|s| s.as_i64()).unwrap_or(-1);
        let num = |key: &str| data.and_then(|d| d.get(key)).and_then(|v| v.as_i64());
        let text = |v: Option<&serde_json::Value>| {
            v.and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
        };
        match etype {
            "model/selection" => {
                self.seed_selection(text(data.and_then(|d| d.get("model"))), text(data.and_then(|d| d.get("provider"))));
                None
            }
            "llm/retry-started" => {
                if let (Some(turn), Some(step)) = (num("turn"), num("step")) {
                    let retry = num("retry").unwrap_or(1);
                    self.retry_axis.insert((turn, step), retry);
                }
                None
            }
            "assistant/message" | "assistant/attempt" => {
                let usage = usage_of_event(event)?;
                let buckets = crate::pricing::normalize_usage(Some(&usage));
                if buckets.total() <= 0.0 {
                    return None;
                }
                // 没有 turn/step 的 usage 无法折叠进账本（JS 同样丢弃）。
                let (turn, step) = (num("turn")?, num("step")?);
                let retry = self.retry_axis.get(&(turn, step)).copied().unwrap_or(0);
                let t = event.get("time").and_then(|x| x.as_i64()).unwrap_or(now);
                let model = text(data.and_then(|d| d.get("usage")).and_then(|u| u.get("model")))
                    .or_else(|| self.model.clone())
                    .or_else(|| text(data.and_then(|d| d.get("model"))));
                let provider = text(data.and_then(|d| d.get("provider"))).or_else(|| self.provider.clone());
                Some(TurnUsage { turn, step, retry, seq, t, model, provider, usage: canonical_usage(&buckets) })
            }
            _ => None,
        }
    }
}

/// 把一张开场快照折成账本事实：历史 usage 采样与 `tokenUsage` 基线（照抄 JS `ingestSnapshotRecords` / `ingestBaseline`）。
///
/// 与 JS 的一处刻意差异：历史采样的模型/候选后端回落到快照里的最近选择（`modelSelection`），
/// JS 历史采样没有这层回落，会按兜底模型计价、且没有候选后端归属。快照选择与历史采样属于同一会话，
/// 用它归属更准确；价格差异只在快照选择模型与采样实际模型不同、且采样自身不带模型时出现。
/// 历史里的 `model/selection` 事件不改写记住的模型（JS 同样只在实时事件上记住模型）。
pub fn harvest_of_snapshot(snapshot: &serde_json::Value, now: i64) -> UsageHarvest {
    let (model, provider) = selection_of_snapshot(snapshot);
    let mut fold = UsageFold::new(None, None);
    fold.seed_selection(model.clone(), provider.clone());
    let mut samples: Vec<TurnUsage> = Vec::new();
    if let Some(records) = snapshot.get("records").and_then(|r| r.as_array()) {
        for entry in records {
            let event = if entry.get("type").and_then(|t| t.as_str()) == Some("event") {
                match entry.get("event") {
                    Some(event) => event,
                    None => continue,
                }
            } else {
                entry
            };
            if event.get("type").and_then(|t| t.as_str()) == Some("model/selection") {
                continue;
            }
            if let Some(sample) = fold.observe(event, now) {
                push_usage_sample(&mut samples, sample);
            }
        }
    }
    let projections = snapshot.get("projections");
    let as_of_seq = projections.and_then(|p| p.get("asOfSeq")).and_then(|s| s.as_i64()).unwrap_or(-1);
    let base = projections
        .and_then(|p| p.get("values"))
        .and_then(|v| v.get("tokenUsage"))
        .filter(|u| !u.is_null())
        .map(|u| BaseFact {
            as_of_seq,
            usage: canonical_usage(&crate::pricing::normalize_usage(Some(u))),
            model,
            provider,
        });
    UsageHarvest { samples, base }
}

#[cfg(test)]
mod usage_tests;

pub struct FollowStream {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    stream_id: String,
    /// 打开时的游标：快照里 seq <= baseline 的事件都是历史，不能当新回合
    baseline_seq: i64,
    current_turn: Option<i64>,
    turn_text: String,
    /// 最近一次 `turn/end` 的 data（含 reason/error），failover 用它判断这一轮是不是失败收尾。
    pub last_ended: serde_json::Value,
    /// 本回合收到的 usage 采样（`turn/start` 时清空，`turn/end` 后由 pipeline 落账）。
    pub turn_usage: Vec<TurnUsage>,
    /// 最近一条 live 事件的 seq（没有 usage 明细时写 `b:null` 记录要用）。
    pub last_seq: i64,
    /// 账本折叠状态（模型/候选后端/重试轴），与 JS 同会话口径，见 `UsageFold`。
    fold: UsageFold,
    /// 最近一次 `model/selection`：usage 里没带 model/provider 时兜底。
    pub model: Option<String>,
    pub provider: Option<String>,
    /// 开场快照里的历史事件与待处理队列（回合核对用，ticket 10）。
    pub snapshot: Snapshot,
    briefing: Option<(RuntimeContext, String, String, String)>,
    /// 本回合的工具调用（`turn/start` 时清空）。
    pub tools: Vec<ToolFact>,
}

/// 开场快照：历史事件、inbox 与覆盖范围。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub events: Vec<serde_json::Value>,
    /// 快照里看到的 inbox（`projections.values.inbox`）；`None` = 快照没带这项。
    pub inbox: Option<Vec<serde_json::Value>>,
    /// 快照自带的 `hasMore`（若有）。
    pub has_more: Option<bool>,
    pub max_messages: usize,
    /// 快照里的账本事实：历史 usage 采样与 `tokenUsage` 基线（见 `harvest_of_snapshot`）。
    pub harvest: UsageHarvest,
}

impl Snapshot {
    /// 快照是否覆盖了整个会话日志：消息数不到上限、事件数不到上限，且没有 `hasMore=true`。
    /// 分页按 `user/message` + `assistant/message` 从尾部往回数到 `maxMessages`（DSH
    /// `paginate`），不足上限就说明没有被截断。
    pub fn covers_whole_log(&self) -> bool {
        let messages = self
            .events
            .iter()
            .filter(|e| matches!(e.get("type").and_then(|t| t.as_str()), Some("user/message" | "assistant/message")))
            .count();
        self.has_more != Some(true) && messages < self.max_messages && self.events.len() < self.max_messages
    }
}

/// 回合里的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFact {
    pub call_id: String,
    pub name: String,
    pub done: bool,
    pub ok: bool,
}

impl ToolFact {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"callId": self.call_id, "name": self.name, "done": self.done, "ok": self.ok})
    }
}

fn call_id_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// 把一条事件折进工具调用表（`tool/call` 新增，`tool/result` 按 callId 收尾）。
pub fn fold_tool_event(tools: &mut Vec<ToolFact>, event: &serde_json::Value) {
    let data = event.get("data");
    match event.get("type").and_then(|t| t.as_str()) {
        Some("tool/call") => {
            let call_id = call_id_text(data.and_then(|d| d.get("callId")));
            let name = data.and_then(|d| d.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
            tools.push(ToolFact { call_id, name, done: false, ok: false });
        }
        Some("tool/result") => {
            let message = data.and_then(|d| d.get("message"));
            let call_id = call_id_text(message.and_then(|m| m.get("source")).and_then(|s| s.get("callId")));
            let errored = message.and_then(|m| m.get("isError")).and_then(|v| v.as_bool()) == Some(true)
                || data.and_then(|d| d.get("error")).is_some_and(|e| !e.is_null());
            if let Some(tool) = tools.iter_mut().rev().find(|t| t.call_id == call_id && !t.done) {
                tool.done = true;
                tool.ok = !errored;
            }
        }
        _ => {}
    }
}

impl FollowStream {
    /// 订阅一个会话并消费开场快照，记下游标。
    ///
    /// **必须先订阅再投 prompt**：否则回合可能在订阅建立前就结束，
    /// 实时事件流不会重放，只能从快照里读（那样会分不清是这轮还是上一轮的回复）。
    pub async fn open(
        base: &str,
        cookie: &str,
        session_id: &str,
    ) -> Result<Self, String> {
        Self::open_with_max(base, cookie, session_id, 20).await
    }

    /// 与 `open` 相同，但可指定快照携带的历史消息条数（回合核对要看得更远）。
    pub async fn open_with_max(
        base: &str,
        cookie: &str,
        session_id: &str,
        max_messages: usize,
    ) -> Result<Self, String> {
        let ws_url = format!("{}/api/remote.mux", base.replacen("http://", "ws://", 1));
        let mut request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("构造 WS 请求失败：{e}"))?;
        request
            .headers_mut()
            .insert("Cookie", cookie.parse().map_err(|_| "Cookie 头非法".to_string())?);

        let (mut ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| format!("连 /api/remote.mux 失败：{e}"))?;

        let stream_id = mint_request_id();
        let open = serde_json::json!({
            "type": "open",
            "streamId": stream_id,
            "endpoint": "session/follow",
            "payload": {
                "args": {
                    "request": {
                        "address": { "kind": "session", "sessionId": session_id },
                        "maxMessages": max_messages
                    }
                }
            }
        });
        ws.send(Message::Text(open.to_string()))
            .await
            .map_err(|e| format!("发送 open 帧失败：{e}"))?;

        // 等开场快照，拿到 baseline
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let baseline_seq: i64;
        let snapshot: Snapshot;
        let mut sel: (Option<String>, Option<String>) = (None, None);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("等开场快照超时".to_string());
            }
            let msg = match tokio::time::timeout(remaining, ws.next()).await {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => return Err(format!("WS 读失败：{e}")),
                Ok(None) => return Err("WS 在快照前结束".to_string()),
                Err(_) => return Err("等开场快照超时".to_string()),
            };
            let Message::Text(t) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
            if frame.get("streamId").and_then(|s| s.as_str()) != Some(stream_id.as_str()) {
                continue;
            }
            if frame.get("type").and_then(|x| x.as_str()) == Some("error") {
                let code = frame
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("?");
                return Err(format!("订阅被拒：{code}"));
            }
            let Some(value) = frame.get("value") else { continue };
            if value.get("type").and_then(|x| x.as_str()) != Some("snapshot") {
                continue;
            }
            baseline_seq = value
                .get("records")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|rec| rec.get("event")?.get("seq")?.as_i64())
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            sel = selection_of_snapshot(value);
            snapshot = Snapshot {
                events: value
                    .get("records")
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|rec| rec.get("event").cloned().or_else(|| rec.get("type").map(|_| rec.clone())))
                            .collect()
                    })
                    .unwrap_or_default(),
                inbox: inbox_of_values(value.get("projections").and_then(|p| p.get("values"))),
                has_more: value.get("hasMore").and_then(|h| h.as_bool()),
                max_messages,
                harvest: harvest_of_snapshot(value, now_ms() as i64),
            };
            break;
        }

        // 实时折叠与历史采样共用同一份快照选择（JS：`rememberModel(snapshot selection)`）。
        let mut fold = UsageFold::new(None, None);
        let (sel_model, sel_provider) = sel;
        fold.seed_selection(sel_model, sel_provider);
        let (model, provider) = (fold.model(), fold.provider());
        Ok(Self {
            ws,
            stream_id,
            baseline_seq,
            current_turn: None,
            turn_text: String::new(),
            last_ended: serde_json::Value::Null,
            turn_usage: Vec::new(),
            last_seq: 0,
            fold,
            model,
            provider,
            snapshot,
            briefing: None,
            tools: Vec::new(),
        })
    }

    pub fn track_briefing(&mut self, context: &RuntimeContext, key: &str, sid: &str, epoch: &str) -> Result<(),String> {
        let record = crate::session_map::context_record(context,key,sid,epoch)?;
        let seen = record["observedSeq"].as_i64().unwrap_or(0);
        let first = self.snapshot.events.iter().filter_map(|e|e["seq"].as_i64()).min().unwrap_or(0);
        if !self.snapshot.covers_whole_log() && seen > 0 && first > seen.saturating_add(1) {
            crate::session_map::update_context(context,key,sid,epoch,|r| {
                if r["observedSeq"].as_i64()==Some(seen) && r["gapThrough"].as_i64().unwrap_or(0)<first-1 {
                    r["gapThrough"]=serde_json::json!(first-1);
                    r["needsBriefing"]=serde_json::json!(true);
                    if r["pending"]["full"]!=true {r["pending"]=serde_json::Value::Null;}
                }
            })?;
        }
        for event in &self.snapshot.events {
            crate::session_context::observe(context,key,sid,epoch,event)?;
        }
        self.briefing = Some((context.clone(),key.into(),sid.into(),epoch.into()));
        Ok(())
    }

    fn track_mapped_briefing(&mut self, rpc: &Rpc, sid: &str) -> Result<(), String> {
        let context = rpc.credential().context();
        if context.is_isolated() && crate::console::runtime_mode(context) == "reserved2" {
            if let Some((key, _)) = crate::session_map::all(context)?.into_iter().find(|(_, mapped)| mapped == sid) {
                if let Some(epoch) = crate::console::conversation_token(context, &key)? {
                    self.track_briefing(context, &key, sid, &epoch)?;
                }
            }
        }
        Ok(())
    }

    pub fn baseline_seq(&self) -> i64 {
        self.baseline_seq
    }

    /// 本轮 turn 编号（`turn/end` 时用于 `b:null` 记录）。
    pub fn turn_id(&self) -> Option<i64> {
        self.current_turn
    }

    /// 等下一轮结束，返回该轮文本。
    ///
    /// 与 JS 的 createTurnCollector() 对齐：
    ///   turn/start        -> 新建这一轮缓冲
    ///   assistant/chunk   -> 忽略（assistant/message 才是完整组装文本）
    ///   assistant/message -> 累加 text 块
    ///   turn/end          -> 返回
    /// 只接受 seq > baseline 的事件，避免把快照里的历史回合当成新回复。
    pub async fn next_turn(&mut self, timeout: std::time::Duration) -> Result<String, String> {
        self.next_turn_with_start(timeout, || Ok(())).await
    }

    pub async fn next_turn_with_start(&mut self, timeout: std::time::Duration, on_start: impl FnMut() -> Result<(), String> + Send) -> Result<String,String> {
        self.next_turn_with_lifecycle(timeout, on_start, |_| Ok(())).await
    }

    /// Observe the matching end before usage/tool bookkeeping or returning to the caller.
    pub async fn next_turn_with_lifecycle(
        &mut self,
        timeout: std::time::Duration,
        mut on_start: impl FnMut() -> Result<(), String> + Send,
        mut on_end: impl FnMut(&serde_json::Value) -> Result<(), String> + Send,
    ) -> Result<String, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("等回复超时（{}s）", timeout.as_secs()));
            }
            let msg = match tokio::time::timeout(remaining, self.ws.next()).await {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => return Err(format!("WS 读失败：{e}")),
                Ok(None) => return Err("WS 流结束".to_string()),
                Err(_) => return Err(format!("等回复超时（{}s）", timeout.as_secs())),
            };
            let text = match msg {
                Message::Text(t) => t,
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Close(_) => return Err("WS 被关闭".to_string()),
                _ => continue,
            };
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            if frame.get("streamId").and_then(|s| s.as_str()) != Some(self.stream_id.as_str()) {
                continue;
            }
            match frame.get("type").and_then(|t| t.as_str()) {
                Some("error") => {
                    let code = frame
                        .get("error")
                        .and_then(|e| e.get("code"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("?");
                    return Err(format!("流返回错误：{code}"));
                }
                Some("end") => return Err("流提前结束".to_string()),
                Some("item") => {}
                _ => continue,
            }
            let Some(value) = frame.get("value") else { continue };
            if value.get("type").and_then(|t| t.as_str()) != Some("event") {
                continue;
            }
            let Some(event) = value.get("event") else { continue };
            let seq = event.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
            if seq <= self.baseline_seq {
                continue; // 历史
            }
            self.last_seq = seq;
            let etype = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let data = event.get("data");
            let turn_id = data.and_then(|d| d.get("turn")).and_then(|t| t.as_i64());
            if etype == "turn/end" && self.current_turn.is_some() && turn_id == self.current_turn {
                on_end(data.unwrap_or(&serde_json::Value::Null))?;
            }

            // 账本折叠：模型、候选后端、重试轴与 usage 采样（口径见 `UsageFold::observe`）。
            if let Some((context,key,sid,epoch)) = &self.briefing {
                crate::session_context::observe(context,key,sid,epoch,event)?;
            }
            self.observe_usage(event);
            if (etype == "tool/call" || etype == "tool/result") && turn_id == self.current_turn {
                fold_tool_event(&mut self.tools, event);
            }
            match etype {
                "turn/start" => {
                    on_start()?;
                    self.current_turn = turn_id;
                    self.turn_text.clear();
                    self.turn_usage.clear();
                    self.tools.clear();
                }
                "assistant/message" => {
                    if turn_id.is_some() && turn_id == self.current_turn {
                        if let Some(content) = data
                            .and_then(|d| d.get("message"))
                            .and_then(|m| m.get("content"))
                        {
                            self.turn_text.push_str(&blocks_to_text(content));
                        }
                    }
                }
                "turn/end" if self.current_turn.is_some() && turn_id == self.current_turn => {
                    self.last_ended = data.cloned().unwrap_or(serde_json::Value::Null);
                    return Ok(self.turn_text.trim().to_string());
                }
                _ => {}
            }
        }
    }

    /// 读下一条 live 事件（seq 大于快照游标），原样返回。回合核对用：不解释内容，
    /// 由调用方把它折进自己的事件表。
    /// 把一条（实时）事件折进账本折叠：usage 采样按 (turn, step, retry) 保留 seq 最大的一条。
    pub fn observe_usage(&mut self, event: &serde_json::Value) {
        if let Some(sample) = self.fold.observe(event, now_ms() as i64) {
            push_usage_sample(&mut self.turn_usage, sample);
        }
        self.model = self.fold.model();
        self.provider = self.fold.provider();
    }

    /// 快照与实时流里收到的全部账本事实（供回合核对/恢复时写账）。
    pub fn harvest(&self) -> UsageHarvest {
        let mut out = self.snapshot.harvest.clone();
        for sample in &self.turn_usage {
            push_usage_sample(&mut out.samples, sample.clone());
        }
        out
    }

    pub async fn next_event(&mut self, timeout: std::time::Duration) -> Result<serde_json::Value, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("等事件超时".to_string());
            }
            let msg = match tokio::time::timeout(remaining, self.ws.next()).await {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => return Err(format!("WS 读失败：{e}")),
                Ok(None) => return Err("WS 流结束".to_string()),
                Err(_) => return Err("等事件超时".to_string()),
            };
            let text = match msg {
                Message::Text(t) => t,
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Close(_) => return Err("WS 被关闭".to_string()),
                _ => continue,
            };
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            if frame.get("streamId").and_then(|s| s.as_str()) != Some(self.stream_id.as_str()) {
                continue;
            }
            match frame.get("type").and_then(|t| t.as_str()) {
                Some("error") => {
                    let code = frame.get("error").and_then(|e| e.get("code")).and_then(|c| c.as_str()).unwrap_or("?");
                    return Err(format!("流返回错误：{code}"));
                }
                Some("end") => return Err("流提前结束".to_string()),
                Some("item") => {}
                _ => continue,
            }
            let Some(value) = frame.get("value") else { continue };
            if value.get("type").and_then(|t| t.as_str()) != Some("event") {
                continue;
            }
            let Some(event) = value.get("event") else { continue };
            if event.get("seq").and_then(|s| s.as_i64()).unwrap_or(0) <= self.baseline_seq {
                continue;
            }
            if let Some((context,key,sid,epoch)) = &self.briefing {
                crate::session_context::observe(context,key,sid,epoch,event)?;
            }
            return Ok(event.clone());
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// 诊断用：订阅会话并把收到的帧类型打印出来（不做解析）。
// ── 回合核对：某条 prompt 在 DSH 里到底发生了什么（ticket 10）──────────────────────

/// `projections.values.inbox` 里待处理的用户消息（`next-turn` + `next-step`）。
fn inbox_of_values(values: Option<&serde_json::Value>) -> Option<Vec<serde_json::Value>> {
    let inbox = values?.get("inbox")?;
    let mut items = Vec::new();
    for boundary in ["next-turn", "next-step"] {
        if let Some(list) = inbox.get(boundary).and_then(|l| l.as_array()) {
            items.extend(list.iter().cloned());
        }
    }
    Some(items)
}

/// `session/prompt` 写入的用户消息带 `source.rpcId`（DSH `api-proxy` 的 `prompt`）。
fn rpc_id_of_message(message: &serde_json::Value) -> Option<&str> {
    message.get("source")?.get("rpcId")?.as_str()
}

/// 一条 prompt 在 DSH 里的已知事实。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnFacts {
    pub rpc_id: String,
    pub dsh_turn: i64,
    /// `turn/end` 的 data；`None` = 还没结束。
    pub ended: Option<serde_json::Value>,
    pub tools: Vec<ToolFact>,
    /// Assembled assistant text, also retained when probing a completed turn.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromptProbe {
    /// 问不到 DSH：状态未知，稍后重试，不能当作没执行。
    Unreachable(String),
    /// DSH 说会话不存在：这条 prompt 没有也不可能在该会话里执行。
    SessionGone,
    /// 看了日志与队列，没有这条 prompt。`proved` = 日志没被截断，可以断定没执行。
    NotFound { proved: bool },
    /// 在 inbox 里排队，回合还没开始。
    Queued,
    /// 在队列里被取消（`agent/inbox/spliced` outcome=canceled），从未执行。
    RemovedFromQueue,
    Turn(TurnFacts),
}

/// 只看事件表：这些 rpcId 之一的 prompt 是否已成为回合，回合进行到哪。纯函数，便于单测。
pub fn facts_from_events(events: &[serde_json::Value], rpc_ids: &[String]) -> Option<PromptProbe> {
    let seq_of = |e: &serde_json::Value| e.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
    let ours = |rpc: &str| rpc_ids.iter().any(|id| id == rpc);
    let mut user: Option<(i64, String)> = None;
    for event in events {
        match event.get("type").and_then(|t| t.as_str()) {
            Some("user/message") => {
                if let Some(rpc) = event.get("data").and_then(rpc_id_of_message) {
                    if ours(rpc) {
                        user = Some((seq_of(event), rpc.to_string()));
                    }
                }
            }
            Some("agent/inbox/spliced") => {
                let data = event.get("data");
                let canceled = data.and_then(|d| d.get("outcome")).and_then(|o| o.as_str()) == Some("canceled");
                let mine = data
                    .and_then(|d| d.get("inserted"))
                    .and_then(|i| i.as_array())
                    .is_some_and(|items| items.iter().any(|m| rpc_id_of_message(m).is_some_and(ours)));
                if canceled && mine && user.is_none() {
                    return Some(PromptProbe::RemovedFromQueue);
                }
            }
            _ => {}
        }
    }
    let (user_seq, rpc_id) = user?;
    let start = events
        .iter()
        .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("turn/start") && seq_of(e) > user_seq);
    let Some(start) = start else { return Some(PromptProbe::Queued) };
    let dsh_turn = start.get("data").and_then(|d| d.get("turn")).and_then(|t| t.as_i64()).unwrap_or(-1);
    let mut tools = Vec::new();
    let mut ended = None;
    let mut text = String::new();
    for event in events.iter().filter(|e| seq_of(e) >= seq_of(start)) {
        let turn = event.get("data").and_then(|d| d.get("turn")).and_then(|t| t.as_i64());
        if turn != Some(dsh_turn) {
            continue;
        }
        match event.get("type").and_then(|t| t.as_str()) {
            Some("tool/call" | "tool/result") => fold_tool_event(&mut tools, event),
            Some("turn/end") => ended = event.get("data").cloned(),
            Some("assistant/message") => text.push_str(&blocks_to_text(&event["data"]["message"]["content"])),
            _ => {}
        }
    }
    Some(PromptProbe::Turn(TurnFacts { rpc_id, dsh_turn, ended, tools, text }))
}

/// The error code DSH put in a refusal: `<endpoint> 被拒：<code>: <message>` (RPC) or
/// `订阅被拒：<code>` (follow stream). Only the code is structured; the message is free text.
fn refusal_code(error: &str) -> Option<&str> {
    let (_, rest) = error.split_once("被拒：")?;
    let end = rest.find(|c: char| c == ':' || c == '：' || c.is_whitespace()).unwrap_or(rest.len());
    let code = &rest[..end];
    (!code.is_empty()).then_some(code)
}

/// DSH explicitly says the session does not exist: its exact error code is `session-not-found`
/// (or `session/not-found`). Free text such as "not found", an HTTP 404 or "Method not found" is
/// not this answer and proves nothing about a prompt.
pub fn is_session_gone(error: &str) -> bool {
    matches!(refusal_code(error), Some("session-not-found" | "session/not-found"))
}

/// DSH answered the request with an explicit refusal (`ok:false`), or refused it with 401 after
/// the single re-authentication: the request was not carried out. Transport loss, HTTP errors,
/// unreadable or mismatched responses are not refusals; their outcome is unknown.
pub fn is_refused(error: &str) -> bool {
    refusal_code(error).is_some() || error.starts_with("401 ")
}

/// Loose match for idempotent cleanup only (`stop_session_work`, `archive_session`); never a proof.
fn is_not_found(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("not-found") || lower.contains("not found") || lower.contains("not_found")
}

/// 在 DSH 里查这些 rpcId 的 prompt（同一回合的多次尝试都要查）。
pub async fn probe_prompt(rpc: &Rpc, session_id: &str, rpc_ids: &[String]) -> PromptProbe {
    let mut harvest = UsageHarvest::default();
    probe_prompt_with(rpc, session_id, rpc_ids, &mut harvest).await
}

/// 同 `probe_prompt`，并返回开场快照里的账本事实（历史 usage 采样与基线），恢复核对时用它补写账本。
pub async fn probe_prompt_harvest(rpc: &Rpc, session_id: &str, rpc_ids: &[String]) -> (PromptProbe, UsageHarvest) {
    let mut harvest = UsageHarvest::default();
    let probe = probe_prompt_with(rpc, session_id, rpc_ids, &mut harvest).await;
    (probe, harvest)
}

async fn probe_prompt_with(rpc: &Rpc, session_id: &str, rpc_ids: &[String], harvest: &mut UsageHarvest) -> PromptProbe {
    // 1) 队列：session/projections（非激活读取）
    let mut inbox: Option<Vec<serde_json::Value>> = None;
    match rpc.call("session/projections", serde_json::json!({ "sessionId": session_id })) {
        Ok(value) => {
            // null = 会话不在宿主注册表里（没有活动 agent），没有待处理项。
            inbox = Some(if value.is_null() {
                Vec::new()
            } else {
                inbox_of_values(value.get("values")).unwrap_or_default()
            });
        }
        Err(error) if is_session_gone(&error) => return PromptProbe::SessionGone,
        Err(_) => {}
    }
    // 2) 日志：session/follow 的开场快照
    let mut follow = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        FollowStream::open_with_max(&rpc.base, &rpc.cookie(), session_id, 200),
    )
    .await
    {
        Ok(Ok(f)) => f,
        Ok(Err(error)) if is_session_gone(&error) => return PromptProbe::SessionGone,
        Ok(Err(error)) => return PromptProbe::Unreachable(error),
        Err(_) => return PromptProbe::Unreachable("订阅超时（20s）".to_string()),
    };
    if let Err(error) = follow.track_mapped_briefing(rpc, session_id) {
        follow.close().await;
        return PromptProbe::Unreachable(error);
    }
    *harvest = follow.snapshot.harvest.clone();
    let snapshot = follow.snapshot.clone();
    follow.close().await;
    probe_of_snapshot(&snapshot, inbox, rpc_ids)
}

/// What the queue read and the follow snapshot say about these rpcIds.
fn probe_of_snapshot(snapshot: &Snapshot, inbox: Option<Vec<serde_json::Value>>, rpc_ids: &[String]) -> PromptProbe {
    let inbox = inbox.or_else(|| snapshot.inbox.clone());
    if let Some(found) = facts_from_events(&snapshot.events, rpc_ids) {
        return found;
    }
    let queued = inbox.as_ref().is_some_and(|items| {
        items
            .iter()
            .any(|m| rpc_id_of_message(m).is_some_and(|r| rpc_ids.iter().any(|id| id == r)))
    });
    if queued {
        return PromptProbe::Queued;
    }
    PromptProbe::NotFound { proved: inbox.is_some() && snapshot.covers_whole_log() }
}

/// Drive `future` until it finishes, `deadline` passes ("probe timed out") or `cancelled` turns true
/// ("cancelled", polled at most every 50 ms).
async fn bounded_until<T>(
    future: impl std::future::Future<Output = T>,
    deadline: std::time::Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<T, String> {
    tokio::pin!(future);
    loop {
        if cancelled() {
            return Err("cancelled".to_string());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("probe timed out".to_string());
        }
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = tokio::time::sleep(remaining.min(std::time::Duration::from_millis(50))) => {}
        }
    }
}

/// `probe_prompt` for synchronous jobs (memory digest): every step is bounded by `deadline` and
/// polls `cancelled` at most every 50 ms. A cancelled or timed-out probe is `Unreachable`.
pub fn probe_prompt_bounded(
    rpc: &Rpc,
    session_id: &str,
    rpc_ids: &[String],
    deadline: std::time::Instant,
    cancelled: &dyn Fn() -> bool,
) -> PromptProbe {
    let inbox = match rpc.call_with_cancellation(
        "session/projections",
        serde_json::json!({ "sessionId": session_id }),
        deadline,
        cancelled,
    ) {
        Ok(value) if value.is_null() => Some(Vec::new()),
        Ok(value) => Some(inbox_of_values(value.get("values")).unwrap_or_default()),
        Err(error) if is_session_gone(&error) => return PromptProbe::SessionGone,
        Err(_) if cancelled() => return PromptProbe::Unreachable("cancelled".to_string()),
        Err(_) => None,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => return PromptProbe::Unreachable(format!("probe runtime unavailable: {error}")),
    };
    let cookie = rpc.cookie();
    let opened = runtime.block_on(bounded_until(
        FollowStream::open_with_max(&rpc.base, &cookie, session_id, 200),
        deadline,
        cancelled,
    ));
    let follow = match opened {
        Ok(Ok(follow)) => follow,
        Ok(Err(error)) if is_session_gone(&error) => return PromptProbe::SessionGone,
        Ok(Err(error)) | Err(error) => return PromptProbe::Unreachable(error),
    };
    let snapshot = follow.snapshot.clone();
    // The snapshot is in hand: closing the stream is best effort, never past the deadline or a cancel.
    let _ = runtime.block_on(bounded_until(follow.close(), deadline, cancelled));
    probe_of_snapshot(&snapshot, inbox, rpc_ids)
}

/// 等某条 prompt 的回合结束（重启后的重新接入）。每次都先看快照再等 live 事件，所以
/// 快照与订阅之间结束的回合不会漏。返回最终事实；超时返回 `Err`，状态不明。
pub async fn wait_prompt_outcome(
    rpc: &Rpc,
    session_id: &str,
    rpc_ids: &[String],
    timeout: std::time::Duration,
) -> Result<(PromptProbe, UsageHarvest), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut follow = FollowStream::open_with_max(&rpc.base, &rpc.cookie(), session_id, 200).await?;
    follow.track_mapped_briefing(rpc, session_id)?;
    let mut events = follow.snapshot.events.clone();
    loop {
        match facts_from_events(&events, rpc_ids) {
            Some(PromptProbe::Turn(facts)) if facts.ended.is_some() => {
                let harvest = follow.harvest();
                follow.close().await;
                return Ok((PromptProbe::Turn(facts), harvest));
            }
            Some(PromptProbe::RemovedFromQueue) => {
                let harvest = follow.harvest();
                follow.close().await;
                return Ok((PromptProbe::RemovedFromQueue, harvest));
            }
            _ => {}
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            follow.close().await;
            return Err("等待回合结束超时".to_string());
        }
        match follow.next_event(remaining).await {
            Ok(event) => {
                follow.observe_usage(&event);
                events.push(event);
            }
            Err(error) => {
                follow.close().await;
                return Err(error);
            }
        }
    }
}

/// 停下一个会话里的工作：先清 inbox（`session/cancel` 默认保留 inbox），再取消当前回合。
/// 读不到队列不阻止取消；返回清掉的条数，任何一步失败都在最后报错（与 JS `stopSessionWork` 同序）。
pub fn stop_session_work(rpc: &Rpc, session_id: &str) -> Result<usize, String> {
    let mut failure: Option<String> = None;
    let mut removed = 0usize;
    match rpc.call("session/projections", serde_json::json!({ "sessionId": session_id })) {
        Ok(value) => {
            let items = if value.is_null() {
                Vec::new()
            } else {
                inbox_of_values(value.get("values")).unwrap_or_default()
            };
            for item in items {
                let Some(id) = item.get("id").and_then(|i| i.as_str()) else {
                    failure.get_or_insert_with(|| "inbox 项缺少 id，无法清队列".to_string());
                    continue;
                };
                match rpc.call(
                    "session/updateQueue",
                    serde_json::json!({ "sessionId": session_id, "itemId": id, "action": { "kind": "remove" } }),
                ) {
                    Ok(_) => removed += 1,
                    Err(error) if error.contains("queue-item-not-found") => {}
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        Err(error) if is_not_found(&error) => {}
        Err(error) => {
            failure.get_or_insert(error);
        }
    }
    match rpc.call("session/cancel", serde_json::json!({ "sessionId": session_id })) {
        Ok(_) => {}
        Err(error) if is_not_found(&error) => {}
        Err(error) => {
            failure.get_or_insert(error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(removed),
    }
}

/// 归档会话（只隐藏，不终止工作；调用前先 `stop_session_work`）。
pub fn archive_session(rpc: &Rpc, session_id: &str) -> Result<(), String> {
    match rpc.call("workspace/archiveSession", serde_json::json!({ "sessionId": session_id })) {
        Ok(_) => Ok(()),
        Err(error) if is_not_found(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

pub async fn watch_raw(base: &str, cookie: &str, session_id: &str, secs: u64) -> Result<(), String> {
    let ws_url = format!("{}/api/remote.mux", base.replacen("http://", "ws://", 1));
    let mut request = ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("构造 WS 请求失败：{e}"))?;
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().map_err(|_| "Cookie 头非法".to_string())?);
    let (mut ws, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("连 mux 失败：{e}"))?;
    println!("[watch] WS 已连上");

    let stream_id = mint_request_id();
    let open = serde_json::json!({
        "type": "open",
        "streamId": stream_id,
        "endpoint": "session/follow",
        "payload": { "args": { "request": {
            "address": { "kind": "session", "sessionId": session_id },
            "maxMessages": 10
        } } }
    });
    ws.send(Message::Text(open.to_string())).await.map_err(|e| e.to_string())?;
    println!("[watch] 已发送 open（stream {stream_id}）");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    while !deadline.saturating_duration_since(tokio::time::Instant::now()).is_zero() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let head: String = crate::activity::preview_chars(&t, 400);
                println!("[watch] 帧：{head}");
            }
            Ok(Some(Ok(_))) => println!("[watch] 非文本帧"),
            Ok(Some(Err(e))) => { println!("[watch] 错误：{e}"); break }
            Ok(None) => { println!("[watch] 流结束"); break }
            Err(_) => break,
        }
    }
    let _ = ws.close(None).await;
    Ok(())
}


/// 诊断用：拉一次 snapshot，列出事件类型与该会话里所有 assistant 文本。
pub async fn dump_snapshot(base: &str, cookie: &str, session_id: &str) -> Result<(), String> {
    let ws_url = format!("{}/api/remote.mux", base.replacen("http://", "ws://", 1));
    let mut request = ws_url.as_str().into_client_request().map_err(|e| e.to_string())?;
    request.headers_mut().insert("Cookie", cookie.parse().map_err(|_| "Cookie 非法".to_string())?);
    let (mut ws, _r) = tokio_tungstenite::connect_async(request).await.map_err(|e| e.to_string())?;

    let stream_id = mint_request_id();
    let open = serde_json::json!({
        "type": "open", "streamId": stream_id, "endpoint": "session/follow",
        "payload": { "args": { "request": {
            "address": { "kind": "session", "sessionId": session_id },
            "maxMessages": 200
        } } }
    });
    ws.send(Message::Text(open.to_string())).await.map_err(|e| e.to_string())?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !deadline.saturating_duration_since(tokio::time::Instant::now()).is_zero() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(Ok(msg))) = tokio::time::timeout(remaining, ws.next()).await else { continue };
        let Message::Text(t) = msg else { continue };
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
        let Some(value) = frame.get("value") else { continue };
        if value.get("type").and_then(|x| x.as_str()) != Some("snapshot") { continue };

        let cursor = value.get("cursor").and_then(|c| c.as_i64()).unwrap_or(-1);
        let records = value.get("records").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        println!("[dump] snapshot cursor={cursor}，records={}", records.len());

        for rec in &records {
            let Some(ev) = rec.get("event") else { continue };
            let etype = ev.get("type").and_then(|t| t.as_str()).unwrap_or("?");
            let seq = ev.get("seq").and_then(|s| s.as_i64()).unwrap_or(-1);
            match etype {
                "assistant/message" => {
                    let txt = ev
                        .get("data")
                        .and_then(|d| d.get("message"))
                        .and_then(|m| m.get("content"))
                        .map(blocks_to_text)
                        .unwrap_or_default();
                    let turn = ev.get("data").and_then(|d| d.get("turn")).and_then(|t| t.as_i64()).unwrap_or(-1);
                    println!("  [{seq}] {etype} turn={turn} 文本={txt:?}");
                }
                "turn/start" | "turn/end" => {
                    let turn = ev.get("data").and_then(|d| d.get("turn")).and_then(|t| t.as_i64()).unwrap_or(-1);
                    let reason = ev
                        .get("data")
                        .and_then(|d| d.get("reason"))
                        .map(|r| r.to_string())
                        .unwrap_or_default();
                    println!("  [{seq}] {etype} turn={turn} {reason}");
                }
                "error" => println!("  [{seq}] error {}", ev.get("data").cloned().unwrap_or_default()),
                _ => println!("  [{seq}] {etype}"),
            }
        }
        let _ = ws.close(None).await;
        return Ok(());
    }
    Err("没等到 snapshot".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_exact_session_not_found_code_proves_a_session_is_gone() {
        assert!(is_session_gone("session/projections 被拒：session-not-found: session s1 not found"));
        assert!(is_session_gone("订阅被拒：session-not-found"));
        assert!(is_session_gone("所有候选模型都设置失败：x：session/selectModel 被拒：session/not-found: gone"));
        for text in [
            "连 /api/remote.mux 失败：HTTP error: 404 Not Found",
            "session/projections 被拒：internal: Method not found",
            "session/projections 被拒：internal: session not found in cache",
            "响应不是 JSON（session/projections）：expected value；原文 Not Found",
            "订阅被拒：resource not found",
            "session/projections 被拒：workspace-not-found: x",
            "session-not-found",
        ] {
            assert!(!is_session_gone(text), "{text} must not count as a proof");
        }
    }

    use super::*;

    fn test_rpc(base: String) -> Rpc {
        let credential = Credential::fixed(&base, "dsh-auth-ticket04=synthetic-cookie".into());
        Rpc { base, credential }
    }

    fn accept_test_stream(listener: &std::net::TcpListener) -> std::net::TcpStream {
        listener.set_nonblocking(true).expect("set fake listener nonblocking");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("set fake stream blocking");
                    return stream;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(10035) =>
                {
                    assert!(std::time::Instant::now() < deadline, "fake DSH accept timed out");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("accept fake DSH request: {error}"),
            }
        }
    }

    fn read_test_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut raw = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            if let Some(separator) = find_sub(&raw, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&raw[..separator]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if raw.len() >= separator + 4 + content_length {
                    return raw;
                }
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "timed out reading fake DSH request");
            stream
                .set_read_timeout(Some(remaining))
                .expect("set fake request read timeout");
            match stream.read(&mut buffer) {
                Ok(0) => panic!("client closed before complete fake DSH request"),
                Ok(read) => raw.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("read fake DSH request: {error}"),
            }
        }
    }

    fn test_rpc_request_body(raw: &[u8]) -> serde_json::Value {
        let separator = find_sub(raw, b"\r\n\r\n").expect("request header separator");
        serde_json::from_slice(&raw[separator + 4..]).expect("request JSON body")
    }

    fn write_test_rpc_response(stream: &mut std::net::TcpStream, value: serde_json::Value) {
        let body = value.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write fake DSH response");
    }

    fn test_rpc_success(rpc_id: serde_json::Value, value: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "server-response",
            "rpcId": rpc_id,
            "result": { "ok": true, "value": value }
        })
    }

    #[test]
    fn call_rejects_a_reply_for_another_rpc_id() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake DSH");
        let rpc = test_rpc(format!("http://{}", listener.local_addr().unwrap()));
        let server = std::thread::spawn(move || {
            let mut stream = accept_test_stream(&listener);
            let request = test_rpc_request_body(&read_test_request(&mut stream));
            let wrong_id = format!("{}-other", request["rpcId"].as_str().unwrap());
            write_test_rpc_response(
                &mut stream,
                test_rpc_success(
                    serde_json::json!(wrong_id),
                    serde_json::json!(["unrelated"]),
                ),
            );
        });

        let result = rpc.call("session.list", serde_json::json!({}));
        server.join().expect("join fake DSH server");
        assert_eq!(
            result,
            Err("RPC response identity mismatch（session/list）".into())
        );
    }

    #[test]
    fn every_call_path_requires_a_matching_string_rpc_id() {
        type Call = fn(&Rpc) -> Result<serde_json::Value, String>;
        let calls: [(&str, Call); 6] = [
            ("call", |rpc| {
                rpc.call("session.list", serde_json::json!({}))
            }),
            ("call_with_rpc_id", |rpc| {
                rpc.call_with_rpc_id("session.list", serde_json::json!({}), "ticket57-request")
            }),
            ("call_without_refresh", |rpc| {
                rpc.call_without_refresh("session.list", serde_json::json!({}))
            }),
            ("call_with_deadline", |rpc| {
                rpc.call_with_deadline(
                    "session.list",
                    serde_json::json!({}),
                    std::time::Instant::now() + std::time::Duration::from_secs(2),
                )
            }),
            ("call_with_cancellation", |rpc| {
                rpc.call_with_cancellation(
                    "session.list",
                    serde_json::json!({}),
                    std::time::Instant::now() + std::time::Duration::from_secs(2),
                    &|| false,
                )
            }),
            ("call_with_cancellation_as", |rpc| {
                rpc.call_with_cancellation_as(
                    "session.list",
                    serde_json::json!({}),
                    "ticket57-request",
                    std::time::Instant::now() + std::time::Duration::from_secs(2),
                    &|| false,
                )
            }),
        ];
        for (path, call) in calls {
            for reply in ["matching", "different", "missing", "null", "number"] {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake DSH");
                let rpc = test_rpc(format!("http://{}", listener.local_addr().unwrap()));
                let server = std::thread::spawn(move || {
                    let mut stream = accept_test_stream(&listener);
                    let request = test_rpc_request_body(&read_test_request(&mut stream));
                    let mut response =
                        test_rpc_success(request["rpcId"].clone(), serde_json::json!({"items": []}));
                    match reply {
                        "matching" => {}
                        "different" => {
                            response["rpcId"] = serde_json::json!(format!(
                                "{}-other",
                                request["rpcId"].as_str().unwrap()
                            ))
                        }
                        "missing" => {
                            response.as_object_mut().unwrap().remove("rpcId");
                        }
                        "null" => response["rpcId"] = serde_json::Value::Null,
                        "number" => response["rpcId"] = serde_json::json!(57),
                        _ => unreachable!(),
                    }
                    write_test_rpc_response(&mut stream, response);
                });

                let result = call(&rpc);
                server.join().expect("join fake DSH server");
                let expected = if reply == "matching" {
                    Ok(serde_json::json!({"items": []}))
                } else {
                    Err("RPC response identity mismatch（session/list）".into())
                };
                assert_eq!(result, expected, "{path}: {reply} reply");
            }
        }
    }

    #[test]
    fn chunked_decoding() {
        // "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw), b"hello world".to_vec());
    }

    #[test]
    fn arg_wrappers_match_js() {
        assert_eq!(arg_wrapper("session/list"), Some("_request"));
        assert_eq!(arg_wrapper("session/prompt"), Some("request"));
        assert_eq!(arg_wrapper("agentPresets/read"), Some("request"));
        assert_eq!(arg_wrapper("workspace/archiveSession"), Some("request"));
        assert_eq!(arg_wrapper("session/unknown"), None);
        assert!(takes_no_args("agentPresets/list"));
    }

    #[test]
    fn request_id_is_uuid_shaped() {
        let id = mint_request_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5, "不是 UUID 形状: {id}");
        assert_eq!([8, 4, 4, 4, 12], [parts[0].len(), parts[1].len(), parts[2].len(), parts[3].len(), parts[4].len()]);
        assert!(parts[2].starts_with('4'));
        assert_ne!(id, mint_request_id(), "两次生成不能相同");
    }

    #[test]
    fn bounded_endpoint_requires_literal_loopback_socket_address() {
        assert_eq!(
            bounded_loopback_endpoint("http://127.0.0.1:3080")
                .expect("IPv4 loopback endpoint")
                .1,
            "127.0.0.1:3080".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(
            bounded_loopback_endpoint("http://[::1]:3080")
                .expect("IPv6 loopback endpoint")
                .1,
            "[::1]:3080".parse::<std::net::SocketAddr>().unwrap()
        );
        for base in [
            "http://localhost:3080",
            "http://192.0.2.1:3080",
            "http://127.0.0.1:0",
            "http://127.0.0.1:3080/path",
            "https://127.0.0.1:3080",
        ] {
            assert!(
                bounded_loopback_endpoint(base).is_err(),
                "bounded call must reject {base}"
            );
        }
    }

    #[test]
    fn call_with_deadline_rejects_expired_budget_before_connecting() {
        let rpc = test_rpc("http://127.0.0.1:1".into());
        let deadline = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let error = rpc
            .call_with_deadline("session.modelCatalog", serde_json::json!({}), deadline)
            .unwrap_err();
        assert!(error.contains("deadline exceeded during connect"), "{error}");
    }

    #[test]
    fn call_with_deadline_preserves_plain_http_404_status() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake DSH");
        let base = format!("http://{}", listener.local_addr().expect("read fake DSH address"));
        let rpc = test_rpc(base);
        let server = std::thread::spawn(move || {
            let mut stream = accept_test_stream(&listener);
            let request = read_test_request(&mut stream);
            assert_eq!(
                String::from_utf8_lossy(&request).lines().next().unwrap_or_default(),
                "POST /api/session/modelCatalog HTTP/1.1"
            );
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
                )
                .expect("write plain fake DSH 404");
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let error = rpc
            .call_with_deadline("session.modelCatalog", serde_json::json!({}), deadline)
            .expect_err("plain HTTP 404 must be returned as a classified status");
        assert!(error.contains("HTTP 404"), "{error}");
        assert!(error.contains("session/modelCatalog"), "{error}");
        assert!(!error.contains("响应不是 JSON"), "{error}");
        server.join().expect("join fake DSH 404 server");
    }

    #[test]
    fn call_with_deadline_shares_one_budget_across_fallback_requests() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake DSH");
        let base = format!("http://{}", listener.local_addr().expect("read fake DSH address"));
        let rpc = test_rpc(base);
        let server = std::thread::spawn(move || {
            let mut first = accept_test_stream(&listener);
            let first_request = read_test_request(&mut first);
            assert_eq!(
                String::from_utf8_lossy(&first_request)
                    .lines()
                    .next()
                    .unwrap_or_default(),
                "POST /api/session/modelCatalog HTTP/1.1"
            );
            assert_eq!(
                test_rpc_request_body(&first_request)["method"],
                "session/modelCatalog"
            );
            std::thread::sleep(std::time::Duration::from_millis(60));
            write_test_rpc_response(&mut first, test_rpc_success(test_rpc_request_body(&first_request)["rpcId"].clone(), serde_json::json!([])));
            drop(first);

            let mut fallback = accept_test_stream(&listener);
            let fallback_request = read_test_request(&mut fallback);
            assert_eq!(
                String::from_utf8_lossy(&fallback_request)
                    .lines()
                    .next()
                    .unwrap_or_default(),
                "POST /api/session/models HTTP/1.1"
            );
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = fallback.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                )
                .as_bytes(),
            );
        });

        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_millis(180);
        let catalog = rpc
            .call_with_deadline("session.modelCatalog", serde_json::json!({}), deadline)
            .expect("catalog request should finish within the shared deadline");
        assert_eq!(catalog, serde_json::json!([]));
        let fallback_error = rpc
            .call_with_deadline("session.models", serde_json::json!({}), deadline)
            .expect_err("fallback must use the original absolute deadline");
        assert!(
            fallback_error.contains("deadline exceeded"),
            "fallback error should identify the shared deadline: {fallback_error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(260),
            "both requests exceeded their shared deadline: {:?}",
            started.elapsed()
        );
        server.join().expect("join stalled fake DSH server");
    }
}
