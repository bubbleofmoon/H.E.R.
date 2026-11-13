//! AI feedback store (`feedback.json`): a JSON array of `{id, key, level, message, time}`, newest
//! last, at most 500 entries.
//!
//! Port of the JS `readFeedbackEntries` / `appendFeedbackEntry` and the `/api/socialV2/feedback*`
//! state handling. A file that is not a JSON array behaves like JS (`readJsonSafe(..., [])`): the
//! list reads as empty and the next append starts a new one. The one addition (user decision
//! 2026-10-30) is that the damaged file is first renamed to `feedback.json.corrupt-<timestamp>`
//! in the same directory, so the old bytes are kept instead of being overwritten.

use crate::runtime::{RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

pub const FILE: &str = "feedback.json";
const GATE_FILE: &str = "locks/feedback.lock";
const MAX_ENTRIES: usize = 500;
const GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Serializes read-modify-write cycles inside one process.
static GATE: Mutex<()> = Mutex::new(());

type Failure = (u16, String);

fn unavailable() -> Failure {
    (503, "feedback store is unavailable".to_string())
}

/// Held for the duration of one read-modify-write: the in-process mutex plus, in a selected root,
/// the OS lock `locks/feedback.lock` that serializes the console and the live bridge.
struct Gate {
    _process: std::sync::MutexGuard<'static, ()>,
    _file: Option<std::fs::File>,
}

fn acquire(context: &RuntimeContext) -> Result<Gate, Failure> {
    let process = GATE.lock().unwrap_or_else(|p| p.into_inner());
    if !context.is_isolated() {
        return Ok(Gate { _process: process, _file: None });
    }
    let deadline = std::time::Instant::now() + GATE_WAIT;
    loop {
        match context.open_locked_file(RuntimeArea::Data, GATE_FILE) {
            Ok(file) => return Ok(Gate { _process: process, _file: Some(file) }),
            Err(error) if error == "runtime file is already locked" => {
                if std::time::Instant::now() >= deadline {
                    return Err((503, "selected runtime feedback store is busy".to_string()));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return Err(unavailable()),
        }
    }
}

/// Moves a damaged store aside (same directory, atomic rename) and returns the backup name. A
/// backup that cannot be made is an error: the damaged bytes are never overwritten unsaved.
fn quarantine(context: &RuntimeContext) -> Result<(), Failure> {
    let stamp = crate::dsh_mcp_config::utc_stamp();
    for attempt in 0..100 {
        let name = if attempt == 0 { format!("{FILE}.corrupt-{stamp}") } else { format!("{FILE}.corrupt-{stamp}-{attempt}") };
        let renamed = if context.is_isolated() {
            context.rename_file(RuntimeArea::Data, FILE, &name)
        } else {
            let dir = context.data_dir();
            if dir.join(&name).exists() {
                Err("runtime destination already exists".to_string())
            } else {
                std::fs::rename(dir.join(FILE), dir.join(&name)).map_err(|e| e.to_string())
            }
        };
        match renamed {
            Ok(()) => return Ok(()),
            Err(error) if error == "runtime destination already exists" => continue,
            Err(_) => break,
        }
    }
    Err((503, "feedback store is damaged and could not be backed up; it was left unchanged".to_string()))
}

/// Entries of the store, JS `readFeedbackEntries`: a missing file is an empty list; a file that is
/// not a JSON array is moved aside as a backup and also reads as empty. An I/O error other than
/// "not found" is refused rather than guessed at.
fn read_locked(context: &RuntimeContext) -> Result<Vec<Value>, Failure> {
    let text = if context.is_isolated() {
        match context.read_optional_text(RuntimeArea::Data, FILE) {
            Ok(Some(text)) => Some(text),
            Ok(None) => return Ok(Vec::new()),
            // Unreadable as text: damaged (not UTF-8) only if the bytes themselves can be read.
            Err(_) => match context.read_file(RuntimeArea::Data, FILE) {
                Ok(_) => None,
                Err(_) => return Err(unavailable()),
            },
        }
    } else {
        match std::fs::read(context.data_dir().join(FILE)) {
            Ok(bytes) => String::from_utf8(bytes).ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(unavailable()),
        }
    };
    if let Some(text) = &text {
        let body = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        if let Ok(Value::Array(list)) = serde_json::from_str::<Value>(body) {
            return Ok(list);
        }
    }
    quarantine(context)?;
    Ok(Vec::new())
}

fn write_locked(context: &RuntimeContext, list: &[Value]) -> Result<(), Failure> {
    let mut text = serde_json::to_string_pretty(&Value::Array(list.to_vec()))
        .map_err(|e| (500, format!("serialize feedback: {e}")))?;
    text.push('\n');
    context
        .write_atomic(RuntimeArea::Data, FILE, text.as_bytes())
        .map_err(|_| (500, "feedback store write failed; the entry was not saved".to_string()))
}

/// `GET /api/socialV2/feedback`.
pub fn read(context: &RuntimeContext) -> Result<Vec<Value>, Failure> {
    let _gate = acquire(context)?;
    read_locked(context)
}

/// JS `appendFeedbackEntry`: the message is redacted before it is stored, the list is capped at the
/// newest 500.
pub fn append(context: &RuntimeContext, mut entry: Value) -> Result<(), Failure> {
    if let Some(message) = entry.get("message").and_then(Value::as_str).map(str::to_string) {
        entry["message"] = json!(crate::activity::redact(context, &message));
    }
    let _gate = acquire(context)?;
    let mut list = read_locked(context)?;
    list.push(entry);
    if list.len() > MAX_ENTRIES {
        let excess = list.len() - MAX_ENTRIES;
        list.drain(0..excess);
    }
    write_locked(context, &list)
}

/// `POST /api/socialV2/feedback-clear`: the operator's reset.
pub fn clear(context: &RuntimeContext) -> Result<(), Failure> {
    let _gate = acquire(context)?;
    write_locked(context, &[])
}

/// JS `Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 8)`.
pub fn new_id(now_ms: i64) -> String {
    fn base36(mut n: u64) -> String {
        if n == 0 {
            return "0".to_string();
        }
        let mut digits = Vec::new();
        while n > 0 {
            digits.push(std::char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
            n /= 36;
        }
        digits.iter().rev().collect()
    }
    // The random tail only has to differ between entries of the same millisecond.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x9e37_79b9);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut r = nanos ^ (now_ms as u64).rotate_left(17) ^ COUNTER.fetch_add(0x1_0000_0001, std::sync::atomic::Ordering::Relaxed);
    let mut tail = String::new();
    for _ in 0..6 {
        r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        tail.push(std::char::from_digit(((r >> 33) % 36) as u32, 36).unwrap_or('0'));
    }
    format!("{}-{tail}", base36(now_ms.max(0) as u64))
}

/// Per-session limit of 5 per minute and 20 per hour (JS `feedbackTimes`). The table is process
/// memory, as in JS: a restart starts the windows again. `scope` keeps two selected roots apart.
pub fn allow(scope: &str, key: &str, now_ms: i64) -> bool {
    static TIMES: Mutex<Option<HashMap<(String, String), Vec<i64>>>> = Mutex::new(None);
    let mut guard = TIMES.lock().unwrap_or_else(|p| p.into_inner());
    let list = guard.get_or_insert_with(HashMap::new).entry((scope.to_string(), key.to_string())).or_default();
    let minute = list.iter().filter(|t| now_ms - **t < 60_000).count();
    let hour = list.iter().filter(|t| now_ms - **t < 3_600_000).count();
    if minute >= 5 || hour >= 20 {
        return false;
    }
    list.push(now_ms);
    if list.len() > 100 {
        let excess = list.len() - 100;
        list.drain(0..excess);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_window_allows_five_per_minute_and_twenty_per_hour() {
        let scope = "rate-unit";
        for i in 0..5 {
            assert!(allow(scope, "group:1", 1_000 + i));
        }
        assert!(!allow(scope, "group:1", 2_000), "sixth within a minute");
        assert!(allow(scope, "group:2", 2_000), "other session");
        // After a minute the minute window is clear again; the hour window still counts.
        let mut t = 70_000;
        let mut granted = 5;
        while allow(scope, "group:1", t) {
            granted += 1;
            t += 61_000;
            if granted > 40 {
                break;
            }
        }
        assert_eq!(granted, 20, "twenty in the hour, then refused");
    }

    #[test]
    fn ids_have_the_js_shape() {
        let id = new_id(1_760_000_000_000);
        let (head, tail) = id.split_once('-').unwrap();
        assert_eq!(head, "mgj6k3cw");
        assert_eq!(tail.len(), 6);
        assert_ne!(new_id(1), new_id(1));
    }
}
