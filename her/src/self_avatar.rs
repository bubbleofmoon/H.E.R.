//! Her own QQ avatar for `qq_get_self_image` (#206). Selected runtimes fetch it from the one qlogo
//! address for her own uin (`<data>/identity.json`), keep it in `<data>/self-avatar.cache` for 30 days
//! and fetch at most once per 15 minutes per runtime and uin: every fetch first writes and reads back an attempt
//! reservation (`<data>/self-avatar.attempt`). `None` from `current` means: serve the static portrait.
//!
//! The fetch goes through `safe_fetch::fetch_exact` (public pinned address, no redirect followed, size
//! cap, image magic, time budget). Unit tests replace it with `FETCH_TEST_HOOK`; with no hook set the
//! fetch fails, so no test contacts qlogo.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use std::path::{Path, PathBuf};

use crate::media_normalize::Normalized;
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

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
/// The attempt reservations (JSON) in the runtime data folder: one `{uin, attemptAt}` entry per account that
/// fetched within `FAILURE_BACKOFF_MS`. Each fetch writes and reads back its reservation first, so the backoff
/// holds per account across restarts and processes, even when the cache cannot be stored or read. Not
/// `.json`, like `CACHE_FILE`.
pub const ATTEMPT_FILE: &str = "self-avatar.attempt";
/// A reservation file larger than this is unusable (no fetch). Only the accounts that fetched in the last
/// 15 minutes are kept, so the file stays far below this.
const ATTEMPT_FILE_MAX: u64 = 16 * 1024;
/// The folder under the runtime data folder where the avatar fetch's curl writes its body and header files.
const TRANSFER_FOLDER: &str = "self-avatar-transfer";
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
    /// Unit tests' signal, on this thread, that a picture's record is about to be stored: the account may change there.
    pub(crate) static STORE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = std::cell::RefCell::new(None);
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
    current_with_clock(context, now_ms, max_pixels, &mut |url: &str| fetch_live(context, url))
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
            // Her account may have changed while the picture was normalized: then the portrait is served.
            return identity_still(context, &uin).then_some(normalized);
        }
        if record.failure_recent(now_ms) {
            // A failure dated ahead of the clock (the clock moved back) is restamped now and stays in force.
            if record.failed_at.is_some_and(|at| at > now_ms) {
                let restamped = json!({"version": 1, "uin": uin, "failedAt": now_ms});
                let _ = context.write_atomic(RuntimeArea::Data, CACHE_FILE, restamped.to_string().as_bytes());
            }
            return None;
        }
    }
    // Every fetch needs a new reservation on disk: a cache that is missing, damaged, unreadable or cannot
    // be stored still allows at most one fetch per `FAILURE_BACKOFF_MS`, across restarts and processes.
    if !reserve(context, &uin, now_ms) {
        return None;
    }
    let fetched = fetch(&avatar_url(&uin)).ok();
    // Her account changed while the fetch ran: the bytes are the old account's picture.
    if own_uin(context).as_deref() != Some(uin.as_str()) {
        return None;
    }
    let normalized = fetched.as_deref().and_then(|bytes| decode(bytes, max_pixels));
    // Her account may have changed while the picture was normalized: nothing is stored or served then.
    if !identity_still(context, &uin) {
        return None;
    }
    let record = match (&fetched, &normalized) {
        (Some(bytes), Some(_)) => json!({"version": 1, "uin": uin, "fetchedAt": now_ms, "data": STANDARD.encode(bytes)}),
        _ => json!({"version": 1, "uin": uin, "failedAt": now_ms}),
    };
    #[cfg(test)]
    STORE_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
    let _ = context.write_atomic(RuntimeArea::Data, CACHE_FILE, record.to_string().as_bytes());
    // The cache replacement may have retried while her account changed: the picture is not served then.
    if !identity_still(context, &uin) {
        return None;
    }
    normalized
}

/// Her uin as the identity file says it is now: a changed account must not be served or stored under the old uin.
fn identity_still(context: &RuntimeContext, uin: &str) -> bool {
    own_uin(context).as_deref() == Some(uin)
}

/// The reservations in the attempt file, `(uin, attemptAt)` per account. A missing file holds none; an
/// oversized, unreadable, unparsable or not version-1 file is unusable (`None`: no fetch runs).
fn read_attempts(context: &RuntimeContext) -> Option<Vec<(String, i64)>> {
    if context.optional_file_len(RuntimeArea::Data, ATTEMPT_FILE).ok()? > ATTEMPT_FILE_MAX {
        return None;
    }
    let Some(text) = context.read_optional_text(RuntimeArea::Data, ATTEMPT_FILE).ok()? else {
        return Some(Vec::new());
    };
    if text.len() as u64 > ATTEMPT_FILE_MAX {
        return None;
    }
    let value: Value = serde_json::from_str(&text).ok()?;
    if value.get("version").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    value
        .get("attempts")?
        .as_array()?
        .iter()
        .map(|entry| Some((entry.get("uin")?.as_str()?.to_string(), entry.get("attemptAt")?.as_i64()?)))
        .collect()
}

/// Writes her reservation for `uin` at `now_ms` and reads it back; `false` (no fetch) when the file is
/// unusable, when `uin` has a reservation in force (younger than `FAILURE_BACKOFF_MS`), or when the write or
/// the read-back fails. A reservation of `uin` dated ahead of the clock (the clock moved back) is restamped at
/// now and counts as in force, so it allows no fetch now. Other accounts' reservations in force are kept,
/// those dated ahead of the clock included; older ones are dropped.
fn reserve(context: &RuntimeContext, uin: &str, now_ms: i64) -> bool {
    let Some(attempts) = read_attempts(context) else {
        return false;
    };
    // The age of a reservation: negative when it is dated ahead of the clock.
    let age = |at: i64| now_ms.checked_sub(at);
    let in_force = |at: i64| age(at).is_some_and(|age| age < FAILURE_BACKOFF_MS);
    let own_recent = attempts
        .iter()
        .any(|(owner, at)| owner == uin && age(*at).is_some_and(|age| (0..FAILURE_BACKOFF_MS).contains(&age)));
    if own_recent {
        return false;
    }
    let own_ahead = attempts.iter().any(|(owner, at)| owner == uin && age(*at).is_some_and(|age| age < 0));
    let mut kept: Vec<Value> = attempts
        .iter()
        .filter(|(owner, at)| owner != uin && in_force(*at))
        .map(|(owner, at)| json!({"uin": owner, "attemptAt": at}))
        .collect();
    kept.push(json!({"uin": uin, "attemptAt": now_ms}));
    let reservation = json!({"version": 1, "attempts": kept}).to_string();
    let written = context.write_atomic(RuntimeArea::Data, ATTEMPT_FILE, reservation.as_bytes()).is_ok()
        && context.read_optional_text(RuntimeArea::Data, ATTEMPT_FILE).ok().flatten().as_deref() == Some(reservation.as_str());
    // A reservation dated ahead of the clock was just restamped now: it is in force, so this call does not fetch.
    written && !own_ahead
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

/// The production fetch: an allowed address only, through `safe_fetch::fetch_exact`, with its files in the
/// transfer folder.
fn fetch_live(context: &RuntimeContext, url: &str) -> Result<Vec<u8>, String> {
    if !url_allowed(url) {
        return Err("avatar address refused".to_string());
    }
    let folder = transfer_folder(context)?;
    transfer(url, &folder)
}

/// The folder for the avatar fetch's body and header files: `<data>/self-avatar-transfer`, created when missing
/// through the runtime's path rules (links refused). Leftovers of earlier transfers (`her-fetch-*`) are removed
/// before each fetch, so no transfer file is left in the system temporary folder or beside the data.
fn transfer_folder(context: &RuntimeContext) -> Result<PathBuf, String> {
    let folder = context
        .resolve_path(RuntimeArea::Data, TRANSFER_FOLDER, PathIntent::CreateDirectory)
        .map_err(str::to_string)?;
    let entries = std::fs::read_dir(&folder).map_err(|_| "transfer folder is unavailable".to_string())?;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("her-fetch-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(folder)
}

#[cfg(not(test))]
fn transfer(url: &str, _folder: &Path) -> Result<Vec<u8>, String> {
    crate::safe_fetch::fetch_exact(url, MAX_BYTES, FETCH_BUDGET).map(|fetched| fetched.bytes)
}

#[cfg(test)]
fn transfer(url: &str, _folder: &Path) -> Result<Vec<u8>, String> {
    FETCH_TEST_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(url),
        None => Err("no test fetch".to_string()),
    })
}

/// Avatar bytes the model may see: within `MAX_BYTES`, image magic, and they normalize.
fn decode(bytes: &[u8], max_pixels: u64) -> Option<Normalized> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES || !crate::media_normalize::looks_like_image(bytes) {
        return None;
    }
    #[cfg(test)]
    NORMALIZE_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
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

    /// A failure younger than `FAILURE_BACKOFF_MS`; one dated ahead of the clock counts as in force.
    fn failure_recent(&self, now_ms: i64) -> bool {
        self.failed_at
            .and_then(|at| now_ms.checked_sub(at))
            .is_some_and(|age| age < FAILURE_BACKOFF_MS)
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
