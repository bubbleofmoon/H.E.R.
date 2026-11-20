//! Storage and transaction boundary for the conversation state.

use crate::runtime::{runtime_io_error, PathIntent, RuntimeArea, RuntimeContext};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) const FILE: &str = "social-v2.json";
pub(crate) const RESERVED_FILE: &str = "social-v1.json";
const LEGACY_FILE: &str = crate::legacy_path!("qq-bridge/state/social-v2.json");
const AGENT_TOKEN: &str = "agentToken";
pub(crate) const SOCIAL_STATE_GATE: &str = "locks/social-v2.lock";
pub(crate) const SOCIAL_STATE_GATE_WAIT: Duration = Duration::from_secs(5);
static LEGACY_GATE: Mutex<()> = Mutex::new(());

type Failure = (u16, String);

/// Read the token field, including the empty strings some older records hold.
pub(crate) fn agent_token(record: &Value) -> Option<&str> {
    record.get(AGENT_TOKEN).and_then(Value::as_str)
}

pub(crate) fn active_agent_token(record: &Value) -> Option<&str> {
    agent_token(record).filter(|token| !token.is_empty())
}

pub(crate) fn active_agent_tokens(state: &Value) -> impl Iterator<Item = &str> {
    state["conversations"]
        .as_object()
        .into_iter()
        .flat_map(|records| records.values())
        .filter_map(active_agent_token)
}

pub(crate) fn matches_agent_token(record: &Value, presented: &str) -> bool {
    active_agent_token(record).is_some_and(|expected| {
        expected.len() == presented.len()
            && expected
                .bytes()
                .zip(presented.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    })
}

/// Some handovers initialize an entire record only when the token field is absent or null.
pub(crate) fn agent_token_is_null(record: &Value) -> bool {
    record[AGENT_TOKEN].is_null()
}

/// Idle session rotation compares the exact stored field, including any malformed value.
pub(crate) fn same_stored_agent_token(before: &Value, after: &Value) -> bool {
    before[AGENT_TOKEN] == after[AGENT_TOKEN]
}

/// Non-reversible identity stored in turn and send records.
pub(crate) fn token_epoch(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest[..7].iter().map(|b| format!("{b:02x}")).collect() // RED-M5
}

/// A fresh agent token from two OS-seeded ids.
fn fresh_agent_token() -> String {
    format!(
        "{}{}",
        crate::turn_ledger::mint_id(""),
        crate::turn_ledger::mint_id("")
    )
}

/// Rotate a token in a caller's existing social transaction.
pub(crate) fn rotate_agent_token(record: &mut Value) -> String {
    let token = fresh_agent_token();
    record[AGENT_TOKEN] = json!(&token);
    token
}

pub(crate) fn rotate_all_agent_tokens(state: &mut Value) {
    if let Some(records) = state["conversations"].as_object_mut() {
        for record in records.values_mut() {
            rotate_agent_token(record);
        }
    }
}

pub(crate) fn read_conversation(
    context: &RuntimeContext,
    key: &str,
) -> Result<Option<Value>, String> {
    let state = read_console_document(context).map_err(|(_, message)| message)?;
    Ok(state
        .get("conversations")
        .and_then(|records| records.get(key))
        .cloned())
}

/// Read the active token using the console's selected and legacy read policies.
pub(crate) fn read_agent_token(
    context: &RuntimeContext,
    key: &str,
) -> Result<Option<String>, String> {
    let state = read_console_document(context).map_err(|(_, message)| message)?;
    Ok(active_agent_token(&state["conversations"][key]).map(str::to_string))
}

/// Compare and rotate under the same gate. Return the replacement only when the token matched.
pub(crate) fn revoke_agent_token_if_current(
    context: &RuntimeContext,
    key: &str,
    expected: &str,
) -> Result<Option<String>, String> {
    update(context, |state| {
        if agent_token(&state["conversations"][key]) != Some(expected) {
            return Ok(None);
        }
        Ok(Some(rotate_agent_token(&mut state["conversations"][key])))
    })
    .map_err(|(_, message)| message)
}

/// Return the token now in force, initializing a missing record or rotating when requested.
pub(crate) fn ensure_or_rotate_agent_token(
    context: &RuntimeContext,
    key: &str,
    rotate: bool,
) -> Result<String, String> {
    update(context, |state| {
        let invalid = || (500, "runtime data is invalid".to_string());
        let records = state
            .as_object_mut()
            .ok_or_else(invalid)?
            .entry("conversations")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(invalid)?;
        let record = records
            .entry(key)
            .or_insert_with(|| new_conversation(context));
        if !record.is_object() {
            return Err(invalid());
        }
        match active_agent_token(record) {
            Some(token) if !rotate => Ok(token.to_string()),
            _ => Ok(rotate_agent_token(record)),
        }
    })
    .map_err(|(_, message)| message)
}

/// First-generation simulation persists its own generation alongside the participation record.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ReservedGeneration(String);

impl ReservedGeneration {
    pub(crate) fn new() -> Self {
        Self(crate::turn_ledger::mint_id("v1-"))
    }

    pub(crate) fn retire(&mut self) {
        self.0 = crate::turn_ledger::mint_id("exit-");
    }
}

pub(crate) fn reserved_document_exists(context: &RuntimeContext) -> Result<bool, String> {
    Ok(context
        .read_optional_text(RuntimeArea::Data, RESERVED_FILE)?
        .is_some())
}

pub(crate) fn read_reserved_document(
    context: &RuntimeContext,
) -> Result<crate::reserved::State, String> {
    match context.read_optional_text(RuntimeArea::Data, RESERVED_FILE)? {
        Some(text) => serde_json::from_str(&text).map_err(|_| "reserved state is invalid".into()),
        None => Ok(crate::reserved::State::default()),
    }
}

/// The reserved gate follows the v2 social gate when a reset holds both.
/// The reserved state is replaced under its own gate.
pub(crate) fn update_reserved_document<T>(
    context: &RuntimeContext,
    update: impl FnOnce(&mut crate::reserved::State) -> Result<T, String>,
) -> Result<T, String> {
    let deadline = Instant::now() + SOCIAL_STATE_GATE_WAIT;
    let _gate = loop {
        match context.open_locked_file(RuntimeArea::Data, "locks/social-v1.lock") {
            Ok(gate) => break gate,
            Err(error)
                if error == "runtime file is already locked" && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("reserved state gate: {error}")),
        }
    };
    let mut state = read_reserved_document(context)?;
    let result = update(&mut state)?;
    let bytes = serde_json::to_vec_pretty(&state).map_err(|error| error.to_string())?;
    context.write_atomic(RuntimeArea::Data, RESERVED_FILE, &bytes)?;
    Ok(result)
}

fn read_text(context: &RuntimeContext) -> Result<String, String> {
    let bytes = if context.is_isolated() {
        let path = context.resolve_path_detailed(RuntimeArea::Data, FILE, PathIntent::ReadFile)?;
        if context.resolve_path_detailed(RuntimeArea::Data, FILE, PathIntent::ReadFile)? != path {
            return Err("path changed class=leaf".into());
        }
        fs::read(path).map_err(|e| runtime_io_error("leaf/read", &e))?
    } else {
        fs::read(LEGACY_FILE).map_err(|e| runtime_io_error("leaf/read", &e))?
    };
    String::from_utf8(bytes).map_err(|_| "kind=InvalidData class=encoding".to_string())
}

pub(crate) fn validate_startup_document(path: &Path) -> Result<(), &'static str> {
    let text = fs::read_to_string(path).map_err(|_| "runtime data is unavailable")?;
    let state: Value = serde_json::from_str(&text).map_err(|_| "runtime data is invalid")?;
    if !state.is_object() {
        return Err("runtime data is invalid");
    }
    Ok(())
}

pub(crate) fn read_document(context: &RuntimeContext) -> Result<Value, String> {
    let text = read_text(context)?;
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "social-state/parse kind=InvalidData line={} column={}",
            e.line(),
            e.column()
        )
    })
}

pub(crate) fn read_document_with_bom(context: &RuntimeContext) -> Option<Value> {
    let text = read_text(context).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

pub(crate) fn read_optional_document(context: &RuntimeContext) -> Result<Value, String> {
    match context.read_optional_text(RuntimeArea::Data, FILE)? {
        Some(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
        None => Ok(empty_document()),
    }
}

pub(crate) fn preflight_reset(context: &RuntimeContext) -> Result<(), Failure> {
    let text = context
        .read_optional_text(RuntimeArea::Data, FILE)
        .ok()
        .flatten()
        .ok_or_else(|| (503, "runtime data is unavailable".to_string()))?;
    let state: Value =
        serde_json::from_str(&text).map_err(|_| (503, "runtime data is invalid".to_string()))?;
    if !state.is_object()
        || state
            .get("conversations")
            .is_some_and(|records| !records.is_object())
    {
        return Err((503, "runtime data is invalid".to_string()));
    }
    Ok(())
}

/// A fresh v2 conversation record with default wake configuration, empty message windows and a
/// new agent token.
pub(crate) fn new_conversation(context: &RuntimeContext) -> Value {
    let cfg = context.load_config().unwrap_or_else(|_| json!({}));
    let random = u64::from_str_radix(&crate::turn_ledger::mint_id("")[..8], 16).unwrap_or(0) as f64
        / 4_294_967_296.0;
    json!({
        "wakeConfig": crate::wake::default_wake_config(&cfg, crate::turn_ledger::now_ms(), random),
        "recentMessages": [], "unread": [], "lastWakeReason": "", "lastAiReplyAt": 0, "lastActionAt": 0,
        (AGENT_TOKEN): fresh_agent_token(), "bootstrapSent": false, "wakeTimes": [], "sendTimes": [],
    })
}

fn empty_document() -> Value {
    json!({"conversations": {}})
}

pub(crate) fn read_legacy_document(context: &RuntimeContext) -> Value {
    debug_assert!(!context.is_isolated());
    read_document(context).unwrap_or_else(|_| empty_document())
}

pub(crate) fn read_console_document(context: &RuntimeContext) -> Result<Value, Failure> {
    let mut state = if context.is_isolated() {
        let state = read_document(context).map_err(|e| (500, format!("social-state/read: {e}")))?;
        if !state.is_object() {
            return Err((500, "runtime data is invalid".to_string()));
        }
        state
    } else {
        read_legacy_document(context)
    };
    strip_receipts(&mut state);
    Ok(state)
}

fn strip_receipts(state: &mut Value) {
    if let Some(conversations) = state
        .get_mut("conversations")
        .and_then(Value::as_object_mut)
    {
        for conversation in conversations.values_mut().filter_map(Value::as_object_mut) {
            conversation.remove("modelSeenSeqs");
        }
    }
}

enum Lock {
    Selected { _file: File },
    Legacy { _guard: MutexGuard<'static, ()> },
}

/// A social transaction for one runtime root. The interaction gate, when needed, precedes
/// this gate. Selected atomic replacement takes the authority gate while this guard is held.
pub(crate) struct Gate<'a> {
    context: &'a RuntimeContext,
    _lock: Lock,
}

impl std::fmt::Debug for Gate<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Gate").finish_non_exhaustive()
    }
}

impl Gate<'_> {
    pub(crate) fn read(&self) -> Result<Value, Failure> {
        read_console_document(self.context)
    }

    pub(crate) fn commit(&self, state: &Value) -> Result<(), String> {
        let mut state = state.clone();
        strip_receipts(&mut state);
        if self.context.is_isolated() {
            let text =
                serde_json::to_string_pretty(&state).map_err(|e| format!("序列化失败：{e}"))?;
            self.context
                .write_atomic(RuntimeArea::Data, FILE, text.as_bytes())
        } else {
            crate::console::write_json_atomic(LEGACY_FILE, &state)
        }
    }
}

#[cfg(test)]
thread_local! {
    static ON_GATE_CONTENTION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Keep waiting through transient contention. A dead owner releases its OS lock, and a live
/// writer that still holds it past the deadline leaves this caller's state unchanged.
fn acquire_social_state_lock(context: &RuntimeContext) -> Result<File, Failure> {
    let deadline = Instant::now() + SOCIAL_STATE_GATE_WAIT;
    loop {
        match context.open_locked_file(RuntimeArea::Data, SOCIAL_STATE_GATE) {
            Ok(file) => return Ok(file),
            Err(error) if error == "runtime file is already locked" => {
                #[cfg(test)]
                ON_GATE_CONTENTION.with(|hook| {
                    if let Some(notify) = hook.take() {
                        notify();
                    }
                });
                if Instant::now() >= deadline {
                    return Err((503, "selected runtime social state is busy".to_string()));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err((503, format!("social-state/gate: {error}"))),
        }
    }
}

pub(crate) fn acquire(context: &RuntimeContext) -> Result<Gate<'_>, Failure> {
    let file = acquire_social_state_lock(context)?;
    Ok(Gate {
        context,
        _lock: Lock::Selected { _file: file },
    })
}

pub(crate) fn acquire_legacy_gate(context: &RuntimeContext) -> Gate<'_> {
    assert!(!context.is_isolated());
    let guard = LEGACY_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Gate {
        context,
        _lock: Lock::Legacy { _guard: guard },
    }
}

/// Legacy reset reads and writes the raw document, including receipts on other records.
pub(crate) fn update_legacy_document(
    context: &RuntimeContext,
    update: impl FnOnce(&mut Value),
) -> Result<(), String> {
    let _gate = acquire_legacy_gate(context);
    let mut state = read_legacy_document(context);
    update(&mut state);
    crate::console::write_json_atomic(LEGACY_FILE, &state)
}

pub(crate) fn update_selected<T>(
    context: &RuntimeContext,
    update: impl FnOnce(&mut Value) -> Result<T, Failure>,
) -> Result<(Value, T), Failure> {
    if !context.is_isolated() {
        return Err((500, "selected runtime is required".to_string()));
    }
    let (gate, mut state) = (acquire(context)?, read_console_document(context)?);
    let result = update(&mut state)?;
    gate.commit(&state)
        .map_err(|error| (500, format!("social-state/write: {error}")))?;
    Ok((state, result))
}

pub(crate) fn update<T>(
    context: &RuntimeContext,
    update: impl FnOnce(&mut Value) -> Result<T, Failure>,
) -> Result<T, Failure> {
    if context.is_isolated() {
        return update_selected(context, update).map(|(_, value)| value);
    }
    let gate = acquire_legacy_gate(context);
    let mut state = gate.read()?;
    let result = update(&mut state)?;
    gate.commit(&state).map_err(|error| (500, error))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tests::runtime_fixture;
    use std::sync::mpsc;
    use std::time::Duration;

    fn token_fixture() -> crate::runtime::tests::FixtureRoot {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.test-fixtures")
            .join(crate::turn_ledger::mint_id("conversation-token-"));
        runtime_fixture(
            root,
            &json!({"consoleToken": "fixture-token"}),
            &json!({
                "dsh": "http://127.0.0.1:1",
                "model": "http://127.0.0.1:2",
                "onebot": "http://127.0.0.1:3"
            }),
        )
    }

    #[test]
    fn token_reads_tolerate_missing_and_malformed_fields() {
        let fixture = token_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        update(&context, |state| {
            state["conversations"] = json!({
                "group:1": {"agentToken": {"old": true}},
                "group:2": {"agentToken": ""},
                "group:3": {"agentToken": "fixture-live-token"},
                "group:4": {"custom": true},
                "group:5": {"agentToken": null},
                "group:6": false
            });
            Ok(())
        })
        .unwrap();
        for key in [
            "group:1", "group:2", "group:4", "group:5", "group:6", "group:7",
        ] {
            assert!(
                read_agent_token(&context, key).unwrap().is_none(),
                "only a nonempty string is an active token"
            );
        }
        assert_eq!(
            read_agent_token(&context, "group:3").unwrap().as_deref(),
            Some("fixture-live-token"),
            "a stored string token remains readable"
        );
    }

    #[test]
    fn token_rotation_preserves_unrelated_state() {
        let fixture = token_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let key = "group:1";
        update(&context, |state| {
            state["custom"] = json!({"keep": true});
            state["conversations"] = json!({
                "group:1": {"agentToken": false, "custom": {"keep": [1, 2]}},
                "group:2": {"custom": "other record"}
            });
            Ok(())
        })
        .unwrap();
        let token = ensure_or_rotate_agent_token(&context, key, false).unwrap();
        assert_eq!(
            ensure_or_rotate_agent_token(&context, key, false).unwrap(),
            token,
            "an existing usable token is kept unless rotation is requested"
        );
        let replacement = revoke_agent_token_if_current(&context, key, &token)
            .unwrap()
            .unwrap();
        assert_ne!(replacement, token);
        assert_eq!(
            read_console_document(&context).unwrap(),
            json!({
                "custom": {"keep": true},
                "conversations": {
                    "group:1": {"agentToken": replacement, "custom": {"keep": [1, 2]}},
                    "group:2": {"custom": "other record"}
                }
            }),
            "token rotation must preserve all unrelated state"
        );
    }

    #[test]
    fn stale_token_cannot_revoke_replacement() {
        let fixture = token_fixture();
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let key = "group:1";
        let token = ensure_or_rotate_agent_token(&context, key, false).unwrap();
        let replacement = ensure_or_rotate_agent_token(&context, key, true).unwrap();
        assert_ne!(replacement, token);
        assert!(
            revoke_agent_token_if_current(&context, key, &token)
                .unwrap()
                .is_none(),
            "an obsolete token must not rotate its replacement"
        );
        assert_eq!(read_agent_token(&context, key).unwrap(), Some(replacement));
    }

    #[test]
    fn token_epoch_keeps_wire_identity() {
        assert_eq!(
            token_epoch("abc"),
            "ba7816bf8f01cfea",
            "the token epoch must keep its stored SHA-256 prefix"
        );
    }

    #[test]
    fn concurrent_updates_preserve_both_changes() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.test-fixtures")
            .join(crate::turn_ledger::mint_id("social-state-"));
        let fixture = runtime_fixture(
            root,
            &json!({"consoleToken": "fixture-token"}),
            &json!({
                "dsh": "http://127.0.0.1:1",
                "model": "http://127.0.0.1:2",
                "onebot": "http://127.0.0.1:3"
            }),
        );
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let key = "group:1";
        update(&context, |state| {
            state["conversations"][key] = json!({"untouched": true});
            Ok(())
        })
        .unwrap();

        let (first_open, wait_for_first) = mpsc::channel();
        let (second_waiting, wait_for_second) = mpsc::channel();
        let (release_first, wait_for_release) = mpsc::channel();
        let timeout = Duration::from_secs(4);
        let (waited, first_result, second_result) = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                update(&context, move |state| {
                    state["conversations"][key]["first"] = json!(1);
                    first_open.send(()).unwrap();
                    wait_for_release.recv_timeout(timeout).unwrap();
                    Ok(())
                })
            });
            wait_for_first.recv_timeout(timeout).unwrap();
            let second = scope.spawn(|| {
                ON_GATE_CONTENTION.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || second_waiting.send(()).unwrap()));
                });
                update(&context, |state| {
                    state["conversations"][key]["second"] = json!(2);
                    Ok(())
                })
            });
            let waited = wait_for_second.recv_timeout(timeout);
            let _ = release_first.send(());
            (waited, first.join(), second.join())
        });
        waited.expect("the second updater reached the held social gate");
        first_result.unwrap().unwrap();
        second_result.unwrap().unwrap();
        assert_eq!(
            read_console_document(&context).unwrap()["conversations"][key],
            json!({"untouched": true, "first": 1, "second": 2}),
            "both updates must survive contention"
        );
    }
}
