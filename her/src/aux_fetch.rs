//! Network side of the auxiliary servers: the JS `safe-fetch.js` rules (what may be fetched) and a curl transport
//! (how bytes move; her links no TLS stack). All trust decisions stay in Rust: the URL, the DNS answers and the
//! redirect chain are validated here, and curl is pinned to an address that passed (`--resolve`), never trusted.
//!
//! Every curl child is created without a console window (`subprocess::command_without_console`), receives its whole
//! request (URL, headers, API key, body) on stdin so no secret appears in a process listing, is bounded by curl's
//! own `--max-time`, is killed when it is dropped and (Linux) dies with its parent.

use std::io::{Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use url::Url;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_ADDRESS_ATTEMPTS: usize = 4;
const MAX_REDIRECTS: usize = 5;
const FETCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0 Safari/537.36";

// ----- address policy (JS isPrivateIp / resolveSafeAddresses) --------------------------------------------------------

fn ipv4_text(num: u32) -> String {
    format!("{}.{}.{}.{}", num >> 24, (num >> 16) & 255, (num >> 8) & 255, num & 255)
}

fn is_hex_group(s: &str) -> bool {
    (1..=4).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_dotted(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// JS `ipv4FromLast32`.
fn ipv4_from_last32(lower: &str) -> Option<String> {
    let parts: Vec<&str> = lower.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let last = parts[parts.len() - 1];
    let second = parts[parts.len() - 2];
    if is_dotted(last) {
        return Some(last.to_string());
    }
    if is_hex_group(second) && is_hex_group(last) {
        let num = (u32::from_str_radix(second, 16).ok()? << 16) | u32::from_str_radix(last, 16).ok()?;
        return Some(ipv4_text(num));
    }
    None
}

/// JS `parseEmbeddedIpv4` (the input is the canonical lower-case IPv6 text without brackets).
fn embedded_ipv4(h: &str) -> Option<String> {
    let lower = h.to_ascii_lowercase();
    if !lower.contains(':') {
        return None;
    }
    // trailing dotted quad
    let tail_start = lower.rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |i| i + 1);
    if is_dotted(&lower[tail_start..]) {
        return Some(lower[tail_start..].to_string());
    }
    if let Some(rest) = lower.strip_prefix("::") {
        let rest = rest.strip_prefix("ffff:").unwrap_or(rest);
        if let Some((a, b)) = rest.split_once(':') {
            if is_hex_group(a) && is_hex_group(b) {
                let num = (u32::from_str_radix(a, 16).ok()? << 16) | u32::from_str_radix(b, 16).ok()?;
                return Some(ipv4_text(num));
            }
        }
        if let Some(embedded) = ipv4_from_last32(&lower) {
            return Some(embedded);
        }
    }
    if lower.starts_with("64:ff9b") {
        if let Some(embedded) = ipv4_from_last32(&lower) {
            return Some(embedded);
        }
    }
    if let Some(rest) = lower.strip_prefix("64:ff9b:") {
        let rest = rest.strip_prefix("::").unwrap_or(rest);
        if is_dotted(rest) {
            return Some(rest.to_string());
        }
        if let Some((a, b)) = rest.split_once(':') {
            if is_hex_group(a) && is_hex_group(b) {
                let num = (u32::from_str_radix(a, 16).ok()? << 16) | u32::from_str_radix(b, 16).ok()?;
                return Some(ipv4_text(num));
            }
        }
    }
    None
}

/// The WHATWG serialization of an IPv6 literal (what `new URL('http://[..]/').hostname` yields in JS).
fn canonical_ipv6(text: &str) -> Option<String> {
    let url = Url::parse(&format!("http://[{text}]/")).ok()?;
    let host = url.host_str()?;
    Some(host.trim_start_matches('[').trim_end_matches(']').to_string())
}

/// JS `isPrivateIp`: anything that is not a valid public unicast literal is "private".
pub fn is_private_ip(ip: &str) -> bool {
    let lowered = ip.to_ascii_lowercase();
    let stripped = lowered.strip_prefix('[').unwrap_or(&lowered);
    let h = stripped.strip_suffix(']').unwrap_or(stripped);
    if h.contains('%') {
        return true;
    }
    if h.parse::<std::net::Ipv4Addr>().is_ok() {
        let p: Vec<u32> = h.split('.').map(|s| s.parse().unwrap_or(0)).collect();
        return p[0] == 10
            || p[0] == 127
            || p[0] == 0
            || (p[0] == 169 && p[1] == 254)
            || (p[0] == 172 && (16..=31).contains(&p[1]))
            || (p[0] == 192 && p[1] == 168)
            || (p[0] == 100 && (64..=127).contains(&p[1]))
            || (p[0] == 198 && (18..=19).contains(&p[1]))
            || (p[0] == 192 && p[1] == 0 && p[2] == 0)
            || p[0] >= 224;
    }
    if h.parse::<std::net::Ipv6Addr>().is_err() {
        return true;
    }
    let Some(h) = canonical_ipv6(h) else { return true };
    let h = h.as_str();
    if h == "::" || h == "::1" || h.starts_with("fc") || h.starts_with("fd") {
        return true;
    }
    let b = h.as_bytes();
    if b.len() >= 3 && b[0] == b'f' && b[1] == b'e' && matches!(b[2], b'8' | b'9' | b'a' | b'b' | b'c' | b'd' | b'e' | b'f') {
        return true;
    }
    if h.starts_with("64:ff9b:1:") {
        return true;
    }
    if let Some(embedded) = embedded_ipv4(h) {
        return is_private_ip(&embedded);
    }
    if h.starts_with("2001:db8") || h.starts_with("2001:2:") || h.starts_with("2001:10:") || h.starts_with("2001:20:") {
        return true;
    }
    if let Some(rest) = h.strip_prefix("2002:") {
        let mut groups = rest.splitn(3, ':');
        if let (Some(a), Some(b), Some(_)) = (groups.next(), groups.next(), groups.next()) {
            if is_hex_group(a) && is_hex_group(b) {
                let num = (u32::from_str_radix(a, 16).unwrap_or(0) << 16) | u32::from_str_radix(b, 16).unwrap_or(0);
                if is_private_ip(&ipv4_text(num)) {
                    return true;
                }
            }
        }
    }
    h.starts_with("ff")
}

/// JS `orderAddresses`: IPv4 first, the two families interleaved.
fn order_addresses(addresses: &[IpAddr]) -> Vec<IpAddr> {
    let v4: Vec<IpAddr> = addresses.iter().copied().filter(IpAddr::is_ipv4).collect();
    let v6: Vec<IpAddr> = addresses.iter().copied().filter(IpAddr::is_ipv6).collect();
    let mut out = Vec::new();
    for i in 0..v4.len().max(v6.len()) {
        out.extend(v4.get(i));
        out.extend(v6.get(i));
    }
    out
}

/// DNS lookup seam: the real resolver in production, a fixed table in unit tests.
pub(crate) type Resolver<'a> = dyn Fn(&str) -> Result<Vec<IpAddr>, String> + 'a;

pub(crate) fn system_resolver(host: &str) -> Result<Vec<IpAddr>, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let host = host.to_string();
    let lookup_host = host.clone();
    std::thread::spawn(move || {
        let result = (lookup_host.as_str(), 0u16)
            .to_socket_addrs()
            .map(|addrs| addrs.map(|a| a.ip()).collect::<Vec<_>>())
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(_) => Err(format!("操作超时(5000ms)：DNS 解析 {host}")),
    }
}

/// JS `resolveSafeAddresses`: every answer must be public; returns the attempt order.
pub(crate) fn resolve_safe_addresses_with(hostname: &str, resolver: &Resolver) -> Result<Vec<IpAddr>, String> {
    let lowered = hostname.to_lowercase();
    let h = lowered.strip_prefix('[').unwrap_or(&lowered);
    let h = h.strip_suffix(']').unwrap_or(h);
    let h = h.strip_suffix('.').unwrap_or(h);
    if h.is_empty() {
        return Err("主机名为空".to_string());
    }
    if h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local") {
        return Err("禁止访问内网/本机地址".to_string());
    }
    if let Ok(ip) = h.parse::<IpAddr>() {
        if is_private_ip(h) {
            return Err("禁止访问内网/本机地址".to_string());
        }
        return Ok(vec![ip]);
    }
    let addresses = resolver(h).map_err(|e| format!("域名解析失败：{e}"))?;
    if addresses.is_empty() {
        return Err("域名没有解析结果".to_string());
    }
    if addresses.iter().any(|a| is_private_ip(&a.to_string())) {
        return Err("域名解析到内网/本机地址，已阻止".to_string());
    }
    let ordered = order_addresses(&addresses);
    Ok(if ordered.is_empty() { vec![addresses[0]] } else { ordered })
}

pub(crate) fn resolve_safe_addresses(hostname: &str) -> Result<Vec<IpAddr>, String> {
    resolve_safe_addresses_with(hostname, &system_resolver)
}

/// A URL that passed the policy together with the addresses it may be reached on.
pub(crate) struct Validated {
    pub url: Url,
    pub addresses: Vec<IpAddr>,
}

/// JS `validateFetchUrl`.
pub(crate) fn validate_fetch_url_with(raw: &str, resolver: &Resolver) -> Result<Validated, String> {
    let url = Url::parse(crate::aux_text::js_trim(raw)).map_err(|_| "URL 无效".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("仅允许 http/https".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL 不能包含凭据".to_string());
    }
    let host = url.host_str().ok_or_else(|| "主机名为空".to_string())?;
    let addresses = resolve_safe_addresses_with(host, resolver)?;
    Ok(Validated { url, addresses })
}

pub(crate) fn validate_fetch_url(raw: &str) -> Result<Validated, String> {
    validate_fetch_url_with(raw, &system_resolver)
}

// ----- transport ------------------------------------------------------------------------------------------------------

/// Why a transfer produced no usable response.
#[derive(Debug, PartialEq)]
pub(crate) enum TransferError {
    Timeout,
    Interrupted,
    Other(String),
}

pub(crate) struct Head {
    pub status: u16,
    pub location: Option<String>,
}

/// What the body reader decides after each chunk.
pub(crate) enum Flow {
    Continue,
    /// Enough was read: stop the transfer on purpose (not an error).
    Stop,
    Abort(String),
}

pub(crate) struct Transfer<'a> {
    pub url: &'a Url,
    pub pin: IpAddr,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub budget: Duration,
}

/// The system curl: `curl.exe` on Windows (the name that resolves to the bundled binary), `curl` elsewhere.
pub(crate) fn curl_program() -> &'static str {
    curl_program_for(cfg!(windows))
}

pub(crate) fn curl_program_for(windows: bool) -> &'static str {
    if windows {
        "curl.exe"
    } else {
        "curl"
    }
}

fn config_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0B}' => out.push_str("\\v"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn spawn_curl(transfer: &Transfer) -> Result<ChildGuard, TransferError> {
    let secs = format!("{:.3}", transfer.budget.as_secs_f64().max(0.001));
    let mut command = crate::subprocess::command_without_console(curl_program());
    command.args(["-sS", "-i", "--globoff", "--max-redirs", "0", "--noproxy", "*", "--connect-timeout", &secs, "--max-time", &secs]);
    if let Some(host) = transfer.url.host_str() {
        if host.parse::<IpAddr>().is_err() && !host.starts_with('[') {
            let port = transfer.url.port_or_known_default().unwrap_or(80);
            let pin = match transfer.pin {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            };
            command.args(["--resolve", &format!("{host}:{port}:{pin}")]);
        }
    }
    command.args(["-K", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe prctl between fork and exec; the child dies with this server.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|e| TransferError::Other(format!("调用 curl 失败：{e}")))?;
    let mut config = format!("url = {}\n", config_quote(transfer.url.as_str()));
    for (name, value) in &transfer.headers {
        config.push_str(&format!("header = {}\n", config_quote(&format!("{name}: {value}"))));
    }
    if let Some(body) = &transfer.body {
        config.push_str(&format!("data-binary = {}\n", config_quote(body)));
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(config.as_bytes());
    }
    Ok(ChildGuard(child))
}

fn parse_head(raw: &[u8]) -> Option<Head> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse::<u16>().ok()?;
    let location = lines.find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("location").then(|| value.trim().to_string())
    });
    Some(Head { status, location })
}

/// How reading the curl output ended.
enum ReadEnd {
    /// The reader asked to stop (`Flow::Stop`).
    Stopped(Head),
    Aborted(String),
    /// EOF; the head is present when the response started.
    Eof(Option<Head>),
}

fn read_response(
    stdout: &mut impl Read,
    on_head: &mut dyn FnMut(&Head) -> Flow,
    on_body: &mut dyn FnMut(&[u8]) -> Flow,
) -> ReadEnd {
    let mut pending: Vec<u8> = Vec::new();
    let mut head: Option<Head> = None;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => return ReadEnd::Eof(head),
            Ok(n) => n,
        };
        if head.is_some() {
            match on_body(&chunk[..n]) {
                Flow::Continue => continue,
                Flow::Stop => return ReadEnd::Stopped(head.take().expect("head present")),
                Flow::Abort(message) => return ReadEnd::Aborted(message),
            }
        }
        pending.extend_from_slice(&chunk[..n]);
        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            let Some(parsed) = parse_head(&pending[..end]) else {
                return ReadEnd::Aborted("响应头无法解析".to_string());
            };
            let rest = pending.split_off(end + 4);
            pending.clear();
            if (100..200).contains(&parsed.status) && parsed.status != 101 {
                // interim response (100 Continue ...): the real head follows
                pending = rest;
                continue;
            }
            match on_head(&parsed) {
                Flow::Continue => {}
                Flow::Stop => return ReadEnd::Stopped(parsed),
                Flow::Abort(message) => return ReadEnd::Aborted(message),
            }
            if !rest.is_empty() {
                match on_body(&rest) {
                    Flow::Continue => {}
                    Flow::Stop => return ReadEnd::Stopped(parsed),
                    Flow::Abort(message) => return ReadEnd::Aborted(message),
                }
            }
            head = Some(parsed);
            break;
        }
    }
}

/// Run one pinned request. `on_head` may end the transfer early (a redirect needs no body); `on_body` sees every
/// body chunk. Returns the head and whether the whole body was read (false: stopped on purpose).
pub(crate) fn run_transfer(
    transfer: &Transfer,
    on_head: &mut dyn FnMut(&Head) -> Flow,
    on_body: &mut dyn FnMut(&[u8]) -> Flow,
) -> Result<(Head, bool), TransferError> {
    let mut child = spawn_curl(transfer)?;
    let mut stdout = child.0.stdout.take().ok_or_else(|| TransferError::Other("curl 无输出".to_string()))?;
    let mut stderr = child.0.stderr.take();
    let diagnostics = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(s) = stderr.as_mut() {
            let _ = Read::take(s, 8192).read_to_string(&mut text);
        }
        text
    });
    let outcome = read_response(&mut stdout, on_head, on_body);
    if !matches!(outcome, ReadEnd::Eof(_)) {
        let _ = child.0.kill();
    }
    drop(stdout);
    let code = child.0.wait().ok().and_then(|s| s.code());
    let diagnostics = diagnostics.join().unwrap_or_default();
    match outcome {
        ReadEnd::Stopped(head) => Ok((head, false)),
        ReadEnd::Aborted(message) => Err(TransferError::Other(message)),
        ReadEnd::Eof(Some(head)) if code == Some(0) => Ok((head, true)),
        ReadEnd::Eof(_) if code == Some(28) => Err(TransferError::Timeout),
        ReadEnd::Eof(Some(_)) => Err(TransferError::Interrupted),
        ReadEnd::Eof(None) => {
            let text = diagnostics.trim().trim_start_matches("curl: ");
            Err(TransferError::Other(if text.is_empty() { "curl 无输出".to_string() } else { text.to_string() }))
        }
    }
}

/// The seam between the fetch logic and curl: the production transport is `run_transfer`.
pub(crate) type TransferFn<'a> = dyn Fn(&Transfer, &mut dyn FnMut(&Head) -> Flow, &mut dyn FnMut(&[u8]) -> Flow) -> Result<(Head, bool), TransferError> + 'a;

pub(crate) fn curl_transfer(
    transfer: &Transfer,
    on_head: &mut dyn FnMut(&Head) -> Flow,
    on_body: &mut dyn FnMut(&[u8]) -> Flow,
) -> Result<(Head, bool), TransferError> {
    run_transfer(transfer, on_head, on_body)
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Decode UTF-8 the way Node's `StringDecoder` does: invalid bytes become U+FFFD; an incomplete tail is held back
/// until more bytes arrive (`complete` flushes it as one U+FFFD).
fn decode_utf8(bytes: &[u8], complete: bool) -> String {
    let mut out = String::new();
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                return out;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(valid).unwrap_or(""));
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        rest = &after[n..];
                    }
                    None => {
                        if complete {
                            out.push('\u{FFFD}');
                        }
                        return out;
                    }
                }
            }
        }
    }
}

enum Hop {
    Redirect { status: u16, location: String },
    Page { status: u16, body: String, truncated: bool },
}

fn transfer_message(error: TransferError, host: &str) -> String {
    match error {
        TransferError::Timeout => format!("请求超时：{host}"),
        TransferError::Interrupted => "响应体读取中断".to_string(),
        TransferError::Other(message) => message,
    }
}

fn fetch_hop(url: &Url, addresses: &[IpAddr], limit: usize, transfer_fn: &TransferFn) -> Result<Hop, String> {
    let host = url.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_string();
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let attempts = addresses.len().min(MAX_ADDRESS_ATTEMPTS);
    let mut last_error: Option<String> = None;
    for pin in addresses.iter().take(attempts) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= Duration::from_millis(250) {
            break;
        }
        let budget = if addresses.len() == 1 { REQUEST_TIMEOUT } else { ATTEMPT_TIMEOUT.min(remaining) };
        let transfer = Transfer {
            url,
            pin: *pin,
            headers: vec![
                ("user-agent".into(), FETCH_USER_AGENT.into()),
                ("accept".into(), "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".into()),
                ("accept-language".into(), "zh-CN,zh;q=0.9".into()),
            ],
            body: None,
            budget,
        };
        let mut redirect: Option<(u16, String)> = None;
        let mut bytes: Vec<u8> = Vec::new();
        let mut text_len = 0usize;
        let mut on_head = |head: &Head| {
            if is_redirect(head.status) {
                redirect = Some((head.status, head.location.clone().unwrap_or_default()));
                Flow::Stop
            } else {
                Flow::Continue
            }
        };
        let mut on_body = |chunk: &[u8]| {
            bytes.extend_from_slice(chunk);
            // characters never outnumber bytes, so decoding can wait until the byte count could reach the limit
            if bytes.len() >= limit {
                text_len = decode_utf8(&bytes, false).chars().count();
                if text_len >= limit {
                    return Flow::Stop;
                }
            }
            Flow::Continue
        };
        match transfer_fn(&transfer, &mut on_head, &mut on_body) {
            Ok((head, complete)) => {
                if let Some((status, location)) = redirect {
                    return Ok(Hop::Redirect { status, location });
                }
                let decoded = decode_utf8(&bytes, complete);
                let truncated = decoded.chars().count() >= limit;
                let body: String = decoded.chars().take(limit).collect();
                return Ok(Hop::Page { status: head.status, body, truncated });
            }
            Err(error) => last_error = Some(transfer_message(error, &host)),
        }
    }
    let mut error = last_error.unwrap_or_else(|| format!("请求失败：{host}"));
    if attempts > 1 && !error.is_empty() {
        error = format!("{error}（已尝试 {attempts} 个地址）");
    }
    Err(error)
}

/// JS `safeFetch(url, maxChars)`: the page text (first `max_chars` characters) with status and truncation flag.
pub(crate) fn fetch_text_with(raw_url: &str, max_chars: usize, resolver: &Resolver, transfer_fn: &TransferFn) -> Result<Value, String> {
    let mut current = validate_fetch_url_with(raw_url, resolver)?;
    for _ in 0..=MAX_REDIRECTS {
        match fetch_hop(&current.url, &current.addresses, max_chars, transfer_fn)? {
            Hop::Redirect { status, location } => {
                if location.is_empty() {
                    return Err(format!("重定向缺少 Location: {status}"));
                }
                let next = current.url.join(&location).map_err(|_| "URL 无效".to_string())?;
                current = validate_fetch_url_with(next.as_str(), resolver)?;
            }
            Hop::Page { status, body, truncated } => {
                return Ok(json!({"url": current.url.as_str(), "statusCode": status, "truncated": truncated, "body": body}));
            }
        }
    }
    Err("重定向次数过多，已停止".to_string())
}

pub(crate) fn fetch_text(raw_url: &str, max_chars: usize) -> Result<Value, String> {
    fetch_text_with(raw_url, max_chars, &system_resolver, &curl_transfer)
}

/// One JSON POST to an endpoint whose addresses already passed the policy. Redirects are refused; an HTTP status
/// is a result (returned), network failures move on to the next address (at most two).
pub(crate) fn post_json_with(
    url: &Url,
    addresses: &[IpAddr],
    headers: Vec<(String, String)>,
    body: &str,
    timeout: Duration,
    max_bytes: usize,
    transfer_fn: &TransferFn,
) -> Result<(u16, String), String> {
    let deadline = Instant::now() + timeout;
    let attempts = addresses.len().min(2);
    if attempts == 0 {
        return Err("没有任何可用地址".to_string());
    }
    let mut last_error: Option<String> = None;
    for pin in addresses.iter().take(attempts) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= Duration::from_millis(500) {
            break;
        }
        let budget = if attempts == 1 { timeout } else { remaining.min(timeout.div_f64(2.0)) };
        let transfer = Transfer { url, pin: *pin, headers: headers.clone(), body: Some(body.to_string()), budget };
        let mut bytes: Vec<u8> = Vec::new();
        let mut on_head = |head: &Head| {
            if (300..400).contains(&head.status) {
                Flow::Abort(format!("联网搜索端点返回重定向（HTTP {}），已拒绝跟随", head.status))
            } else {
                Flow::Continue
            }
        };
        let mut on_body = |chunk: &[u8]| {
            bytes.extend_from_slice(chunk);
            if bytes.len() > max_bytes {
                Flow::Abort("联网搜索响应体过大，已中止".to_string())
            } else {
                Flow::Continue
            }
        };
        match transfer_fn(&transfer, &mut on_head, &mut on_body) {
            Ok((head, _)) => return Ok((head.status, String::from_utf8_lossy(&bytes).into_owned())),
            Err(TransferError::Timeout) => {
                last_error = Some(format!("联网搜索请求超时（{}s）", (budget.as_secs_f64()).round() as u64));
            }
            Err(TransferError::Interrupted) => last_error = Some("联网搜索响应中断".to_string()),
            Err(TransferError::Other(message)) => last_error = Some(message),
        }
    }
    Err(last_error.unwrap_or_else(|| "联网搜索请求失败".to_string()))
}

pub(crate) fn post_json(url: &Url, addresses: &[IpAddr], headers: Vec<(String, String)>, body: &str, timeout: Duration, max_bytes: usize) -> Result<(u16, String), String> {
    post_json_with(url, addresses, headers, body, timeout, max_bytes, &curl_transfer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_address_policy_matches_the_frozen_js() {
        let oracle: Value = serde_json::from_str(include_str!("../tests/fixtures/aux-oracle.json")).expect("aux oracle");
        let cases = oracle["privateIp"].as_array().unwrap();
        assert!(cases.len() > 90);
        for case in cases {
            let ip = case[0].as_str().unwrap();
            assert_eq!(is_private_ip(ip), case[1].as_bool().unwrap(), "ip {ip:?}");
        }
    }

    // ----- fetch logic against a scripted transport ------------------------------------------------------------------

    use std::cell::RefCell;

    fn table(host: &str) -> Result<Vec<IpAddr>, String> {
        match host {
            "public.test" => Ok(vec!["2606:4700::1".parse().unwrap(), "93.184.216.34".parse().unwrap(), "93.184.216.35".parse().unwrap()]),
            "rebind.test" => Ok(vec!["93.184.216.34".parse().unwrap(), "10.0.0.5".parse().unwrap()]),
            "empty.test" => Ok(vec![]),
            _ => Err("getaddrinfo ENOTFOUND".to_string()),
        }
    }

    type Script = Box<dyn Fn(&Transfer) -> Result<(u16, Option<String>, Vec<Vec<u8>>), TransferError>>;

    /// Drive `fetch_text_with` with a scripted transport; the log records `(url, pinned address, budget seconds)`.
    fn run(url: &str, limit: usize, script: Script) -> (Result<Value, String>, Vec<(String, String, u64)>) {
        let log: RefCell<Vec<(String, String, u64)>> = RefCell::new(Vec::new());
        let transfer = |t: &Transfer, on_head: &mut dyn FnMut(&Head) -> Flow, on_body: &mut dyn FnMut(&[u8]) -> Flow| {
            log.borrow_mut().push((t.url.to_string(), t.pin.to_string(), t.budget.as_secs()));
            let (status, location, chunks) = script(t)?;
            let head = Head { status, location };
            if let Flow::Stop = on_head(&head) {
                return Ok((head, false));
            }
            for chunk in chunks {
                if let Flow::Stop = on_body(&chunk) {
                    return Ok((head, false));
                }
            }
            Ok((head, true))
        };
        let result = fetch_text_with(url, limit, &table, &transfer);
        (result, log.into_inner())
    }

    fn page(status: u16, body: &str) -> Script {
        let body = body.as_bytes().to_vec();
        Box::new(move |_| Ok((status, None, vec![body.clone()])))
    }

    #[test]
    fn a_page_is_returned_with_status_url_and_untruncated_body() {
        let (result, log) = run("HTTP://Public.Test/a b?q=1#frag", 50_000, page(200, "你好 world"));
        assert_eq!(result.unwrap(), json!({"url": "http://public.test/a%20b?q=1#frag", "statusCode": 200, "truncated": false, "body": "你好 world"}));
        // IPv4 first, families interleaved; three addresses -> the per-attempt budget applies
        assert_eq!(log, vec![("http://public.test/a%20b?q=1#frag".to_string(), "93.184.216.34".to_string(), 8)]);
    }

    #[test]
    fn the_body_is_cut_at_the_character_limit_and_flagged_truncated() {
        let (result, _) = run("http://public.test/", 4, page(200, "你好世界再见"));
        assert_eq!(result.unwrap()["body"], "你好世界");
        // a body that is exactly the limit is flagged too (the frozen JS flags it when the limit is reached)
        let (result, _) = run("http://public.test/", 4, page(200, "abcd"));
        let value = result.unwrap();
        assert_eq!((value["body"].as_str().unwrap(), value["truncated"].as_bool().unwrap()), ("abcd", true));
        let (result, _) = run("http://public.test/", 5, page(200, "abcd"));
        assert_eq!(result.unwrap()["truncated"], false);
    }

    #[test]
    fn multi_byte_characters_split_across_chunks_decode_and_invalid_bytes_become_replacement_characters() {
        let bytes = "中文".as_bytes().to_vec();
        let split: Script = Box::new(move |_| Ok((200, None, vec![bytes[..2].to_vec(), bytes[2..].to_vec(), vec![0xff, b'x']])));
        let (result, _) = run("http://public.test/", 100, split);
        assert_eq!(result.unwrap()["body"], "中文\u{fffd}x");
        let cut: Script = Box::new(|_| Ok((200, None, vec![vec![b'a', 0xe4, 0xb8]])));
        let (result, _) = run("http://public.test/", 100, cut);
        assert_eq!(result.unwrap()["body"], "a\u{fffd}");
    }

    #[test]
    fn non_success_statuses_are_results_not_errors() {
        let (result, _) = run("http://public.test/", 100, page(404, "gone"));
        let value = result.unwrap();
        assert_eq!((value["statusCode"].as_u64(), value["body"].as_str()), (Some(404), Some("gone")));
    }

    #[test]
    fn every_redirect_hop_is_revalidated_and_the_chain_is_bounded() {
        // a redirect to a loopback target is refused before any connection
        let to_loopback: Script = Box::new(|_| Ok((302, Some("http://127.0.0.1:8080/admin".into()), vec![])));
        let (result, log) = run("http://public.test/", 100, to_loopback);
        assert_eq!(result.unwrap_err(), "禁止访问内网/本机地址");
        assert_eq!(log.len(), 1);
        // a relative Location resolves against the current URL; a DNS answer containing a private address is refused
        let hops = std::cell::Cell::new(0);
        let relative: Script = Box::new(move |t| {
            hops.set(hops.get() + 1);
            if t.url.path() == "/dir/start" {
                Ok((301, Some("../next?x=1".into()), vec![]))
            } else {
                Ok((200, None, vec![b"done".to_vec()]))
            }
        });
        let (result, _) = run("http://public.test/dir/start", 100, relative);
        assert_eq!(result.unwrap()["url"], "http://public.test/next?x=1");
        let rebind: Script = Box::new(|_| Ok((307, Some("http://rebind.test/".into()), vec![])));
        assert_eq!(run("http://public.test/", 100, rebind).0.unwrap_err(), "域名解析到内网/本机地址，已阻止");
        let missing: Script = Box::new(|_| Ok((302, None, vec![])));
        assert_eq!(run("http://public.test/", 100, missing).0.unwrap_err(), "重定向缺少 Location: 302");
        let endless: Script = Box::new(|_| Ok((302, Some("/again".into()), vec![])));
        let (result, log) = run("http://public.test/", 100, endless);
        assert_eq!(result.unwrap_err(), "重定向次数过多，已停止");
        assert_eq!(log.len(), 6, "the first request plus five redirects");
    }

    #[test]
    fn the_url_policy_rejects_before_any_transfer() {
        for (url, why) in [
            ("ftp://public.test/", "仅允许 http/https"),
            ("not a url", "URL 无效"),
            ("http://user:pw@public.test/", "URL 不能包含凭据"),
            ("http://localhost/", "禁止访问内网/本机地址"),
            ("http://printer.local/", "禁止访问内网/本机地址"),
            ("http://a.localhost./", "禁止访问内网/本机地址"),
            ("http://127.0.0.1/", "禁止访问内网/本机地址"),
            ("http://2130706433/", "禁止访问内网/本机地址"),
            ("http://0x7f.1/", "禁止访问内网/本机地址"),
            ("http://[::1]/", "禁止访问内网/本机地址"),
            ("http://[::ffff:10.0.0.1]/", "禁止访问内网/本机地址"),
            ("http://rebind.test/", "域名解析到内网/本机地址，已阻止"),
            ("http://empty.test/", "域名没有解析结果"),
            ("http://nx.test/", "域名解析失败：getaddrinfo ENOTFOUND"),
        ] {
            assert_eq!(run(url, 100, Box::new(|_| panic!("no transfer may start"))).0.unwrap_err(), why, "{url}");
        }
    }

    #[test]
    fn network_failures_move_to_the_next_address_and_the_error_names_the_attempts() {
        let calls = std::cell::Cell::new(0);
        let flaky: Script = Box::new(move |_| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(TransferError::Timeout)
            } else {
                Ok((200, None, vec![b"third".to_vec()]))
            }
        });
        let (result, log) = run("http://public.test/", 100, flaky);
        assert_eq!(result.unwrap()["body"], "third");
        assert_eq!(log.iter().map(|l| l.1.as_str()).collect::<Vec<_>>(), ["93.184.216.34", "2606:4700::1", "93.184.216.35"]);
        let down: Script = Box::new(|_| Err(TransferError::Timeout));
        assert_eq!(run("http://public.test/", 100, down).0.unwrap_err(), "请求超时：public.test（已尝试 3 个地址）");
        let reset: Script = Box::new(|_| Err(TransferError::Interrupted));
        assert_eq!(run("http://public.test/", 100, reset).0.unwrap_err(), "响应体读取中断（已尝试 3 个地址）");
        // one address: the whole 20 s budget, no attempt count in the message
        let single: Script = Box::new(|_| Err(TransferError::Other("connect refused".into())));
        let (result, log) = run("http://93.184.216.34/", 100, single);
        assert_eq!(result.unwrap_err(), "connect refused");
        assert_eq!(log[0].2, 20);
    }
}
