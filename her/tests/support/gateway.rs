//! One threaded loopback OneBot HTTP fake. It records every `(action, body)`, answers a configured
//! envelope per action (default: ok with a fresh `message_id` counting from 91000), can delay an
//! action, and serves a listener that is handed over still bound.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    seen: Mutex<Vec<(String, Value)>>,
    replies: Mutex<HashMap<String, Value>>,
    delays: Mutex<HashMap<String, u64>>,
    next_id: AtomicU64,
}

pub struct FakeGateway {
    pub url: String,
    pub port: u16,
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FakeGateway {
    pub fn start() -> Self {
        Self::serve(super::ports::reserve().into_listener())
    }

    pub fn serve(listener: TcpListener) -> Self {
        listener
            .set_nonblocking(true)
            .expect("nonblocking fake gateway listener");
        let port = listener.local_addr().expect("fake gateway addr").port();
        let state = Arc::new(State {
            next_id: AtomicU64::new(91000),
            ..State::default()
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_state, worker_stop) = (state.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let state = worker_state.clone();
                        thread::spawn(move || answer(stream, &state));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            port,
            state,
            stop,
            worker: Some(worker),
        }
    }

    pub fn seen(&self, action: &str) -> Vec<Value> {
        self.state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(a, _)| a == action)
            .map(|(_, b)| b.clone())
            .collect()
    }
    pub fn count(&self, action: &str) -> usize {
        self.seen(action).len()
    }
    /// Waits up to 5 s for `count` requests of `action` and returns what arrived.
    pub fn wait_for(&self, action: &str, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = self.seen(action);
            if got.len() >= count || Instant::now() > deadline {
                return got;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    /// Answers `action` with this whole response envelope from now on.
    pub fn reply(&self, action: &str, envelope: Value) {
        self.state
            .replies
            .lock()
            .unwrap()
            .insert(action.to_string(), envelope);
    }
    pub fn delay(&self, action: &str, ms: u64) {
        self.state
            .delays
            .lock()
            .unwrap()
            .insert(action.to_string(), ms);
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn answer(stream: TcpStream, state: &State) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let Some(path) = line.split_whitespace().nth(1).map(str::to_string) else {
        return;
    };
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let action = path.trim_start_matches('/').to_string();
    state.seen.lock().unwrap().push((action.clone(), body));
    let delay = state
        .delays
        .lock()
        .unwrap()
        .get(&action)
        .copied()
        .unwrap_or(0);
    if delay > 0 {
        thread::sleep(Duration::from_millis(delay));
    }
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let response = state
        .replies
        .lock()
        .unwrap()
        .get(&action)
        .cloned()
        .unwrap_or_else(|| json!({"status":"ok","retcode":0,"data":{"message_id":id}}))
        .to_string();
    let _ = write!(
        reader.get_mut(),
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
}
