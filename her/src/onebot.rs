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

use std::io::{Read, Write};

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

/// Classify a send reply that arrived in full.
///
/// The frozen JS bridge treats every non-`ok`/non-zero reply as a failed send and never retries,
/// so it has no classification to copy. OneBot gives no guarantee for other failure codes: some
/// gateways (NapCat style, retcode 100/200/timeouts) report a failure after the message already
/// went out, and `status:"async"` (retcode 1) only means queued. Only the request-level refusals
/// above are therefore `NotCommitted`; every other failure is `Unknown`, which is verified
/// against history and never resent automatically.
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
    if retcode.is_some_and(|code| REFUSAL_RETCODES.contains(&code)) {
        Err(SendError::NotCommitted(message))
    } else {
        Err(SendError::Unknown(message))
    }
}

/// OneBot HTTP 客户端。
#[derive(Clone)]
pub struct OneBot {
    host: String,
    port: u16,
    token: String,
    strict_address: Option<std::net::SocketAddr>,
}

pub(crate) struct PendingResponse(std::net::TcpStream);

impl PendingResponse {
    pub(crate) fn outcome(self) -> Result<serde_json::Value, SendError> {
        let (status, raw) = OneBot::read_response(self.0, None)
            .map_err(|(_, detail)| SendError::Unknown(format!("OneBot HTTP 连接失败：{detail}")))?;
        classify_send_reply(status, &raw)
    }
}

#[cfg(test)]
mod bounded_media_tests {
    use super::*;
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
        let host = base
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("127.0.0.1")
            .split(':')
            .next()
            .unwrap_or("127.0.0.1")
            .to_string();
        let port = base
            .trim_start_matches("http://")
            .split('/')
            .next()
            .and_then(|hp| hp.split(':').nth(1))
            .and_then(|p| p.parse().ok())
            .unwrap_or(3000);
        Self { host, port, token, strict_address: None }
    }

    /// Build a client for an explicitly selected loopback endpoint.
    ///
    /// Runtime-root callers must not inherit the legacy port-3000 fallback or
    /// resolve a hostname outside the selected dependency contract. This parser
    /// deliberately accepts only literal loopback IP addresses and an explicit
    /// non-zero port; IPv6 authorities keep their brackets for both connect and
    /// Host header formatting.
    pub fn try_new_url(http_url: &str, token: String) -> Result<Self, String> {
        let authority = http_url
            .strip_prefix("http://")
            .ok_or_else(|| "OneBot endpoint must use http://".to_string())?;
        if authority
            .chars()
            .any(|ch| matches!(ch, '/' | '?' | '#' | '@' | '\\'))
        {
            return Err("OneBot endpoint must be a bare loopback authority".to_string());
        }
        let (host, port_text, bracketed_host) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, remainder) = bracketed
                .split_once(']')
                .ok_or_else(|| "OneBot endpoint has an invalid IPv6 authority".to_string())?;
            let port = remainder
                .strip_prefix(':')
                .ok_or_else(|| "OneBot endpoint requires an explicit port".to_string())?;
            (host, port, true)
        } else {
            let (host, port) = authority
                .rsplit_once(':')
                .ok_or_else(|| "OneBot endpoint requires an explicit port".to_string())?;
            (host, port, false)
        };
        let address = host
            .parse::<std::net::IpAddr>()
            .map_err(|_| "OneBot endpoint host must be a literal loopback IP".to_string())?;
        if address.is_ipv6() != bracketed_host {
            return Err("IPv6 OneBot endpoints must use bracketed authority syntax".to_string());
        }
        if !address.is_loopback() {
            return Err("OneBot endpoint must use a loopback IP".to_string());
        }
        let port = port_text
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| "OneBot endpoint requires an explicit non-zero port".to_string())?;
        let host = match address {
            std::net::IpAddr::V4(address) => address.to_string(),
            std::net::IpAddr::V6(address) => format!("[{address}]"),
        };
        Ok(Self {
            host,
            port,
            token,
            strict_address: Some(std::net::SocketAddr::new(address, port)),
        })
    }

    /// 调任意 OneBot action（贴纸同步等非发送类操作要用）。
    pub fn request(&self, action: &str, params: serde_json::Value) -> Option<serde_json::Value> {
        let body = params.to_string();
        let raw = self.post(&format!("/{action}"), &body)?;
        serde_json::from_str(&raw).ok()
    }

    /// Like `request`, with a longer read deadline (voice readback waits up to 30 s).
    pub fn request_with_timeout(&self, action: &str, params: serde_json::Value, timeout: std::time::Duration) -> Option<serde_json::Value> {
        let (_, raw) = self.post_with_timeout(&format!("/{action}"), &params.to_string(), None, timeout).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Read-only metadata request, bounded across connect/write/read by one absolute deadline.
    pub fn request_with_deadline(&self, action: &str, params: serde_json::Value, deadline: std::time::Instant) -> Result<serde_json::Value, String> {
        if self.strict_address.is_none() { return Err("a selected loopback endpoint is required".into()); }
        let stream = self.transmit_until(&format!("/{action}"), &params.to_string(), deadline)
            .map_err(|(_, reason)| reason)?;
        let (status, body) = Self::read_response_until(stream, Some(64 * 1024), Some(deadline))
            .map_err(|(_, reason)| reason)?;
        if status != 200 { return Err(format!("gateway returned HTTP {status}")); }
        serde_json::from_str(&body).map_err(|_| "gateway response is not JSON".into())
    }

    /// Cosmetic typing status: bounded connect/write/read, never a prerequisite for a send.
    pub fn set_input_status(&self, user_id: i64, event_type: u8) {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
        let address = self.strict_address.or_else(|| {
            let ip = if self.host == "localhost" { IpAddr::V4(Ipv4Addr::LOCALHOST) }
                else { self.host.parse().ok()? };
            Some(SocketAddr::new(ip,self.port))
        });
        // DNS resolution has no bounded std API; cosmetic status may be skipped for legacy DNS hosts.
        let Some(address) = address else { return };
        let budget = std::time::Duration::from_millis(250);
        #[cfg(test)]
        let (budget, returned) = INPUT_STATUS_TEST.with(|slot| {
            slot.borrow().as_ref().map(|(budget, returned)| (*budget, Some(returned.clone())))
                .unwrap_or((budget, None))
        });
        #[cfg(test)]
        let _returned = InputStatusReturned(if event_type == 0 { returned } else { None });
        let deadline = std::time::Instant::now() + budget;
        let Ok(mut stream) = TcpStream::connect_timeout(&address,std::time::Duration::from_millis(250)) else {return};
        let body = serde_json::json!({"user_id":user_id,"event_type":event_type}).to_string();
        let request = format!("POST /set_input_status HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n{body}",self.host,self.port,body.len(),self.token);
        let mut remaining = request.as_bytes();
        while !remaining.is_empty() {
            let Some(budget) = deadline.checked_duration_since(std::time::Instant::now()).filter(|b|!b.is_zero()) else {return};
            if stream.set_write_timeout(Some(budget)).is_err() {return;}
            match stream.write(remaining) {
                Ok(0) => return,
                Ok(count) => remaining=&remaining[count..],
                Err(error) if error.kind()==std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
        let mut response=[0;4096];
        let mut read=0;
        while read<response.len() {
            let Some(budget) = deadline.checked_duration_since(std::time::Instant::now()).filter(|b|!b.is_zero()) else {return};
            if stream.set_read_timeout(Some(budget)).is_err() {return;}
            match stream.read(&mut response[read..]) {
                Ok(0) => return,
                Ok(count) => read+=count,
                Err(error) if error.kind()==std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }

    /// Send one voice (`record` segment holding a local file path) and classify the outcome like
    /// `send_one_outcome`. The gateway converts to SILK before answering, so the deadline is 60 s.
    pub fn send_record_outcome(&self, target: &Target, file: &str) -> Result<serde_json::Value, SendError> {
        self.send_record_outcome_within(target, file, std::time::Duration::from_secs(60))
    }

    fn send_record_outcome_within(&self, target: &Target, file: &str, deadline: std::time::Duration) -> Result<serde_json::Value, SendError> {
        let segments = serde_json::json!([{ "type": "record", "data": { "file": file } }]);
        let payload = match target {
            Target::Group(id) => serde_json::json!({ "group_id": id, "message": segments }),
            Target::Private(id) => serde_json::json!({ "user_id": id, "message": segments }),
        };
        let (http_status, raw) = self
            .post_with_timeout(&format!("/{}", target.action()), &payload.to_string(), None, deadline)
            .map_err(|(reached, detail)| {
                let message = self.send_connection_error(detail);
                if reached { SendError::Unknown(message) } else { SendError::NotCommitted(message) }
            })?;
        classify_send_reply(http_status, &raw)
    }

    /// Media replies must be bounded on the wire, before JSON/base64 copies.
    pub fn request_bounded(&self, action: &str, params: serde_json::Value, max_bytes: usize) -> Option<serde_json::Value> {
        let (_, raw) = self.post_with_status_limit(&format!("/{action}"), &params.to_string(), Some(max_bytes)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn post(&self, path: &str, body: &str) -> Option<String> {
        self.post_detailed(path, body).ok()
    }

    fn post_detailed(&self, path: &str, body: &str) -> Result<String, String> {
        self.post_classified(path, body).map_err(|(_, detail)| detail)
    }

    /// Like `post_detailed`, but reports whether the request bytes may already have reached the
    /// gateway. `false` only for failures before any write; a write error or later failure is
    /// conservatively reported as possibly delivered.
    fn post_classified(&self, path: &str, body: &str) -> Result<String, (bool, String)> {
        self.post_with_status(path, body).map(|(_, body)| body)
    }

    /// `post_classified` that also returns the HTTP status code of the reply.
    fn post_with_status(&self, path: &str, body: &str) -> Result<(u16, String), (bool, String)> {
        self.post_with_status_limit(path, body, None)
    }

    fn post_with_status_limit(&self, path: &str, body: &str, max_bytes: Option<usize>) -> Result<(u16, String), (bool, String)> {
        let s = self.transmit(path, body)?;
        Self::read_response(s, max_bytes)
    }

    /// Like `post_with_status_limit` with a caller-chosen read deadline (voice waits for SILK conversion).
    fn post_with_timeout(&self, path: &str, body: &str, max_bytes: Option<usize>, timeout: std::time::Duration) -> Result<(u16, String), (bool, String)> {
        let s = self.transmit(path, body)?;
        s.set_read_timeout(Some(timeout)).map_err(|error| (true, format!("set read timeout: {error}")))?;
        Self::read_response(s, max_bytes)
    }

    // A successful return means the entire request has been handed to the socket. Callers may
    // release submission coordination here; waiting for the peer must not hold that gate.
    fn transmit(&self, path: &str, body: &str) -> Result<std::net::TcpStream, (bool, String)> {
        self.transmit_until(path, body, std::time::Instant::now() + std::time::Duration::from_secs(15))
    }

    fn transmit_until(&self, path: &str, body: &str, deadline: std::time::Instant) -> Result<std::net::TcpStream, (bool, String)> {
        let budget = deadline.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero())
            .ok_or_else(|| (false, "request deadline exceeded".to_string()))?;
        let address = self
            .strict_address
            .map(|address| address.to_string())
            .unwrap_or_else(|| format!("{}:{}", self.host, self.port));
        let mut s = match self.strict_address {
            Some(address) => std::net::TcpStream::connect_timeout(&address, budget.min(std::time::Duration::from_secs(5))),
            None => std::net::TcpStream::connect(&address),
        }
        .map_err(|error| (false, format!("connect {address}: {error} (raw_os={:?})", error.raw_os_error())))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(15)))
            .map_err(|error| (false, format!("set read timeout: {error}")))?;
        if self.strict_address.is_some() {
            s.set_write_timeout(Some(std::time::Duration::from_secs(15)))
                .map_err(|error| (false, format!("set write timeout: {error}")))?;
        }
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n{}",
            self.host,
            self.port,
            body.len(),
            self.token,
            body
        );
        // Each write uses the remaining total budget, so a slowly draining peer cannot
        // extend authority ownership indefinitely through repeated partial writes.
        if self.strict_address.is_some() {
            let mut remaining = req.as_bytes();
            while !remaining.is_empty() {
                let budget = deadline.checked_duration_since(std::time::Instant::now())
                    .filter(|budget| !budget.is_zero())
                    .ok_or_else(|| (true, "write request: transmission deadline exceeded".to_string()))?;
                s.set_write_timeout(Some(budget))
                    .map_err(|error| (true, format!("set write timeout: {error}")))?;
                match s.write(remaining) {
                    Ok(0) => return Err((true, "write request: connection closed".to_string())),
                    Ok(count) => remaining = &remaining[count..],
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err((true, format!("write request: {error} (raw_os={:?})", error.raw_os_error()))),
                }
            }
        } else {
            // Any attempted write may leave a partial request in flight.
            s.write_all(req.as_bytes())
                .map_err(|error| (true, format!("write request: {error} (raw_os={:?})", error.raw_os_error())))?;
        }
        Ok(s)
    }

    fn read_response(s: std::net::TcpStream, max_bytes: Option<usize>) -> Result<(u16, String), (bool, String)> {
        Self::read_response_until(s, max_bytes, None)
    }

    fn read_response_until(mut s: std::net::TcpStream, max_bytes: Option<usize>, deadline: Option<std::time::Instant>) -> Result<(u16, String), (bool, String)> {
        let mut buf = String::new();
        let wire_limit = max_bytes.map(|n| n.saturating_add(16 * 1024));
        if let Some(deadline) = deadline {
            let mut bytes = Vec::new();
            let mut block = [0; 8192];
            loop {
                let remaining = deadline.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero())
                    .ok_or_else(|| (true, "response deadline exceeded".to_string()))?;
                s.set_read_timeout(Some(remaining)).map_err(|e| (true, format!("set response deadline: {e}")))?;
                match s.read(&mut block) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&block[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err((true, format!("read response: {e}"))),
                }
                if wire_limit.is_some_and(|limit| bytes.len() > limit) { return Err((true, "response exceeds metadata limit".into())); }
            }
            buf = String::from_utf8(bytes).map_err(|_| (true, "response is not UTF-8".into()))?;
        } else {
            (&mut s).take(wire_limit.map(|n| n as u64 + 1).unwrap_or(u64::MAX)).read_to_string(&mut buf)
                .map_err(|error| (true, format!("read response: {error} (raw_os={:?})", error.raw_os_error())))?;
        }
        if wire_limit.is_some_and(|n| buf.len() > n) {
            return Err((true, "response exceeds media limit".to_string()));
        }
        let (head, b) = buf
            .split_once("\r\n\r\n")
            .ok_or_else(|| (true, "response has no HTTP header/body separator".to_string()))?;
        if max_bytes.is_some_and(|n| b.len() > n || head.len() > 16 * 1024) {
            return Err((true, "response exceeds media limit".to_string()));
        }
        let http_status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        Ok((http_status, b.to_string()))
    }

    fn send_connection_error(&self, detail: String) -> String {
        if self.strict_address.is_some() {
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
            let wording = v.get("wording").and_then(|w| w.as_str()).unwrap_or("");
            return Err(format!("OneBot 发送失败：retcode={retcode} {wording}"));
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
        let (http_status, raw) = self
            .post_with_status(&format!("/{action}"), &payload.to_string())
            .map_err(|(reached, detail)| {
                let message = self.send_connection_error(detail);
                if reached {
                    SendError::Unknown(message)
                } else {
                    SendError::NotCommitted(message)
                }
            })?;
        classify_send_reply(http_status, &raw)
    }

    pub(crate) fn transmit_payload(
        &self,
        action: &str,
        payload: &serde_json::Value,
    ) -> Result<PendingResponse, SendError> {
        self.transmit(&format!("/{action}"), &payload.to_string())
            .map(PendingResponse)
            .map_err(|(reached, detail)| {
                let message = self.send_connection_error(detail);
                if reached { SendError::Unknown(message) } else { SendError::NotCommitted(message) }
            })
    }

    fn segment_payload(
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
            let wording = v.get("wording").and_then(|w| w.as_str()).unwrap_or("");
            return Err(format!("OneBot 发送失败：retcode={retcode} {wording}"));
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
        Self { max_chars: 800, gap_base_ms: 800, gap_per_char_ms: 20, max_gap_ms: 10_000 }
    }
}

impl SendTuning {
    /// 从 config.json 的 socialV2.send 读出节奏参数。
    pub fn from_config(cfg: &serde_json::Value) -> Self {
        let s = cfg.get("socialV2").and_then(|v| v.get("send"));
        let get = |k: &str, d: u64| -> u64 {
            s.and_then(|x| x.get(k)).and_then(|v| v.as_u64()).unwrap_or(d)
        };
        Self {
            max_chars: get("maxMessageChars", 800) as usize,
            gap_base_ms: get("gapBaseMs", 800),
            gap_per_char_ms: get("gapPerCharMs", 20),
            max_gap_ms: get("maxGapMs", 10_000),
        }
    }

    pub fn max_per_minute(cfg: &serde_json::Value) -> usize {
        cfg.get("socialV2")
            .and_then(|v| v.get("send"))
            .and_then(|s| s.get("maxSendPerMinute"))
            .and_then(|v| v.as_u64())
            .unwrap_or(8) as usize
    }

    pub fn max_per_hour(cfg: &serde_json::Value) -> usize {
        cfg.get("socialV2")
            .and_then(|v| v.get("send"))
            .and_then(|s| s.get("maxSendPerHour"))
            .and_then(|v| v.as_u64())
            .unwrap_or(60) as usize
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
    fn mute_refusal_retcode_100_with_result_120_is_not_committed() {
        for body in [
            r#"{"status":"failed","retcode":100,"message":"sendMsg failed: result=120","wording":"x"}"#,
            r#"{"status":"failed","retcode":100,"wording":"result = 120"}"#,
            r#"{"status":"failed","retcode":100,"msg":"Result:120"}"#,
            r#"{"status":"failed","retcode":100,"data":{"result":120}}"#,
            r#"{"status":"failed","retcode":100,"wording":"result：120"}"#,
        ] {
            match classify_send_reply(200, body) {
                Err(SendError::NotCommitted(_error)) => {} // A2 adds: assert!(is_mute_refusal(&_error), "{body}: {_error}")
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
}
