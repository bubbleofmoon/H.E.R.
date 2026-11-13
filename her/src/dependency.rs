//! Shared retry/backoff policy for dependency calls (ticket 25, AC2).
//!
//! One place that answers "may this failed call be tried again, and after how long?":
//! a bounded number of attempts, exponential backoff with a cap, a total deadline measured from the
//! first attempt, and an idempotency class. An operation whose outcome can be unknown (a prompt after
//! acceptance, any OneBot send) is `Idempotency::Never` and is never retried by this layer.
//!
//! Everything here is pure: time comes from a [`Clock`] and sleeping from a caller-supplied closure, so
//! tests run without waiting. Wiring into the live call sites happens with the dependency-recovery
//! wiring (phase B); until then the items are unused.
#![allow(dead_code)]

use std::sync::atomic::{AtomicI64, Ordering};

/// Wall-clock source. Production code uses [`SystemClock`]; tests use [`ManualClock`].
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The bridge clock (`parity_clock::now_ms`: host time, or the fixed parity epoch in parity builds).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::parity_clock::now_ms()
    }
}

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(start_ms: i64) -> Self {
        Self(AtomicI64::new(start_ms))
    }
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Whether repeating a failed call can duplicate an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idempotency {
    /// Reads, `session/selectModel`, `settings/describe`, `agentPresets/list`, and a 401 refused before processing.
    Safe,
    /// The effect may already have happened: `session/prompt` after acceptance is unknown, any OneBot send.
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUpReason {
    /// The call is `Idempotency::Never`.
    NotRetryable,
    /// `max_attempts` calls were made.
    AttemptsExhausted,
    /// The total deadline (from the first attempt) is used up or the next wait would cross it.
    DeadlineExceeded,
}

impl GiveUpReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotRetryable => "not-retryable",
            Self::AttemptsExhausted => "attempts-exhausted",
            Self::DeadlineExceeded => "deadline-exceeded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Wait `after_ms`, then make attempt number `next_attempt` (1-based).
    Retry {
        after_ms: u64,
        next_attempt: u32,
    },
    GiveUp(GiveUpReason),
}

/// Attempts, backoff and total time for one class of safe operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total calls allowed, including the first (1 means never retry).
    pub max_attempts: u32,
    /// Wait before attempt 2; doubles for each further attempt.
    pub base_ms: u64,
    /// No single wait exceeds this.
    pub cap_ms: u64,
    /// Measured from the first attempt's start; no retry is scheduled past it.
    pub total_deadline_ms: u64,
    /// A wait is shortened by up to this percentage (0..=100); jitter never lengthens a wait, so `cap_ms` holds.
    pub jitter_pct: u8,
}

impl RetryPolicy {
    /// `session/selectModel`: two attempts per candidate with a flat 1 s pause, as the frozen JS `ensureChatModel`.
    pub const SELECT_MODEL: Self = Self {
        max_attempts: 2,
        base_ms: 1000,
        cap_ms: 1000,
        total_deadline_ms: 90_000,
        jitter_pct: 0,
    };
    /// Any other safe DSH read (`settings/describe`, `agentPresets/list`, `session/follow` setup, a 401 re-auth).
    pub const SAFE_RPC: Self = Self {
        max_attempts: 3,
        base_ms: 500,
        cap_ms: 4000,
        total_deadline_ms: 30_000,
        jitter_pct: 20,
    };
    /// Replaying work queued while DSH was down: JS `min(3000 * 2^(n-1), 60000)` and five failures.
    pub const QUEUE_FLUSH: Self = Self {
        max_attempts: 5,
        base_ms: 3000,
        cap_ms: 60_000,
        total_deadline_ms: 600_000,
        jitter_pct: 0,
    };

    /// Wait before the attempt that follows `failures` failed attempts (`failures >= 1`).
    /// `jitter_unit` is a number in `[0, 1)`; 0 gives the full delay.
    pub fn delay_ms(&self, failures: u32, jitter_unit: f64) -> u64 {
        let shift = failures.saturating_sub(1).min(30);
        let raw = self.base_ms.saturating_mul(1u64 << shift).min(self.cap_ms);
        let unit = if jitter_unit.is_finite() {
            jitter_unit.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let cut = (raw as f64 * f64::from(self.jitter_pct.min(100)) / 100.0 * unit) as u64;
        raw.saturating_sub(cut)
    }

    pub fn start(&self, now_ms: i64) -> RetryState {
        RetryState {
            policy: *self,
            started_ms: now_ms,
            failures: 0,
        }
    }
}

/// Progress of one operation through a [`RetryPolicy`].
#[derive(Debug, Clone)]
pub struct RetryState {
    policy: RetryPolicy,
    started_ms: i64,
    failures: u32,
}

impl RetryState {
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Record a failed attempt at `now_ms` and decide what happens next.
    pub fn on_failure(
        &mut self,
        now_ms: i64,
        idempotency: Idempotency,
        jitter_unit: f64,
    ) -> RetryDecision {
        self.failures = self.failures.saturating_add(1);
        if idempotency == Idempotency::Never {
            return RetryDecision::GiveUp(GiveUpReason::NotRetryable);
        }
        let elapsed = now_ms.saturating_sub(self.started_ms).max(0) as u64;
        if elapsed >= self.policy.total_deadline_ms {
            return RetryDecision::GiveUp(GiveUpReason::DeadlineExceeded);
        }
        if self.failures >= self.policy.max_attempts {
            return RetryDecision::GiveUp(GiveUpReason::AttemptsExhausted);
        }
        let wait = self.policy.delay_ms(self.failures, jitter_unit);
        if elapsed.saturating_add(wait) >= self.policy.total_deadline_ms {
            return RetryDecision::GiveUp(GiveUpReason::DeadlineExceeded);
        }
        RetryDecision::Retry {
            after_ms: wait,
            next_attempt: self.failures + 1,
        }
    }
}

/// A deterministic pseudo-random number in `[0, 1)` from a time and a salt (no `rand` dependency).
pub fn jitter_unit(now_ms: i64, salt: u64) -> f64 {
    let mut x = (now_ms as u64) ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// The final failure of [`run_with_retry`].
#[derive(Debug, PartialEq, Eq)]
pub struct RetryFailure<E> {
    pub error: E,
    /// Calls made, including the last.
    pub attempts: u32,
    pub reason: GiveUpReason,
}

/// Run `op(attempt)` under `policy`. `is_retryable(&error)` says whether the failure is the kind a safe retry
/// can fix (for example a connection refused before the request was written, or a 401 after which the caller
/// re-authenticated); `sleep(ms)` waits (tests pass a closure that advances a [`ManualClock`]).
pub fn run_with_retry<T, E>(
    policy: &RetryPolicy,
    clock: &dyn Clock,
    idempotency: Idempotency,
    is_retryable: impl Fn(&E) -> bool,
    mut sleep: impl FnMut(u64),
    mut op: impl FnMut(u32) -> Result<T, E>,
) -> Result<T, RetryFailure<E>> {
    let mut state = policy.start(clock.now_ms());
    let mut attempt = 1u32;
    loop {
        let error = match op(attempt) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let now = clock.now_ms();
        let unit = jitter_unit(now, u64::from(attempt));
        let decision = if is_retryable(&error) {
            state.on_failure(now, idempotency, unit)
        } else {
            state.on_failure(now, Idempotency::Never, unit)
        };
        match decision {
            RetryDecision::Retry {
                after_ms,
                next_attempt,
            } => {
                sleep(after_ms);
                attempt = next_attempt;
            }
            RetryDecision::GiveUp(reason) => {
                return Err(RetryFailure {
                    error,
                    attempts: attempt,
                    reason,
                })
            }
        }
    }
}

/// Backoff for a reconnect loop that never gives up but must not storm: the wait doubles up to a cap and
/// restarts only after a connection that actually delivered traffic (the frozen JS event-stream rule).
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    base_ms: u64,
    cap_ms: u64,
    jitter_pct: u8,
    warn_at: u32,
    streak: u32,
}

impl ReconnectBackoff {
    pub fn new(base_ms: u64, cap_ms: u64, jitter_pct: u8, warn_at: u32) -> Self {
        Self {
            base_ms,
            cap_ms,
            jitter_pct,
            warn_at,
            streak: 0,
        }
    }
    /// JS `session/events` stream: 3 s doubling to 30 s, warning at streak 3.
    pub fn event_stream() -> Self {
        Self::new(3000, 30_000, 10, 3)
    }
    /// The OneBot gateway link (the previous flat 2 s/5 s becomes 2 s doubling to 30 s).
    pub fn gateway() -> Self {
        Self::new(2000, 30_000, 10, 3)
    }
    pub fn streak(&self) -> u32 {
        self.streak
    }
    /// A connect or stream failed. Returns how long to wait before trying again.
    pub fn on_failure(&mut self, jitter_unit: f64) -> u64 {
        self.streak = self.streak.saturating_add(1);
        RetryPolicy {
            max_attempts: u32::MAX,
            base_ms: self.base_ms,
            cap_ms: self.cap_ms,
            total_deadline_ms: u64::MAX,
            jitter_pct: self.jitter_pct,
        }
        .delay_ms(self.streak, jitter_unit)
    }
    /// True exactly when the failure streak reaches the warning threshold (one warning per outage, not one per retry).
    pub fn should_warn(&self) -> bool {
        self.streak == self.warn_at
    }
    /// The connection delivered traffic: the next failure starts from the base wait again.
    pub fn on_traffic(&mut self) {
        self.streak = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn delay_doubles_to_the_cap_and_jitter_only_shortens() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_ms: 1000,
            cap_ms: 5000,
            total_deadline_ms: 1_000_000,
            jitter_pct: 50,
        };
        let full: Vec<u64> = (1..=5).map(|n| p.delay_ms(n, 0.0)).collect();
        assert_eq!(full, vec![1000, 2000, 4000, 5000, 5000]);
        for n in 1..=5 {
            let jittered = p.delay_ms(n, 0.999_999);
            assert!(
                jittered <= full[n as usize - 1],
                "jitter never lengthens a wait"
            );
            assert!(
                jittered >= full[n as usize - 1] / 2,
                "jitter is bounded by jitter_pct"
            );
        }
        assert_eq!(
            p.delay_ms(200, 0.0),
            5000,
            "huge failure counts do not overflow"
        );
        assert_eq!(
            p.delay_ms(2, f64::NAN),
            2000,
            "a bad jitter input means no jitter"
        );
    }

    #[test]
    fn attempts_are_bounded() {
        let p = RetryPolicy {
            max_attempts: 3,
            base_ms: 100,
            cap_ms: 1000,
            total_deadline_ms: 1_000_000,
            jitter_pct: 0,
        };
        let mut s = p.start(0);
        assert_eq!(
            s.on_failure(10, Idempotency::Safe, 0.0),
            RetryDecision::Retry {
                after_ms: 100,
                next_attempt: 2
            }
        );
        assert_eq!(
            s.on_failure(120, Idempotency::Safe, 0.0),
            RetryDecision::Retry {
                after_ms: 200,
                next_attempt: 3
            }
        );
        assert_eq!(
            s.on_failure(330, Idempotency::Safe, 0.0),
            RetryDecision::GiveUp(GiveUpReason::AttemptsExhausted)
        );
        assert_eq!(s.failures(), 3);
    }

    #[test]
    fn total_deadline_stops_retries_even_with_attempts_left() {
        let p = RetryPolicy {
            max_attempts: 100,
            base_ms: 1000,
            cap_ms: 1000,
            total_deadline_ms: 2500,
            jitter_pct: 0,
        };
        let mut s = p.start(1_000);
        assert!(matches!(
            s.on_failure(1_100, Idempotency::Safe, 0.0),
            RetryDecision::Retry { .. }
        ));
        assert!(matches!(
            s.on_failure(2_200, Idempotency::Safe, 0.0),
            RetryDecision::Retry { .. }
        ));
        // 2_900 - 1_000 = 1_900 elapsed; the next 1_000 ms wait would cross 2_500.
        assert_eq!(
            s.on_failure(2_900, Idempotency::Safe, 0.0),
            RetryDecision::GiveUp(GiveUpReason::DeadlineExceeded)
        );
        // Already past the deadline.
        let mut late = p.start(0);
        assert_eq!(
            late.on_failure(9_000, Idempotency::Safe, 0.0),
            RetryDecision::GiveUp(GiveUpReason::DeadlineExceeded)
        );
    }

    #[test]
    fn never_idempotent_calls_are_not_retried() {
        let mut s = RetryPolicy::SAFE_RPC.start(0);
        assert_eq!(
            s.on_failure(1, Idempotency::Never, 0.0),
            RetryDecision::GiveUp(GiveUpReason::NotRetryable)
        );
    }

    #[test]
    fn named_policies_are_bounded() {
        for p in [
            RetryPolicy::SELECT_MODEL,
            RetryPolicy::SAFE_RPC,
            RetryPolicy::QUEUE_FLUSH,
        ] {
            assert!(p.max_attempts >= 1 && p.max_attempts < 10);
            assert!(p.base_ms <= p.cap_ms);
            assert!(p.total_deadline_ms > 0 && p.total_deadline_ms <= 600_000);
        }
        assert_eq!(
            RetryPolicy::SELECT_MODEL.delay_ms(1, 0.0),
            1000,
            "JS sleeps 1 s between selectModel attempts"
        );
        let queue: Vec<u64> = (1..=5)
            .map(|n| RetryPolicy::QUEUE_FLUSH.delay_ms(n, 0.0))
            .collect();
        assert_eq!(queue, vec![3000, 6000, 12_000, 24_000, 48_000]);
        assert_eq!(
            RetryPolicy::QUEUE_FLUSH.delay_ms(6, 0.0),
            60_000,
            "JS min(3000*2^(n-1), 60000)"
        );
    }

    #[test]
    fn run_with_retry_succeeds_after_transient_failures_without_real_sleeping() {
        let clock = ManualClock::new(1_000);
        let slept = RefCell::new(Vec::new());
        let calls = Cell::new(0);
        let result: Result<&str, RetryFailure<&str>> = run_with_retry(
            &RetryPolicy {
                max_attempts: 4,
                base_ms: 100,
                cap_ms: 400,
                total_deadline_ms: 10_000,
                jitter_pct: 0,
            },
            &clock,
            Idempotency::Safe,
            |_| true,
            |ms| {
                slept.borrow_mut().push(ms);
                clock.advance(ms as i64);
            },
            |attempt| {
                calls.set(calls.get() + 1);
                if attempt < 3 {
                    Err("refused")
                } else {
                    Ok("done")
                }
            },
        );
        assert_eq!(result, Ok("done"));
        assert_eq!(calls.get(), 3);
        assert_eq!(*slept.borrow(), vec![100, 200]);
    }

    #[test]
    fn run_with_retry_gives_up_with_the_last_error_and_attempt_count() {
        let clock = ManualClock::new(0);
        let calls = Cell::new(0);
        let err = run_with_retry::<(), &str>(
            &RetryPolicy {
                max_attempts: 3,
                base_ms: 10,
                cap_ms: 10,
                total_deadline_ms: 10_000,
                jitter_pct: 0,
            },
            &clock,
            Idempotency::Safe,
            |_| true,
            |ms| clock.advance(ms as i64),
            |_| {
                calls.set(calls.get() + 1);
                Err("down")
            },
        )
        .unwrap_err();
        assert_eq!(
            (err.error, err.attempts, err.reason),
            ("down", 3, GiveUpReason::AttemptsExhausted)
        );
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn run_with_retry_never_calls_a_never_idempotent_op_twice() {
        let clock = ManualClock::new(0);
        let calls = Cell::new(0);
        let err = run_with_retry::<(), &str>(
            &RetryPolicy::SAFE_RPC,
            &clock,
            Idempotency::Never,
            |_| true,
            |_| panic!("must not sleep"),
            |_| {
                calls.set(calls.get() + 1);
                Err("outcome unknown")
            },
        )
        .unwrap_err();
        assert_eq!(
            (calls.get(), err.attempts, err.reason),
            (1, 1, GiveUpReason::NotRetryable)
        );
    }

    #[test]
    fn run_with_retry_does_not_retry_errors_the_caller_says_are_final() {
        let clock = ManualClock::new(0);
        let calls = Cell::new(0);
        let err = run_with_retry::<(), &str>(
            &RetryPolicy::SAFE_RPC,
            &clock,
            Idempotency::Safe,
            |e| *e != "bad request",
            |_| panic!("must not sleep"),
            |_| {
                calls.set(calls.get() + 1);
                Err("bad request")
            },
        )
        .unwrap_err();
        assert_eq!((calls.get(), err.reason), (1, GiveUpReason::NotRetryable));
    }

    #[test]
    fn run_with_retry_respects_the_total_deadline_when_calls_are_slow() {
        let clock = ManualClock::new(0);
        let calls = Cell::new(0);
        let err = run_with_retry::<(), &str>(
            &RetryPolicy {
                max_attempts: 50,
                base_ms: 100,
                cap_ms: 100,
                total_deadline_ms: 1_000,
                jitter_pct: 0,
            },
            &clock,
            Idempotency::Safe,
            |_| true,
            |ms| clock.advance(ms as i64),
            |_| {
                calls.set(calls.get() + 1);
                clock.advance(400); // each call itself takes 400 ms
                Err("slow failure")
            },
        )
        .unwrap_err();
        assert_eq!(err.reason, GiveUpReason::DeadlineExceeded);
        assert!(
            calls.get() <= 3,
            "bounded by time, not by the 50 attempts: {}",
            calls.get()
        );
    }

    #[test]
    fn reconnect_backoff_doubles_caps_warns_once_and_resets_after_traffic() {
        let mut b = ReconnectBackoff::new(3000, 30_000, 0, 3);
        let waits: Vec<u64> = (0..6).map(|_| b.on_failure(0.0)).collect();
        assert_eq!(waits, vec![3000, 6000, 12_000, 24_000, 30_000, 30_000]);
        let mut b = ReconnectBackoff::event_stream();
        let mut warnings = 0;
        for _ in 0..10 {
            b.on_failure(0.0);
            if b.should_warn() {
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1, "one warning per outage");
        b.on_traffic();
        assert_eq!(b.streak(), 0);
        assert_eq!(
            b.on_failure(0.0),
            3000,
            "a stream that delivered traffic restarts the backoff"
        );
    }

    #[test]
    fn a_long_outage_makes_a_bounded_number_of_reconnects() {
        let mut b = ReconnectBackoff::gateway();
        let (mut t, mut attempts) = (0u64, 0u32);
        while t < 3_600_000 {
            t += b.on_failure(0.0);
            attempts += 1;
        }
        assert!(
            attempts <= 140,
            "one hour of outage must not storm: {attempts} attempts"
        );
    }

    #[test]
    fn jitter_unit_is_in_range_and_varies() {
        let values: Vec<f64> = (0..50)
            .map(|i| jitter_unit(1_700_000_000_000 + i, 7))
            .collect();
        assert!(values.iter().all(|v| (0.0..1.0).contains(v)));
        assert!(values.windows(2).any(|w| w[0] != w[1]));
        assert_eq!(
            jitter_unit(5, 9),
            jitter_unit(5, 9),
            "deterministic for the same inputs"
        );
    }

    #[test]
    fn system_clock_follows_the_bridge_clock() {
        let a = SystemClock.now_ms();
        assert!(a > 1_600_000_000_000);
    }
}
