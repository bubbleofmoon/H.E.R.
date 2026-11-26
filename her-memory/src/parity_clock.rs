//! Wall-clock source for the live turn path.
//!
//! The default build (and every release build) reads the host clock only; the
//! override path below is compiled out. The non-default Cargo feature
//! `parity-clock` is used by synthetic Rust integration fixtures. With the feature, `now_ms()` starts at the
//! fixed epoch `HER_PARITY_EPOCH_MS` and advances with monotonic elapsed
//! time. A frozen clock would never fire the scheduler's own timers, which
//! compare `now` with deadlines that are set `now + delay`.
//! A fixture can publish a nonnegative millisecond advance in the optional
//! `HER_PARITY_ADVANCE_FILE` after witnessing a live operation. Publish a
//! complete file by rename; an absent or invalid file contributes no advance.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch: host clock, or (parity-clock feature with
/// `HER_PARITY_EPOCH_MS` set) the fixed epoch plus elapsed monotonic time.
pub fn now_ms() -> i64 {
    #[cfg(feature = "parity-clock")]
    {
        if let Some(ms) = fixed::now_ms() {
            return ms;
        }
    }
    host_now_ms()
}

fn host_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(feature = "parity-clock")]
mod fixed {
    use std::sync::OnceLock;
    use std::time::Instant;

    struct Origin {
        epoch_ms: i64,
        started: Instant,
        advance_file: Option<std::path::PathBuf>,
    }

    static ORIGIN: OnceLock<Option<Origin>> = OnceLock::new();

    /// Fixed epoch plus elapsed monotonic time, or None when the env var is absent.
    pub fn now_ms() -> Option<i64> {
        let origin = ORIGIN.get_or_init(|| {
            let epoch_ms = std::env::var("HER_PARITY_EPOCH_MS").ok()?.parse::<i64>().ok()?;
            eprintln!("[parity-clock] fixed epoch_ms={epoch_ms}");
            Some(Origin {
                epoch_ms,
                started: Instant::now(),
                advance_file: std::env::var_os("HER_PARITY_ADVANCE_FILE").map(Into::into),
            })
        });
        origin.as_ref().map(|o| {
            let advance = o
                .advance_file
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| text.trim().parse::<i64>().ok())
                .filter(|ms| *ms >= 0)
                .unwrap_or(0);
            o.epoch_ms
                .saturating_add(o.started.elapsed().as_millis() as i64)
                .saturating_add(advance)
        })
    }
}
