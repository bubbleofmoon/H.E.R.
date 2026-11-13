//! Loopback ports that stay bound until their owner takes them, and owned console startup.
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

/// A bound loopback listener. While it lives, no other socket can bind its port.
pub struct Reserved {
    listener: TcpListener,
    port: u16,
}

pub fn reserve() -> Reserved {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve loopback port");
    let port = listener
        .local_addr()
        .expect("reserved loopback addr")
        .port();
    Reserved { listener, port }
}

impl Reserved {
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn http_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
    /// Hands the still-bound socket to an in-process fake: the port is never free.
    pub fn into_listener(self) -> TcpListener {
        self.listener
    }
    /// Frees the port for a child process that binds it itself. Call it on the line before the
    /// spawn; prefer `start_console`, which also proves ownership and retries.
    pub fn release(self) -> u16 {
        self.port
    }
}

/// Starts an owned console child. It returns once `/api/health` names this child's PID and port.
/// Each attempt reserves a fresh port, releases it on the line before `spawn(port)`, and on an
/// early exit (the console cannot bind a port someone else took) or missing owned health it reaps
/// the child and retries on a port no earlier attempt used. 3 attempts; `timeout` bounds each one.
/// `spawn` must rewrite any fixture state that names the port before it starts the child.
pub fn start_console(timeout: Duration, mut spawn: impl FnMut(u16) -> Child) -> (Child, u16) {
    const ATTEMPTS: usize = 3;
    let mut tried: Vec<u16> = Vec::new();
    let mut failures = Vec::new();
    for attempt in 1..=ATTEMPTS {
        // Keep rejected listeners bound so the allocator cannot hand back a tried port.
        let mut rejected = Vec::new();
        let reservation = loop {
            let candidate = reserve();
            if !tried.contains(&candidate.port()) {
                break candidate;
            }
            rejected.push(candidate);
        };
        let port = reservation.release();
        tried.push(port);
        let mut child = spawn(port);
        match wait_owned_health(&mut child, port, timeout) {
            Ok(()) => return (child, port),
            Err(reason) => {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
                eprintln!("console startup attempt {attempt}/{ATTEMPTS}: {reason}");
                failures.push(format!("attempt {attempt}/{ATTEMPTS}: {reason}"));
            }
        }
    }
    panic!(
        "console startup exhausted {ATTEMPTS} attempts (port taken or no owned health):\n{}",
        failures.join("\n")
    );
}

/// Waits until `/api/health` on `port` answers 200 with `ok`, this child's PID and this port.
/// Fails as soon as the child exits.
pub fn wait_owned_health(child: &mut Child, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut last: Option<(u16, Value)> = None;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect console child: {error}"))?
        {
            return Err(format!(
                "child {} on port {port} exited with {status}; last health {last:?}",
                child.id()
            ));
        }
        if let Some((status, body)) = probe_health(port) {
            if status == 200
                && body["ok"] == true
                && body["pid"] == child.id()
                && body["port"] == port
            {
                return Ok(());
            }
            last = Some((status, body));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "child {} on port {port} had no owned health within {timeout:?}; last health {last:?}",
                child.id()
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn probe_health(port: u16) -> Option<(u16, Value)> {
    // A foreign listener can accept and never answer. Bound each probe.
    let timeout = Duration::from_millis(200);
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(
        stream,
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((status, serde_json::from_str(body).ok()?))
}
