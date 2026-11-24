//! Event-driven readiness waits replacing fixed sleeps in integration suites.
use std::thread;
use std::time::{Duration, Instant};

/// Polls `pred` every `interval` until it returns `true` or `timeout` expires.
pub fn wait_until(timeout: Duration, interval: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(interval);
    }
}

/// Asserts that `pred` becomes `true` within `timeout`, evaluating `diag` only on failure.
pub fn assert_eventually<F, D>(timeout: Duration, interval: Duration, pred: F, diag: D)
where
    F: FnMut() -> bool,
    D: FnOnce() -> String,
{
    if !wait_until(timeout, interval, pred) {
        panic!("{}", diag());
    }
}
