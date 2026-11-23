//! The turn-engine seam: run one admitted turn — admit, stream, cancel, report facts.
//!
//! The pipeline owns admission authority (turn ledger, agent token, mode and permission checks),
//! prompt composition and delivery. An adapter runs the model work of one admitted turn: it submits
//! the prompt, streams the turn, stops when the turn is cancelled and reports the turn's facts
//! (`Facts`), from which the pipeline writes the turn timing record.
//!
//! Adapters: DSH (`Adapter::Dsh`: DSH sessions, model failover and the follow-stream collector in
//! `pipeline/dsh_adapter.rs`), the harness (`Adapter::Harness`, `harness.rs`) and, in tests, the
//! scripted adapter (`Script`). `pipeline.rs` still sets up the DSH client (`Rpc`) and the other
//! DSH fields in `Pipeline::build` until #13 removes DSH. Retiring DSH deletes `Engine::Dsh`,
//! `Adapter::Dsh`, the `pipeline::dsh_adapter` module, its registration points in `pipeline.rs`
//! and that setup.

use crate::runtime::RuntimeContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The turn engine selected by the scoped setting, reserved2 CLI pin and global default.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Dsh,
    Harness,
}

impl Engine {
    pub fn selected(context: &RuntimeContext, cfg: &Value, conversation: &str, mode: &str) -> Result<Self, String> {
        if !context.is_isolated() {
            return Ok(Self::Dsh);
        }
        if cfg.get("conversationEngines").is_some() {
            crate::social_state::import_engine_choices(context)?;
        }
        Self::read_choice(context, cfg, conversation, mode)
    }

    pub(crate) fn read_choice(context: &RuntimeContext, cfg: &Value, conversation: &str, mode: &str) -> Result<Self, String> {
        let inherit_default = mode != "reserved2" || crate::pipeline::pinned_cli_backend(cfg, conversation).is_none();
        Self::parse(&crate::settings::engine_choice(context, cfg, conversation, inherit_default)?)
    }

    pub(crate) fn parse(value: &Value) -> Result<Self, String> {
        match value.as_str() {
            Some("dsh") => Ok(Self::Dsh),
            Some("harness") => Ok(Self::Harness),
            _ => Err("unknown conversation engine selection".into()),
        }
    }

    pub(crate) fn supports(self, mode: &str) -> bool {
        self == Self::Dsh || matches!(mode, "chat" | "closed-agent" | "reserved2")
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Dsh => "dsh",
            Self::Harness => "harness",
        }
    }

    /// Its turns need DSH: the outage queue, the login, a session mapping and DSH presets.
    pub fn uses_dsh(self) -> bool {
        matches!(self, Self::Dsh)
    }
}

/// Refusal of a participation mode the harness does not run yet.
pub(crate) const UNSUPPORTED_MODE: &str =
    "harness supports chat, owner-private closed-agent and reserved2 modes only";

pub(crate) const ADMISSION_CHANGED: &str = "turn engine changed before admission";

/// The adapter that runs one admitted turn.
#[derive(Clone)]
pub(crate) enum Adapter {
    Dsh,
    Harness,
    #[cfg(test)]
    Scripted(Script),
}

impl Adapter {
    /// The adapter of the engine selected for `conversation`.
    pub(crate) fn for_engine(engine: Engine) -> Self {
        match engine {
            Engine::Dsh => Self::Dsh,
            Engine::Harness => Self::Harness,
        }
    }

    /// The turn ledger's `backend` of its turns.
    pub(crate) fn id(&self) -> &'static str {
        match self {
            Self::Dsh => Engine::Dsh.id(),
            Self::Harness => Engine::Harness.id(),
            #[cfg(test)]
            Self::Scripted(_) => "scripted",
        }
    }

    /// The scripted adapter also supports all participation modes.
    pub(crate) fn supports(&self, mode: &str) -> bool {
        match self {
            Self::Harness => Engine::Harness.supports(mode),
            _ => true,
        }
    }

    /// Its turns need DSH (`Engine::uses_dsh`).
    pub(crate) fn uses_dsh(&self) -> bool {
        matches!(self, Self::Dsh)
    }
}

/// What one admitted turn reported when it ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Facts {
    /// The turn ended without an engine, model or cancellation failure.
    pub ok: bool,
    /// `completed`, `error`, `aborted`, `cancelled` or `command`; `None` when no end was observed.
    pub end_kind: Option<String>,
    /// Why the turn was cancelled (`docs/agents/sessions-recovery.md`); `None` when it was not.
    pub cancel_reason: Option<&'static str>,
    /// The ledger attempt id of the turn's last submission (`attemptRpcId`).
    pub attempt: String,
    /// `provider/model` that ran the turn.
    pub model: Option<String>,
    /// Ledger attempts of the turn.
    pub attempts: usize,
    pub prompt_chars: usize,
    /// Engine-specific phases in milliseconds, in record order (DSH: `cliMs`, `sessMs`,
    /// `modelMs`, `openMs`, `acceptMs`).
    pub segments: Vec<(&'static str, i64)>,
    /// Milliseconds the model worked on the turn.
    pub turn_ms: i64,
    /// Milliseconds of the whole adapter run.
    pub total_ms: i64,
}

impl Facts {
    /// The facts of a harness turn that ended with `status`. `invalidated`: the pipeline cancelled
    /// it because the conversation's mode, permission or token changed. Harness-native cancel
    /// reasons: `harnessCancelled` (its generation was revoked or reset) and `harnessDeadline`
    /// (the turn budget ran out).
    pub(crate) fn of_harness(
        status: &crate::harness::Status,
        invalidated: bool,
        attempt: String,
        model: String,
        prompt_chars: usize,
        turn_ms: i64,
        total_ms: i64,
    ) -> Self {
        use crate::harness::Status;
        let (end_kind, cancel_reason) = match status {
            Status::Completed(_) | Status::ObservationSleep => (Some("completed"), None),
            Status::Cancelled if invalidated => (Some("cancelled"), Some("turnInvalidated")),
            Status::Cancelled => (Some("cancelled"), Some("harnessCancelled")),
            Status::Failed(reason) if reason == crate::harness::DEADLINE_EXCEEDED => {
                (None, Some("harnessDeadline"))
            }
            Status::Failed(_) => (Some("error"), None),
            Status::Running | Status::Paused { .. } => (None, None),
        };
        Self {
            ok: matches!(status, Status::Completed(_) | Status::ObservationSleep),
            end_kind: end_kind.map(str::to_string),
            cancel_reason,
            attempt,
            model: Some(model),
            attempts: 1,
            prompt_chars,
            segments: Vec::new(),
            turn_ms,
            total_ms,
        }
    }

    /// The `logs/turn-timing.jsonl` record of these facts. `turn` is the bridge turn id (none
    /// without a ledger turn). Field order is the record's: segments sit before `turnMs`.
    pub(crate) fn timing_record(&self, key: &str, turn: Option<&str>, ts: i64) -> Value {
        let mut record = serde_json::json!({
            "ts": ts, "key": key, "ok": self.ok, "endKind": self.end_kind,
            "cancelled": self.cancel_reason.is_some(), "cancelReason": self.cancel_reason,
            "turnId": turn, "attemptRpcId": self.attempt, "model": self.model,
            "attempts": self.attempts, "promptChars": self.prompt_chars,
        });
        for (name, ms) in &self.segments {
            record[*name] = serde_json::json!(ms);
        }
        record["turnMs"] = serde_json::json!(self.turn_ms);
        record["totalMs"] = serde_json::json!(self.total_ms);
        record
    }
}

/// The scripted test adapter: every admitted turn answers `reply` and records what it was asked.
/// Limit: it never observes cancellation or the turn deadline, so its facts never name a cancel reason.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct Script {
    pub reply: Result<String, String>,
    /// The end kind it reports for a reply (`completed`, `aborted`, ...).
    pub end_kind: &'static str,
    /// A send tool of the turn finished successfully.
    pub send_tool_ok: bool,
    /// (conversation, prompt) of each turn it ran.
    pub seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

#[cfg(test)]
impl Script {
    pub(crate) fn answering(text: &str) -> Self {
        Self {
            reply: Ok(text.to_string()),
            end_kind: "completed",
            send_tool_ok: false,
            seen: Default::default(),
        }
    }

    pub(crate) fn seen(&self) -> Vec<(String, String)> {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Status;
    use serde_json::json;

    #[test]
    fn seam_engine_selection_and_adapter_capabilities() {
        assert_eq!(Engine::parse(&json!("harness")), Ok(Engine::Harness));
        assert_eq!(Engine::parse(&json!("dsh")), Ok(Engine::Dsh));
        assert!(Engine::parse(&json!("other")).is_err(), "unknown selections fail closed");
        assert!(Engine::Dsh.uses_dsh() && !Engine::Harness.uses_dsh(), "only DSH turns need DSH");
        let harness = Adapter::for_engine(Engine::Harness);
        let dsh = Adapter::for_engine(Engine::Dsh);
        assert_eq!((dsh.id(), harness.id()), ("dsh", "harness"));
        for mode in ["chat", "closed-agent", "reserved", "reserved2"] {
            assert!(dsh.supports(mode), "DSH runs {mode}");
        }
        assert!(harness.supports("chat") && harness.supports("closed-agent"));
        assert!(
            !harness.supports("reserved") && harness.supports("reserved2"),
            "the harness runs second-generation simulation and refuses first-generation reserved"
        );
        assert!(dsh.uses_dsh() && !harness.uses_dsh(), "only the DSH adapter needs DSH");
        let scripted = Adapter::Scripted(Script::answering("ok"));
        assert_eq!(scripted.id(), "scripted");
        assert!(!scripted.uses_dsh(), "the scripted adapter never touches DSH");
        assert!(
            ["chat", "closed-agent", "reserved", "reserved2"].iter().all(|mode| scripted.supports(mode)),
            "the scripted adapter runs every mode"
        );
    }

    #[test]
    fn seam_dsh_facts_keep_the_collector_timing_record() {
        let facts = Facts {
            ok: true,
            end_kind: Some("completed".into()),
            cancel_reason: None,
            attempt: "rpc-1".into(),
            model: Some("first/model-a".into()),
            attempts: 2,
            prompt_chars: 12,
            segments: vec![("cliMs", 1), ("sessMs", 2), ("modelMs", 3), ("openMs", 4), ("acceptMs", 5)],
            turn_ms: 6,
            total_ms: 21,
        };
        assert_eq!(
            facts.timing_record("group:42", Some("turn-1"), 1700).to_string(),
            r#"{"ts":1700,"key":"group:42","ok":true,"endKind":"completed","cancelled":false,"cancelReason":null,"turnId":"turn-1","attemptRpcId":"rpc-1","model":"first/model-a","attempts":2,"promptChars":12,"cliMs":1,"sessMs":2,"modelMs":3,"openMs":4,"acceptMs":5,"turnMs":6,"totalMs":21}"#,
            "the DSH record keeps its fields and their order"
        );
        let cancelled = Facts { ok: false, end_kind: None, cancel_reason: Some("collectorTimeout"), ..facts };
        let record = cancelled.timing_record("group:42", None, 1701);
        assert_eq!(
            (record["cancelled"].clone(), record["cancelReason"].clone(), record["endKind"].clone(), record["turnId"].clone()),
            (json!(true), json!("collectorTimeout"), Value::Null, Value::Null),
            "a cancelled DSH turn without a ledger turn"
        );
    }

    #[test]
    fn seam_harness_facts_name_how_the_turn_ended() {
        let facts = |status: &Status, invalidated: bool| {
            Facts::of_harness(status, invalidated, "harness:turn-1".into(), "openai/fake-chat".into(), 5, 3, 4)
        };
        let completed = facts(&Status::Completed("hi".into()), false);
        assert_eq!(
            (completed.ok, completed.end_kind.as_deref(), completed.cancel_reason),
            (true, Some("completed"), None),
            "a completed harness turn"
        );
        assert_eq!(
            (completed.attempt.as_str(), completed.model.as_deref(), completed.attempts, completed.prompt_chars),
            ("harness:turn-1", Some("openai/fake-chat"), 1, 5)
        );
        assert_eq!((completed.turn_ms, completed.total_ms), (3, 4));
        assert!(completed.segments.is_empty(), "harness records carry no DSH phases");
        for (status, invalidated, end_kind, reason) in [
            (Status::Cancelled, true, Some("cancelled"), Some("turnInvalidated")),
            (Status::Cancelled, false, Some("cancelled"), Some("harnessCancelled")),
            (Status::Failed(crate::harness::DEADLINE_EXCEEDED.into()), false, None, Some("harnessDeadline")),
            (Status::Failed("model/backend failure".into()), false, Some("error"), None),
            (Status::Paused { reason: "send-unresolved".into(), detail: "unknown delivery".into() }, false, None, None),
        ] {
            let ended = facts(&status, invalidated);
            assert_eq!(
                (ended.ok, ended.end_kind.as_deref(), ended.cancel_reason),
                (false, end_kind, reason),
                "{status:?} invalidated={invalidated}"
            );
        }
        let record = facts(&Status::Cancelled, false).timing_record("private:8808", Some("turn-1"), 9);
        assert_eq!(
            (record["cancelled"].clone(), record["cancelReason"].clone()),
            (json!(true), json!("harnessCancelled"))
        );
    }
}
