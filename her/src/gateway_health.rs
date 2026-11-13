//! SnowLuma upstream health (ticket 25, AC3/AC5): "connected is not the same as receiving".
//!
//! The same state the frozen JS keeps as `snowlumaHealth` (`bridge.js` 3126-3175): when the last event packet arrived,
//! the account's online flag, the receive-chain verdict `good` (from the heartbeat payload or from `get_status`) and
//! whether the bridge has already warned about it. `snapshot` is the read-only view the administrator status shows;
//! `report` is the edge trigger (one warning when it turns degraded, one notice when it recovers).

use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

pub const STATUS_POLL_MS: u64 = 60_000;
pub const REPORT_INTERVAL_MS: u64 = 30_000;
const DEFAULT_SILENCE_WARN_MS: i64 = 10 * 60_000;

#[derive(Debug, Clone, Default)]
pub struct GatewayHealth {
    pub connected: bool,
    pub account_online: Option<bool>,
    pub good: Option<bool>,
    pub good_source: String,
    pub good_error: String,
    pub last_packet_at: i64,
    pub last_packet_kind: String,
    pub silence_warn_ms: i64,
    degraded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Steady,
    Degraded(String),
    Recovered,
}

impl GatewayHealth {
    pub fn new() -> Self {
        Self { silence_warn_ms: DEFAULT_SILENCE_WARN_MS, ..Self::default() }
    }

    /// `snowluma.silenceWarnMs` (at least 60 s), as the JS constant.
    pub fn set_silence_warn(&mut self, configured: Option<i64>) {
        self.silence_warn_ms = configured.filter(|n| *n > 0).unwrap_or(DEFAULT_SILENCE_WARN_MS).max(60_000);
    }

    pub fn note_packet(&mut self, kind: &str, now_ms: i64) {
        self.last_packet_at = now_ms;
        self.last_packet_kind = kind.to_string();
    }

    pub fn note_good(&mut self, good: Option<bool>, source: &str, now_ms: i64) {
        let _ = now_ms;
        if let Some(good) = good {
            self.good = Some(good);
            self.good_source = source.to_string();
            self.good_error.clear();
        }
    }

    pub fn note_poll_error(&mut self, error: &str) {
        self.good_error = crate::activity::preview_chars(error, 200);
    }

    pub fn snapshot(&self, now_ms: i64) -> Value {
        let ago = (self.last_packet_at > 0).then(|| now_ms - self.last_packet_at);
        let silent = self.last_packet_at > 0 && ago.is_some_and(|a| a > self.silence_warn_ms);
        let confirmed = self.good == Some(false);
        let hint = if confirmed {
            Some("SnowLuma 自报接收链路异常（good=false）：它的 [Hook] 收不到 QQ 客户端的数据，消息进不来。这不是桥接的问题。请查看 SnowLuma 的 logs/snowluma-*.log 里的 [Hook] 行（receive path stale / process enumeration timed out），并考虑升级 SnowLuma。".to_string())
        } else if silent {
            Some(format!(
                "已连接但 {} 分钟没有收到任何事件包（连 30 秒一次的心跳都没有）。这说明 SnowLuma 的 OneBot 服务/进程本身可能已经卡住或不在了，而不只是收不到 QQ 数据；请直接查看 SnowLuma 的 logs/snowluma-*.log。",
                (ago.unwrap_or(0) as f64 / 60000.0).round() as i64
            ))
        } else {
            None
        };
        json!({
            "connected": self.connected,
            "accountOnline": self.account_online,
            "good": self.good,
            "goodSource": if self.good_source.is_empty() { Value::Null } else { json!(self.good_source) },
            "goodError": if self.good_error.is_empty() { Value::Null } else { json!(self.good_error) },
            "lastPacketAgoMs": ago,
            "lastPacketKind": if self.last_packet_kind.is_empty() { Value::Null } else { json!(self.last_packet_kind) },
            "silent": silent,
            "degraded": confirmed || silent,
            "hint": hint,
        })
    }

    /// Edge trigger over the snapshot (JS `reportSnowlumaHealth`).
    pub fn report(&mut self, now_ms: i64) -> Report {
        let snap = self.snapshot(now_ms);
        let degraded = snap["degraded"].as_bool().unwrap_or(false);
        if degraded && !self.degraded {
            self.degraded = true;
            return Report::Degraded(snap["hint"].as_str().unwrap_or("").to_string());
        }
        if !degraded && self.degraded {
            self.degraded = false;
            return Report::Recovered;
        }
        Report::Steady
    }
}

fn state() -> &'static Mutex<GatewayHealth> {
    static S: OnceLock<Mutex<GatewayHealth>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(GatewayHealth::new()))
}

pub fn with<R>(f: impl FnOnce(&mut GatewayHealth) -> R) -> R {
    f(&mut state().lock().unwrap_or_else(|p| p.into_inner()))
}

pub fn snapshot_json(now_ms: i64) -> Value {
    with(|h| h.snapshot(now_ms))
}

/// What the console reports when no live bridge has published a fresh snapshot.
pub fn unknown_json() -> Value {
    json!({
        "connected": false, "accountOnline": null, "good": null, "goodSource": null, "goodError": null,
        "lastPacketAgoMs": null, "lastPacketKind": null, "silent": false, "degraded": false, "hint": null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotfix128_fix1_health_error_masks_before_the_200_character_cut() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let text = format!("{}{token} trailing", "x ".repeat(95));
        let mut health = GatewayHealth::new();
            health.note_poll_error(&text);
            let snapshot = health.snapshot(1);
            let preview = snapshot["goodError"].as_str().unwrap();
        assert_eq!(preview, format!("{}*** traili", "x ".repeat(95)));
    }


    #[test]
    fn a_confirmed_bad_receive_chain_warns_once_and_recovers_once() {
        let mut h = GatewayHealth::new();
        h.connected = true;
        h.note_packet("heartbeat", 1_000);
        assert_eq!(h.report(2_000), Report::Steady);
        h.note_good(Some(false), "heartbeat", 3_000);
        assert!(matches!(h.report(4_000), Report::Degraded(hint) if hint.contains("good=false")));
        assert_eq!(h.report(5_000), Report::Steady, "no second warning while it stays bad");
        h.note_good(Some(true), "get_status", 6_000);
        assert_eq!(h.report(7_000), Report::Recovered);
        assert_eq!(h.report(8_000), Report::Steady);
    }

    #[test]
    fn silence_only_counts_past_the_configured_threshold_and_a_non_boolean_good_is_ignored() {
        let mut h = GatewayHealth::new();
        h.note_packet("message", 1_000);
        h.note_good(None, "get_status", 1);
        assert_eq!(h.snapshot(2_000)["good"], Value::Null);
        assert_eq!(h.snapshot(9 * 60_000 + 1_000)["silent"], false);
        assert_eq!(h.snapshot(11 * 60_000 + 1_000)["silent"], true);
        h.set_silence_warn(Some(5_000));
        assert_eq!(h.silence_warn_ms, 60_000, "never below 60 s");
        assert_eq!(h.snapshot(62_000)["silent"], true);
        assert_eq!(h.snapshot(30_000)["silent"], false);
    }
}
