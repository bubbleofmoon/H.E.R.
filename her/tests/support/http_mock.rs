//! Concurrent loopback HTTP mock server and shared bounded HTTP helpers.
use serde_json::Value;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct MockRequest {
    pub request_line: String,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl MockRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct MockResponse {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl MockResponse {
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            reason: "OK",
            content_type: "application/json",
            body: body.to_string().into_bytes(),
        }
    }
}

pub enum MockAction {
    Reply(MockResponse),
    Close,
}

/// Reads one HTTP/1.x request up to `Content-Length` without blocking past `read_limit` or `stop`.
/// Returns `None` on EOF (including 0-byte connect probes), malformed framing, timeout, or stop.
pub fn read_http_request(
    stream: &mut TcpStream,
    stop: &AtomicBool,
    read_limit: Duration,
) -> Option<MockRequest> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + read_limit;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        let n = match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return None,
        };
        request.extend_from_slice(&buf[..n]);
        if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]);
            let mut lines = head.lines();
            let request_line = lines.next()?.trim_end_matches('\r').to_string();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut headers = Vec::new();
            let mut content_length = 0usize;
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    let name = name.trim().to_string();
                    let value = value.trim().to_string();
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.parse::<usize>().unwrap_or(0);
                    }
                    headers.push((name, value));
                }
            }
            let total = end.checked_add(4)?.checked_add(content_length)?;
            if request.len() >= total {
                return Some(MockRequest {
                    request_line,
                    method,
                    path,
                    headers,
                    body: request[end + 4..total].to_vec(),
                });
            }
        }
    }
    None
}

pub fn write_http_response(stream: &mut TcpStream, response: &MockResponse) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)
}

/// Sends one HTTP/1.1 JSON request to `127.0.0.1:port` and returns `(status, parsed_json)`.
pub fn http_json_call(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Option<(u16, Value)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .ok()?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let has_host = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"));
    let mut head = if has_host {
        format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n")
    } else {
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n")
    };
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).ok()?;
    stream.write_all(payload.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((status, serde_json::from_str(body).unwrap_or(Value::Null)))
}

/// Multi-threaded loopback HTTP server where each accepted TCP stream runs on its own worker
/// thread, so a slow or 0-byte connection never stalls concurrent requests.
pub struct ConcurrentMockServer {
    port: u16,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ConcurrentMockServer {
    pub fn start<F>(handler: F) -> Self
    where
        F: Fn(MockRequest, &AtomicBool) -> MockAction + Send + Sync + 'static,
    {
        Self::serve(super::ports::reserve().into_listener(), handler)
    }

    pub fn serve<F>(listener: TcpListener, handler: F) -> Self
    where
        F: Fn(MockRequest, &AtomicBool) -> MockAction + Send + Sync + 'static,
    {
        listener
            .set_nonblocking(true)
            .expect("nonblocking concurrent mock listener");
        let port = listener.local_addr().expect("mock server addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let workers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let stop_accept = stop.clone();
        let workers_accept = workers.clone();
        let accept_thread = thread::spawn(move || {
            while !stop_accept.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let stop_worker = stop_accept.clone();
                        let handler = handler.clone();
                        let handle = thread::spawn(move || {
                            if let Some(req) =
                                read_http_request(&mut stream, &stop_worker, Duration::from_secs(5))
                            {
                                if let MockAction::Reply(resp) = handler(req, &stop_worker) {
                                    let _ = write_http_response(&mut stream, &resp);
                                }
                            }
                        });
                        if let Ok(mut guard) = workers_accept.lock() {
                            guard.retain(|h| !h.is_finished());
                            guard.push(handle);
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            stop,
            accept_thread: Some(accept_thread),
            workers,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for ConcurrentMockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        let drained = match self.workers.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for worker in drained {
            let _ = worker.join();
        }
    }
}
