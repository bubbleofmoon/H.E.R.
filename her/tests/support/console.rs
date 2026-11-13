//! Owned console readiness shared by the process fixtures.

use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

pub fn reserve_port() -> TcpListener {
    TcpListener::bind(("127.0.0.1", 0)).expect("reserve console port")
}

/// Call with an owned child after releasing the reservation immediately before spawn.
/// `respawn` must update any fake relay's port before starting the replacement child.
pub fn wait_for_console(
    child: &mut Child,
    port: &mut u16,
    root: &Path,
    token: &str,
    timeout: Duration,
    mut respawn: impl FnMut(u16) -> Child,
) {
    const ATTEMPTS: usize = 3;
    let mut failures = Vec::new();
    let mut ports = vec![*port];
    for attempt in 1..=ATTEMPTS {
        let deadline = Instant::now() + timeout;
        let mut health = (0, Value::Null);
        let exit = loop {
            let exit = child.try_wait().expect("inspect owned console child");
            if exit.is_some() {
                break exit;
            }
            health = probe_health(*port, token).unwrap_or((0, Value::Null));
            if health.0 == 200
                && health.1["ok"] == true
                && health.1["pid"] == child.id()
                && health.1["port"] == *port
            {
                return;
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(100));
        };
        let failure = format!(
            "attempt {attempt}/{ATTEMPTS}: child {} port {}, exit {exit:?}, health {health:?}",
            child.id(),
            port
        );
        eprintln!("console startup {failure}");
        failures.push(failure);
        // Reap only our failed/unready child, including the last failed attempt.
        if exit.is_none() {
            child.kill().expect("stop unready owned console child");
        }
        child.wait().expect("reap owned console child");
        if attempt == ATTEMPTS {
            break;
        }
        // Keep rejected allocations bound so the next attempt always gets a fresh port.
        let mut rejected = Vec::new();
        let reservation = loop {
            let listener = reserve_port();
            let next = listener.local_addr().expect("retry console addr").port();
            if !ports.contains(&next) {
                *port = next;
                break listener;
            }
            rejected.push(listener);
        };
        ports.push(*port);
        drop(reservation);
        *child = respawn(*port);
    }
    panic!("console startup exhausted {ATTEMPTS} attempts (port collision or missing owned PID/port health):\n{}\nconsole stderr:\n{}",
        failures.join("\n"), fs::read_to_string(root.join("console-err.log")).unwrap_or_default());
}

fn probe_health(port: u16, token: &str) -> Option<(u16, Value)> {
    // A foreign listener can accept and never answer. Bound each readiness probe.
    let timeout = Duration::from_millis(200);
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(stream, "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-console-token: {token}\r\nConnection: close\r\n\r\n").ok()?;
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
