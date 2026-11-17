//! Her own QQ avatar for `qq_get_self_image` (#206). Selected runtimes fetch it from the one qlogo
//! address for her own uin (`<data>/identity.json`), keep it in `<data>/self-avatar.cache` for 30 days
//! and fetch at most once per 15 minutes per runtime: every fetch first writes and reads back an attempt
//! reservation (`<data>/self-avatar.attempt`). `None` from `current` means: serve the static portrait.
//!
//! The fetch goes through `safe_fetch::fetch_exact` (public pinned address, no redirect followed, size
//! cap, image magic, time budget). Unit tests replace it with `FETCH_TEST_HOOK`; with no hook set the
//! fetch fails, so no test contacts qlogo.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use crate::media_normalize::Normalized;
use crate::runtime::{RuntimeArea, RuntimeContext};

/// The only host the avatar fetch contacts.
pub const HOST: &str = "q1.qlogo.cn";
/// The qlogo size parameter (its largest size).
pub const SIZE: &str = "640";
/// Largest avatar accepted, in bytes.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// A stored avatar is fresh for 30 days (the gateway's own web console caches qlogo for as long).
pub const TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// After a fetch attempt for the same uin, no new attempt for 15 minutes.
pub const FAILURE_BACKOFF_MS: i64 = 15 * 60 * 1000;
/// Transfer budget for one fetch, all address attempts together.
#[cfg_attr(test, allow(dead_code))]
pub const FETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);
/// The cache record (JSON) in the runtime data folder. Not `.json`: state backups take only the data
/// folder's `.json` files, and a restored old record must not come back.
pub const CACHE_FILE: &str = "self-avatar.cache";
/// A cache record larger than this is ignored (base64 of `MAX_BYTES` plus the other fields).
const CACHE_FILE_MAX: u64 = (MAX_BYTES as u64) * 4 / 3 + 4096;
/// The attempt reservation (JSON) in the runtime data folder: her uin and the time of the last fetch
/// attempt. It is written and read back before every fetch, so the backoff holds across restarts and
/// processes even when the cache cannot be stored or read. Not `.json`, like `CACHE_FILE`.
pub const ATTEMPT_FILE: &str = "self-avatar.attempt";
/// A reservation larger than this is ignored (the next reservation replaces it).
const ATTEMPT_FILE_MAX: u64 = 1024;
/// One check-fetch-store at a time per runtime, across processes: a second caller waits here and then
/// finds the stored avatar or failure. The OS releases the lock when its holder exits.
pub const GATE_FILE: &str = "locks/self-avatar.lock";
/// How long a caller waits for the gate before serving the static portrait (longer than one fetch:
/// 5 s lookup plus `FETCH_BUDGET`).
const GATE_WAIT: std::time::Duration = std::time::Duration::from_millis(if cfg!(test) { 2_000 } else { 15_000 });

#[cfg(test)]
thread_local! {
    /// Unit tests' replacement for the network fetch. With none set the fetch fails.
    pub(crate) static FETCH_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str) -> Result<Vec<u8>, String>>>> =
        std::cell::RefCell::new(None);
    /// Unit tests' signal, on this thread, that a caller found the gate taken and is about to wait.
    pub(crate) static GATE_WAIT_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = std::cell::RefCell::new(None);
    /// Unit tests' signal, on this thread, that a picture is about to be normalized: the account may change there.
    pub(crate) static NORMALIZE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = std::cell::RefCell::new(None);
}

/// A QQ uin as qlogo takes it: 5 to 11 ASCII digits without a leading zero.
pub fn valid_uin(uin: &str) -> bool {
    (5..=11).contains(&uin.len()) && uin.bytes().all(|b| b.is_ascii_digit()) && !uin.starts_with('0')
}

/// Her own uin from `<data>/identity.json` (`userId`, a string), when it is a valid uin.
pub fn own_uin(context: &RuntimeContext) -> Option<String> {
    let text = context.read_optional_text(RuntimeArea::Data, "identity.json").ok().flatten()?;
    let identity: Value = serde_json::from_str(&text).ok()?;
    let uin = identity.get("userId").and_then(Value::as_str)?;
    valid_uin(uin).then(|| uin.to_string())
}

/// The avatar address for `uin`.
pub fn avatar_url(uin: &str) -> String {
    format!("https://{HOST}/g?b=qq&nk={uin}&s={SIZE}")
}

/// Whether `url` is exactly an avatar address: https, `HOST`, port 443, path `/g` and the query
/// `b=qq&nk=<valid uin>&s=<SIZE>` in that order, nothing else.
pub fn url_allowed(url: &str) -> bool {
    let Some(parsed) = crate::safe_fetch::parse_url(url) else {
        return false;
    };
    let uin = parsed
        .rest
        .strip_prefix("/g?b=qq&nk=")
        .and_then(|rest| rest.strip_suffix(&format!("&s={SIZE}")));
    parsed.scheme == "https" && parsed.host == HOST && parsed.port == 443 && uin.is_some_and(valid_uin)
}

/// Her current avatar, normalized for the model, or `None` for the static portrait: a fresh stored
/// avatar for her uin, else one fetch when no attempt for that uin is younger than `FAILURE_BACKOFF_MS`
/// and a new attempt reservation is written and reads back. The fetch's outcome is stored (a failed
/// store is ignored: the reservation keeps the backoff); a fetch during which her uin changed is dropped.
pub fn current(context: &RuntimeContext, now_ms: &dyn Fn() -> i64, max_pixels: u64) -> Option<Normalized> {
    current_with_clock(context, now_ms, max_pixels, &mut fetch_live)
}

/// `current_with_clock` with a fixed time, for the unit tests.
#[cfg(test)]
pub(crate) fn current_with(
    context: &RuntimeContext,
    now_ms: i64,
    max_pixels: u64,
    fetch: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>,
) -> Option<Normalized> {
    current_with_clock(context, &|| now_ms, max_pixels, fetch)
}

/// The clock is read once the gate is held, so a caller that waited for another caller's fetch judges and
/// stores by its own time, not by an earlier one.
pub(crate) fn current_with_clock(
    context: &RuntimeContext,
    now_ms: &dyn Fn() -> i64,
    max_pixels: u64,
    fetch: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>,
) -> Option<Normalized> {
    if !context.is_isolated() {
        return None;
    }
    let _gate = gate(context)?;
    let uin = own_uin(context)?;
    let now_ms = now_ms();
    if let Some(record) = read_cache(context).filter(|record| record.uin == uin) {
        if let Some(normalized) = record.fresh(now_ms).and_then(|bytes| decode(bytes, max_pixels)) {
            return Some(normalized);
        }
        if record.failure_recent(now_ms) {
            return None;
        }
    }
    // Every fetch needs a new reservation on disk: a cache that is missing, damaged, unreadable or cannot
    // be stored still allows at most one fetch per `FAILURE_BACKOFF_MS`, across restarts and processes.
    if attempt_recent(context, &uin, now_ms) || !reserve(context, &uin, now_ms) {
        return None;
    }
    let fetched = fetch(&avatar_url(&uin)).ok();
    // Her account changed while the fetch ran: the bytes are the old account's picture.
    if own_uin(context).as_deref() != Some(uin.as_str()) {
        return None;
    }
    let normalized = fetched.as_deref().and_then(|bytes| decode(bytes, max_pixels));
    let record = match (&fetched, &normalized) {
        (Some(bytes), Some(_)) => json!({"version": 1, "uin": uin, "fetchedAt": now_ms, "data": STANDARD.encode(bytes)}),
        _ => json!({"version": 1, "uin": uin, "failedAt": now_ms}),
    };
    let _ = context.write_atomic(RuntimeArea::Data, CACHE_FILE, record.to_string().as_bytes());
    normalized
}

/// Whether the stored reservation is for `uin` and at most `now_ms` and younger than `FAILURE_BACKOFF_MS`.
/// A missing, oversized, unreadable or damaged reservation is not recent (`reserve` then replaces it).
fn attempt_recent(context: &RuntimeContext, uin: &str, now_ms: i64) -> bool {
    if !context.optional_file_len(RuntimeArea::Data, ATTEMPT_FILE).is_ok_and(|len| len <= ATTEMPT_FILE_MAX) {
        return false;
    }
    let Some(value) = context
        .read_optional_text(RuntimeArea::Data, ATTEMPT_FILE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    else {
        return false;
    };
    value.get("version").and_then(Value::as_i64) == Some(1)
        && value.get("uin").and_then(Value::as_str) == Some(uin)
        && value
            .get("attemptAt")
            .and_then(Value::as_i64)
            .and_then(|at| now_ms.checked_sub(at))
            .is_some_and(|age| (0..FAILURE_BACKOFF_MS).contains(&age))
}

/// Writes the reservation for `uin` at `now_ms` and reads it back; `false` (no fetch) unless both work.
fn reserve(context: &RuntimeContext, uin: &str, now_ms: i64) -> bool {
    let reservation = json!({"version": 1, "uin": uin, "attemptAt": now_ms}).to_string();
    context.write_atomic(RuntimeArea::Data, ATTEMPT_FILE, reservation.as_bytes()).is_ok()
        && context.read_optional_text(RuntimeArea::Data, ATTEMPT_FILE).ok().flatten().as_deref() == Some(reservation.as_str())
}

/// The exclusive gate file, held until the returned handle drops; `None` when it stays taken for
/// `GATE_WAIT` or cannot be opened.
fn gate(context: &RuntimeContext) -> Option<std::fs::File> {
    let deadline = std::time::Instant::now() + GATE_WAIT;
    loop {
        match context.open_locked_file(RuntimeArea::Data, GATE_FILE) {
            Ok(file) => return Some(file),
            Err(error) if error == "runtime file is already locked" && std::time::Instant::now() < deadline => {
                #[cfg(test)]
                GATE_WAIT_TEST_HOOK.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().as_mut() {
                        hook();
                    }
                });
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// The production fetch: an allowed address only, through `safe_fetch::fetch_exact`.
fn fetch_live(url: &str) -> Result<Vec<u8>, String> {
    if !url_allowed(url) {
        return Err("avatar address refused".to_string());
    }
    transfer(url)
}

#[cfg(not(test))]
fn transfer(url: &str) -> Result<Vec<u8>, String> {
    crate::safe_fetch::fetch_exact(url, MAX_BYTES, FETCH_BUDGET).map(|fetched| fetched.bytes)
}

#[cfg(test)]
fn transfer(url: &str) -> Result<Vec<u8>, String> {
    FETCH_TEST_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(url),
        None => Err("no test fetch".to_string()),
    })
}

/// Avatar bytes the model may see: within `MAX_BYTES`, image magic, and they normalize.
fn decode(bytes: &[u8], max_pixels: u64) -> Option<Normalized> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES || !crate::safe_fetch::looks_like_image(bytes) {
        return None;
    }
    crate::media_normalize::normalize(bytes, max_pixels).ok()
}

struct CacheRecord {
    uin: String,
    fetched_at: Option<i64>,
    failed_at: Option<i64>,
    data: Option<Vec<u8>>,
}

impl CacheRecord {
    /// The stored bytes while `fetchedAt` is at most `now_ms` and younger than `TTL_MS`.
    fn fresh(&self, now_ms: i64) -> Option<&[u8]> {
        let age = now_ms.checked_sub(self.fetched_at?)?;
        if (0..TTL_MS).contains(&age) { self.data.as_deref() } else { None }
    }

    /// A failure recorded at most `now_ms` and younger than `FAILURE_BACKOFF_MS`.
    fn failure_recent(&self, now_ms: i64) -> bool {
        self.failed_at
            .and_then(|at| now_ms.checked_sub(at))
            .is_some_and(|age| (0..FAILURE_BACKOFF_MS).contains(&age))
    }
}

/// The stored record; missing, oversized, unreadable or not version 1 is `None`. Such a cache never
/// fetches on its own: the attempt reservation still limits the fetches.
fn read_cache(context: &RuntimeContext) -> Option<CacheRecord> {
    let len = context.optional_file_len(RuntimeArea::Data, CACHE_FILE).ok()?;
    if len == 0 || len > CACHE_FILE_MAX {
        return None;
    }
    let text = context.read_optional_text(RuntimeArea::Data, CACHE_FILE).ok().flatten()?;
    if text.len() as u64 > CACHE_FILE_MAX {
        return None;
    }
    let value: Value = serde_json::from_str(&text).ok()?;
    if value.get("version").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    Some(CacheRecord {
        uin: value.get("uin").and_then(Value::as_str)?.to_string(),
        fetched_at: value.get("fetchedAt").and_then(Value::as_i64),
        failed_at: value.get("failedAt").and_then(Value::as_i64),
        data: value.get("data").and_then(Value::as_str).and_then(|data| STANDARD.decode(data).ok()),
    })
}
