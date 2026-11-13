//! The recorded send (issue #70): the one owner of the send outbox protocol.
//!
//! Every chat, tool, voice and sticker send goes through here:
//!   · `send_chain` — the process-wide send order (JS `sendChain`); callers hold it from their
//!     last validity check through the gateway answer and the unknown-outcome check;
//!   · `Recorded::begin` / `resume` — claim the batch (one runner per batch) and, for a new
//!     batch, durably record its intent before any request;
//!   · `attempt`, `cancelled`, `superseded` — the records around one item's request;
//!   · `settle` — the item's outcome. An unknown outcome is recorded, then checked against the
//!     gateway history; only evidence there turns it into a verified confirmation. Nothing here
//!     sends again: an unknown item stays unknown until history proves it.
//!
//! Record format and folding stay in `send_outbox.rs`; reply classification stays in
//! `onebot::classify_send_reply`.

use std::sync::{Mutex, MutexGuard};

use serde_json::{json, Map, Value};

use crate::onebot::{OneBot, SendError};
use crate::runtime::RuntimeContext;
use crate::send_outbox::{
    append_record, claim_batch, new_batch_id, read_records, BatchClaim, ItemState,
};

/// Refusal for a batch another caller is running.
pub const BATCH_BUSY: &str = "该批次正在进行中，请稍后再试";

static SEND_CHAIN: Mutex<()> = Mutex::new(());

/// The process-wide send order. Poisoning cannot corrupt it: it guards no data.
pub fn send_chain() -> MutexGuard<'static, ()> {
    SEND_CHAIN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A OneBot message id, sent as a number or a numeric string.
pub fn message_id(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// What proves delivery of an unknown item: our own message after the attempt with this text
/// (compared in canonical face form), or with a segment of this type (`record`, `image`).
#[derive(Clone, Copy, Debug)]
pub enum Proof<'a> {
    Text(&'a str),
    Segment(&'static str),
}

/// The conversation whose recent history is searched.
pub struct History<'a> {
    pub ob: &'a OneBot,
    /// `group` or `private`.
    pub kind: &'a str,
    pub id_s: &'a str,
}

#[derive(Debug)]
pub enum BeginError {
    /// Another caller holds this batch.
    Busy,
    /// The intent could not be recorded; nothing may be sent.
    Intent(String),
}

/// One item's settled outcome.
#[derive(Debug)]
pub struct Settled {
    pub state: ItemState,
    /// Gateway failure detail of a not-committed or unknown request, kept when history later
    /// proves delivery.
    pub error: Option<String>,
    /// The first outcome record that could not be appended.
    pub record_error: Option<String>,
    /// Gateway reply of a request it confirmed itself.
    pub data: Option<Value>,
}

/// A claimed batch; the claim is released on drop.
pub struct Recorded<'a> {
    context: &'a RuntimeContext,
    clock: fn() -> i64,
    batch_id: String,
    _claim: BatchClaim,
}

impl<'a> Recorded<'a> {
    /// Claim a new batch and durably record its intent. `fields` holds the batch-specific intent
    /// fields (key, epoch, messages, and as the caller needs kind, purpose, delays, replyTo,
    /// atUser, turnScope); `t` defaults to `created_at`.
    pub fn begin(
        context: &'a RuntimeContext,
        clock: fn() -> i64,
        created_at: i64,
        fields: Value,
    ) -> Result<Self, BeginError> {
        let batch_id = new_batch_id(created_at);
        let claim = claim_batch(&batch_id).ok_or(BeginError::Busy)?;
        let mut intent = Map::new();
        intent.insert("batch".into(), json!(batch_id));
        intent.insert("k".into(), json!("intent"));
        if let Value::Object(fields) = fields {
            intent.extend(fields);
        }
        intent.entry("t").or_insert_with(|| json!(created_at));
        append_record(context, &Value::Object(intent)).map_err(BeginError::Intent)?;
        Ok(Self {
            context,
            clock,
            batch_id,
            _claim: claim,
        })
    }

    /// Claim an existing batch to resume it; `None` while another caller runs it.
    pub fn resume(context: &'a RuntimeContext, clock: fn() -> i64, batch_id: &str) -> Option<Self> {
        let claim = claim_batch(batch_id)?;
        Some(Self {
            context,
            clock,
            batch_id: batch_id.to_string(),
            _claim: claim,
        })
    }

    pub fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn append(&self, kind: &str, idx: usize, extra: &[(&str, Value)]) -> Result<i64, String> {
        let t = (self.clock)();
        let mut record = Map::new();
        record.insert("batch".into(), json!(self.batch_id));
        record.insert("k".into(), json!(kind));
        record.insert("idx".into(), json!(idx));
        for (name, value) in extra {
            record.insert((*name).into(), value.clone());
        }
        record.insert("t".into(), json!(t));
        append_record(self.context, &Value::Object(record)).map(|()| t)
    }

    /// Record the attempt right before the request; returns its time (history evidence must not
    /// be older). A failure means the request must not be made.
    pub fn attempt(&self, idx: usize, turn_scope: Option<&str>) -> Result<i64, String> {
        match turn_scope {
            Some(scope) => self.append("attempt", idx, &[("turnScope", json!(scope))]),
            None => self.append("attempt", idx, &[]),
        }
    }

    pub fn cancelled(&self, idx: usize, error: Option<&str>) -> Result<(), String> {
        match error {
            Some(error) => self.append("cancelled", idx, &[("error", json!(error))]),
            None => self.append("cancelled", idx, &[]),
        }
        .map(|_| ())
    }

    /// One revision discard decision covering every remaining planned index.
    pub fn superseded(&self, indices: &[usize], error: &str) -> Result<(), String> {
        let record = json!({ "batch": self.batch_id, "k": "superseded", "indices": indices, "error": error, "t": (self.clock)() });
        append_record(self.context, &record)
    }

    fn confirmed(&self, idx: usize, message_id: Option<i64>, verified: bool) -> Result<(), String> {
        if verified {
            self.append(
                "confirmed",
                idx,
                &[("messageId", json!(message_id)), ("verified", json!(true))],
            )
        } else {
            self.append("confirmed", idx, &[("messageId", json!(message_id))])
        }
        .map(|_| ())
    }

    /// Look for proof that item `idx`, attempted at `since_ms`, was delivered. One history message
    /// proves one item: `exclude` plus every id the outbox confirmed in the ten minutes before
    /// `since_ms` onward are skipped. A confirmation without a message id, or an unreadable outbox,
    /// proves nothing. Proof appends a verified confirmation (its append result is returned with the id).
    pub fn confirm_from_history(
        &self,
        idx: usize,
        since_ms: i64,
        history: &History<'_>,
        proof: Proof<'_>,
        exclude: &[i64],
    ) -> Option<(i64, Result<(), String>)> {
        // Ten minutes is wider than the 30-message history: every earlier confirmation that could
        // share this attempt's whole-second times falls inside the window.
        let (claimed, unidentified) = confirmed_since(self.context, since_ms - 600_000)?;
        if unidentified {
            return None;
        }
        let mut exclude = exclude.to_vec();
        exclude.extend(claimed);
        let found = history_evidence(history, since_ms, &exclude, proof)?;
        Some((found, self.confirmed(idx, Some(found), true)))
    }

    /// Record one request's outcome. An unknown outcome is recorded first and then checked
    /// against history; it is never sent again from here.
    pub fn settle(
        &self,
        idx: usize,
        attempt_ms: i64,
        outcome: Result<Value, SendError>,
        history: &History<'_>,
        proof: Proof<'_>,
        exclude: &[i64],
    ) -> Settled {
        match outcome {
            Ok(data) => {
                let id = message_id(data.get("message_id"));
                let record_error = self.confirmed(idx, id, false).err();
                Settled {
                    state: ItemState::Confirmed {
                        message_id: id,
                        verified: false,
                    },
                    error: None,
                    record_error,
                    data: Some(data),
                }
            }
            Err(SendError::NotCommitted(error)) => {
                let record_error = self
                    .append("not_committed", idx, &[("error", json!(error))])
                    .err();
                Settled {
                    state: ItemState::NotCommitted,
                    error: Some(error),
                    record_error,
                    data: None,
                }
            }
            Err(SendError::Unknown(error)) => {
                let mut record_error = self
                    .append("unknown", idx, &[("error", json!(error))])
                    .err();
                let state =
                    match self.confirm_from_history(idx, attempt_ms, history, proof, exclude) {
                        Some((found, recorded)) => {
                            if record_error.is_none() {
                                record_error = recorded.err();
                            }
                            ItemState::Confirmed {
                                message_id: Some(found),
                                verified: true,
                            }
                        }
                        None => ItemState::Unknown,
                    };
                Settled {
                    state,
                    error: Some(error),
                    record_error,
                    data: None,
                }
            }
        }
    }
}

/// Message ids of the outbox's `confirmed` records dated at or after `since_ms`, and whether one of
/// them has no integer message id (it cannot be excluded by id). `None` when the outbox cannot be
/// read: nothing is then proven.
fn confirmed_since(context: &RuntimeContext, since_ms: i64) -> Option<(Vec<i64>, bool)> {
    let records = read_records(context).ok()?;
    let confirmed: Vec<&Value> = records
        .iter()
        .filter(|r| r["k"] == "confirmed" && r["t"].as_i64().unwrap_or(0) >= since_ms)
        .collect();
    let unidentified = confirmed.iter().any(|r| !r["messageId"].is_i64());
    let ids = confirmed
        .iter()
        .filter_map(|r| r["messageId"].as_i64())
        .collect();
    Some((ids, unidentified))
}

/// Our own message in the target's recent history, dated at or after `since_ms`, that matches
/// `proof` and is not excluded. Exactly one such message is proof; zero or several is not proven,
/// and an unreadable history is not proven either.
pub fn history_evidence(
    history: &History<'_>,
    since_ms: i64,
    exclude: &[i64],
    proof: Proof<'_>,
) -> Option<i64> {
    let ob = history.ob;
    let login = ob.request("get_login_info", json!({}))?;
    let me = message_id(login.get("data")?.get("user_id"))?;
    let id: i64 = history.id_s.parse().ok()?;
    let (action, params) = if history.kind == "group" {
        (
            "get_group_msg_history",
            json!({ "group_id": id, "count": 30 }),
        )
    } else {
        (
            "get_friend_msg_history",
            json!({ "user_id": id, "count": 30 }),
        )
    };
    let response = ob.request(action, params)?;
    let messages = response.get("data")?.get("messages")?.as_array()?;
    // Evidence must be dated at or after the attempt (QQ message times are whole seconds): an
    // identical older message is not proof. Clock skew can only turn a delivered item into
    // "unknown", which is never resent, so the window errs towards no duplicate.
    let threshold_s = since_ms / 1000;
    let mut candidates = messages.iter().rev().filter_map(|m| {
        let sender = message_id(
            m.get("sender")
                .and_then(|s| s.get("user_id"))
                .or(m.get("user_id")),
        )?;
        let time = m.get("time").and_then(Value::as_i64).unwrap_or(0);
        let found = message_id(m.get("message_id"))?;
        (sender == me
            && time >= threshold_s
            && proof_matches(m, proof)
            && !exclude.contains(&found))
        .then_some(found)
    });
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}

fn proof_matches(message: &Value, proof: Proof<'_>) -> bool {
    match proof {
        Proof::Text(text) => history_text_matches(message, text),
        Proof::Segment(kind) => message
            .get("message")
            .and_then(Value::as_array)
            .is_some_and(|segments| {
                segments
                    .iter()
                    .any(|s| s.get("type").and_then(Value::as_str) == Some(kind))
            }),
    }
}

pub fn history_text_matches(message: &Value, text: &str) -> bool {
    let expected = crate::outbound_message::normalize(text);
    !expected.is_empty()
        && crate::outbound_message::normalize(&plain_text_of_message(message)) == expected
}

/// Plain text of a history record (text segments, or the raw string forms).
fn plain_text_of_message(message: &Value) -> String {
    match message.get("message") {
        Some(Value::Array(segments)) => segments
            .iter()
            .filter_map(|s| match s["type"].as_str() {
                Some("text") => s["data"]["text"].as_str().map(str::to_owned),
                Some("face") => Some(format!(
                    "[表情{}]",
                    crate::console::social_policy_js_string(&s["data"]["id"])
                )),
                _ => None,
            })
            .collect(),
        Some(Value::String(text)) => text.clone(),
        _ => message
            .get("raw_message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> (std::path::PathBuf, RuntimeContext) {
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(crate::turn_ledger::mint_id(tag)),
        );
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"synthetic-review-token-abcdefghij"}"#,
        )
        .unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    fn clock() -> i64 {
        5_000
    }

    fn unreachable() -> OneBot {
        OneBot::new("http://127.0.0.1:1", String::new())
    }

    #[test]
    fn begin_claims_the_batch_and_records_the_intent_first() {
        let (root, context) = root("her-recorded-begin-");
        let recorded = Recorded::begin(
            &context,
            clock,
            1_000,
            json!({"key":"group:1","epoch":"e","messages":["a"]}),
        )
        .unwrap();
        assert!(
            Recorded::resume(&context, clock, recorded.batch_id()).is_none(),
            "one runner per batch"
        );
        let records = read_records(&context).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["k"], "intent");
        assert_eq!(records[0]["batch"], recorded.batch_id());
        assert_eq!(records[0]["t"], 1_000);
        assert_eq!(records[0]["messages"], json!(["a"]));
        let id = recorded.batch_id().to_string();
        drop(recorded);
        assert!(
            Recorded::resume(&context, clock, &id).is_some(),
            "the claim is released on drop"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_fold_to_the_states_the_outbox_expects() {
        let (root, context) = root("her-recorded-fold-");
        let ob = unreachable();
        let history = History {
            ob: &ob,
            kind: "group",
            id_s: "1",
        };
        let recorded = Recorded::begin(
            &context,
            clock,
            1_000,
            json!({"key":"group:1","epoch":"e","messages":["a","b","c"]}),
        )
        .unwrap();
        assert_eq!(recorded.attempt(0, Some("scope")).unwrap(), 5_000);
        let ok = recorded.settle(
            0,
            5_000,
            Ok(json!({"message_id":"7"})),
            &history,
            Proof::Text("a"),
            &[],
        );
        assert_eq!(
            ok.state,
            ItemState::Confirmed {
                message_id: Some(7),
                verified: false
            }
        );
        recorded.attempt(1, None).unwrap();
        let refused = recorded.settle(
            1,
            5_000,
            Err(SendError::NotCommitted("no".into())),
            &history,
            Proof::Text("b"),
            &[],
        );
        assert_eq!(
            (refused.state, refused.error.as_deref()),
            (ItemState::NotCommitted, Some("no"))
        );
        recorded.cancelled(2, None).unwrap();
        let records = read_records(&context).unwrap();
        assert_eq!(records[1]["turnScope"], "scope");
        assert!(records[5].get("error").is_none());
        let batch = crate::send_outbox::fold_batch(&records, recorded.batch_id()).unwrap();
        assert_eq!(
            batch.items,
            vec![
                ItemState::Confirmed {
                    message_id: Some(7),
                    verified: false
                },
                ItemState::NotCommitted,
                ItemState::Cancelled
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unknown_outcome_without_history_evidence_stays_unknown() {
        let (root, context) = root("her-recorded-unknown-");
        let ob = unreachable();
        let history = History {
            ob: &ob,
            kind: "group",
            id_s: "1",
        };
        let recorded = Recorded::begin(
            &context,
            clock,
            1_000,
            json!({"key":"group:1","epoch":"e","messages":["a"]}),
        )
        .unwrap();
        let at = recorded.attempt(0, None).unwrap();
        let settled = recorded.settle(
            0,
            at,
            Err(SendError::Unknown("lost".into())),
            &history,
            Proof::Text("a"),
            &[],
        );
        assert_eq!(
            (
                settled.state,
                settled.error.as_deref(),
                settled.record_error
            ),
            (ItemState::Unknown, Some("lost"), None)
        );
        let kinds: Vec<String> = read_records(&context)
            .unwrap()
            .iter()
            .filter_map(|r| r["k"].as_str().map(str::to_string))
            .collect();
        assert_eq!(kinds, ["intent", "attempt", "unknown"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn proofs_match_canonical_text_and_typed_segments() {
        let face = json!({"message":[{"type":"face","data":{"id":28}}]});
        assert!(proof_matches(&face, Proof::Text("[表情028]")));
        assert!(!proof_matches(&face, Proof::Segment("record")));
        assert!(proof_matches(
            &json!({"message":[{"type":"record","data":{}}]}),
            Proof::Segment("record")
        ));
        assert!(
            !proof_matches(&json!({"message":[]}), Proof::Text("")),
            "empty text never proves delivery"
        );
    }

    fn clock_a() -> i64 {
        1_000_000
    }

    fn clock_b() -> i64 {
        1_000_900
    }

    fn same_text(id: i64, time: i64) -> Value {
        json!({"message_id": id, "time": time, "sender": {"user_id": 10001},
            "message": [{"type": "text", "data": {"text": "same"}}]})
    }

    /// A gateway that answers login (user 10001) and one history read with `messages`, then stops.
    fn history_gateway(messages: Value) -> (OneBot, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let ob = OneBot::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            String::new(),
        );
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut paths = Vec::new();
            while paths.len() < 2 && std::time::Instant::now() < deadline {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                paths.push(path.clone());
                let reply = match path.as_str() {
                    "/get_login_info" => json!({"status": "ok", "retcode": 0, "data": {"user_id": 10001}}),
                    "/get_group_msg_history" => json!({"status": "ok", "retcode": 0, "data": {"messages": messages.clone()}}),
                    _ => json!({"status": "failed", "retcode": 1404}),
                }
                .to_string();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
            paths
        });
        (ob, worker)
    }

    fn same_text_intent(
        context: &RuntimeContext,
        clock: fn() -> i64,
        created_at: i64,
    ) -> Recorded<'_> {
        Recorded::begin(
            context,
            clock,
            created_at,
            json!({"key": "group:1", "epoch": "e", "messages": ["same"]}),
        )
        .unwrap()
    }

    #[test]
    fn an_earlier_confirmation_in_the_same_qq_second_claims_its_message() {
        let (root, context) = root("her-recorded-claim-a-");
        let (ob, gateway) = history_gateway(json!([same_text(11, 1000)]));
        let history = History {
            ob: &ob,
            kind: "group",
            id_s: "1",
        };
        let a = same_text_intent(&context, clock_a, 1_000_000);
        a.attempt(0, None).unwrap();
        let first = a.settle(
            0,
            1_000_000,
            Ok(json!({"message_id": 11})),
            &history,
            Proof::Text("same"),
            &[],
        );
        assert_eq!(
            first.state,
            ItemState::Confirmed {
                message_id: Some(11),
                verified: false
            }
        );
        drop(a);
        let b = same_text_intent(&context, clock_b, 1_000_900);
        let at = b.attempt(0, None).unwrap();
        let second = b.settle(
            0,
            at,
            Err(SendError::Unknown("lost".into())),
            &history,
            Proof::Text("same"),
            &[],
        );
        let paths = gateway.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            second.state,
            ItemState::Unknown,
            "batch A already claims message 11: {paths:?}"
        );
    }

    #[test]
    fn a_confirmation_without_a_message_id_leaves_a_later_unknown_unproven() {
        let (root, context) = root("her-recorded-claim-b-");
        let (ob, gateway) = history_gateway(json!([same_text(11, 1000)]));
        let history = History {
            ob: &ob,
            kind: "group",
            id_s: "1",
        };
        let a = same_text_intent(&context, clock_a, 1_000_000);
        a.attempt(0, None).unwrap();
        let first = a.settle(
            0,
            1_000_000,
            Ok(json!({})),
            &history,
            Proof::Text("same"),
            &[],
        );
        assert_eq!(
            first.state,
            ItemState::Confirmed {
                message_id: None,
                verified: false
            }
        );
        drop(a);
        let b = same_text_intent(&context, clock_b, 1_000_900);
        let at = b.attempt(0, None).unwrap();
        let second = b.settle(
            0,
            at,
            Err(SendError::Unknown("lost".into())),
            &history,
            Proof::Text("same"),
            &[],
        );
        let paths = gateway.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            second.state,
            ItemState::Unknown,
            "an unidentified confirmation cannot exclude its message: {paths:?}"
        );
    }

    #[test]
    fn two_own_matching_messages_after_the_attempt_prove_nothing() {
        let (root, context) = root("her-recorded-several-");
        let (ob, gateway) = history_gateway(json!([same_text(21, 1000), same_text(22, 1000)]));
        let history = History {
            ob: &ob,
            kind: "group",
            id_s: "1",
        };
        let b = same_text_intent(&context, clock_b, 1_000_900);
        let at = b.attempt(0, None).unwrap();
        let settled = b.settle(
            0,
            at,
            Err(SendError::Unknown("lost".into())),
            &history,
            Proof::Text("same"),
            &[],
        );
        let paths = gateway.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            settled.state,
            ItemState::Unknown,
            "two candidates cannot say which one is ours: {paths:?}"
        );
    }

    #[test]
    fn an_unreadable_outbox_proves_nothing() {
        let (root, context) = root("her-recorded-unreadable-");
        let (ob, gateway) = history_gateway(json!([same_text(11, 1000)]));
        let history = History {
            ob: &ob,
            kind: "group",
            id_s: "1",
        };
        std::fs::write(
            root.join("data")
                .join(crate::send_outbox::ISOLATED_OUTBOX_FILE),
            "not json\n",
        )
        .unwrap();
        let b = same_text_intent(&context, clock_b, 1_000_900);
        let at = b.attempt(0, None).unwrap();
        let settled = b.settle(
            0,
            at,
            Err(SendError::Unknown("lost".into())),
            &history,
            Proof::Text("same"),
            &[],
        );
        let paths = gateway.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            settled.state,
            ItemState::Unknown,
            "the exclusion list cannot be read, so nothing is proven: {paths:?}"
        );
    }
}
