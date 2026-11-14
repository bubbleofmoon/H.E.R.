//! Durable outbox for outgoing social sends (ticket 08).
//!
//! Every send batch appends records to one JSONL file before any OneBot request is made:
//!   · `intent`    — the batch contents, delays, resolved reply/@ target and token epoch;
//!   · `attempt`   — written immediately before one item's OneBot request;
//!   · outcomes    — `confirmed` (optionally `verified` through history), `not_committed`
//!                   (definite refusal, nothing was delivered), `unknown` (request written, no
//!                   usable answer), `cancelled` (validity check stopped the item before its request),
//!                   `superseded` (one revision discard decision with all remaining `indices`, terminal
//!                   and never resumed; legacy per-item `idx` records remain readable).
//!
//! Folding the records gives each item one state. An `attempt` without an outcome (crash, lost
//! process) folds to `unknown`, so restarts never resend it blindly. Only `pending`,
//! `not_committed` and `cancelled` items may be resent by a resume; `confirmed` items are never
//! resent; `unknown` items are resent only after verification shows they were not delivered and
//! the operator-visible state changes accordingly.
//!
//! Records hold message text and ids but never tokens: the batch carries only a SHA-256 prefix of
//! the agent token ("epoch"), so a resume can refuse results that belong to an earlier turn.
//!
//! Writers go through `recorded_send.rs`; this module owns the record format and folding.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::runtime::{RuntimeArea, RuntimeContext};

/// Rust-owned legacy outbox; written only by this process (same pattern as the Rust token ledger).
pub const LEGACY_OUTBOX_FILE: &str = "C:/qqbot/state/send-outbox-rust.jsonl";
/// Relative name inside the selected runtime data area.
pub const ISOLATED_OUTBOX_FILE: &str = "send-outbox.jsonl";

/// Non-reversible identity of the agent token a batch was created under.
pub fn token_epoch(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Read every outbox record in append order. A truncated record (crash mid-append) is skipped with a
/// warning; any other damaged line is an error (see `runtime::parse_evidence_jsonl`).
pub fn read_records(context: &RuntimeContext) -> Result<Vec<Value>, String> {
    let text = if context.is_isolated() {
        context
            .read_optional_text(RuntimeArea::Data, ISOLATED_OUTBOX_FILE)
            .map_err(str::to_string)?
            .unwrap_or_default()
    } else {
        match std::fs::read_to_string(LEGACY_OUTBOX_FILE) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("读取发送账本失败：{error}")),
        }
    };
    crate::runtime::parse_evidence_jsonl(&text, ISOLATED_OUTBOX_FILE, "发送账本行损坏")
}

/// Append one record. A failure means the caller must not perform the network action it guards.
pub fn append_record(context: &RuntimeContext, record: &Value) -> Result<(), String> {
    if context.is_isolated() {
        let mut line = serde_json::to_vec(record).map_err(|e| format!("序列化发送账本失败：{e}"))?;
        line.push(b'\n');
        context.append_bytes_durable(RuntimeArea::Data, ISOLATED_OUTBOX_FILE, &line)
    } else {
        crate::ledger::append_record_to(LEGACY_OUTBOX_FILE, record)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemState {
    /// Never attempted.
    Pending,
    /// Attempt written, no outcome: treated as unknown.
    Attempted,
    Confirmed { message_id: Option<i64>, verified: bool },
    NotCommitted,
    Unknown,
    Cancelled,
    /// Discarded for revision before submission; never resume obsolete content.
    Superseded,
}

impl ItemState {
    pub fn label(&self) -> &'static str {
        match self {
            ItemState::Pending => "pending",
            ItemState::Attempted | ItemState::Unknown => "unknown",
            ItemState::Confirmed { .. } => "confirmed",
            ItemState::NotCommitted => "not_committed",
            ItemState::Cancelled => "cancelled",
            ItemState::Superseded => "superseded",
        }
    }

    /// Safe to send again: nothing was delivered for this item.
    pub fn resendable(&self) -> bool {
        matches!(self, ItemState::Pending | ItemState::NotCommitted | ItemState::Cancelled)
    }

    /// Possibly delivered: only verification may change it.
    pub fn needs_verification(&self) -> bool {
        matches!(self, ItemState::Attempted | ItemState::Unknown)
    }
}

#[derive(Debug, Clone)]
pub struct Batch {
    pub id: String,
    pub key: String,
    pub epoch: String,
    pub kind: String,
    pub messages: Vec<String>,
    pub delays: Vec<i64>,
    pub reply_to: Option<i64>,
    pub at_user: Option<i64>,
    pub created_at: i64,
    pub items: Vec<ItemState>,
    /// Time of the latest `attempt` record per item (ms); history evidence must be newer.
    pub attempt_at: Vec<Option<i64>>,
}

impl Batch {
    pub fn indices_where(&self, predicate: impl Fn(&ItemState) -> bool) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, state)| predicate(state))
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Earliest time a delivery of item `idx` can have happened: its latest attempt, else the
    /// batch creation.
    pub fn verify_since(&self, idx: usize) -> i64 {
        self.attempt_at.get(idx).copied().flatten().unwrap_or(self.created_at)
    }

    pub fn confirmed_count(&self) -> usize {
        self.indices_where(|s| matches!(s, ItemState::Confirmed { .. })).len()
    }

    pub fn to_json(&self) -> Value {
        let items: Vec<Value> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, state)| {
                let mut item = json!({
                    "idx": idx,
                    "state": state.label(),
                    "text": self.messages.get(idx).cloned().unwrap_or_default(),
                });
                if let ItemState::Confirmed { message_id, verified } = state {
                    item["messageId"] = json!(message_id);
                    item["verified"] = json!(verified);
                }
                item
            })
            .collect();
        let out = json!({
            "batchId": self.id,
            "key": self.key,
            "createdAt": self.created_at,
            "total": self.items.len(),
            "sent": self.confirmed_count(),
            "unknown": self.indices_where(|s| s.needs_verification()),
            "pending": self.indices_where(|s| s.resendable()),
            "items": items,
        });
        out
    }
}

fn record_idx(record: &Value) -> Option<usize> {
    record.get("idx").and_then(Value::as_u64).map(|v| v as usize)
}

/// Fold one batch. Later records override earlier ones, except terminal superseded items.
pub fn fold_batch(records: &[Value], batch_id: &str) -> Option<Batch> {
    let mut batch: Option<Batch> = None;
    for record in records {
        if record.get("batch").and_then(Value::as_str) != Some(batch_id) {
            continue;
        }
        match record.get("k").and_then(Value::as_str) {
            Some("intent") => {
                let messages: Vec<String> = record
                    .get("messages")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(|m| m.as_str().unwrap_or_default().to_string()).collect())
                    .unwrap_or_default();
                let delays: Vec<i64> = record
                    .get("delays")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_i64).collect())
                    .unwrap_or_default();
                batch = Some(Batch {
                    id: batch_id.to_string(),
                    key: record.get("key").and_then(Value::as_str).unwrap_or_default().to_string(),
                    epoch: record.get("epoch").and_then(Value::as_str).unwrap_or_default().to_string(),
                    kind: record.get("kind").and_then(Value::as_str).unwrap_or("text").to_string(),
                    items: vec![ItemState::Pending; messages.len()],
                    attempt_at: vec![None; messages.len()],
                    messages,
                    delays,
                    reply_to: record.get("replyTo").and_then(Value::as_i64),
                    at_user: record.get("atUser").and_then(Value::as_i64),
                    created_at: record.get("t").and_then(Value::as_i64).unwrap_or(0),
                });
            }
            Some("superseded") => {
                let Some(current) = batch.as_mut() else { continue };
                // New decisions list every remaining planned index in a single append. Keep
                // reading the original per-item records from previously deployed binaries.
                let indices: Vec<usize> = match record.get("indices").and_then(Value::as_array) {
                    Some(indices) => indices.iter().filter_map(Value::as_u64).map(|idx| idx as usize).collect(),
                    None => record_idx(record).into_iter().collect(),
                };
                for idx in indices {
                    if let Some(state) = current.items.get_mut(idx) {
                        *state = ItemState::Superseded;
                    }
                }
            }
            Some(kind) => {
                let Some(current) = batch.as_mut() else { continue };
                let Some(idx) = record_idx(record) else { continue };
                if idx >= current.items.len() {
                    continue;
                }
                if current.items[idx] == ItemState::Superseded {
                    continue;
                }
                if kind == "attempt" {
                    current.attempt_at[idx] = record.get("t").and_then(Value::as_i64);
                }
                current.items[idx] = match kind {
                    "attempt" => ItemState::Attempted,
                    "confirmed" => ItemState::Confirmed {
                        message_id: record.get("messageId").and_then(Value::as_i64),
                        verified: record.get("verified").and_then(Value::as_bool).unwrap_or(false),
                    },
                    "not_committed" => ItemState::NotCommitted,
                    "unknown" => ItemState::Unknown,
                    "cancelled" => ItemState::Cancelled,
                    _ => continue,
                };
            }
            None => {}
        }
    }
    batch
}

/// Exclusive in-process claim on one batch: while it is held, no other send or resume may run
/// that batch, so an item can never be attempted by two callers at once. The outbox itself is
/// written by this process only, so a process-wide registry is the whole lock; a crash drops it
/// and the folded attempt records keep unresolved items unknown.
pub struct BatchClaim(String);

fn claimed_batches() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static CLAIMED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    CLAIMED.get_or_init(Default::default)
}

/// Claim `batch_id`; `None` when another caller is running it.
pub fn claim_batch(batch_id: &str) -> Option<BatchClaim> {
    let mut claimed = claimed_batches().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    claimed.insert(batch_id.to_string()).then(|| BatchClaim(batch_id.to_string()))
}

impl Drop for BatchClaim {
    fn drop(&mut self) {
        let mut claimed = claimed_batches().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        claimed.remove(&self.0);
    }
}

/// Batch summaries for one conversation key, newest last, at most `limit`.
pub fn list_batches(records: &[Value], key: &str, limit: usize) -> Vec<Value> {
    let mut ids: Vec<String> = Vec::new();
    for record in records {
        if record.get("k").and_then(Value::as_str) == Some("intent")
            && record.get("key").and_then(Value::as_str) == Some(key)
        {
            if let Some(id) = record.get("batch").and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        }
    }
    let start = ids.len().saturating_sub(limit);
    ids[start..]
        .iter()
        .filter_map(|id| fold_batch(records, id))
        .map(|batch| batch.to_json())
        .collect()
}

/// Build a batch id that is unique across processes and calls within one process.
pub fn new_batch_id(now_ms: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("b{now_ms}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Per-item progress of one batch as returned to callers and the admin view.
pub fn progress_json(batch_id: &str, items: &[ItemState]) -> Value {
    let indices = |predicate: fn(&ItemState) -> bool| -> Vec<usize> {
        items
            .iter()
            .enumerate()
            .filter(|(_, state)| predicate(state))
            .map(|(idx, _)| idx)
            .collect()
    };
    let sent = items.iter().filter(|s| matches!(s, ItemState::Confirmed { .. })).count();
    json!({
        "batchId": batch_id,
        "total": items.len(),
        "sent": sent,
        "failed": items.len() - sent,
        "unknown": indices(|s| s.needs_verification()),
        "pending": indices(|s| s.resendable()),
        "notCommitted": indices(|s| matches!(s, ItemState::NotCommitted)),
        "cancelled": indices(|s| matches!(s, ItemState::Cancelled)),
        "superseded": indices(|s| matches!(s, ItemState::Superseded)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(batch: &str, key: &str, messages: &[&str]) -> Value {
        json!({ "batch": batch, "k": "intent", "key": key, "epoch": "e1", "messages": messages, "delays": [], "t": 1000 })
    }

    fn ledger_root(tag: &str) -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id(tag)));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    #[test]
    fn a_crash_torn_prefix_is_skipped_at_the_tail_and_after_a_later_append() {
        let (root, context) = ledger_root("her-outbox-torn-");
        append_record(&context, &intent("b1", "group:1", &["a"])).unwrap();
        let path = root.join("data").join(ISOLATED_OUTBOX_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(br#"{"batch":"b1","k":"att"#);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_records(&context).expect("a torn tail is not a read error").len(), 1);
        append_record(&context, &json!({ "batch": "b1", "k": "attempt", "idx": 0, "t": 2 })).unwrap();
        let records = read_records(&context).expect("the torn prefix, now a middle line, is still skipped");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["k"], "attempt");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_complete_record_missing_only_its_newline_is_kept() {
        let (root, context) = ledger_root("her-outbox-nonl-");
        let path = root.join("data").join(ISOLATED_OUTBOX_FILE);
        std::fs::write(&path, format!("{}\n{}", intent("b1", "group:1", &["a"]), json!({"batch":"b1","k":"attempt","idx":0,"t":2}))).unwrap();
        assert_eq!(read_records(&context).unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_malformed_line_that_is_not_a_truncated_prefix_is_still_an_error() {
        let (root, context) = ledger_root("her-outbox-mid-");
        let path = root.join("data").join(ISOLATED_OUTBOX_FILE);
        std::fs::write(&path, format!("{}\nnot json\n{}\n", intent("b1", "group:1", &["a"]), intent("b2", "group:1", &["b"]))).unwrap();
        assert!(read_records(&context).is_err());
        std::fs::write(&path, format!("{}\n{{\"a\":1}}}}\n", intent("b1", "group:1", &["a"]))).unwrap();
        assert!(read_records(&context).is_err(), "a damaged line that is not a truncated prefix is evidence, not a torn append");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attempt_without_outcome_folds_to_unknown_and_is_not_resendable() {
        let records = vec![
            intent("b1", "group:1", &["a", "b", "c"]),
            json!({ "batch": "b1", "k": "attempt", "idx": 0, "t": 1 }),
            json!({ "batch": "b1", "k": "confirmed", "idx": 0, "messageId": 5, "t": 2 }),
            json!({ "batch": "b1", "k": "attempt", "idx": 1, "t": 3 }),
        ];
        let batch = fold_batch(&records, "b1").expect("batch folds");
        assert_eq!(batch.items[0], ItemState::Confirmed { message_id: Some(5), verified: false });
        assert_eq!(batch.items[1].label(), "unknown");
        assert!(!batch.items[1].resendable());
        assert_eq!(batch.items[2], ItemState::Pending);
        assert!(batch.items[2].resendable());
        assert_eq!(batch.indices_where(|s| s.resendable()), vec![2]);
    }

    #[test]
    fn later_outcome_overrides_earlier_one_for_the_same_item() {
        let records = vec![
            intent("b2", "group:1", &["a"]),
            json!({ "batch": "b2", "k": "not_committed", "idx": 0, "t": 2 }),
            json!({ "batch": "b2", "k": "attempt", "idx": 0, "t": 3 }),
            json!({ "batch": "b2", "k": "confirmed", "idx": 0, "messageId": 9, "verified": true, "t": 4 }),
        ];
        let batch = fold_batch(&records, "b2").unwrap();
        assert_eq!(batch.items[0], ItemState::Confirmed { message_id: Some(9), verified: true });
    }

    #[test]
    fn progress_counts_sent_failed_unknown_and_pending() {
        let items = vec![
            ItemState::Confirmed { message_id: Some(1), verified: false },
            ItemState::NotCommitted,
            ItemState::Unknown,
            ItemState::Cancelled,
        ];
        let progress = progress_json("b3", &items);
        assert_eq!(progress["sent"], 1);
        assert_eq!(progress["failed"], 3);
        assert_eq!(progress["unknown"], json!([2]));
        assert_eq!(progress["pending"], json!([1, 3]));
    }

    #[test]
    fn list_batches_filters_by_key_and_keeps_the_newest() {
        let records = vec![
            intent("b4", "group:1", &["a"]),
            intent("b5", "group:2", &["x"]),
            intent("b6", "group:1", &["b"]),
        ];
        let listed = list_batches(&records, "group:1", 1);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["batchId"], "b6");
    }

    #[test]
    fn a_batch_can_be_claimed_by_one_caller_at_a_time() {
        let first = claim_batch("claim-test-1").expect("free batch is claimable");
        assert!(claim_batch("claim-test-1").is_none(), "held batch is refused");
        assert!(claim_batch("claim-test-2").is_some(), "other batches are independent");
        drop(first);
        assert!(claim_batch("claim-test-1").is_some(), "released batch is claimable again");
    }

    #[test]
    fn verification_window_starts_at_the_latest_attempt() {
        let records = vec![
            intent("b7", "group:1", &["a", "b"]),
            json!({ "batch": "b7", "k": "attempt", "idx": 0, "t": 2000 }),
            json!({ "batch": "b7", "k": "attempt", "idx": 0, "t": 3000 }),
        ];
        let batch = fold_batch(&records, "b7").unwrap();
        assert_eq!(batch.verify_since(0), 3000);
        assert_eq!(batch.verify_since(1), 1000, "never attempted: batch creation");
    }

    #[test]
    fn token_epoch_is_stable_and_does_not_reveal_the_token() {
        let epoch = token_epoch("secret-token-value");
        assert_eq!(epoch.len(), 16);
        assert_eq!(epoch, token_epoch("secret-token-value"));
        assert!(!epoch.contains("secret"));
    }
}
