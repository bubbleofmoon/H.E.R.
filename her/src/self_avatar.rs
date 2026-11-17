//! #206 run 1 stub: run 2 replaces this whole file.
#![allow(dead_code)]

use crate::media_normalize::Normalized;
use crate::runtime::RuntimeContext;

pub const HOST: &str = "q1.qlogo.cn";
pub const SIZE: &str = "640";
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
pub const TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const FAILURE_BACKOFF_MS: i64 = 15 * 60 * 1000;
pub const CACHE_FILE: &str = "self-avatar.cache";
pub const GATE_FILE: &str = "locks/self-avatar.lock";

#[cfg(test)]
thread_local! {
    pub(crate) static FETCH_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str) -> Result<Vec<u8>, String>>>> =
        std::cell::RefCell::new(None);
}

pub fn valid_uin(_uin: &str) -> bool {
    false
}

pub fn avatar_url(uin: &str) -> String {
    format!("https://{HOST}/g?b=qq&nk={uin}&s={SIZE}")
}

pub fn url_allowed(_url: &str) -> bool {
    false
}

pub(crate) fn current_with(
    _context: &RuntimeContext,
    _now_ms: i64,
    _max_pixels: u64,
    _fetch: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>,
) -> Option<Normalized> {
    None
}
