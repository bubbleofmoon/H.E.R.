//! Storage and transaction boundary for the v2 social state.

use crate::runtime::{runtime_io_error, PathIntent, RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) const FILE: &str = "social-v2.json";
const LEGACY_FILE: &str = crate::legacy_path!("qq-bridge/state/social-v2.json");
pub(crate) const SOCIAL_STATE_GATE: &str = "locks/social-v2.lock";
pub(crate) const SOCIAL_STATE_GATE_WAIT: Duration = Duration::from_secs(5);
static LEGACY_GATE: Mutex<()> = Mutex::new(());

type Failure = (u16, String);

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
        "agentToken": crate::console::fresh_agent_token(), "bootstrapSent": false, "wakeTimes": [], "sendTimes": [],
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
