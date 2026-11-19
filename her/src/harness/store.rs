//! Selected-runtime conversation facts. Only route labels are persisted.
use super::{Event, Status, TurnRequest};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use genai::chat::{ChatMessage, ContentPart, ToolCall};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde_json::{json, Value};
use std::sync::Mutex;

const DB: &str = "conversations.db";
const STORAGE_ERROR: &str = "harness conversation storage is unavailable";

/// One completed turn's replayed facts, in order, and the event sequence of its last fact.
#[derive(Clone, Debug)]
pub(crate) struct Round {
    pub through: i64,
    pub messages: Vec<ChatMessage>,
}

/// What an admission replays (`Store::admit`): the newest checkpoint's summary and the completed turns after it.
#[derive(Clone, Debug)]
pub(crate) struct History {
    pub session: String,
    /// The summary the session's newest checkpoint carries (`None`: no checkpoint, or one without a summary).
    pub summary: Option<String>,
    /// The completed turns after that checkpoint, oldest first.
    pub rounds: Vec<Round>,
    /// When the session's last turn ended (ms): the idle start of the idle rotation check. `None`: none ended yet.
    pub idle_from: Option<i64>,
}

impl History {
    /// The replayed messages: the summary (`handover::summary_message`), then each round's facts.
    pub(crate) fn messages(&self) -> Vec<ChatMessage> {
        let mut messages: Vec<ChatMessage> =
            self.summary.as_deref().map(super::handover::summary_message).into_iter().collect();
        for round in &self.rounds {
            messages.extend(round.messages.iter().cloned());
        }
        messages
    }
}

pub(crate) struct Store {
    context: RuntimeContext,
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(context: &RuntimeContext) -> Result<Self, String> {
        // Validate SQLite's sidecars through the same containment boundary as the database.
        for name in [
            DB,
            "conversations.db-wal",
            "conversations.db-shm",
            "conversations.db-journal",
        ] {
            context
                .resolve_path(RuntimeArea::Data, name, PathIntent::WriteFile)
                .map_err(|_| STORAGE_ERROR)?;
        }
        let path = context
            .resolve_path(RuntimeArea::Data, DB, PathIntent::WriteFile)
            .map_err(|_| STORAGE_ERROR)?;
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| STORAGE_ERROR)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            .map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let version: i64 = tx
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|_| STORAGE_ERROR)?;
        if version == 0 {
            tx.execute_batch("
                CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, conversation TEXT NOT NULL, generation TEXT NOT NULL,
                    UNIQUE(conversation,generation));
                CREATE TABLE turns (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                    conversation TEXT NOT NULL, profile TEXT NOT NULL, state TEXT NOT NULL,
                    begun_ms INTEGER NOT NULL, ended_ms INTEGER, result TEXT NOT NULL DEFAULT '');
                CREATE UNIQUE INDEX one_active_turn ON turns(conversation) WHERE state='running';
                CREATE TABLE model_attempts (
                    id TEXT PRIMARY KEY, turn_id TEXT NOT NULL REFERENCES turns(id),
                    model TEXT NOT NULL, endpoint TEXT NOT NULL, state TEXT NOT NULL,
                    begun_ms INTEGER NOT NULL, ended_ms INTEGER, response TEXT);
                CREATE TABLE events (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id),
                    turn_id TEXT NOT NULL REFERENCES turns(id), attempt_id TEXT REFERENCES model_attempts(id),
                    kind TEXT NOT NULL, payload TEXT NOT NULL, created_ms INTEGER NOT NULL);
                CREATE INDEX session_events ON events(session_id,seq);
                CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT,'events are append only'); END;
                CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT,'events are append only'); END;
                PRAGMA user_version=1;").map_err(|_| STORAGE_ERROR)?;
        } else if version != 1 && version != 2 {
            return Err("unsupported harness conversation database version".into());
        }
        if version <= 1 {
            tx.execute_batch(
                "CREATE TABLE tool_runs (
                id TEXT PRIMARY KEY, turn_id TEXT NOT NULL REFERENCES turns(id),
                attempt_id TEXT NOT NULL REFERENCES model_attempts(id), call_id TEXT NOT NULL,
                name TEXT NOT NULL, arguments TEXT NOT NULL, state TEXT NOT NULL,
                begun_ms INTEGER NOT NULL, ended_ms INTEGER, result TEXT,
                UNIQUE(attempt_id,call_id)); PRAGMA user_version=2;",
            )
            .map_err(|_| STORAGE_ERROR)?;
        }
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(Self {
            context: context.clone(),
            conn: Mutex::new(conn),
        })
    }

    /// Metadata needed to reconcile a stopped, restored snapshot with the retained ledger.
    pub fn unfinished(&self) -> Result<Vec<(String, String, String, i64)>, String> {
        let conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let mut stmt = conn
            .prepare(
                "SELECT t.id,t.conversation,s.generation,t.begun_ms FROM turns t
            JOIN sessions s ON s.id=t.session_id WHERE t.state='running'",
            )
            .map_err(|_| STORAGE_ERROR)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|_| STORAGE_ERROR)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| STORAGE_ERROR)?;
        Ok(rows)
    }

    /// A restored intent without a definite outcome still needs administrator confirmation.
    pub fn has_unsettled_tools(&self, turn: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tool_runs WHERE turn_id=?1 AND state IN ('running','uncertain'))",
            [turn],
            |r| r.get(0),
        )
        .map_err(|_| STORAGE_ERROR.into())
    }

    pub fn status(&self, turn: &str) -> Result<Option<Status>, String> {
        let conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let row: Option<(String, String)> = conn
            .query_row("SELECT state,result FROM turns WHERE id=?1", [turn], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .map_err(|_| STORAGE_ERROR)?;
        row.map(|(state, result)| match state.as_str() {
            "running" => Ok(Status::Running),
            "completed" => Ok(Status::Completed(result)),
            "failed" => Ok(Status::Failed(result)),
            "cancelled" => Ok(Status::Cancelled),
            "paused" => {
                let pause: Value = serde_json::from_str(&result).map_err(|_| STORAGE_ERROR)?;
                Ok(Status::Paused {
                    reason: pause["reason"].as_str().ok_or(STORAGE_ERROR)?.into(),
                    detail: pause["detail"].as_str().ok_or(STORAGE_ERROR)?.into(),
                })
            }
            _ => Err(STORAGE_ERROR.into()),
        })
        .transpose()
    }

    /// The latest completed canonical wake is the only delivery receipt eligible for replay.
    /// If a checkpoint covers its full briefing, the next wake supplies a fresh base.
    pub(super) fn wake_briefing(&self, conversation: &str, generation: &str) -> Result<Value, String> {
        let conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let current: Option<(String, String)> = conn.query_row(
            "SELECT e.session_id,e.payload FROM events e JOIN turns t ON e.turn_id=t.id
             JOIN sessions s ON s.id=e.session_id
             WHERE s.conversation=?1 AND s.generation=?2 AND e.kind='wake_briefing'
               AND t.state='completed' ORDER BY e.seq DESC LIMIT 1",
            params![conversation, generation], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(|_| STORAGE_ERROR)?;
        let Some((session, payload)) = current else {
            return Ok(crate::briefing_delivery::fresh(conversation, generation));
        };
        let mut record: Value = serde_json::from_str(&payload).map_err(|_| STORAGE_ERROR)?;
        let through = latest_checkpoint(&conn, &session)?.0;
        if through >= record["fullSeq"].as_i64().ok_or(STORAGE_ERROR)? {
            crate::briefing_delivery::history_gap(&mut record, through);
        }
        Ok(record)
    }

    #[cfg(test)]
    pub fn admit(&self, request: &TurnRequest) -> Result<History, String> {
        self.admit_with_briefing(request, None)
    }

    /// Atomic admission and attempt intent, before the model request is allowed to start. Returns what the turn
    /// replays: the session's newest checkpoint summary, the completed turns after it and its idle start.
    pub(super) fn admit_with_briefing(&self, request: &TurnRequest, briefing: Option<Value>) -> Result<History, String> {
        let known = record_credentials(&self.context)?;
        let initial = crate::engine_switch::summary(
            &self.context,
            &request.conversation,
            &request.generation,
            crate::turn_engine::Engine::Harness,
        )?.map(|summary| safe_text(&known, &summary));
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let active: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM turns WHERE conversation=?1 AND state IN ('running','paused'))",
                [&request.conversation],
                |r| r.get(0),
            )
            .map_err(|_| STORAGE_ERROR)?;
        if active {
            return Err("harness conversation has an unfinished turn".into());
        }
        tx.execute("INSERT INTO sessions(id,conversation,generation) VALUES(?1,?2,?3) ON CONFLICT(conversation,generation) DO NOTHING",
            params![crate::turn_ledger::mint_id("harness-"), request.conversation, request.generation]).map_err(|_| STORAGE_ERROR)?;
        let session: String = tx
            .query_row(
                "SELECT id FROM sessions WHERE conversation=?1 AND generation=?2",
                params![request.conversation, request.generation],
                |r| r.get(0),
            )
            .map_err(|_| STORAGE_ERROR)?;
        let (through, summary) = latest_checkpoint(&tx, &session)?;
        let summary = if through == 0 { summary.or(initial) } else { summary };
        let idle_from: Option<i64> = tx
            .query_row("SELECT MAX(ended_ms) FROM turns WHERE session_id=?1", [&session], |r| r.get(0))
            .map_err(|_| STORAGE_ERROR)?;
        let history = {
            let mut stmt = tx.prepare("SELECT e.seq,e.turn_id,e.kind,e.payload FROM events e JOIN turns t ON e.turn_id=t.id
                WHERE e.session_id=?1 AND e.seq>?2 AND t.state='completed' AND e.kind IN ('user','assistant','tool_result') ORDER BY e.seq").map_err(|_| STORAGE_ERROR)?;
            let facts = stmt
                .query_map(params![session, through], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(|_| STORAGE_ERROR)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| STORAGE_ERROR)?;
            // Facts are grouped by turn: one round each, in event order.
            let mut rounds: Vec<(String, Round)> = Vec::new();
            for (seq, turn, kind, payload) in facts {
                let payload: Value = serde_json::from_str(&payload).map_err(|_| STORAGE_ERROR)?;
                let message: ChatMessage = match payload.get("message") {
                    Some(message) => serde_json::from_value(message.clone()).map_err(|_| STORAGE_ERROR)?,
                    // Version 1 events predate tools. Preserve their append-only bytes.
                    None => {
                        let text = payload["text"].as_str().ok_or(STORAGE_ERROR)?.to_string();
                        if kind == "user" {
                            ChatMessage::user(text)
                        } else {
                            ChatMessage::assistant(text).with_reasoning_content(
                                payload["raw"]["captured_reasoning_content"]
                                    .as_str()
                                    .map(str::to_string),
                            )
                        }
                    }
                };
                match rounds.last_mut() {
                    Some((last, round)) if *last == turn => {
                        round.through = seq;
                        round.messages.push(message);
                    }
                    _ => rounds.push((turn, Round { through: seq, messages: vec![message] })),
                }
            }
            History {
                session: session.clone(),
                summary,
                rounds: rounds.into_iter().map(|(_, round)| round).collect(),
                idle_from,
            }
        };
        let now = crate::parity_clock::now_ms();
        tx.execute("INSERT INTO turns(id,session_id,conversation,profile,state,begun_ms) VALUES(?1,?2,?3,?4,'running',?5)",
            params![request.id, session, request.conversation, request.profile, now]).map_err(|_| STORAGE_ERROR)?;
        // The first attempt goes to the turn's start route (`ModelRoute::start`).
        let route = request.route.routes.get(request.route.start).ok_or(STORAGE_ERROR)?;
        let attempt = crate::turn_ledger::mint_id("model-attempt-");
        tx.execute("INSERT INTO model_attempts(id,turn_id,model,endpoint,state,begun_ms) VALUES(?1,?2,?3,?4,'running',?5)",
            params![attempt, request.id, safe_text(&known, &route.model), safe_text(&known, &request.route.endpoint), now]).map_err(|_| STORAGE_ERROR)?;
        append(
            &known,
            &tx,
            &session,
            &request.id,
            None,
            "turn_admitted",
            json!({"generation":request.generation,"profile":request.profile,"tools":request.tools.definitions(),"limits":{"maxSteps":request.limits.max_steps,"maxTokens":request.limits.max_tokens,"imageInput":request.limits.image_input,"toolResultChars":request.limits.tool_result_chars}}),
        )?;
        append(
            &known,
            &tx,
            &session,
            &request.id,
            None,
            "user",
            json!({"text":request.prompt,"message":request.user_message()}),
        )?;
        if let Some(mut record) = briefing {
            // The preceding insert is the canonical user fact. The receipt and fact commit together.
            let seq = tx.last_insert_rowid();
            if record["pending"]["full"] == true {
                record["fullSeq"] = json!(seq);
            }
            if !crate::briefing_delivery::delivered(&mut record, &request.id, seq) {
                return Err(STORAGE_ERROR.into());
            }
            append(&known, &tx, &session, &request.id, None, "wake_briefing", record)?;
        }
        append(
            &known,
            &tx,
            &session,
            &request.id,
            Some(&attempt),
            "model_attempt_started",
            json!({"model":route.model,"endpoint":request.route.endpoint,"route":route.id}),
        )?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(history)
    }

    /// Complete exactly this model attempt, durably retaining its complete canonical call list.
    pub fn complete_step(&self, turn: &str, assistant: &Event) -> Result<String, String> {
        let known = record_credentials(&self.context)?;
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, turn)?;
        let attempt =
            end_attempt(&known, &tx, &session, turn, "completed", Some(assistant))?.ok_or(STORAGE_ERROR)?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(attempt)
    }

    /// Close an incomplete stream before a retry can create another attempt.
    pub fn fail_attempt(&self, turn: &str) -> Result<(), String> {
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, turn)?;
        end_attempt(&[], &tx, &session, turn, "failed", None)?.ok_or(STORAGE_ERROR)?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(())
    }

    /// A new attempt of the running turn on route `route` of its chain (a step, a retry or a failover).
    pub fn next_attempt(&self, request: &TurnRequest, route: usize) -> Result<(), String> {
        let route = request.route.routes.get(route).ok_or(STORAGE_ERROR)?;
        let known = record_credentials(&self.context)?;
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, &request.id)?;
        let attempt = crate::turn_ledger::mint_id("model-attempt-");
        tx.execute("INSERT INTO model_attempts(id,turn_id,model,endpoint,state,begun_ms) VALUES(?1,?2,?3,?4,'running',?5)",
            params![attempt, request.id, safe_text(&known, &route.model), safe_text(&known, &request.route.endpoint), crate::parity_clock::now_ms()]).map_err(|_| STORAGE_ERROR)?;
        append(
            &known,
            &tx,
            &session,
            &request.id,
            Some(&attempt),
            "model_attempt_started",
            json!({"model":route.model,"endpoint":request.route.endpoint,"route":route.id}),
        )?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(())
    }

    /// Intent commits before the blocking handler may be scheduled, including refused calls.
    pub fn tool_start(&self, turn: &str, attempt: &str, call: &ToolCall) -> Result<String, String> {
        let known = record_credentials(&self.context)?;
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, turn)?;
        let id = crate::turn_ledger::mint_id("tool-run-");
        tx.execute("INSERT INTO tool_runs(id,turn_id,attempt_id,call_id,name,arguments,state,begun_ms) VALUES(?1,?2,?3,?4,?5,?6,'running',?7)",
            params![id,turn,attempt,protocol_label(&known, &call.call_id, "her_call_"),safe_text(&known, &call.fn_name),safe_record(&known, &call.fn_arguments).to_string(),crate::parity_clock::now_ms()]).map_err(|_| STORAGE_ERROR)?;
        append(
            &known,
            &tx,
            &session,
            turn,
            Some(attempt),
            "tool_started",
            json!({"id":id,"call":call}),
        )?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(id)
    }

    pub fn tool_result(
        &self,
        turn: &str,
        run: &str,
        message: &ChatMessage,
        outcome: &Value,
    ) -> Result<(), String> {
        let known = record_credentials(&self.context)?;
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, turn)?;
        let attempt: String = tx
            .query_row(
                "SELECT attempt_id FROM tool_runs WHERE id=?1 AND turn_id=?2 AND state='running'",
                params![run, turn],
                |r| r.get(0),
            )
            .map_err(|_| STORAGE_ERROR)?;
        let state = if outcome["_meta"]["herDelivery"] == "unknown" {
            "uncertain"
        } else if outcome["isError"] == true {
            "refused"
        } else {
            "completed"
        };
        tx.execute(
            "UPDATE tool_runs SET state=?2,ended_ms=?3,result=?4 WHERE id=?1",
            params![
                run,
                state,
                crate::parity_clock::now_ms(),
                safe_record(&known, outcome).to_string()
            ],
        )
        .map_err(|_| STORAGE_ERROR)?;
        append(
            &known,
            &tx,
            &session,
            turn,
            Some(&attempt),
            "tool_result",
            json!({"id":run,"state":state,"result":outcome,"message":message}),
        )?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(())
    }

    /// Records the running turn's history step (`harness/handover.rs`) in one transaction: a summary that failed
    /// (`checkpoint_failed`: purpose and failure class), then the session's new checkpoint (`checkpoint`: purpose, the
    /// sequence of the last fact it covers and the summary it carries); either may be absent. Later admissions replay
    /// from the newest checkpoint (`admit`); no fact is rewritten.
    pub fn checkpoint(&self, turn: &str, failed: Option<Value>, checkpoint: Option<Value>) -> Result<(), String> {
        let known = record_credentials(&self.context)?;
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, turn)?;
        if let Some(failed) = failed {
            append(&known, &tx, &session, turn, None, "checkpoint_failed", failed)?;
        }
        if let Some(checkpoint) = checkpoint {
            append(&known, &tx, &session, turn, None, "checkpoint", checkpoint)?;
        }
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(())
    }

    /// Commit the complete assistant fact and terminal states together, before notifying delivery.
    pub fn finish(
        &self,
        turn: &str,
        status: &Status,
        assistant: Option<&Event>,
    ) -> Result<(), String> {
        let pause;
        let (state, result) = match status {
            Status::Running => return Err("cannot finish a running harness turn".into()),
            Status::Completed(text) => ("completed", text.as_str()),
            Status::Failed(reason) => ("failed", reason.as_str()),
            Status::Cancelled => ("cancelled", "cancelled"),
            Status::Paused { reason, detail } => {
                pause = json!({"reason":reason,"detail":detail}).to_string();
                ("paused", pause.as_str())
            }
        };
        let known = record_credentials(&self.context)?;
        let result = safe_text(&known, result);
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session: String = tx
            .query_row(
                "SELECT session_id FROM turns WHERE id=?1 AND state IN ('running','paused')",
                [turn],
                |r| r.get(0),
            )
            .map_err(|_| STORAGE_ERROR)?;
        end_attempt(&known, &tx, &session, turn, state, assistant)?;
        let now = crate::parity_clock::now_ms();
        tx.execute(
            "UPDATE turns SET state=?2,ended_ms=?3,result=?4 WHERE id=?1",
            params![turn, state, now, &result],
        )
        .map_err(|_| STORAGE_ERROR)?;
        append(
            &known,
            &tx,
            &session,
            turn,
            None,
            if state == "paused" {
                "turn_paused"
            } else {
                "turn_ended"
            },
            json!({"state":state,"result":result}),
        )?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(())
    }
}

/// The newest checkpoint of `session` (`Store::checkpoint`): the sequence of the last fact it covers and the summary it
/// carries; `(0, None)` without one.
fn latest_checkpoint(tx: &Connection, session: &str) -> Result<(i64, Option<String>), String> {
    let payload: Option<String> = tx
        .query_row(
            "SELECT payload FROM events WHERE session_id=?1 AND kind='checkpoint' ORDER BY seq DESC LIMIT 1",
            [session],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| STORAGE_ERROR)?;
    let Some(payload) = payload else {
        return Ok((0, None));
    };
    let payload: Value = serde_json::from_str(&payload).map_err(|_| STORAGE_ERROR)?;
    let through = payload["through"].as_i64().ok_or(STORAGE_ERROR)?;
    Ok((through, payload["summary"].as_str().map(str::to_string)))
}

fn running_session(tx: &Transaction<'_>, turn: &str) -> Result<String, String> {
    tx.query_row(
        "SELECT session_id FROM turns WHERE id=?1 AND state='running'",
        [turn],
        |r| r.get(0),
    )
    .map_err(|_| STORAGE_ERROR.into())
}

fn end_attempt(
    known: &[String],
    tx: &Transaction<'_>,
    session: &str,
    turn: &str,
    state: &str,
    assistant: Option<&Event>,
) -> Result<Option<String>, String> {
    let attempt: Option<String> = tx
        .query_row(
            "SELECT id FROM model_attempts WHERE turn_id=?1 AND state='running'",
            [turn],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| STORAGE_ERROR)?;
    let Some(attempt) = attempt else {
        if assistant.is_some() {
            return Err(STORAGE_ERROR.into());
        }
        return Ok(None);
    };
    let raw = match assistant {
        Some(Event::Assistant { text, raw, message }) => {
            append(
                known,
                tx,
                session,
                turn,
                Some(&attempt),
                "assistant",
                json!({"text":text,"raw":raw,"message":message}),
            )?;
            Some(safe_record(known, raw).to_string())
        }
        _ => None,
    };
    tx.execute(
        "UPDATE model_attempts SET state=?2,ended_ms=?3,response=?4 WHERE id=?1 AND turn_id=?5",
        params![attempt, state, crate::parity_clock::now_ms(), raw, turn],
    )
    .map_err(|_| STORAGE_ERROR)?;
    append(
        known,
        tx,
        session,
        turn,
        Some(&attempt),
        "model_attempt_ended",
        json!({"state":state}),
    )?;
    Ok(Some(attempt))
}

// A shared mask cannot be used as a protocol/uniqueness key. Reserved-prefix
// inputs are aliased too, so ordinary IDs cannot collide with generated aliases.
fn protocol_label(known: &[String], label: &str, prefix: &str) -> String {
    if !label.starts_with(prefix) && safe_text(known, label) == label {
        return label.to_string();
    }
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(label.as_bytes())))
}

// Snapshot before taking the conversations connection lock. Both stores can wait on SQLite readers.
pub(super) fn record_credentials(context: &RuntimeContext) -> Result<Vec<String>, String> {
    let mut known = crate::activity::known_session_tokens(context);
    known.extend(crate::settings::harness_credential(context));
    known.extend(crate::settings::stored_secrets(context)?);
    Ok(known)
}

fn safe_text(known: &[String], text: &str) -> String {
    crate::activity::mask_log_text(text, known)
}

fn safe_record(known: &[String], value: &Value) -> Value {
    crate::activity::mask_json_session_tokens(value, known)
}

fn safe_message(known: &[String], mut message: ChatMessage) -> Result<Value, String> {
    for part in message.content.iter_mut() {
        match part {
            ContentPart::Text(text) | ContentPart::ReasoningContent(text) => {
                *text = crate::activity::mask_log_text(text, known);
            }
            ContentPart::ToolCall(call) => {
                call.call_id = protocol_label(known, &call.call_id, "her_call_");
                call.fn_name = protocol_label(known, &call.fn_name, "her_tool_");
                call.fn_arguments = crate::activity::mask_json_session_tokens(&call.fn_arguments, known);
                // Provider signatures must remain valid for canonical replay.
            }
            ContentPart::ToolResponse(response) => {
                response.call_id = protocol_label(known, &response.call_id, "her_call_");
                if let Some(name) = &mut response.fn_name {
                    *name = protocol_label(known, name, "her_tool_");
                }
                response.content = crate::activity::mask_log_text(&response.content, known);
            }
            ContentPart::Binary(binary) => {
                if let Some(name) = &mut binary.name {
                    *name = crate::activity::mask_log_text(name, known);
                }
                // Encoded bytes/URLs and MIME types are operational image data.
            }
            ContentPart::ThoughtSignature(_) => {}
            ContentPart::Custom(custom) => {
                custom.data = crate::activity::mask_json_session_tokens(&custom.data, known);
            }
        }
    }
    serde_json::to_value(message).map_err(|_| STORAGE_ERROR.into())
}

fn append(
    known: &[String],
    tx: &Transaction<'_>,
    session: &str,
    turn: &str,
    attempt: Option<&str>,
    kind: &str,
    payload: Value,
) -> Result<(), String> {
    let mut safe = safe_record(known, &payload);
    if let Some(message) = payload.get("message") {
        let message = serde_json::from_value(message.clone()).map_err(|_| STORAGE_ERROR)?;
        safe["message"] = safe_message(known, message)?;
    }
    tx.execute("INSERT INTO events(session_id,turn_id,attempt_id,kind,payload,created_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![session,turn,attempt,kind,safe.to_string(),crate::parity_clock::now_ms()]).map_err(|_| STORAGE_ERROR)?;
    Ok(())
}

/// HER's completed harness replies in `conversation` after position `after` = (created ms, event
/// sequence), in that order: (created_ms, seq, text). No database means none. Never creates or migrates storage.
pub(crate) fn assistant_texts(context: &RuntimeContext, conversation: &str, after: (i64, i64)) -> Result<Vec<(i64, i64, String)>, String> {
    let path = context.resolve_path(RuntimeArea::Data, DB, PathIntent::ReadOptionalFile).map_err(|_| STORAGE_ERROR)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW)
        .map_err(|_| STORAGE_ERROR)?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|_| STORAGE_ERROR)?;
    let mut stmt = conn.prepare("SELECT e.created_ms, e.seq, e.payload FROM events e JOIN turns t ON e.turn_id = t.id
        WHERE t.conversation = ?1 AND t.state = 'completed' AND e.kind = 'assistant'
        AND (e.created_ms > ?2 OR (e.created_ms = ?2 AND e.seq > ?3)) ORDER BY e.created_ms, e.seq")
        .map_err(|_| STORAGE_ERROR)?;
    let rows = stmt.query_map(params![conversation, after.0, after.1], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))
        .map_err(|_| STORAGE_ERROR)?;
    let mut out = Vec::new();
    for row in rows {
        let (at, seq, payload) = row.map_err(|_| STORAGE_ERROR)?;
        let payload: Value = serde_json::from_str(&payload).map_err(|_| STORAGE_ERROR)?;
        let text = payload["text"].as_str().unwrap_or("").trim();
        if !text.is_empty() {
            out.push((at, seq, text.to_string()));
        }
    }
    Ok(out)
}

fn switch_reader(context: &RuntimeContext) -> Result<Option<Connection>, String> {
    for sidecar in ["conversations.db-wal", "conversations.db-shm", "conversations.db-journal"] {
        context.resolve_path(RuntimeArea::Data, sidecar, PathIntent::ReadOptionalFile).map_err(|_| STORAGE_ERROR)?;
    }
    let path = context.resolve_path(RuntimeArea::Data, DB, PathIntent::ReadOptionalFile).map_err(|_| STORAGE_ERROR)?;
    if !path.exists() { return Ok(None); }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW)
        .map_err(|_| STORAGE_ERROR)?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|_| STORAGE_ERROR)?;
    Ok(Some(conn))
}

pub(crate) fn has_generation(context: &RuntimeContext, key: &str, generation: &str) -> Result<bool, String> {
    let Some(conn) = switch_reader(context)? else { return Ok(false) };
    conn.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE conversation=?1 AND generation=?2)",
        params![key, generation], |row| row.get(0)).map_err(|_| STORAGE_ERROR.to_string())
}

/// Read the latest completed text events in the source generation, with a byte bound before JSON allocation.
pub(crate) fn switch_history(context: &RuntimeContext, key: &str, generation: &str, limit: usize) -> Result<Vec<String>, String> {
    let Some(conn) = switch_reader(context)? else { return Ok(Vec::new()) };
    let mut stmt = conn.prepare("SELECT e.kind, CASE WHEN length(CAST(e.payload AS BLOB)) <= 65536 THEN e.payload END
        FROM sessions s JOIN events e ON e.session_id = s.id JOIN turns t ON e.turn_id = t.id
        WHERE s.conversation = ?1 AND s.generation = ?2 AND t.state = 'completed'
        AND e.kind IN ('user','assistant') ORDER BY e.seq DESC LIMIT ?3").map_err(|_| STORAGE_ERROR)?;
    let rows = stmt.query_map(params![key, generation, limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    }).map_err(|_| STORAGE_ERROR)?;
    let mut result = Vec::new();
    for row in rows {
        let (kind, Some(text)) = row.map_err(|_| STORAGE_ERROR)? else { continue };
        let data: Value = serde_json::from_str(&text).map_err(|_| STORAGE_ERROR)?;
        if let Some(message) = data["text"].as_str() {
            let label = if kind == "user" { "〔对方〕" } else { "〔我〕" };
            result.push(format!("{label}{message}"));
        }
    }
    result.reverse();
    Ok(result)
}

/// The newest model attempt of `conversation`'s session in `generation` that returned a response, ordered by its
/// `model_attempt_started` event (the append-only sequence, not the wall clock): the idle rotation rule's evidence
/// (`harness::rotation`). An older attempt never stands in for a newest one without reported usage. No database,
/// session or answered attempt means `None`. Never creates or migrates storage.
pub(crate) fn last_call(context: &RuntimeContext, conversation: &str, generation: &str) -> Result<Option<super::rotation::LastCall>, String> {
    let path = context.resolve_path(RuntimeArea::Data, DB, PathIntent::ReadOptionalFile).map_err(|_| STORAGE_ERROR)?;
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW)
        .map_err(|_| STORAGE_ERROR)?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|_| STORAGE_ERROR)?;
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT a.model, a.response, e.payload FROM events e JOIN sessions s ON e.session_id = s.id JOIN model_attempts a ON e.attempt_id = a.id
            WHERE s.conversation = ?1 AND s.generation = ?2 AND e.kind = 'model_attempt_started' AND a.response IS NOT NULL
            ORDER BY e.seq DESC LIMIT 1",
            params![conversation, generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|_| STORAGE_ERROR)?;
    let Some((model, response, started)) = row else {
        return Ok(None);
    };
    let response: Value = serde_json::from_str(&response).map_err(|_| STORAGE_ERROR)?;
    let input = response
        .pointer("/captured_usage/prompt_tokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0);
    Ok(Some(super::rotation::LastCall { route: attempt_route(model, &started), input }))
}

/// The route id a `model_attempt_started` payload names. An attempt from before routes names none: its model id was
/// its route id, the one route of `harness.model`.
fn attempt_route(model: String, started: &str) -> String {
    serde_json::from_str::<Value>(started)
        .ok()
        .and_then(|payload| payload["route"].as_str().map(str::to_string))
        .unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{Limits, ModelRoute, Tools};
    use crate::runtime::tests::runtime_fixture;
    use genai::chat::{Binary, BinarySource, ContentPart};

    #[test]
    fn ticket219_core_harness_records_mask_stored_credentials() {
        let root = std::env::temp_dir().join(format!("her-record-credential-{}", crate::turn_ledger::mint_id("")));
        let fixture = runtime_fixture(root, &json!({"consoleToken": "fixture-console"}), &json!({
            "dsh": "http://127.0.0.1:1",
            "model": "http://127.0.0.1:2",
            "onebot": "http://127.0.0.1:3",
        }));
        let root = &fixture.0;
        std::fs::write(root.join("config/harness-api-key"), "fixture-harness-record-key").unwrap();
        let context = RuntimeContext::from_root(root).unwrap();
        let secret = "fixture-endpoint-record-key";
        crate::settings::apply(&context, crate::settings::CLI_MODEL_API_KEY.key, &json!(secret), "fixture", "console").unwrap();
        let known = record_credentials(&context).unwrap();
        let captured = safe_record(&known, &json!({"body": secret}));
        let message = safe_message(&known, ChatMessage::assistant(format!("before {secret} after"))).unwrap();
        assert!(!captured.to_string().contains(secret) && !message.to_string().contains(secret), "harness record exposed a stored credential");
        let route = ModelRoute::for_model(&context, "fixture-model").unwrap();
        assert_eq!(route.redact(&format!("fixture-harness-record-key {secret}")), "[REDACTED] [REDACTED]");
    }

    #[test]
    fn ticket219_core_harness_withholds_payload_when_credentials_are_unreadable() {
        let root = std::env::temp_dir().join(format!("her-record-unreadable-{}", crate::turn_ledger::mint_id("")));
        let fixture = runtime_fixture(root, &json!({"consoleToken": "fixture-console"}), &json!({
            "dsh": "http://127.0.0.1:1",
            "model": "http://127.0.0.1:2",
            "onebot": "http://127.0.0.1:3",
        }));
        let root = &fixture.0;
        let context = RuntimeContext::from_root(root).unwrap();
        let store = Store::open(&context).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch("INSERT INTO sessions(id,conversation,generation) VALUES('fixture','private:81','epoch');\nINSERT INTO turns(id,session_id,conversation,profile,state,begun_ms) VALUES('turn','fixture','private:81','fixture','running',1);").unwrap();
        }
        std::fs::write(root.join("data/settings.db"), b"unreadable fixture").unwrap();
        let result = store.checkpoint("turn", None, Some(json!({"summary":"fixture-unknown-key"})));
        let facts: i64 = store.conn.lock().unwrap().query_row("SELECT count(*) FROM events", [], |row| row.get(0)).unwrap();
        assert!(result.is_err() && facts == 0, "harness wrote a payload without a credential snapshot");
    }

    #[test]
    fn hotfix128_binary_records_preserve_opaque_data_for_replay() {
        let cfg = json!({"ownerQQ":8808,"consoleToken":"synthetic-console-128","harness":{"model":"fake-chat"}});
        let root = std::env::temp_dir().join(format!(
            "her-store-opaque-128-{}",
            crate::turn_ledger::mint_id("")
        ));
        let fixture = runtime_fixture(root, &cfg, &json!({
            "dsh": "http://127.0.0.1:43101",
            "model": "http://127.0.0.1:43102",
            "onebot": "http://127.0.0.1:43103"
        }));
        let root = &fixture.0;
        std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-128").unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
        let context = RuntimeContext::from_root(root).unwrap();
        let token = crate::conversation_authority::ensure_or_rotate(&context, "private:8808", false).unwrap();
        let generation = crate::send_outbox::token_epoch(&token);
        let mut request = TurnRequest {
            id:"opaque-first".into(), conversation:"private:8808".into(), generation:generation.clone(), prompt:"ordinary input".into(), media:Vec::new(), profile:"closed-agent".into(),
            route:ModelRoute::from_runtime(&context, &cfg).unwrap(), deadline:std::time::Duration::from_secs(10),
            tools:Tools::capture(&context, &cfg, "closed-agent", "private:8808", &generation).unwrap(), limits:Limits::from_config(&cfg),
        };
        let store = Store::open(&context).unwrap();
        store.admit(&request).unwrap();
        // A decodable PNG may contain a token-shaped run in its opaque base64 bytes.
        use base64::Engine as _;
        let token_shape = "a".repeat(48);
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgba8(1, 1).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        while bytes.len() % 3 != 0 { bytes.push(0); }
        bytes.extend(base64::engine::general_purpose::STANDARD.decode(format!("zzzz{token_shape}zzzz")).unwrap());
        assert!(image::load_from_memory(&bytes).is_ok());
        let opaque = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert!(opaque.contains(&token_shape));
        let message = ChatMessage::assistant(vec![
            ContentPart::Text(format!("ordinary {token_shape}")),
            ContentPart::Binary(Binary::from_base64("image/png", opaque.clone(), None)),
            ContentPart::ThoughtSignature(opaque.clone()),
        ]);
        let event = Event::Assistant { text:format!("ordinary {token_shape}"), raw:json!({"diagnostic":token_shape}), message };
        store.finish(&request.id, &Status::Completed("done".into()), Some(&event)).unwrap();
        request.id = "opaque-next".into();
        let history = store.admit(&request).unwrap().messages();
        let parts: Vec<_> = history.iter().flat_map(|m| m.content.iter()).collect();
        let binary = parts.iter().find_map(|p| match p { ContentPart::Binary(b) => Some(b), _ => None }).unwrap();
        let BinarySource::Base64(data) = &binary.source else { panic!("binary source changed") };
        assert_eq!(data.as_ref(), opaque, "canonical binary bytes must survive record masking");
        assert!(parts.iter().any(|p| matches!(p, ContentPart::ThoughtSignature(signature) if signature == &opaque)));
        assert!(parts.iter().any(|p| matches!(p, ContentPart::Text(text) if text == "ordinary ***")), "model text is still masked");
    }

    #[test]
    fn harness_idle_rotation_last_call_reads_the_newest_answered_request_of_the_session() {
        use crate::harness::rotation::LastCall;
        let cfg = json!({"ownerQQ":8808,"consoleToken":"synthetic-console-165","harness":{"model":"fake-chat"}});
        let root = std::env::temp_dir().join(format!(
            "her-store-last-call-165-{}",
            crate::turn_ledger::mint_id("")
        ));
        let fixture = runtime_fixture(root, &cfg, &json!({
            "dsh": "http://127.0.0.1:43101",
            "model": "http://127.0.0.1:43102",
            "onebot": "http://127.0.0.1:43103"
        }));
        let root = &fixture.0;
        std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-165").unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
        let context = RuntimeContext::from_root(root).unwrap();
        let token = crate::conversation_authority::ensure_or_rotate(&context, "private:8808", false).unwrap();
        let generation = crate::send_outbox::token_epoch(&token);
        assert_eq!(last_call(&context, "private:8808", &generation).unwrap(), None, "no database: no request");
        assert!(!root.join("data").join(DB).exists(), "reading never creates the database");
        let mut request = TurnRequest {
            id:"last-call-first".into(), conversation:"private:8808".into(), generation:generation.clone(), prompt:"ordinary input".into(), media:Vec::new(), profile:"closed-agent".into(),
            route:ModelRoute::from_runtime(&context, &cfg).unwrap(), deadline:std::time::Duration::from_secs(10),
            tools:Tools::capture(&context, &cfg, "closed-agent", "private:8808", &generation).unwrap(), limits:Limits::from_config(&cfg),
        };
        let answer = |raw: Value| Event::Assistant { text: "answer".into(), raw, message: ChatMessage::assistant("answer".to_string()) };
        let store = Store::open(&context).unwrap();
        store.admit(&request).unwrap();
        store.finish(&request.id, &Status::Completed("answer".into()), Some(&answer(json!({"captured_usage":{"prompt_tokens":600_000,"total_tokens":600_100}})))).unwrap();
        let first = Some(LastCall { route: "fake-chat".into(), input: Some(600_000) });
        assert_eq!(last_call(&context, "private:8808", &generation).unwrap(), first, "the answered request's model and total input");
        request.id = "last-call-cancelled".into();
        store.admit(&request).unwrap();
        store.finish(&request.id, &Status::Cancelled, None).unwrap();
        assert_eq!(last_call(&context, "private:8808", &generation).unwrap(), first, "a request that never answered is not evidence");
        request.id = "last-call-unreported".into();
        store.admit(&request).unwrap();
        store.finish(&request.id, &Status::Completed("answer".into()), Some(&answer(json!({})))).unwrap();
        assert_eq!(
            last_call(&context, "private:8808", &generation).unwrap(),
            Some(LastCall { route: "fake-chat".into(), input: None }),
            "the newest answered request counts even without usage; an older one never stands in"
        );
        assert_eq!(last_call(&context, "private:8808", "other-generation").unwrap(), None, "another generation's session has its own evidence");
        assert_eq!(last_call(&context, "private:9909", &generation).unwrap(), None, "another conversation too");
    }

    #[test]
    fn harness_idle_rotation_last_call_names_the_route_its_attempt_recorded() {
        use crate::harness::rotation::LastCall;
        let cfg = json!({"ownerQQ":8808,"consoleToken":"synthetic-console-7","harness":{"routes":[{"id":"flash","model":"fake-flash"}]}});
        let root = std::env::temp_dir().join(format!(
            "her-store-last-route-7-{}",
            crate::turn_ledger::mint_id("")
        ));
        let fixture = runtime_fixture(root, &cfg, &json!({
            "dsh": "http://127.0.0.1:43101",
            "model": "http://127.0.0.1:43102",
            "onebot": "http://127.0.0.1:43103"
        }));
        let root = &fixture.0;
        std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-7").unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
        let context = RuntimeContext::from_root(root).unwrap();
        let token = crate::conversation_authority::ensure_or_rotate(&context, "private:8808", false).unwrap();
        let generation = crate::send_outbox::token_epoch(&token);
        let request = TurnRequest {
            id:"last-route".into(), conversation:"private:8808".into(), generation:generation.clone(), prompt:"ordinary input".into(), media:Vec::new(), profile:"closed-agent".into(),
            route:ModelRoute::from_runtime(&context, &cfg).unwrap(), deadline:std::time::Duration::from_secs(10),
            tools:Tools::capture(&context, &cfg, "closed-agent", "private:8808", &generation).unwrap(), limits:Limits::from_config(&cfg),
        };
        let store = Store::open(&context).unwrap();
        store.admit(&request).unwrap();
        let answer = Event::Assistant { text: "answer".into(), raw: json!({"captured_usage":{"prompt_tokens":600_000}}), message: ChatMessage::assistant("answer".to_string()) };
        store.finish(&request.id, &Status::Completed("answer".into()), Some(&answer)).unwrap();
        assert_eq!(
            last_call(&context, "private:8808", &generation).unwrap(),
            Some(LastCall { route: "flash".into(), input: Some(600_000) }),
            "rotation tables are keyed by the route id, not the model id"
        );
    }

    #[test]
    fn harness_idle_rotation_an_attempt_without_a_route_names_its_model() {
        assert_eq!(attempt_route("fake-flash".into(), r#"{"model":"fake-flash","endpoint":"x","route":"flash"}"#), "flash");
        assert_eq!(attempt_route("old-model".into(), r#"{"model":"old-model","endpoint":"x"}"#), "old-model", "an attempt from before routes");
        assert_eq!(attempt_route("old-model".into(), r#"{"model":"old-model","route":7}"#), "old-model");
        assert_eq!(attempt_route("m".into(), "not json"), "m");
    }
}
