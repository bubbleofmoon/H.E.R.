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

    /// Atomic admission and attempt intent, before the model request is allowed to start.
    pub fn admit(&self, request: &TurnRequest) -> Result<Vec<ChatMessage>, String> {
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
        let history = {
            let mut stmt = tx.prepare("SELECT e.kind,e.payload FROM events e JOIN turns t ON e.turn_id=t.id
                WHERE e.session_id=?1 AND t.state='completed' AND e.kind IN ('user','assistant','tool_result') ORDER BY e.seq").map_err(|_| STORAGE_ERROR)?;
            let facts = stmt
                .query_map([&session], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|_| STORAGE_ERROR)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| STORAGE_ERROR)?;
            let mut messages = Vec::new();
            for (kind, payload) in facts {
                let payload: Value = serde_json::from_str(&payload).map_err(|_| STORAGE_ERROR)?;
                if let Some(message) = payload.get("message") {
                    messages
                        .push(serde_json::from_value(message.clone()).map_err(|_| STORAGE_ERROR)?);
                    continue;
                }
                // Version 1 events predate tools. Preserve their append-only bytes.
                let text = payload["text"].as_str().ok_or(STORAGE_ERROR)?.to_string();
                messages.push(if kind == "user" {
                    ChatMessage::user(text)
                } else {
                    ChatMessage::assistant(text).with_reasoning_content(
                        payload["raw"]["captured_reasoning_content"]
                            .as_str()
                            .map(str::to_string),
                    )
                });
            }
            messages
        };
        let now = crate::parity_clock::now_ms();
        tx.execute("INSERT INTO turns(id,session_id,conversation,profile,state,begun_ms) VALUES(?1,?2,?3,?4,'running',?5)",
            params![request.id, session, request.conversation, request.profile, now]).map_err(|_| STORAGE_ERROR)?;
        let attempt = crate::turn_ledger::mint_id("model-attempt-");
        tx.execute("INSERT INTO model_attempts(id,turn_id,model,endpoint,state,begun_ms) VALUES(?1,?2,?3,?4,'running',?5)",
            params![attempt, request.id, safe_text(&self.context, &request.route.model), safe_text(&self.context, &request.route.endpoint), now]).map_err(|_| STORAGE_ERROR)?;
        append(
            &self.context,
            &tx,
            &session,
            &request.id,
            None,
            "turn_admitted",
            json!({"generation":request.generation,"profile":request.profile,"tools":request.tools.definitions(),"limits":{"maxSteps":request.limits.max_steps,"maxTokens":request.limits.max_tokens}}),
        )?;
        append(
            &self.context,
            &tx,
            &session,
            &request.id,
            None,
            "user",
            json!({"text":request.prompt,"message":ChatMessage::user(request.prompt.clone())}),
        )?;
        append(
            &self.context,
            &tx,
            &session,
            &request.id,
            Some(&attempt),
            "model_attempt_started",
            json!({"model":request.route.model,"endpoint":request.route.endpoint}),
        )?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(history)
    }

    /// Complete exactly this model attempt, durably retaining its complete canonical call list.
    pub fn complete_step(&self, turn: &str, assistant: &Event) -> Result<String, String> {
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, turn)?;
        let attempt =
            end_attempt(&self.context, &tx, &session, turn, "completed", Some(assistant))?.ok_or(STORAGE_ERROR)?;
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
        end_attempt(&self.context, &tx, &session, turn, "failed", None)?.ok_or(STORAGE_ERROR)?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(())
    }

    pub fn next_attempt(&self, request: &TurnRequest) -> Result<(), String> {
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, &request.id)?;
        let attempt = crate::turn_ledger::mint_id("model-attempt-");
        tx.execute("INSERT INTO model_attempts(id,turn_id,model,endpoint,state,begun_ms) VALUES(?1,?2,?3,?4,'running',?5)",
            params![attempt, request.id, safe_text(&self.context, &request.route.model), safe_text(&self.context, &request.route.endpoint), crate::parity_clock::now_ms()]).map_err(|_| STORAGE_ERROR)?;
        append(
            &self.context,
            &tx,
            &session,
            &request.id,
            Some(&attempt),
            "model_attempt_started",
            json!({"model":request.route.model,"endpoint":request.route.endpoint}),
        )?;
        tx.commit().map_err(|_| STORAGE_ERROR)?;
        Ok(())
    }

    /// Intent commits before the blocking handler may be scheduled, including refused calls.
    pub fn tool_start(&self, turn: &str, attempt: &str, call: &ToolCall) -> Result<String, String> {
        let mut conn = self.conn.lock().map_err(|_| STORAGE_ERROR)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| STORAGE_ERROR)?;
        let session = running_session(&tx, turn)?;
        let id = crate::turn_ledger::mint_id("tool-run-");
        tx.execute("INSERT INTO tool_runs(id,turn_id,attempt_id,call_id,name,arguments,state,begun_ms) VALUES(?1,?2,?3,?4,?5,?6,'running',?7)",
            params![id,turn,attempt,protocol_label(&self.context, &call.call_id, "her_call_"),safe_text(&self.context, &call.fn_name),safe_record(&self.context, &call.fn_arguments).to_string(),crate::parity_clock::now_ms()]).map_err(|_| STORAGE_ERROR)?;
        append(
            &self.context,
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
                safe_record(&self.context, outcome).to_string()
            ],
        )
        .map_err(|_| STORAGE_ERROR)?;
        append(
            &self.context,
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
        let result = safe_text(&self.context, result);
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
        end_attempt(&self.context, &tx, &session, turn, state, assistant)?;
        let now = crate::parity_clock::now_ms();
        tx.execute(
            "UPDATE turns SET state=?2,ended_ms=?3,result=?4 WHERE id=?1",
            params![turn, state, now, &result],
        )
        .map_err(|_| STORAGE_ERROR)?;
        append(
            &self.context,
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

fn running_session(tx: &Transaction<'_>, turn: &str) -> Result<String, String> {
    tx.query_row(
        "SELECT session_id FROM turns WHERE id=?1 AND state='running'",
        [turn],
        |r| r.get(0),
    )
    .map_err(|_| STORAGE_ERROR.into())
}

fn end_attempt(
    context: &RuntimeContext,
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
                context,
                tx,
                session,
                turn,
                Some(&attempt),
                "assistant",
                json!({"text":text,"raw":raw,"message":message}),
            )?;
            Some(safe_record(context, raw).to_string())
        }
        _ => None,
    };
    tx.execute(
        "UPDATE model_attempts SET state=?2,ended_ms=?3,response=?4 WHERE id=?1 AND turn_id=?5",
        params![attempt, state, crate::parity_clock::now_ms(), raw, turn],
    )
    .map_err(|_| STORAGE_ERROR)?;
    append(
        context,
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
fn protocol_label(context: &RuntimeContext, label: &str, prefix: &str) -> String {
    if !label.starts_with(prefix) && safe_text(context, label) == label {
        return label.to_string();
    }
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(label.as_bytes())))
}

fn safe_text(context: &RuntimeContext, text: &str) -> String {
    crate::activity::mask_log_text(text, &crate::activity::known_session_tokens(context))
}

fn safe_record(context: &RuntimeContext, value: &Value) -> Value {
    crate::activity::mask_json_session_tokens(value, &crate::activity::known_session_tokens(context))
}

fn safe_message(context: &RuntimeContext, mut message: ChatMessage) -> Result<Value, String> {
    let known = crate::activity::known_session_tokens(context);
    for part in message.content.iter_mut() {
        match part {
            ContentPart::Text(text) | ContentPart::ReasoningContent(text) => {
                *text = crate::activity::mask_log_text(text, &known);
            }
            ContentPart::ToolCall(call) => {
                call.call_id = protocol_label(context, &call.call_id, "her_call_");
                call.fn_name = protocol_label(context, &call.fn_name, "her_tool_");
                call.fn_arguments = crate::activity::mask_json_session_tokens(&call.fn_arguments, &known);
                // Provider signatures must remain valid for canonical replay.
            }
            ContentPart::ToolResponse(response) => {
                response.call_id = protocol_label(context, &response.call_id, "her_call_");
                if let Some(name) = &mut response.fn_name {
                    *name = protocol_label(context, name, "her_tool_");
                }
                response.content = crate::activity::mask_log_text(&response.content, &known);
            }
            ContentPart::Binary(binary) => {
                if let Some(name) = &mut binary.name {
                    *name = crate::activity::mask_log_text(name, &known);
                }
                // Encoded bytes/URLs and MIME types are operational image data.
            }
            ContentPart::ThoughtSignature(_) => {}
            ContentPart::Custom(custom) => {
                custom.data = crate::activity::mask_json_session_tokens(&custom.data, &known);
            }
        }
    }
    serde_json::to_value(message).map_err(|_| STORAGE_ERROR.into())
}

fn append(
    context: &RuntimeContext,
    tx: &Transaction<'_>,
    session: &str,
    turn: &str,
    attempt: Option<&str>,
    kind: &str,
    payload: Value,
) -> Result<(), String> {
    let mut safe = safe_record(context, &payload);
    if let Some(message) = payload.get("message") {
        let message = serde_json::from_value(message.clone()).map_err(|_| STORAGE_ERROR)?;
        safe["message"] = safe_message(context, message)?;
    }
    tx.execute("INSERT INTO events(session_id,turn_id,attempt_id,kind,payload,created_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![session,turn,attempt,kind,safe.to_string(),crate::parity_clock::now_ms()]).map_err(|_| STORAGE_ERROR)?;
    Ok(())
}

/// HER's completed harness replies in `conversation` created at or after `since_ms`, oldest first:
/// (created_ms, text). No database means none. Never creates or migrates storage.
pub(crate) fn assistant_texts(context: &RuntimeContext, conversation: &str, since_ms: i64) -> Result<Vec<(i64, String)>, String> {
    let path = context.resolve_path(RuntimeArea::Data, DB, PathIntent::ReadOptionalFile).map_err(|_| STORAGE_ERROR)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW)
        .map_err(|_| STORAGE_ERROR)?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|_| STORAGE_ERROR)?;
    let mut stmt = conn.prepare("SELECT e.created_ms, e.payload FROM events e JOIN turns t ON e.turn_id = t.id
        WHERE t.conversation = ?1 AND t.state = 'completed' AND e.kind = 'assistant' AND e.created_ms >= ?2 ORDER BY e.seq")
        .map_err(|_| STORAGE_ERROR)?;
    let rows = stmt.query_map(params![conversation, since_ms], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(|_| STORAGE_ERROR)?;
    let mut out = Vec::new();
    for row in rows {
        let (at, payload) = row.map_err(|_| STORAGE_ERROR)?;
        let payload: Value = serde_json::from_str(&payload).map_err(|_| STORAGE_ERROR)?;
        let text = payload["text"].as_str().unwrap_or("").trim();
        if !text.is_empty() {
            out.push((at, text.to_string()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{Limits, ModelRoute, Tools};
    use genai::chat::{Binary, BinarySource, ContentPart};

    #[test]
    fn hotfix128_binary_records_preserve_opaque_data_for_replay() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-store-opaque-128-{}", crate::turn_ledger::mint_id(""))));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let _cleanup = Cleanup(root.clone());
        for dir in ["config", "data", "resources"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        let cfg = json!({"ownerQQ":8808,"consoleToken":"synthetic-console-128","harness":{"model":"fake-chat"}});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-128").unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let token = crate::conversation_authority::ensure_or_rotate(&context, "private:8808", false).unwrap();
        let generation = crate::send_outbox::token_epoch(&token);
        let mut request = TurnRequest {
            id:"opaque-first".into(), conversation:"private:8808".into(), generation:generation.clone(), prompt:"ordinary input".into(), profile:"closed-agent".into(),
            route:ModelRoute::from_runtime(&context, &cfg).unwrap(), deadline:std::time::Duration::from_secs(10),
            tools:Tools::capture(&context, &cfg, "private:8808", &generation).unwrap(), limits:Limits::from_config(&cfg),
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
        let history = store.admit(&request).unwrap();
        let parts: Vec<_> = history.iter().flat_map(|m| m.content.iter()).collect();
        let binary = parts.iter().find_map(|p| match p { ContentPart::Binary(b) => Some(b), _ => None }).unwrap();
        let BinarySource::Base64(data) = &binary.source else { panic!("binary source changed") };
        assert_eq!(data.as_ref(), opaque, "canonical binary bytes must survive record masking");
        assert!(parts.iter().any(|p| matches!(p, ContentPart::ThoughtSignature(signature) if signature == &opaque)));
        assert!(parts.iter().any(|p| matches!(p, ContentPart::Text(text) if text == "ordinary ***")), "model text is still masked");
    }
}
