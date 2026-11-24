//! OneBot 发送链路（P2-B）：文本分条、CQ 转义、限速。
//!
//! 与 JS 版对齐：
//!   端点   POST {httpUrl}/{send_group_msg|send_private_msg}
//!   参数   { group_id | user_id, message: [{type:"text", data:{text}}] }
//!   鉴权   Authorization: Bearer <accessToken>
//!   成功   { status:"ok", retcode:0 }
//!   转义   只把 "[CQ:" 换成 "[CQ："（与 escapeCqText 一致）
//!   分条   maxMessageChars（默认 800）
//!   限速   maxSendPerMinute=8、maxSendPerHour=60
//!   间隔   gapBaseMs + gapPerCharMs * 字数，上限 maxGapMs

#[cfg(test)]
thread_local! {
    // A response-barrier fixture may need longer than the cosmetic production deadline for fsync.
    pub(crate) static INPUT_STATUS_TEST: std::cell::RefCell<Option<(
        std::time::Duration, std::sync::Arc<std::sync::atomic::AtomicBool>
    )>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct InputStatusReturned(Option<std::sync::Arc<std::sync::atomic::AtomicBool>>);
#[cfg(test)]
impl Drop for InputStatusReturned {
    fn drop(&mut self) {
        if let Some(returned) = &self.0 {
            returned.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// 发送目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Group(i64),
    Private(i64),
}

impl Target {
    pub fn action(&self) -> &'static str {
        match self {
            Target::Group(_) => "send_group_msg",
            Target::Private(_) => "send_private_msg",
        }
    }
    /// 会话映射用的键（P5 的 state.sessions 用，目前还没接上）。
    #[allow(dead_code)]
    pub fn key(&self) -> String {
        match self {
            Target::Group(id) => format!("group:{id}"),
            Target::Private(id) => format!("private:{id}"),
        }
    }
}

/// 与 JS 的 escapeCqText 等价：只处理 `[CQ:` 前缀，避免消息里出现 CQ 码。
pub fn escape_cq(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if lower[i..].starts_with("[cq:") {
            out.push_str("[CQ："); // 全角冒号，破坏 CQ 码语法
            i += 4;
        } else {
            // 按字符推进（不能用字节，中文会乱）
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 按字符数分条（不切断 UTF-8，不切断已有的换行边界优先）。
/// 按 QQ 单条消息长度上限切分。**与 JS `md-to-plain.js` 的 `splitForQQ` 等价。**
///
/// 原来的实现是硬切（每 max 个字符切一刀），和 JS 差三件事：
///   1. JS **优先在换行处切**（`lastIndexOf('\n', max-1)`），不会把一句话从中间劈开；
///   2. JS 把换行符**留给前一段**，拼接后不丢换行；
///   3. JS 按 **UTF-16 码元**计数（QQ 的上限就是这个口径），
///      而 Rust 按标量值数 —— 一个 emoji 在 JS 里算 2 个，在 Rust 里只算 1 个，
///      于是 emoji 多的消息会被切得比预期长，可能撞 QQ 的上限。
///
/// Count UTF-16 units while keeping Unicode scalars and supported QQ face tokens atomic.
/// An atom larger than the configured limit occupies its own part to ensure forward progress.
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let safe_max = max_chars.max(1);
    let mut rest = text;
    let mut out: Vec<String> = Vec::new();
    while !rest.is_empty() {
        let mut end = 0;
        let mut units = 0;
        let mut newline = None;
        for atom in crate::outbound_message::atoms(rest) {
            let size = atom.encode_utf16().count();
            if units + size > safe_max && end > 0 {
                break;
            }
            if atom == "\n" && end > 0 {
                newline = Some(end + atom.len());
            }
            end += atom.len();
            units += size;
            if units >= safe_max {
                break;
            }
        }
        if end < rest.len() {
            end = newline.unwrap_or(end);
        }
        out.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    // 空输入 → **空数组**（不是 [""]）。JS 就是这样，而且 `[""]` 会让 send_text
    // 真的去发一条空消息 —— 那比"什么都不发"糟糕得多。
    out
}
/// 条间延迟：gapBaseMs + gapPerCharMs * 上一条字数，封顶 maxGapMs。
pub fn gap_ms(last_len: usize, base: u64, per_char: u64, cap: u64) -> u64 {
    let raw = base.saturating_add(per_char.saturating_mul(last_len as u64));
    raw.min(if cap == 0 { u64::MAX } else { cap })
}

/// 滑动窗口限速器（分钟/小时两个窗口）。
#[derive(Debug)]
pub struct RateLimiter {
    pub max_per_minute: usize,
    pub max_per_hour: usize,
    sent: Vec<u128>,
}

impl RateLimiter {
    pub fn new(max_per_minute: usize, max_per_hour: usize) -> Self {
        Self { max_per_minute, max_per_hour, sent: Vec::new() }
    }

    /// 现在能不能发；不能的话返回还要等多少毫秒。
    pub fn check(&mut self, now: u128) -> Result<(), u128> {
        self.sent.retain(|t| now.saturating_sub(*t) < 3_600_000);
        let in_minute = self.sent.iter().filter(|t| now.saturating_sub(**t) < 60_000).count();
        let in_hour = self.sent.len();
        if self.max_per_hour > 0 && in_hour >= self.max_per_hour {
            let oldest = self.sent.first().copied().unwrap_or(now);
            return Err((oldest + 3_600_000).saturating_sub(now));
        }
        if self.max_per_minute > 0 && in_minute >= self.max_per_minute {
            let cutoff = now.saturating_sub(60_000);
            let oldest_in_minute = self
                .sent
                .iter()
                .filter(|t| **t > cutoff)
                .min()
                .copied()
                .unwrap_or(now);
            return Err((oldest_in_minute + 60_000).saturating_sub(now));
        }
        Ok(())
    }

    pub fn record(&mut self, now: u128) {
        self.sent.push(now);
    }
}

/// Outcome of a failed send attempt, as far as the outbox needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    NotCommitted(String),
    Unknown(String),
}

/// OneBot v11 HTTP error codes: the gateway rejected the request itself (bad request, bad
/// token, forbidden, unknown action) before running the send action, so nothing was delivered.
const REFUSAL_HTTP_STATUS: [u16; 5] = [400, 401, 403, 404, 426];
/// The same refusals as `retcode` in the OneBot envelope (1400/1401/1403/1404).
const REFUSAL_RETCODES: [i64; 4] = [1400, 1401, 1403, 1404];

/// Error text of a failed OneBot send reply; a group-mute refusal carries `MUTE_REFUSAL` (#46).
fn send_failure_text(v: &serde_json::Value, retcode: i64) -> String {
    let wording = v.get("wording").and_then(|w| w.as_str()).unwrap_or("");
    let marker = if is_mute_reply(v) { MUTE_REFUSAL } else { "" };
    format!("OneBot 发送失败：retcode={retcode} {wording}{marker}")
}

/// Marker in a not-committed error for a group-mute refusal (#46).
pub(crate) const MUTE_REFUSAL: &str = "（被禁言 result=120）";

/// OneBot retcode 100 carrying QQ `result=120`: she is muted in that group and QQ refused the
/// message before delivery, so it is a definite refusal (#46). Retcode 100 without it stays unknown.
pub(crate) fn is_mute_reply(v: &serde_json::Value) -> bool {
    if v.get("retcode").and_then(|r| r.as_i64()) != Some(100) { return false; }
    v.pointer("/data/result").and_then(|r| r.as_i64()) == Some(120)
        || ["message", "wording", "msg"].iter()
            .any(|f| v.get(*f).and_then(|s| s.as_str()).is_some_and(has_result_120))
}

fn has_result_120(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.match_indices("result").any(|(at, _)| {
        let rest = lower[at + 6..].trim_start();
        let Some(rest) = rest.strip_prefix(['=', ':', '：']) else { return false };
        let rest = rest.trim_start();
        rest.starts_with("120") && !rest[3..].starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Whether a not-committed error came from a group-mute refusal.
pub(crate) fn is_mute_refusal(error: &str) -> bool { error.contains(MUTE_REFUSAL) }

/// Classify a send reply that arrived in full.
///
/// The frozen JS bridge treats every non-`ok`/non-zero reply as a failed send and never retries,
/// so it has no classification to copy. OneBot gives no guarantee for other failure codes: some
/// gateways (NapCat style, retcode 100/200/timeouts) report a failure after the message already
/// went out, and `status:"async"` (retcode 1) only means queued. Only the request-level refusals
/// above are therefore `NotCommitted`; every other failure is `Unknown`, which is verified
/// against history and never resent automatically.
///
/// Retcode 100 with `result=120` (muted) is also `NotCommitted`; retcode 100 without it stays `Unknown`.
pub(crate) fn classify_send_reply(http_status: u16, raw: &str) -> Result<serde_json::Value, SendError> {
    if REFUSAL_HTTP_STATUS.contains(&http_status) {
        return Err(SendError::NotCommitted(format!("OneBot 拒绝请求：HTTP {http_status}")));
    }
    if http_status != 0 && !(200..300).contains(&http_status) {
        return Err(SendError::Unknown(format!("OneBot 返回 HTTP {http_status}；原文 {}", crate::activity::preview_chars(raw, 200))));
    }
    let v: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| SendError::Unknown(format!("响应不是 JSON：{e}；原文 {}", crate::activity::preview_chars(raw, 200))))?;
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let retcode = v.get("retcode").and_then(|r| r.as_i64());
    if status == "ok" && retcode == Some(0) {
        return Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null));
    }
    let wording = v.get("wording").and_then(|w| w.as_str()).unwrap_or("");
    let message = format!("OneBot 发送失败：retcode={} {wording}", retcode.unwrap_or(-1));
    if is_mute_reply(&v) {
        return Err(SendError::NotCommitted(format!("{message}{MUTE_REFUSAL}")));
    }
    if retcode.is_some_and(|code| REFUSAL_RETCODES.contains(&code)) {
        Err(SendError::NotCommitted(message))
    } else {
        Err(SendError::Unknown(message))
    }
}

/// OneBot HTTP 客户端。
#[derive(Clone)]
pub struct OneBot {
    #[cfg(test)]
    host: String,
    #[cfg(test)]
    port: u16,
    token: String,
    endpoint: Option<crate::loopback_http::LoopbackEndpoint>,
    strict: bool,
    transport: std::sync::Arc<dyn crate::loopback_http::Transport>,
}

pub(crate) struct PendingResponse(crate::loopback_http::PendingHttpResponse);

impl PendingResponse {
    pub(crate) fn outcome(self) -> Result<serde_json::Value, SendError> {
        let reply = self
            .0
            .read(crate::loopback_http::ResponseLimits::new(
                crate::loopback_http::DEFAULT_MAX_BODY_BYTES,
            ))
            .map_err(|err| SendError::Unknown(format!("OneBot HTTP 连接失败：{}", err.detail)))?;
        let raw = reply
            .body_utf8()
            .map_err(|detail| SendError::Unknown(format!("OneBot HTTP 连接失败：{detail}")))?;
        classify_send_reply(reply.status, raw)
    }
}

#[cfg(test)]
mod bounded_media_tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn hf119_typing_status_returns_when_gateway_never_answers() {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port=listener.local_addr().unwrap().port();
        let (release,held)=std::sync::mpsc::channel();
        let server=std::thread::spawn(move || {
            let (mut stream,_)=listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let mut request=Vec::new();
            loop {
                let mut buf=[0;1024];
                let n=stream.read(&mut buf).unwrap();
                if n==0 {break;}
                request.extend_from_slice(&buf[..n]);
                if let Some(split)=request.windows(4).position(|w|w==b"\r\n\r\n") {
                    let head=String::from_utf8_lossy(&request[..split]);
                    let size=head.lines().find_map(|l|l.strip_prefix("Content-Length: ").and_then(|n|n.parse::<usize>().ok())).unwrap();
                    if request.len()>=split+4+size {break;}
                }
            }
            let _=held.recv_timeout(std::time::Duration::from_secs(3));
            String::from_utf8(request).unwrap()
        });
        let ob=OneBot::try_new_url(&format!("http://127.0.0.1:{port}"),"synthetic".into()).unwrap();
        let start=std::time::Instant::now();
        ob.set_input_status(7707,1);
        let elapsed=start.elapsed();
        release.send(()).unwrap();
        let request=server.join().unwrap();
        assert!(request.starts_with("POST /set_input_status HTTP/1.1"));
        assert!(elapsed<std::time::Duration::from_secs(2),"cosmetic status blocked: {elapsed:?}");
    }
    #[test]
    // Exercise the bounded wire reader through its gateway request interface.
    // The peer stays open, so success requires stopping at the byte cap.
    fn review_media_gateway_reply_stops_at_wire_cap() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, held) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n").unwrap();
            let _ = stream.write_all(&[b'A'; 32 * 1024]);
            let _ = held.recv_timeout(std::time::Duration::from_secs(3));
        });
        let ob = OneBot::try_new_url(&format!("http://127.0.0.1:{port}"), "synthetic".into()).unwrap();
        let started = std::time::Instant::now();
        assert!(ob.request_bounded("get_image", serde_json::json!({"file":"image.png"}), 1024).is_none());
        let elapsed = started.elapsed();
        release.send(()).unwrap(); server.join().unwrap();
        assert!(elapsed < std::time::Duration::from_secs(2), "read past cap until the peer closed: {elapsed:?}");
    }
}

#[cfg(test)]
mod voice_send_tests {
    use super::*;
    use std::io::{Read, Write};

    /// A gateway that reads the request and then stays silent: the send outcome is unknown, never a refusal.
    #[test]
    fn voice_send_that_times_out_waiting_for_the_reply_is_unknown() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf).unwrap();
            let _ = held.recv_timeout(std::time::Duration::from_secs(5));
            let _ = stream.write_all(b"");
        });
        let ob = OneBot::try_new_url(&format!("http://127.0.0.1:{port}"), "t".into()).unwrap();
        let started = std::time::Instant::now();
        let outcome = ob.send_record_outcome_within(&Target::Group(1), "/x/a.mp3", std::time::Duration::from_millis(300));
        let elapsed = started.elapsed();
        release.send(()).unwrap();
        server.join().unwrap();
        match outcome {
            Err(SendError::Unknown(message)) => assert!(message.contains("read response"), "{message}"),
            other => panic!("expected an unknown outcome, got {other:?}"),
        }
        assert!(elapsed < std::time::Duration::from_secs(3), "the read deadline applies: {elapsed:?}");
    }

    /// Nothing listens: the request never reached the gateway, so the voice was not delivered.
    #[test]
    fn voice_send_that_cannot_connect_is_not_committed() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ob = OneBot::try_new_url(&format!("http://127.0.0.1:{port}"), "t".into()).unwrap();
        assert!(matches!(ob.send_record_outcome(&Target::Private(2), "/x/a.mp3"), Err(SendError::NotCommitted(_))));
    }
}

impl OneBot {
    pub fn new(http_url: &str, token: String) -> Self {
        let base = http_url.trim_end_matches('/');
        let endpoint = crate::loopback_http::LoopbackEndpoint::parse(base).ok();
        #[cfg(test)]
        let (host, port) = match &endpoint {
            Some(ep) => (ep.host().to_string(), ep.port()),
            None => {
                let authority = base
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .unwrap_or("127.0.0.1");
                let host = authority.split(':').next().unwrap_or("127.0.0.1").to_string();
                let port = authority
                    .split(':')
                    .nth(1)
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(3000);
                (host, port)
            }
        };
        Self {
            #[cfg(test)]
            host,
            #[cfg(test)]
            port,
            token,
            endpoint,
            strict: false,
            transport: std::sync::Arc::new(crate::loopback_http::TcpTransport),
        }
    }

    /// Build a client for an explicitly selected loopback endpoint.
    ///
    /// Runtime-root callers must not inherit the legacy port-3000 fallback or
    /// resolve a hostname outside the selected dependency contract. This parser
    /// deliberately accepts only literal loopback IP addresses and an explicit
    /// non-zero port; IPv6 authorities keep their brackets for both connect and
    /// Host header formatting.
    pub fn try_new_url(http_url: &str, token: String) -> Result<Self, String> {
        let ep = crate::loopback_http::LoopbackEndpoint::parse(http_url)?;
        Ok(Self::from_endpoint(
            ep,
            token,
            std::sync::Arc::new(crate::loopback_http::TcpTransport),
        ))
    }

    pub(crate) fn try_new_authority(authority: &str, token: String) -> Result<Self, String> {
        let ep = crate::loopback_http::LoopbackEndpoint::parse_authority(authority)?;
        Ok(Self::from_endpoint(
            ep,
            token,
            std::sync::Arc::new(crate::loopback_http::TcpTransport),
        ))
    }

    #[cfg(test)]
    pub(crate) fn with_transport(
        http_url: &str,
        token: String,
        transport: std::sync::Arc<dyn crate::loopback_http::Transport>,
    ) -> Result<Self, String> {
        let ep = crate::loopback_http::LoopbackEndpoint::parse(http_url)?;
        Ok(Self::from_endpoint(ep, token, transport))
    }

    fn from_endpoint(
        ep: crate::loopback_http::LoopbackEndpoint,
        token: String,
        transport: std::sync::Arc<dyn crate::loopback_http::Transport>,
    ) -> Self {
        Self {
            #[cfg(test)]
            host: ep.host().to_string(),
            #[cfg(test)]
            port: ep.port(),
            token,
            endpoint: Some(ep),
            strict: true,
            transport,
        }
    }

    /// 调任意 OneBot action（贴纸同步等非发送类操作要用）。
    pub fn request(&self, action: &str, params: serde_json::Value) -> Option<serde_json::Value> {
        let reply = self
            .post_raw(
                &format!("/{action}"),
                &params.to_string(),
                crate::loopback_http::Deadlines::split(
                    crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                    crate::loopback_http::DEFAULT_WRITE_TIMEOUT,
                    crate::loopback_http::DEFAULT_READ_TIMEOUT,
                ),
                crate::loopback_http::ResponseLimits::new(
                    crate::loopback_http::DEFAULT_MAX_BODY_BYTES,
                ),
            )
            .ok()?;
        reply
            .parse_json::<serde_json::Value>(
                crate::loopback_http::JsonSurrogates::ReplaceLoneWithFffd,
            )
            .ok()
            .map(|(v, _)| v)
    }

    /// Like `request`, with a longer read deadline (voice readback waits up to 30 s).
    pub fn request_with_timeout(
        &self,
        action: &str,
        params: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Option<serde_json::Value> {
        let reply = self
            .post_raw(
                &format!("/{action}"),
                &params.to_string(),
                crate::loopback_http::Deadlines::split(
                    crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                    crate::loopback_http::DEFAULT_WRITE_TIMEOUT,
                    timeout,
                ),
                crate::loopback_http::ResponseLimits::new(
                    crate::loopback_http::DEFAULT_MAX_BODY_BYTES,
                ),
            )
            .ok()?;
        reply
            .parse_json::<serde_json::Value>(
                crate::loopback_http::JsonSurrogates::ReplaceLoneWithFffd,
            )
            .ok()
            .map(|(v, _)| v)
    }

    /// Read-only metadata request, bounded across connect/write/read by one absolute deadline.
    pub fn request_with_deadline(
        &self,
        action: &str,
        params: serde_json::Value,
        deadline: std::time::Instant,
    ) -> Result<serde_json::Value, String> {
        if !self.strict {
            return Err("a selected loopback endpoint is required".into());
        }
        let total = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| "request deadline exceeded".to_string())?;
        let reply = self
            .post_raw(
                &format!("/{action}"),
                &params.to_string(),
                crate::loopback_http::Deadlines::within(
                    total,
                    crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                ),
                crate::loopback_http::ResponseLimits::new(64 * 1024),
            )
            .map_err(|err| err.detail)?;
        if reply.status != 200 {
            return Err(format!("gateway returned HTTP {}", reply.status));
        }
        reply
            .parse_json::<serde_json::Value>(
                crate::loopback_http::JsonSurrogates::ReplaceLoneWithFffd,
            )
            .map(|(v, _)| v)
            .map_err(|_| "gateway response is not JSON".into())
    }

    /// Cosmetic typing status: bounded connect/write/read, never a prerequisite for a send.
    pub fn set_input_status(&self, user_id: i64, event_type: u8) {
        if self.endpoint.is_none() {
            return;
        }
        let budget = std::time::Duration::from_millis(250);
        #[cfg(test)]
        let (budget, returned) = INPUT_STATUS_TEST.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|(budget, returned)| (*budget, Some(returned.clone())))
                .unwrap_or((budget, None))
        });
        #[cfg(test)]
        let _returned = InputStatusReturned(if event_type == 0 { returned } else { None });
        let body = serde_json::json!({"user_id": user_id, "event_type": event_type}).to_string();
        let _ = self.post_raw(
            "/set_input_status",
            &body,
            crate::loopback_http::Deadlines::within(
                budget,
                std::time::Duration::from_millis(250),
            ),
            crate::loopback_http::ResponseLimits::new(4096),
        );
    }

    /// Send one voice (`record` segment holding a local file path) and classify the outcome like
    /// `send_one_outcome`. The gateway converts to SILK before answering, so the deadline is 60 s.
    pub fn send_record_outcome(
        &self,
        target: &Target,
        file: &str,
    ) -> Result<serde_json::Value, SendError> {
        self.send_record_outcome_within(target, file, std::time::Duration::from_secs(60))
    }

    fn send_record_outcome_within(
        &self,
        target: &Target,
        file: &str,
        deadline: std::time::Duration,
    ) -> Result<serde_json::Value, SendError> {
        let segments = serde_json::json!([{ "type": "record", "data": { "file": file } }]);
        let payload = match target {
            Target::Group(id) => serde_json::json!({ "group_id": id, "message": segments }),
            Target::Private(id) => serde_json::json!({ "user_id": id, "message": segments }),
        };
        self.post_classified_outcome(
            &format!("/{}", target.action()),
            &payload.to_string(),
            crate::loopback_http::Deadlines::split(
                crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                crate::loopback_http::DEFAULT_WRITE_TIMEOUT,
                deadline,
            ),
        )
    }

    /// Media replies must be bounded on the wire, before JSON/base64 copies.
    pub fn request_bounded(
        &self,
        action: &str,
        params: serde_json::Value,
        max_bytes: usize,
    ) -> Option<serde_json::Value> {
        let reply = self
            .post_raw(
                &format!("/{action}"),
                &params.to_string(),
                crate::loopback_http::Deadlines::split(
                    crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                    crate::loopback_http::DEFAULT_WRITE_TIMEOUT,
                    crate::loopback_http::DEFAULT_READ_TIMEOUT,
                ),
                crate::loopback_http::ResponseLimits::new(max_bytes),
            )
            .ok()?;
        reply
            .parse_json::<serde_json::Value>(
                crate::loopback_http::JsonSurrogates::ReplaceLoneWithFffd,
            )
            .ok()
            .map(|(v, _)| v)
    }

    pub(crate) fn post_raw(
        &self,
        path: &str,
        body: &str,
        deadlines: crate::loopback_http::Deadlines,
        limits: crate::loopback_http::ResponseLimits,
    ) -> Result<crate::loopback_http::HttpResponse, crate::loopback_http::HttpError> {
        self.transmit_http("POST", path, Some(body.as_bytes()), deadlines)?
            .read(limits)
    }

    pub(crate) fn get_raw_path(
        &self,
        path: &str,
        timeout: std::time::Duration,
        max_body_bytes: usize,
    ) -> Result<crate::loopback_http::HttpResponse, crate::loopback_http::HttpError> {
        self.transmit_http(
            "GET",
            path,
            None,
            crate::loopback_http::Deadlines::with_shared_transmit(
                std::time::Duration::from_secs(10),
                timeout,
            ),
        )?
        .read(crate::loopback_http::ResponseLimits::new(max_body_bytes))
    }

    fn transmit_raw(
        &self,
        path: &str,
        body: &str,
    ) -> Result<crate::loopback_http::PendingHttpResponse, crate::loopback_http::HttpError> {
        self.transmit_http(
            "POST",
            path,
            Some(body.as_bytes()),
            crate::loopback_http::Deadlines::split(
                crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                crate::loopback_http::DEFAULT_WRITE_TIMEOUT,
                crate::loopback_http::DEFAULT_READ_TIMEOUT,
            ),
        )
    }

    fn transmit_http(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        deadlines: crate::loopback_http::Deadlines,
    ) -> Result<crate::loopback_http::PendingHttpResponse, crate::loopback_http::HttpError> {
        let endpoint = self.require_endpoint()?;
        let auth = self.bearer_header();
        let mut headers = Vec::new();
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        if let Some(auth) = auth.as_deref() {
            headers.push(("Authorization", auth));
        }
        let req = crate::loopback_http::HttpRequest {
            method,
            path,
            headers: &headers,
            body,
        };
        crate::loopback_http::transmit(&*self.transport, endpoint, &req, deadlines)
    }

    fn require_endpoint(&self) -> Result<&crate::loopback_http::LoopbackEndpoint, crate::loopback_http::HttpError> {
        self.endpoint.as_ref().ok_or_else(|| {
            crate::loopback_http::HttpError::not_committed(
                false,
                "OneBot endpoint must be a valid loopback HTTP URL",
            )
        })
    }

    fn bearer_header(&self) -> Option<String> {
        (!self.token.is_empty()).then(|| format!("Bearer {}", self.token))
    }

    fn post_classified_outcome(
        &self,
        path: &str,
        body: &str,
        deadlines: crate::loopback_http::Deadlines,
    ) -> Result<serde_json::Value, SendError> {
        let reply = self
            .post_raw(
                path,
                body,
                deadlines,
                crate::loopback_http::ResponseLimits::new(
                    crate::loopback_http::DEFAULT_MAX_BODY_BYTES,
                ),
            )
            .map_err(|err| {
                let message = self.send_connection_error(err.detail);
                if err.status == crate::loopback_http::CommitStatus::Unknown {
                    SendError::Unknown(message)
                } else {
                    SendError::NotCommitted(message)
                }
            })?;
        let raw = reply
            .body_utf8()
            .map_err(|detail| SendError::Unknown(self.send_connection_error(detail)))?;
        classify_send_reply(reply.status, raw)
    }

    fn post_detailed(&self, path: &str, body: &str) -> Result<String, String> {
        let reply = self
            .post_raw(
                path,
                body,
                crate::loopback_http::Deadlines::split(
                    crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                    crate::loopback_http::DEFAULT_WRITE_TIMEOUT,
                    crate::loopback_http::DEFAULT_READ_TIMEOUT,
                ),
                crate::loopback_http::ResponseLimits::new(
                    crate::loopback_http::DEFAULT_MAX_BODY_BYTES,
                ),
            )
            .map_err(|err| err.detail)?;
        reply.body_utf8().map(str::to_string)
    }

    fn send_connection_error(&self, detail: String) -> String {
        if self.strict {
            format!("OneBot HTTP 连接失败：{detail}")
        } else {
            "OneBot HTTP 连接失败".to_string()
        }
    }

    /// 发一条带引用/@ 的消息（不分条）。`reply_to` / `at_user` 为 None 时不带该段。
    ///
    /// 段顺序与 JS 的 `sendMessagesV2` 一致：**引用在最前，@ 在正文前**。
    /// 顺序错了 QQ 会把 @ 显示成正文的一部分。
    pub fn send_one_with(
        &self,
        target: &Target,
        text: &str,
        reply_to: Option<i64>,
        at_user: Option<i64>,
    ) -> Result<serde_json::Value, String> {
        let payload = Self::segment_payload(target, text, reply_to, at_user);
        let raw = self
            .post_detailed(&format!("/{}", target.action()), &payload.to_string())
            .map_err(|error| self.send_connection_error(error))?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("响应不是 JSON：{e}；原文 {}", crate::activity::preview_chars(&raw, 200)))?;
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
        let retcode = v.get("retcode").and_then(|r| r.as_i64()).unwrap_or(-1);
        if status != "ok" || retcode != 0 {
            return Err(send_failure_text(&v, retcode));
        }
        Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }
    /// Send one message and classify the outcome for the durable outbox.
    ///
    /// `NotCommitted`: the gateway refused it or the request never reached it, so resending
    /// cannot duplicate a message. `Unknown`: the request was written but no usable answer came
    /// back (lost reply, timeout, malformed body); the gateway may have delivered it, so the caller
    /// must verify before any retry. Payload shape and segment order match `send_one_with`.
    pub fn send_one_outcome(
        &self,
        target: &Target,
        text: &str,
        reply_to: Option<i64>,
        at_user: Option<i64>,
    ) -> Result<serde_json::Value, SendError> {
        let payload = Self::segment_payload(target, text, reply_to, at_user);
        self.send_payload_outcome(target.action(), &payload)
    }

    pub(crate) fn send_payload_outcome(
        &self,
        action: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, SendError> {
        self.post_classified_outcome(
            &format!("/{action}"),
            &payload.to_string(),
            crate::loopback_http::Deadlines::split(
                crate::loopback_http::DEFAULT_CONNECT_TIMEOUT,
                crate::loopback_http::DEFAULT_WRITE_TIMEOUT,
                crate::loopback_http::DEFAULT_READ_TIMEOUT,
            ),
        )
    }

    pub(crate) fn transmit_payload(
        &self,
        action: &str,
        payload: &serde_json::Value,
    ) -> Result<PendingResponse, SendError> {
        self.transmit_raw(&format!("/{action}"), &payload.to_string())
            .map(PendingResponse)
            .map_err(|err| {
                let message = self.send_connection_error(err.detail);
                if err.status == crate::loopback_http::CommitStatus::Unknown {
                    SendError::Unknown(message)
                } else {
                    SendError::NotCommitted(message)
                }
            })
    }

    pub(crate) fn segment_payload(
        target: &Target,
        text: &str,
        reply_to: Option<i64>,
        at_user: Option<i64>,
    ) -> serde_json::Value {
        let mut segs: Vec<serde_json::Value> = Vec::new();
        if let Some(id) = reply_to {
            segs.push(serde_json::json!({ "type": "reply", "data": { "id": id.to_string() } }));
        }
        if let Some(qq) = at_user {
            segs.push(serde_json::json!({ "type": "at", "data": { "qq": qq.to_string() } }));
        }
        segs.extend(crate::outbound_message::segments(text));
        match target {
            Target::Group(id) => serde_json::json!({ "group_id": id, "message": segs }),
            Target::Private(id) => serde_json::json!({ "user_id": id, "message": segs }),
        }
    }

    /// 发一条（不分条）。返回 OneBot 的 data。
    pub fn send_one(&self, target: &Target, text: &str) -> Result<serde_json::Value, String> {
        let payload = match target {
            Target::Group(id) => serde_json::json!({
                "group_id": id,
                "message": [{ "type": "text", "data": { "text": escape_cq(text) } }]
            }),
            Target::Private(id) => serde_json::json!({
                "user_id": id,
                "message": [{ "type": "text", "data": { "text": escape_cq(text) } }]
            }),
        };
        let raw = self
            .post_detailed(&format!("/{}", target.action()), &payload.to_string())
            .map_err(|error| self.send_connection_error(error))?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("响应不是 JSON：{e}；原文 {}", crate::activity::preview_chars(&raw, 200)))?;
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
        let retcode = v.get("retcode").and_then(|r| r.as_i64()).unwrap_or(-1);
        if status != "ok" || retcode != 0 {
            return Err(send_failure_text(&v, retcode));
        }
        Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// 发一段文本：自动分条、按字数延迟、遵守分钟/小时限速。
    ///
    /// 返回每条消息的 data（message_id 等）。任何一条失败都会中断并返回错误。
    pub fn send_text(
        &self,
        target: &Target,
        text: &str,
        tune: &SendTuning,
        limiter: &mut RateLimiter,
    ) -> Result<Vec<serde_json::Value>, String> {
        let canonical = crate::outbound_message::normalize(text);
        let parts: Vec<_> = split_message(&canonical, tune.max_chars).into_iter()
            .map(|part| part.trim().to_owned()).filter(|part| !part.is_empty()).collect();
        let mut out = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            // 限速：发之前先确认
            let now = now_ms();
            if let Err(wait) = limiter.check(now) {
                std::thread::sleep(std::time::Duration::from_millis(wait as u64));
            }
            let data = self.send_one_with(target, part, None, None)?;
            limiter.record(now_ms());
            out.push(data);
            // 条间延迟：按上一条字数算，避免连发触发风控
            if i + 1 < parts.len() {
                let wait = gap_ms(part.chars().count(), tune.gap_base_ms, tune.gap_per_char_ms, tune.max_gap_ms);
                if wait > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                }
            }
        }
        Ok(out)
    }
}

/// 发送节奏参数（与 config.json 的 socialV2.send 对应）。
#[derive(Debug, Clone)]
pub struct SendTuning {
    pub max_chars: usize,
    pub gap_base_ms: u64,
    pub gap_per_char_ms: u64,
    pub max_gap_ms: u64,
}

impl Default for SendTuning {
    fn default() -> Self {
        Self::from_config(&serde_json::Value::Null)
    }
}

impl SendTuning {
    /// 从 config.json 的 socialV2.send 读出节奏参数。
    pub fn from_config(cfg: &serde_json::Value) -> Self {
        use crate::settings::send::{GAP_BASE, GAP_MAX, GAP_PER_CHAR, MAX_CHARS};
        Self {
            max_chars: MAX_CHARS.integer(cfg) as usize,
            gap_base_ms: GAP_BASE.integer(cfg) as u64,
            gap_per_char_ms: GAP_PER_CHAR.integer(cfg) as u64,
            max_gap_ms: GAP_MAX.integer(cfg) as u64,
        }
    }

    pub fn max_per_minute(cfg: &serde_json::Value) -> usize {
        crate::settings::send::RATE_MIN.integer(cfg) as usize
    }

    pub fn max_per_hour(cfg: &serde_json::Value) -> usize {
        crate::settings::send::RATE_HOUR.integer(cfg) as usize
    }
}

fn now_ms() -> u128 {
    crate::parity_clock::now_ms() as u128
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn hotfix128_send_error_excerpt_masks_the_complete_response_before_cutting() {
        let raw = format!("{}0123456789abcdef0123456789abcdef0123456789abcdef tail", "x ".repeat(95));
        for status in [200, 500] {
            let (SendError::Unknown(error) | SendError::NotCommitted(error)) = classify_send_reply(status, &raw).unwrap_err();
            assert!(!error.contains("01234567") && error.contains("***"), "{error}");
        }
    }

    #[test]
    fn classify_send_reply_separates_clear_refusals_from_uncertain_failures() {
        let ok = classify_send_reply(200, r#"{"status":"ok","retcode":0,"data":{"message_id":5}}"#).unwrap();
        assert_eq!(ok["message_id"], 5);
        for retcode in [1400, 1401, 1403, 1404] {
            let body = format!(r#"{{"status":"failed","retcode":{retcode}}}"#);
            assert!(matches!(classify_send_reply(200, &body), Err(SendError::NotCommitted(_))), "retcode {retcode}");
        }
        for status in [400, 401, 403, 404, 426] {
            assert!(matches!(classify_send_reply(status, ""), Err(SendError::NotCommitted(_))), "http {status}");
        }
        // May have been delivered, or only queued: never a clear failure.
        for body in [
            r#"{"status":"failed","retcode":100,"wording":"param"}"#,
            r#"{"status":"failed","retcode":200,"wording":"timeout"}"#,
            r#"{"status":"failed","retcode":1200}"#,
            r#"{"status":"async","retcode":1}"#,
            r#"{"status":"failed"}"#,
            r#"{"status":"ok","retcode":0"#,
            "",
        ] {
            assert!(matches!(classify_send_reply(200, body), Err(SendError::Unknown(_))), "body {body}");
        }
        assert!(matches!(classify_send_reply(500, "{}"), Err(SendError::Unknown(_))));
        assert!(matches!(classify_send_reply(502, ""), Err(SendError::Unknown(_))));
    }

    #[test]
    fn send_failure_text_marks_a_mute_refusal() {
        let muted: serde_json::Value = serde_json::from_str(r#"{"status":"failed","retcode":100,"message":"sendMsg failed: result=120","wording":"x"}"#).unwrap();
        let text = send_failure_text(&muted, 100);
        assert!(is_mute_refusal(&text), "{text}");
        assert!(text.contains("retcode=100"), "{text}");
        let other: serde_json::Value = serde_json::from_str(r#"{"status":"failed","retcode":1404,"wording":"nope"}"#).unwrap();
        assert!(!is_mute_refusal(&send_failure_text(&other, 1404)));
        let unknown: serde_json::Value = serde_json::from_str(r#"{"status":"failed","retcode":100,"wording":"other"}"#).unwrap();
        assert!(!is_mute_refusal(&send_failure_text(&unknown, 100)), "retcode 100 without result=120 is not a mute");
    }

    #[test]
    fn mute_refusal_retcode_100_with_result_120_is_not_committed() {
        for body in [
            r#"{"status":"failed","retcode":100,"message":"sendMsg failed: result=120","wording":"x"}"#,
            r#"{"status":"failed","retcode":100,"wording":"result = 120"}"#,
            r#"{"status":"failed","retcode":100,"msg":"Result:120"}"#,
            r#"{"status":"failed","retcode":100,"data":{"result":120}}"#,
            r#"{"status":"failed","retcode":100,"wording":"result：120"}"#,
        ] {
            match classify_send_reply(200, body) {
                Err(SendError::NotCommitted(error)) => assert!(is_mute_refusal(&error), "{body}: {error}"),
                other => panic!("{body}: {other:?}"),
            }
        }
        for body in [
            r#"{"status":"failed","retcode":100,"wording":"result=1200"}"#,
            r#"{"status":"failed","retcode":100,"wording":"result=12"}"#,
            r#"{"status":"failed","retcode":200,"wording":"result=120"}"#,
            r#"{"status":"failed","retcode":100,"data":{"result":121}}"#,
        ] {
            assert!(matches!(classify_send_reply(200, body), Err(SendError::Unknown(_))), "{body}");
        }
    }

    fn fake_onebot_server(
        listener: TcpListener,
        expected_host: String,
        expected_message: serde_json::Value,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let started = std::time::Instant::now();
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if (error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(10035))
                            && started.elapsed() < std::time::Duration::from_secs(5) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept fake OneBot request: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let count = stream.read(&mut chunk).expect("read fake OneBot request");
                assert_ne!(count, 0, "client closed before sending a complete request");
                request.extend_from_slice(&chunk[..count]);
                if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = header
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .expect("request has content length");
                    if request.len() >= header_end + 4 + content_length {
                        assert!(header.starts_with("POST /send_group_msg HTTP/1.1"));
                        assert!(header.lines().any(|line| line == format!("Host: {expected_host}")));
                        assert!(header.lines().any(|line| line == "Authorization: Bearer fixture-token"));
                        let body = &request[header_end + 4..header_end + 4 + content_length];
                        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
                        assert_eq!(body["group_id"], 4321);
                        assert_eq!(body["message"], expected_message);
                        break;
                    }
                }
            }
            let body = r#"{"status":"ok","retcode":0,"data":{"message_id":7}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        })
    }

    fn native_ipv6_echo_probe() -> Result<(), String> {
        fn io_error(stage: &str, error: std::io::Error) -> String {
            format!(
                "{stage}: kind={:?}, raw_os_error={:?}: {error}",
                error.kind(),
                error.raw_os_error()
            )
        }

        let listener = TcpListener::bind("[::1]:0")
            .map_err(|error| io_error("bind", error))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| io_error("set_nonblocking", error))?;
        let address = listener
            .local_addr()
            .map_err(|error| io_error("local_addr", error))?;
        let server = std::thread::spawn(move || -> Result<(), String> {
            let started = std::time::Instant::now();
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if (error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(10035))
                            && started.elapsed() < std::time::Duration::from_secs(5) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => return Err(io_error("accept", error)),
                }
            };
            stream
                .set_nonblocking(false)
                .map_err(|error| io_error("server set_nonblocking", error))?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .map_err(|error| io_error("server set_read_timeout", error))?;
            let mut request = [0u8; 4];
            stream
                .read_exact(&mut request)
                .map_err(|error| io_error("server read_exact", error))?;
            stream
                .write_all(&request)
                .map_err(|error| io_error("server write_all", error))?;
            Ok(())
        });

        let mut client = match TcpStream::connect_timeout(&address, std::time::Duration::from_secs(5)) {
            Ok(stream) => stream,
            Err(error) => {
                let server_result = server.join();
                return Err(format!(
                    "connect: {}; server result: {server_result:?}",
                    io_error("connect", error)
                ));
            }
        };
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|error| io_error("client set_read_timeout", error))?;
        client
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|error| io_error("client set_write_timeout", error))?;
        let exchange = (|| {
            client
                .write_all(b"ping")
                .map_err(|error| io_error("client write_all", error))?;
            let mut echoed = [0u8; 4];
            client
                .read_exact(&mut echoed)
                .map_err(|error| io_error("client read_exact", error))?;
            Ok::<_, String>(echoed)
        })();
        let server_result = server.join().map_err(|_| "server thread panicked".to_string())?;
        server_result?;
        if exchange? != *b"ping" {
            return Err("echoed payload differed".to_string());
        }
        Ok(())
    }

    fn verify_fake_onebot_request(
        listener: TcpListener,
        endpoint: String,
        expected_host: String,
    ) {
        let server = fake_onebot_server(listener, expected_host,
            serde_json::json!([{"type":"text","data":{"text":"fixture message"}}]));
        let client = OneBot::try_new_url(&endpoint, "fixture-token".to_string()).unwrap();
        let reply = client.send_one(&Target::Group(4321), "fixture message");
        let server_result = server.join();
        assert!(server_result.is_ok(), "fake OneBot server failed: {server_result:?}");
        let reply = reply.unwrap();
        assert_eq!(reply, serde_json::json!({"message_id": 7}));
    }

    #[test]
    fn hf124_legacy_text_send_normalizes_before_splitting_and_renders_a_face() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let server = fake_onebot_server(listener, host.clone(),
            serde_json::json!([{"type":"face","data":{"id":"28"}}]));
        let client = OneBot::try_new_url(&format!("http://{host}"), "fixture-token".into()).unwrap();
        let tune = SendTuning { max_chars: 6, gap_base_ms: 0, gap_per_char_ms: 0, max_gap_ms: 0 };
        let reply = client.send_text(&Target::Group(4321), "[表情:给你 糖][表情028]", &tune,
            &mut RateLimiter::new(0, 0));
        let server_result = server.join();
        assert!(server_result.is_ok(), "fake OneBot payload mismatch: {server_result:?}");
        assert_eq!(reply.unwrap(), vec![serde_json::json!({"message_id":7})]);
    }

    #[test]
    fn strict_onebot_http_factory_posts_to_selected_ipv4_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind IPv4 loopback fixture");
        let port = listener.local_addr().unwrap().port();
        verify_fake_onebot_request(
            listener,
            format!("http://127.0.0.1:{port}"),
            format!("127.0.0.1:{port}"),
        );
    }

    #[test]
    fn strict_onebot_http_factory_posts_to_selected_ipv6_port() {
        if let Err(error) = native_ipv6_echo_probe() {
            if error.starts_with("connect:") {
                eprintln!("SKIP: native IPv6 loopback echo probe could not connect: {error}");
                return;
            }
            panic!("native IPv6 loopback echo probe failed outside connect: {error}");
        }
        let listener = match TcpListener::bind("[::1]:0") {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("SKIP: IPv6 loopback fixture unavailable: {error}");
                return;
            }
        };
        let port = listener.local_addr().unwrap().port();
        verify_fake_onebot_request(
            listener,
            format!("http://[::1]:{port}"),
            format!("[::1]:{port}"),
        );
    }

    #[test]
    fn strict_onebot_http_factory_rejects_invalid_or_non_loopback_urls() {
        for invalid in [
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "http://127.0.0.1:65536",
            "http://192.0.2.1:43103",
            "http://localhost:43103",
            "http://[::1]:43103/path",
            "http://127.0.0.1:43103?query",
            "http://::1:43103",
            "http://[127.0.0.1]:43103",
            "http://user@127.0.0.1:43103",
            "https://127.0.0.1:43103",
            "http://[::1:43103",
        ] {
            assert!(
                OneBot::try_new_url(invalid, "fixture-token".to_string()).is_err(),
                "invalid runtime endpoint was accepted: {invalid}"
            );
        }
    }

    #[test]
    fn legacy_onebot_constructor_keeps_its_port_fallback() {
        let legacy = OneBot::new("not-an-endpoint", "fixture-token".to_string());
        assert_eq!(legacy.host, "not-an-endpoint");
        assert_eq!(legacy.port, 3000);
    }
    #[test]
    fn split_prefers_newline_and_keeps_it_with_previous_part() {
        let text = format!("{}\n{}", "a".repeat(10), "b".repeat(10));
        let parts = split_message(&text, 12);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].ends_with('\n'), "换行应留在前一段：{:?}", parts[0]);
        assert_eq!(parts[1], "b".repeat(10));
    }

    #[test]
    fn split_counts_utf16_units_like_js() {
        // 一个 emoji = 2 个 UTF-16 码元；按标量值数会算成 1，切出来就超长
        let parts = split_message(&"🐳".repeat(5), 4);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts.iter().all(|p| p.encode_utf16().count() <= 4));
        assert!(parts.iter().all(|p| !p.contains('\u{FFFD}')), "不能切出孤立代理");
    }

    #[test]
    fn split_empty_input_yields_no_parts() {
        assert!(split_message("", 4000).is_empty(), "空输入不该产生一个空消息");
    }


    #[test]
    fn cq_is_broken() {
        assert_eq!(escape_cq("看图[CQ:image,file=a.jpg]"), "看图[CQ：image,file=a.jpg]");
        assert_eq!(escape_cq("[cq:at,qq=1]"), "[CQ：at,qq=1]");
        assert_eq!(escape_cq("正常中文没有 CQ"), "正常中文没有 CQ");
    }

    #[test]
    fn split_respects_utf8_and_limit() {
        let parts = split_message("你好世界你好世界", 4);
        assert_eq!(parts, vec!["你好世界", "你好世界"]);
        // 不切断中文
        let parts = split_message("一二三四五", 2);
        assert_eq!(parts, vec!["一二", "三四", "五"]);
        // 空串 → **没有段**（与 JS splitForQQ 一致）。
        // 旧实现返回 vec![""]，那会让 send_text 真的去发一条空消息。
        assert!(split_message("", 10).is_empty());
    }

    #[test]
    fn gap_follows_formula() {
        // 800 + 20*10 = 1000，上限 10000
        assert_eq!(gap_ms(10, 800, 20, 10_000), 1000);
        // 封顶
        assert_eq!(gap_ms(10_000, 800, 20, 10_000), 10_000);
        // cap=0 表示不限
        assert_eq!(gap_ms(10, 800, 20, 0), 1000);
    }

    #[test]
    fn rate_limiter_minute_window() {
        let mut rl = RateLimiter::new(2, 100);
        let t0 = 1_000_000u128;
        assert!(rl.check(t0).is_ok());
        rl.record(t0);
        assert!(rl.check(t0 + 1000).is_ok());
        rl.record(t0 + 1000);
        // 第三次要被挡，且等待时间 = 最早那条 + 60s - now
        let wait = rl.check(t0 + 2000).unwrap_err();
        assert_eq!(wait, 60_000 - 2000);
        // 过了窗口就放行
        assert!(rl.check(t0 + 61_000).is_ok());
    }

    #[test]
    fn rate_limiter_hour_window() {
        let mut rl = RateLimiter::new(100, 2);
        let t0 = 5_000_000u128;
        rl.record(t0);
        rl.record(t0 + 10 * 60_000);
        assert!(rl.check(t0 + 20 * 60_000).is_err(), "一小时两条后应该被挡");
        assert!(rl.check(t0 + 3_600_000 + 1).is_ok(), "滑出小时窗口应放行");
    }

    #[test]
    fn target_actions() {
        assert_eq!(Target::Group(1).action(), "send_group_msg");
        assert_eq!(Target::Private(2).action(), "send_private_msg");
        assert_eq!(Target::Group(1).key(), "group:1");
        assert_eq!(Target::Private(2).key(), "private:2");
    }

    fn chunked_http(status: &str, body: &str, split_at: usize) -> Vec<u8> {
        let (a, b) = body.split_at(split_at.min(body.len()));
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        if !a.is_empty() {
            out.push_str(&format!("{:x}\r\n{}\r\n", a.len(), a));
        }
        if !b.is_empty() {
            out.push_str(&format!("{:x}\r\n{}\r\n", b.len(), b));
        }
        out.push_str("0\r\n\r\n");
        out.into_bytes()
    }

    #[test]
    fn issue124_onebot_decodes_chunked_responses_and_caps_decoded_body() {
        use crate::loopback_http::test_support::{MemoryTransport, ScriptedStreamConfig, Step};
        let payload = r#"{"status":"ok","retcode":0,"data":{"message_id":42,"note":"hi \ud83d"}}"#;
        let wire = chunked_http("200 OK", payload, 18);
        let transport = std::sync::Arc::new(MemoryTransport::new(vec![
            Step::Stream(ScriptedStreamConfig::reply_bytes(wire.clone())),
            Step::Stream(ScriptedStreamConfig::reply_bytes(wire.clone())),
            Step::Stream(ScriptedStreamConfig::reply_bytes(wire)),
        ]));
        let ob = OneBot::with_transport(
            "http://127.0.0.1:43103",
            "tok".into(),
            transport.clone(),
        )
        .unwrap();

        // request() decodes chunked body and replaces lone surrogate.
        let reply = ob.request("get_msg", serde_json::json!({"message_id": 1})).unwrap();
        assert_eq!(reply["data"]["message_id"], 42);
        assert_eq!(reply["data"]["note"], "hi \u{FFFD}");

        // request_bounded() applies cap to decoded body length (payload.len() fits, payload.len() - 1 fails).
        let exact = ob.request_bounded("get_image", serde_json::json!({"file": "a.png"}), payload.len());
        assert!(exact.is_some());
        let too_small = ob.request_bounded("get_image", serde_json::json!({"file": "a.png"}), payload.len() - 1);
        assert!(too_small.is_none());
        assert_eq!(transport.connect_count(), 3);
    }

    #[test]
    fn onebot_send_failure_modes_preserve_not_committed_vs_unknown_across_transport_seam() {
        use crate::loopback_http::test_support::{MemoryTransport, ScriptedStreamConfig, Step};
        use std::collections::VecDeque;
        use std::io::ErrorKind;

        let transport = std::sync::Arc::new(MemoryTransport::new(vec![
            // 1. Connect refused -> NotCommitted.
            Step::ConnectError(ErrorKind::ConnectionRefused, "refused"),
            // 2. Connect timeout -> NotCommitted.
            Step::ConnectError(ErrorKind::TimedOut, "connect timed out"),
            // 3. HTTP 403 refusal -> NotCommitted.
            Step::Stream(ScriptedStreamConfig::reply_bytes(
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            )),
            // 4. HTTP 429 -> Unknown (OneBot 429 is outside request-level refusals).
            Step::Stream(ScriptedStreamConfig::reply_bytes(
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            )),
            // 5. HTTP 503 -> Unknown.
            Step::Stream(ScriptedStreamConfig::reply_bytes(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            )),
            // 6. Read timeout after write -> Unknown.
            Step::Stream(ScriptedStreamConfig {
                write_plan: Vec::new(),
                read_plan: VecDeque::from([Err(ErrorKind::TimedOut)]),
                delay_per_read: std::time::Duration::ZERO,
                delay_per_write: std::time::Duration::ZERO,
            }),
            // 7. HTTP 200 with empty body -> Unknown.
            Step::Stream(ScriptedStreamConfig::reply_bytes(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
            )),
            // 8. HTTP 200 with non-ok envelope (retcode 200) -> Unknown.
            Step::Stream(ScriptedStreamConfig::reply_bytes(chunked_http(
                "200 OK",
                r#"{"status":"failed","retcode":200,"wording":"timeout"}"#,
                12,
            ))),
            // 9. HTTP 200 with definite refusal envelope (retcode 1404) -> NotCommitted.
            Step::Stream(ScriptedStreamConfig::reply_bytes(chunked_http(
                "200 OK",
                r#"{"status":"failed","retcode":1404,"wording":"unknown action"}"#,
                15,
            ))),
            // 10. Input status posts to /set_input_status.
            Step::Stream(ScriptedStreamConfig::reply_bytes(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            )),
        ]));
        let ob = OneBot::with_transport(
            "http://127.0.0.1:43103",
            "tok".into(),
            transport.clone(),
        )
        .unwrap();
        let target = Target::Group(101);

        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::NotCommitted(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::NotCommitted(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::NotCommitted(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::Unknown(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::Unknown(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::Unknown(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::Unknown(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::Unknown(_))));
        assert!(matches!(ob.send_one_outcome(&target, "a", None, None), Err(SendError::NotCommitted(_))));
        ob.set_input_status(77, 1);

        let reqs = transport.written_requests();
        assert!(reqs.last().unwrap().starts_with("POST /set_input_status HTTP/1.1"));
    }
}
