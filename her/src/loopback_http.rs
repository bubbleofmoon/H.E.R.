//! Bounded HTTP/1.1 client for literal loopback endpoints (`OneBot`, console tools, SnowLuma WebUI).

use std::borrow::Cow;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const DEFAULT_MAX_HEAD_BYTES: usize = 16 * 1024;
pub(crate) const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHUNK_SIZE_LINE_BYTES: usize = 1024;
const MAX_TRAILER_BYTES: usize = 16 * 1024;
const MAX_SOCKET_TIMEOUT: Duration = Duration::from_secs(3600);

/// A strictly validated literal loopback HTTP authority (`http://<loopback-ip>:<non-zero-port>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoopbackEndpoint {
    addr: SocketAddr,
    host: String,
    port: u16,
}

impl LoopbackEndpoint {
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let authority = url
            .strip_prefix("http://")
            .ok_or_else(|| "OneBot endpoint must use http://".to_string())?;
        Self::parse_authority_inner(authority)
    }

    pub(crate) fn parse_authority(authority: &str) -> Result<Self, String> {
        if authority.contains("://") {
            return Err("loopback endpoint authority must not include a scheme".to_string());
        }
        Self::parse_authority_inner(authority)
    }

    fn parse_authority_inner(authority: &str) -> Result<Self, String> {
        if authority.is_empty()
            || authority.chars().any(|ch| {
                matches!(ch, '/' | '?' | '#' | '@' | '\\')
                    || ch.is_ascii_control()
                    || ch.is_ascii_whitespace()
            })
        {
            return Err("OneBot endpoint must be a bare loopback authority".to_string());
        }
        let (host_str, port_str, bracketed) = if let Some(rest) = authority.strip_prefix('[') {
            let (host, tail) = rest
                .split_once(']')
                .ok_or_else(|| "OneBot endpoint has an invalid IPv6 authority".to_string())?;
            let port = tail
                .strip_prefix(':')
                .ok_or_else(|| "OneBot endpoint requires an explicit port".to_string())?;
            (host, port, true)
        } else {
            let (host, port) = authority
                .rsplit_once(':')
                .ok_or_else(|| "OneBot endpoint requires an explicit port".to_string())?;
            (host, port, false)
        };
        let address = host_str
            .parse::<IpAddr>()
            .map_err(|_| "OneBot endpoint host must be a literal loopback IP".to_string())?;
        if address.is_ipv6() != bracketed {
            return Err("IPv6 OneBot endpoints must use bracketed authority syntax".to_string());
        }
        if !address.is_loopback() {
            return Err("OneBot endpoint must use a loopback IP".to_string());
        }
        if port_str.is_empty() || !port_str.bytes().all(|b| b.is_ascii_digit()) {
            return Err("OneBot endpoint requires an explicit non-zero port".to_string());
        }
        let port = port_str
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| "OneBot endpoint requires an explicit non-zero port".to_string())?;
        let host = match address {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        Ok(Self {
            addr: SocketAddr::new(address, port),
            host,
            port,
        })
    }

    pub(crate) fn localhost_v4(port: u16) -> Result<Self, String> {
        if port == 0 {
            return Err("loopback endpoint requires a non-zero port".to_string());
        }
        Ok(Self {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            host: "127.0.0.1".to_string(),
            port,
        })
    }

    pub(crate) fn socket_addr(&self) -> SocketAddr {
        self.addr
    }

    #[cfg(test)]
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    #[cfg(test)]
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn host_header(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Transport seam for loopback HTTP streams.
pub(crate) trait LoopbackStream: Read + Write + Send {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()>;
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()>;
}

impl LoopbackStream for TcpStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
}

pub(crate) trait Transport: Send + Sync {
    fn connect(
        &self,
        endpoint: &LoopbackEndpoint,
        timeout: Duration,
    ) -> std::io::Result<Box<dyn LoopbackStream>>;
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TcpTransport;

impl Transport for TcpTransport {
    fn connect(
        &self,
        endpoint: &LoopbackEndpoint,
        timeout: Duration,
    ) -> std::io::Result<Box<dyn LoopbackStream>> {
        let stream = TcpStream::connect_timeout(&endpoint.socket_addr(), timeout)?;
        Ok(Box::new(stream))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadlines {
    pub connect: Duration,
    pub write: Duration,
    pub read: Duration,
    pub total: Option<Duration>,
    pub write_from_connect_start: bool,
}

impl Deadlines {
    pub(crate) fn split(connect: Duration, write: Duration, read: Duration) -> Self {
        Self {
            connect,
            write,
            read,
            total: None,
            write_from_connect_start: false,
        }
    }

    pub(crate) fn within(total: Duration, connect_cap: Duration) -> Self {
        Self {
            connect: total.min(connect_cap),
            write: total,
            read: total,
            total: Some(total),
            write_from_connect_start: false,
        }
    }

    pub(crate) fn with_shared_transmit(transmit: Duration, read: Duration) -> Self {
        Self {
            connect: read.min(transmit),
            write: read.min(transmit),
            read,
            total: Some(read),
            write_from_connect_start: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResponseLimits {
    pub max_head_bytes: usize,
    pub max_body_bytes: usize,
}

impl ResponseLimits {
    pub(crate) fn new(max_body_bytes: usize) -> Self {
        Self {
            max_head_bytes: DEFAULT_MAX_HEAD_BYTES,
            max_body_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonSurrogates {
    Strict,
    ReplaceLoneWithFffd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitStatus {
    /// Failed before any write attempt was made; safe to classify as definitely not committed.
    NotCommitted,
    /// Failed at or after a write attempt; the peer may have received and acted on the request.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpError {
    pub status: CommitStatus,
    pub timed_out: bool,
    pub detail: String,
}

impl HttpError {
    pub(crate) fn not_committed(timed_out: bool, detail: impl Into<String>) -> Self {
        Self {
            status: CommitStatus::NotCommitted,
            timed_out,
            detail: detail.into(),
        }
    }

    pub(crate) fn unknown(timed_out: bool, detail: impl Into<String>) -> Self {
        Self {
            status: CommitStatus::Unknown,
            timed_out,
            detail: detail.into(),
        }
    }

    pub(crate) fn to_qq_tools_message(&self) -> String {
        if self.timed_out {
            "The operation was aborted due to timeout".to_string()
        } else {
            "fetch failed".to_string()
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HttpRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Option<&'a [u8]>,
}

#[derive(Debug, Clone)]
pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub(crate) fn body_utf8(&self) -> Result<&str, String> {
        std::str::from_utf8(&self.body).map_err(|_| "response is not UTF-8".to_string())
    }

    pub(crate) fn body_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub(crate) fn parse_json<T: DeserializeOwned>(
        &self,
        surrogates: JsonSurrogates,
    ) -> Result<(T, bool), String> {
        let text = self.body_utf8()?;
        parse_json_text(text, surrogates).map_err(|error| format!("response is not JSON: {error}"))
    }

    pub(crate) fn parse_json_lossy<T: DeserializeOwned>(
        &self,
        surrogates: JsonSurrogates,
    ) -> Result<(T, bool), serde_json::Error> {
        let text = self.body_lossy();
        parse_json_text(&text, surrogates)
    }
}

fn parse_json_text<T: DeserializeOwned>(
    text: &str,
    surrogates: JsonSurrogates,
) -> Result<(T, bool), serde_json::Error> {
    match surrogates {
        JsonSurrogates::Strict => serde_json::from_str::<T>(text).map(|value| (value, false)),
        JsonSurrogates::ReplaceLoneWithFffd => {
            crate::aux_text::parse_json_replacing_lone_surrogates::<T>(text)
                .map(|(value, repaired)| (value, repaired.is_some()))
        }
    }
}

/// Connected stream after the full HTTP/1.1 request was handed to the socket.
pub(crate) struct PendingHttpResponse {
    stream: Box<dyn LoopbackStream>,
    read_deadline: Instant,
}

impl PendingHttpResponse {
    pub(crate) fn read(mut self, limits: ResponseLimits) -> Result<HttpResponse, HttpError> {
        let mut buf = Vec::new();
        let mut block = [0u8; 8192];
        let sep = loop {
            if let Some(sep) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                break sep;
            }
            if buf.len() > limits.max_head_bytes {
                return Err(HttpError::unknown(
                    false,
                    "read response: response headers exceed limit",
                ));
            }
            let n = read_chunk(&mut *self.stream, self.read_deadline, &mut block)?;
            if n == 0 {
                return Err(HttpError::unknown(
                    false,
                    "read response: response has no HTTP header/body separator",
                ));
            }
            buf.extend_from_slice(&block[..n]);
        };
        if sep > limits.max_head_bytes {
            return Err(HttpError::unknown(
                false,
                "read response: response headers exceed limit",
            ));
        }
        let parsed = parse_headers(&buf[..sep])?;
        let initial_body = &buf[sep + 4..];
        let body = match parsed.framing {
            BodyFraming::Empty => Vec::new(),
            BodyFraming::ContentLength(len) => read_content_length_body(
                &mut *self.stream,
                self.read_deadline,
                initial_body,
                len,
                limits.max_body_bytes,
            )?,
            BodyFraming::Chunked => read_chunked_body(
                &mut *self.stream,
                self.read_deadline,
                initial_body,
                limits.max_body_bytes,
            )?,
            BodyFraming::CloseDelimited => read_close_delimited_body(
                &mut *self.stream,
                self.read_deadline,
                initial_body,
                limits.max_body_bytes,
            )?,
        };
        Ok(HttpResponse {
            status: parsed.status,
            body,
        })
    }
}

/// Connect, send the HTTP/1.1 request, and read the full bounded response.
pub(crate) fn request(
    transport: &dyn Transport,
    endpoint: &LoopbackEndpoint,
    req: &HttpRequest<'_>,
    deadlines: Deadlines,
    limits: ResponseLimits,
) -> Result<HttpResponse, HttpError> {
    let pending = transmit(transport, endpoint, req, deadlines)?;
    pending.read(limits)
}

/// Connect and write the full HTTP/1.1 request, returning the stream before reading the response.
pub(crate) fn transmit(
    transport: &dyn Transport,
    endpoint: &LoopbackEndpoint,
    req: &HttpRequest<'_>,
    deadlines: Deadlines,
) -> Result<PendingHttpResponse, HttpError> {
    let wire = build_request_bytes(endpoint, req)?;
    let start = Instant::now();
    let total_deadline = deadlines.total.map(|total| start + total);
    let connect_budget = match total_deadline {
        Some(total_dl) => {
            let left = total_dl
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| HttpError::not_committed(true, "request deadline exceeded"))?;
            deadlines.connect.min(left)
        }
        None => deadlines.connect,
    };
    if connect_budget.is_zero() {
        return Err(HttpError::not_committed(true, "request deadline exceeded"));
    }
    let mut stream = transport
        .connect(endpoint, connect_budget)
        .map_err(|error| {
            HttpError::not_committed(
                is_timeout_kind(error.kind()),
                format!(
                    "connect {}: {error} (raw_os={:?})",
                    endpoint.host_header(),
                    error.raw_os_error()
                ),
            )
        })?;

    let write_base = if deadlines.write_from_connect_start {
        start
    } else {
        Instant::now()
    };
    let mut write_deadline = write_base + deadlines.write;
    if let Some(total_dl) = total_deadline {
        write_deadline = write_deadline.min(total_dl);
    }

    let mut pending = wire.as_slice();
    let mut write_attempted = false;
    while !pending.is_empty() {
        let Some(left) = write_deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            return Err(if write_attempted {
                HttpError::unknown(true, "write request: transmission deadline exceeded")
            } else {
                HttpError::not_committed(true, "write request: transmission deadline exceeded")
            });
        };
        stream
            .set_write_timeout(Some(left.min(MAX_SOCKET_TIMEOUT)))
            .map_err(|error| {
                let detail = format!("set write timeout: {error}");
                if write_attempted {
                    HttpError::unknown(false, detail)
                } else {
                    HttpError::not_committed(false, detail)
                }
            })?;
        write_attempted = true;
        match stream.write(pending) {
            Ok(0) => {
                return Err(HttpError::unknown(
                    false,
                    "write request: connection closed",
                ));
            }
            Ok(count) => pending = &pending[count..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(HttpError::unknown(
                    is_timeout_kind(error.kind()),
                    format!(
                        "write request: {error} (raw_os={:?})",
                        error.raw_os_error()
                    ),
                ));
            }
        }
    }

    let read_deadline = match total_deadline {
        Some(total_dl) => total_dl.min(Instant::now() + deadlines.read),
        None => Instant::now() + deadlines.read,
    };
    Ok(PendingHttpResponse {
        stream,
        read_deadline,
    })
}

fn is_timeout_kind(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn build_request_bytes(
    endpoint: &LoopbackEndpoint,
    req: &HttpRequest<'_>,
) -> Result<Vec<u8>, HttpError> {
    if req.method.is_empty()
        || !req
            .method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(HttpError::not_committed(false, "invalid HTTP method"));
    }
    if !req.path.starts_with('/')
        || req
            .path
            .chars()
            .any(|ch| ch.is_ascii_control() || ch.is_ascii_whitespace())
    {
        return Err(HttpError::not_committed(false, "invalid HTTP request path"));
    }
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        req.method,
        req.path,
        endpoint.host_header()
    );
    for (name, value) in req.headers {
        if name.is_empty()
            || !name.bytes().all(is_valid_header_name_byte)
            || value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
        {
            return Err(HttpError::not_committed(false, "invalid HTTP header"));
        }
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(body) = req.body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let mut wire = head.into_bytes();
    if let Some(body) = req.body {
        wire.extend_from_slice(body);
    }
    Ok(wire)
}

fn is_valid_header_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
}

enum BodyFraming {
    Empty,
    ContentLength(usize),
    Chunked,
    CloseDelimited,
}

struct ParsedHead {
    status: u16,
    framing: BodyFraming,
}

fn parse_headers(head_bytes: &[u8]) -> Result<ParsedHead, HttpError> {
    let head = String::from_utf8_lossy(head_bytes);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(HttpError::unknown(
            false,
            "read response: malformed HTTP version",
        ));
    }
    let status_str = parts.next().unwrap_or("");
    if status_str.len() != 3 || !status_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HttpError::unknown(
            false,
            "read response: malformed HTTP status code",
        ));
    }
    let status = status_str
        .parse::<u16>()
        .map_err(|_| HttpError::unknown(false, "read response: malformed HTTP status code"))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpError::unknown(false, "read response: malformed HTTP header"))?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HttpError::unknown(
                    false,
                    "read response: invalid Content-Length",
                ));
            }
            let parsed = value.parse::<usize>().map_err(|_| {
                HttpError::unknown(false, "read response: invalid Content-Length")
            })?;
            if let Some(prev) = content_length {
                if prev != parsed {
                    return Err(HttpError::unknown(
                        false,
                        "read response: conflicting Content-Length",
                    ));
                }
            } else {
                content_length = Some(parsed);
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(HttpError::unknown(
                    false,
                    "read response: unsupported Transfer-Encoding",
                ));
            }
            chunked = true;
        }
    }
    if chunked && content_length.is_some() {
        return Err(HttpError::unknown(
            false,
            "read response: conflicting Transfer-Encoding and Content-Length",
        ));
    }
    let framing = if chunked {
        BodyFraming::Chunked
    } else if let Some(len) = content_length {
        BodyFraming::ContentLength(len)
    } else if matches!(status, 204 | 304) {
        BodyFraming::Empty
    } else {
        BodyFraming::CloseDelimited
    };
    Ok(ParsedHead { status, framing })
}

fn read_chunk(
    stream: &mut dyn LoopbackStream,
    deadline: Instant,
    block: &mut [u8],
) -> Result<usize, HttpError> {
    loop {
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            return Err(HttpError::unknown(
                true,
                "read response: response deadline exceeded",
            ));
        };
        stream
            .set_read_timeout(Some(remaining.min(MAX_SOCKET_TIMEOUT)))
            .map_err(|error| HttpError::unknown(false, format!("set response deadline: {error}")))?;
        match stream.read(block) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(HttpError::unknown(
                    is_timeout_kind(error.kind()),
                    format!(
                        "read response: {error} (raw_os={:?})",
                        error.raw_os_error()
                    ),
                ));
            }
        }
    }
}

fn read_content_length_body(
    stream: &mut dyn LoopbackStream,
    deadline: Instant,
    initial: &[u8],
    content_length: usize,
    max_body_bytes: usize,
) -> Result<Vec<u8>, HttpError> {
    if content_length > max_body_bytes {
        return Err(HttpError::unknown(
            false,
            "read response: response exceeds limit",
        ));
    }
    if initial.len() >= content_length {
        return Ok(initial[..content_length].to_vec());
    }
    let mut body = Vec::with_capacity(content_length.min(64 * 1024));
    body.extend_from_slice(initial);
    let mut block = [0u8; 8192];
    while body.len() < content_length {
        let n = read_chunk(stream, deadline, &mut block)?;
        if n == 0 {
            return Err(HttpError::unknown(
                false,
                "read response: truncated body",
            ));
        }
        let take = (content_length - body.len()).min(n);
        body.extend_from_slice(&block[..take]);
    }
    Ok(body)
}

fn read_close_delimited_body(
    stream: &mut dyn LoopbackStream,
    deadline: Instant,
    initial: &[u8],
    max_body_bytes: usize,
) -> Result<Vec<u8>, HttpError> {
    if initial.len() > max_body_bytes {
        return Err(HttpError::unknown(
            false,
            "read response: response exceeds limit",
        ));
    }
    let mut body = initial.to_vec();
    let mut block = [0u8; 8192];
    loop {
        let n = read_chunk(stream, deadline, &mut block)?;
        if n == 0 {
            break;
        }
        if n > max_body_bytes.saturating_sub(body.len()) {
            return Err(HttpError::unknown(
                false,
                "read response: response exceeds limit",
            ));
        }
        body.extend_from_slice(&block[..n]);
    }
    Ok(body)
}

enum ChunkState {
    SizeLine,
    Data { remaining: usize },
    DataCrLf,
    Trailers,
}

fn read_chunked_body(
    stream: &mut dyn LoopbackStream,
    deadline: Instant,
    initial: &[u8],
    max_body_bytes: usize,
) -> Result<Vec<u8>, HttpError> {
    let mut buf = initial.to_vec();
    let mut pos = 0usize;
    let mut decoded = Vec::new();
    let mut trailer_bytes = 0usize;
    let mut state = ChunkState::SizeLine;
    let mut block = [0u8; 8192];

    loop {
        match state {
            ChunkState::SizeLine => {
                let tail = &buf[pos..];
                if let Some(crlf) = tail.windows(2).position(|w| w == b"\r\n") {
                    if crlf > MAX_CHUNK_SIZE_LINE_BYTES {
                        return Err(HttpError::unknown(
                            false,
                            "read response: chunk size line too long",
                        ));
                    }
                    let line = &tail[..crlf];
                    let size_part = line.split(|b| *b == b';').next().unwrap_or(&[]);
                    let size_str = std::str::from_utf8(size_part)
                        .map_err(|_| HttpError::unknown(false, "read response: bad chunk size"))?
                        .trim();
                    if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(HttpError::unknown(
                            false,
                            "read response: bad chunk size",
                        ));
                    }
                    let chunk_size = usize::from_str_radix(size_str, 16).map_err(|_| {
                        HttpError::unknown(false, "read response: bad chunk size")
                    })?;
                    pos += crlf + 2;
                    if chunk_size > max_body_bytes.saturating_sub(decoded.len()) {
                        return Err(HttpError::unknown(
                            false,
                            "read response: response exceeds limit",
                        ));
                    }
                    state = if chunk_size == 0 {
                        ChunkState::Trailers
                    } else {
                        ChunkState::Data {
                            remaining: chunk_size,
                        }
                    };
                    continue;
                }
                if tail.len() > MAX_CHUNK_SIZE_LINE_BYTES {
                    return Err(HttpError::unknown(
                        false,
                        "read response: chunk size line too long",
                    ));
                }
            }
            ChunkState::Data { remaining } => {
                let avail = buf.len() - pos;
                if avail > 0 {
                    let take = avail.min(remaining);
                    if take > max_body_bytes.saturating_sub(decoded.len()) {
                        return Err(HttpError::unknown(
                            false,
                            "read response: response exceeds limit",
                        ));
                    }
                    decoded.extend_from_slice(&buf[pos..pos + take]);
                    pos += take;
                    state = if take == remaining {
                        ChunkState::DataCrLf
                    } else {
                        ChunkState::Data {
                            remaining: remaining - take,
                        }
                    };
                    continue;
                }
            }
            ChunkState::DataCrLf => {
                let tail = &buf[pos..];
                if tail.len() >= 2 {
                    if &tail[..2] != b"\r\n" {
                        return Err(HttpError::unknown(
                            false,
                            "read response: bad chunk terminator",
                        ));
                    }
                    pos += 2;
                    state = ChunkState::SizeLine;
                    continue;
                }
            }
            ChunkState::Trailers => {
                let tail = &buf[pos..];
                if let Some(crlf) = tail.windows(2).position(|w| w == b"\r\n") {
                    trailer_bytes = trailer_bytes.saturating_add(crlf + 2);
                    if trailer_bytes > MAX_TRAILER_BYTES {
                        return Err(HttpError::unknown(
                            false,
                            "read response: chunk trailers exceed limit",
                        ));
                    }
                    pos += crlf + 2;
                    if crlf == 0 {
                        return Ok(decoded);
                    }
                    continue;
                }
                if trailer_bytes.saturating_add(tail.len()) > MAX_TRAILER_BYTES {
                    return Err(HttpError::unknown(
                        false,
                        "read response: chunk trailers exceed limit",
                    ));
                }
            }
        }

        if pos > 0 {
            buf.drain(..pos);
            pos = 0;
        }
        let n = read_chunk(stream, deadline, &mut block)?;
        if n == 0 {
            return Err(HttpError::unknown(
                false,
                "read response: truncated chunked body",
            ));
        }
        buf.extend_from_slice(&block[..n]);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    pub(crate) enum Step {
        ConnectError(ErrorKind, &'static str),
        Stream(ScriptedStreamConfig),
    }

    #[derive(Clone)]
    pub(crate) struct ScriptedStreamConfig {
        pub write_plan: Vec<Result<usize, ErrorKind>>,
        pub read_plan: VecDeque<Result<Vec<u8>, ErrorKind>>,
        pub delay_per_read: Duration,
        pub delay_per_write: Duration,
    }

    impl ScriptedStreamConfig {
        pub(crate) fn reply_bytes(bytes: Vec<u8>) -> Self {
            Self::reply_chunks(vec![bytes])
        }

        pub(crate) fn reply_chunks(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                write_plan: Vec::new(),
                read_plan: chunks.into_iter().map(Ok).collect(),
                delay_per_read: Duration::ZERO,
                delay_per_write: Duration::ZERO,
            }
        }
    }

    pub(crate) struct MemoryTransport {
        steps: Mutex<VecDeque<Step>>,
        connect_calls: Mutex<Vec<LoopbackEndpoint>>,
        written: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl MemoryTransport {
        pub(crate) fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: Mutex::new(steps.into_iter().collect()),
                connect_calls: Mutex::new(Vec::new()),
                written: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub(crate) fn connect_count(&self) -> usize {
            self.connect_calls.lock().unwrap().len()
        }

        pub(crate) fn written_requests(&self) -> Vec<String> {
            self.written
                .lock()
                .unwrap()
                .iter()
                .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                .collect()
        }
    }

    impl Transport for MemoryTransport {
        fn connect(
            &self,
            endpoint: &LoopbackEndpoint,
            _timeout: Duration,
        ) -> std::io::Result<Box<dyn LoopbackStream>> {
            self.connect_calls.lock().unwrap().push(endpoint.clone());
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted transport step");
            match step {
                Step::ConnectError(kind, msg) => Err(std::io::Error::new(kind, msg)),
                Step::Stream(cfg) => {
                    let idx = {
                        let mut guard = self.written.lock().unwrap();
                        guard.push(Vec::new());
                        guard.len() - 1
                    };
                    Ok(Box::new(MemoryStream {
                        cfg,
                        write_index: 0,
                        written_sink: Arc::clone(&self.written),
                        sink_idx: idx,
                    }))
                }
            }
        }
    }

    struct MemoryStream {
        cfg: ScriptedStreamConfig,
        write_index: usize,
        written_sink: Arc<Mutex<Vec<Vec<u8>>>>,
        sink_idx: usize,
    }

    impl Read for MemoryStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.cfg.delay_per_read.is_zero() {
                std::thread::sleep(self.cfg.delay_per_read);
            }
            match self.cfg.read_plan.pop_front() {
                None => Ok(0),
                Some(Err(kind)) => Err(std::io::Error::new(kind, "scripted read error")),
                Some(Ok(bytes)) => {
                    if bytes.is_empty() {
                        return Ok(0);
                    }
                    let take = bytes.len().min(buf.len());
                    buf[..take].copy_from_slice(&bytes[..take]);
                    if take < bytes.len() {
                        self.cfg.read_plan.push_front(Ok(bytes[take..].to_vec()));
                    }
                    Ok(take)
                }
            }
        }
    }

    impl Write for MemoryStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.cfg.delay_per_write.is_zero() {
                std::thread::sleep(self.cfg.delay_per_write);
            }
            if let Some(step) = self.cfg.write_plan.get(self.write_index).cloned() {
                self.write_index += 1;
                match step {
                    Ok(limit) => {
                        let take = buf.len().min(limit);
                        self.written_sink.lock().unwrap()[self.sink_idx]
                            .extend_from_slice(&buf[..take]);
                        Ok(take)
                    }
                    Err(kind) => Err(std::io::Error::new(kind, "scripted write error")),
                }
            } else {
                self.written_sink.lock().unwrap()[self.sink_idx].extend_from_slice(buf);
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LoopbackStream for MemoryStream {
        fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&mut self, _timeout: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{MemoryTransport, ScriptedStreamConfig, Step};
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn strict_loopback_validation_accepts_ip_literals_and_rejects_names_and_remote_ips() {
        let v4 = LoopbackEndpoint::parse("http://127.0.0.1:43103").unwrap();
        assert_eq!(v4.host(), "127.0.0.1");
        assert_eq!(v4.port(), 43103);
        assert_eq!(v4.host_header(), "127.0.0.1:43103");

        let v6 = LoopbackEndpoint::parse("http://[::1]:43104").unwrap();
        assert_eq!(v6.host(), "[::1]");
        assert_eq!(v6.port(), 43104);
        assert_eq!(v6.host_header(), "[::1]:43104");

        for invalid in [
            "http://localhost:43103",
            "http://example.com:80",
            "http://192.0.2.1:43103",
            "http://0.0.0.0:43103",
            "https://127.0.0.1:43103",
            "http://127.0.0.1:0",
            "http://127.0.0.1:+80",
            "http://127.0.0.1",
            "http://127.0.0.1:43103/path",
            "http://127.0.0.1:43103?q=1",
            "http://127.0.0.1:43103#frag",
            "http://user@127.0.0.1:43103",
            "http://127.0.0.1:43103\\x",
            "http://[::1:43103",
            "http://[127.0.0.1]:43103",
            "http://::1:43103",
        ] {
            assert!(LoopbackEndpoint::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn chunked_decoder_handles_split_chunks_extensions_and_trailers() {
        let transport = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_chunks(
            vec![
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: ChUnKeD\r\n\r\n5;ext=1\r\nhe".to_vec(),
                b"llo\r\n6\r\n world\r\n0\r\nX-Trailer: ".to_vec(),
                b"ok\r\n\r\n".to_vec(),
            ],
        ))]);
        let ep = LoopbackEndpoint::localhost_v4(43103).unwrap();
        let reply = request(
            &transport,
            &ep,
            &HttpRequest {
                method: "POST",
                path: "/get_login_info",
                headers: &[("Authorization", "Bearer tok")],
                body: Some(b"{}"),
            },
            Deadlines::split(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            ResponseLimits::new(1024),
        )
        .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"hello world");
        let sent = &transport.written_requests()[0];
        assert!(sent.starts_with("POST /get_login_info HTTP/1.1\r\nHost: 127.0.0.1:43103\r\n"));
        assert!(sent.contains("Authorization: Bearer tok\r\n"));
        assert!(sent.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn chunked_errors_reject_truncated_malformed_or_conflicting_headers() {
        for bad in [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhi\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhix\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
                .to_vec(),
        ] {
            let transport =
                MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_bytes(bad))]);
            let ep = LoopbackEndpoint::localhost_v4(43103).unwrap();
            let err = request(
                &transport,
                &ep,
                &HttpRequest {
                    method: "GET",
                    path: "/x",
                    headers: &[],
                    body: None,
                },
                Deadlines::split(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                ),
                ResponseLimits::new(1024),
            )
            .unwrap_err();
            assert_eq!(err.status, CommitStatus::Unknown);
        }
    }

    #[test]
    fn wire_caps_enforce_head_limit_declared_content_length_and_decoded_chunked_limit() {
        let ep = LoopbackEndpoint::localhost_v4(43103).unwrap();
        let req = HttpRequest {
            method: "GET",
            path: "/x",
            headers: &[],
            body: None,
        };
        let deadlines = Deadlines::split(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );

        // Oversized head (> 128 bytes limit).
        let big_head = format!(
            "HTTP/1.1 200 OK\r\nX-Pad: {}\r\nContent-Length: 2\r\n\r\nok",
            "a".repeat(200)
        );
        let t1 = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_bytes(
            big_head.into_bytes(),
        ))]);
        let err = request(
            &t1,
            &ep,
            &req,
            deadlines,
            ResponseLimits {
                max_head_bytes: 128,
                max_body_bytes: 1024,
            },
        )
        .unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(err.detail.contains("headers exceed"), "{}", err.detail);

        // Declared Content-Length exceeding cap fails immediately without waiting for body bytes.
        let t2 = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_bytes(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\n\r\nhi".to_vec(),
        ))]);
        let err = request(&t2, &ep, &req, deadlines, ResponseLimits::new(1024)).unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(err.detail.contains("exceeds limit"), "{}", err.detail);

        // Decoded chunked size applies to decoded payload (10 bytes fits under 10, even with framing).
        let t3 = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_bytes(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n".to_vec(),
        ))]);
        let ok = request(&t3, &ep, &req, deadlines, ResponseLimits::new(10)).unwrap();
        assert_eq!(ok.body, b"helloworld");

        // 11 decoded bytes exceeds cap of 10.
        let t4 = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_bytes(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nhello!\r\n5\r\nworld\r\n0\r\n\r\n".to_vec(),
        ))]);
        let err = request(&t4, &ep, &req, deadlines, ResponseLimits::new(10)).unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(err.detail.contains("exceeds limit"), "{}", err.detail);

        // Many small valid chunks (>16 KiB wire framing) succeed when decoded payload fits max_body_bytes.
        let mut many_chunks = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..4000 {
            many_chunks.extend_from_slice(b"1\r\na\r\n");
        }
        many_chunks.extend_from_slice(b"0\r\n\r\n");
        let t5 = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_bytes(
            many_chunks,
        ))]);
        let ok = request(&t5, &ep, &req, deadlines, ResponseLimits::new(4096)).unwrap();
        assert_eq!(ok.body.len(), 4000);

        // Oversized chunk trailers (> 16 KiB) are rejected.
        let big_trailer = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\nX-Big: {}\r\n\r\n",
            "t".repeat(17 * 1024)
        );
        let t6 = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig::reply_bytes(
            big_trailer.into_bytes(),
        ))]);
        let err = request(&t6, &ep, &req, deadlines, ResponseLimits::new(4096)).unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(err.detail.contains("trailers exceed"), "{}", err.detail);
    }

    #[test]
    fn write_tracking_distinguishes_not_committed_from_unknown() {
        let ep = LoopbackEndpoint::localhost_v4(43103).unwrap();
        let req = HttpRequest {
            method: "POST",
            path: "/send_group_msg",
            headers: &[],
            body: Some(b"{\"group_id\":1}"),
        };
        let deadlines = Deadlines::split(
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_millis(200),
        );

        // Connect error -> NotCommitted.
        let t_conn = MemoryTransport::new(vec![Step::ConnectError(
            ErrorKind::ConnectionRefused,
            "refused",
        )]);
        let err = request(&t_conn, &ep, &req, deadlines, ResponseLimits::new(1024)).unwrap_err();
        assert_eq!(err.status, CommitStatus::NotCommitted);
        assert!(!err.timed_out);
        assert_eq!(t_conn.connect_count(), 1);

        // Connect timeout -> NotCommitted + timed_out.
        let t_conn_to =
            MemoryTransport::new(vec![Step::ConnectError(ErrorKind::TimedOut, "timed out")]);
        let err = request(&t_conn_to, &ep, &req, deadlines, ResponseLimits::new(1024)).unwrap_err();
        assert_eq!(err.status, CommitStatus::NotCommitted);
        assert!(err.timed_out);

        // Invalid request header -> NotCommitted and never calls connect.
        let t_bad_req = MemoryTransport::new(vec![]);
        let err = request(
            &t_bad_req,
            &ep,
            &HttpRequest {
                method: "POST",
                path: "/ok",
                headers: &[("X-Bad", "a\r\nb")],
                body: None,
            },
            deadlines,
            ResponseLimits::new(1024),
        )
        .unwrap_err();
        assert_eq!(err.status, CommitStatus::NotCommitted);
        assert_eq!(t_bad_req.connect_count(), 0);

        // Partial write followed by broken pipe -> Unknown.
        let t_partial = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig {
            write_plan: vec![Ok(8), Err(ErrorKind::BrokenPipe)],
            read_plan: VecDeque::new(),
            delay_per_read: Duration::ZERO,
            delay_per_write: Duration::ZERO,
        })]);
        let err = request(&t_partial, &ep, &req, deadlines, ResponseLimits::new(1024)).unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(!err.timed_out);

        // Read timeout after write -> Unknown + timed_out + "read response" detail.
        let t_read_to = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig {
            write_plan: Vec::new(),
            read_plan: VecDeque::from([Err(ErrorKind::TimedOut)]),
            delay_per_read: Duration::ZERO,
            delay_per_write: Duration::ZERO,
        })]);
        let err = request(&t_read_to, &ep, &req, deadlines, ResponseLimits::new(1024)).unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(err.timed_out);
        assert!(err.detail.contains("read response"), "{}", err.detail);
    }

    #[test]
    fn lone_surrogate_json_decoding_is_optional() {
        let reply = HttpResponse {
            status: 200,
            body: br#"{"text":"hi \ud83d there","pair":"\ud83d\ude00"}"#.to_vec(),
        };
        assert!(reply.parse_json::<serde_json::Value>(JsonSurrogates::Strict).is_err());
        let (val, repaired) = reply
            .parse_json::<serde_json::Value>(JsonSurrogates::ReplaceLoneWithFffd)
            .unwrap();
        assert!(repaired);
        assert_eq!(val["text"], "hi \u{FFFD} there");
        assert_eq!(val["pair"], "\u{1F600}");
    }

    #[test]
    fn split_deadlines_enforce_write_and_read_budgets_independently() {
        let ep = LoopbackEndpoint::localhost_v4(43103).unwrap();
        let req = HttpRequest {
            method: "POST",
            path: "/slow",
            headers: &[],
            body: Some(b"abcdef012345abcd"),
        };

        // Slow partial writes exhaust short write deadline -> Unknown + timed_out.
        let t_write_slow = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig {
            write_plan: vec![Ok(4), Ok(4), Ok(4), Ok(4)],
            read_plan: VecDeque::new(),
            delay_per_read: Duration::ZERO,
            delay_per_write: Duration::from_millis(40),
        })]);
        let err = request(
            &t_write_slow,
            &ep,
            &req,
            Deadlines::with_shared_transmit(Duration::from_millis(60), Duration::from_secs(2)),
            ResponseLimits::new(1024),
        )
        .unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(err.timed_out);
        assert!(err.detail.contains("transmission deadline"), "{}", err.detail);

        // Slow incremental reads exhaust read deadline -> Unknown + timed_out.
        let t_read_slow = MemoryTransport::new(vec![Step::Stream(ScriptedStreamConfig {
            write_plan: Vec::new(),
            read_plan: VecDeque::from([
                Ok(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n".to_vec()),
                Ok(b"12".to_vec()),
                Ok(b"34".to_vec()),
                Ok(b"56".to_vec()),
            ]),
            delay_per_read: Duration::from_millis(40),
            delay_per_write: Duration::ZERO,
        })]);
        let err = request(
            &t_read_slow,
            &ep,
            &req,
            Deadlines::split(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_millis(70),
            ),
            ResponseLimits::new(1024),
        )
        .unwrap_err();
        assert_eq!(err.status, CommitStatus::Unknown);
        assert!(err.timed_out);
        assert!(err.detail.contains("read response"), "{}", err.detail);
    }
}
