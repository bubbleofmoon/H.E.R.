//! The bridge's single live owner interaction. SQLite owns tool intent and recovery.
use super::{store::record_credentials, tools, Failure};
use crate::runtime::{RuntimeArea, RuntimeContext};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

const SNAPSHOT: &str = "pending-owner-interaction.json";
pub(super) const ASK: &str = "ask_owner";
pub(super) const GROUP_SEND: &str = "qq_send_group_message";
const ANSWER_LIMIT: usize = 2_000;
const OPTION_LIMIT: usize = 4;
const TIMEOUT_NOTICE: &str = "⏰ 等待回答超时，已取消该请求。";
pub(super) const TIMED_OUT: &str = "Owner interaction timed out.";

fn approval_prompt(id: &str) -> String {
    format!("回复 /approve {id} 或 /deny {id}")
}

pub(super) fn question_definition() -> Value {
    json!({
        "name": ASK,
        "description": "Ask the owner one question and wait for the answer.",
        "inputSchema": {
            "type":"object", "additionalProperties":false, "required":["question"],
            "properties":{
                "question":{"type":"string"},
                "options":{"type":"array","items":{"type":"string"}}
            }
        }
    })
}

pub(super) fn group_definition(mut definition: Value) -> Value {
    definition["description"] = json!("Ask the owner to approve sending one text message to an allowed group. The message is sent if approved.");
    if let Some(properties) = definition["inputSchema"]["properties"].as_object_mut() {
        properties.remove("token");
        properties.remove("replyToMessageId");
    }
    definition
}

pub(super) struct Question {
    text: String,
    options: Vec<String>,
}

impl Question {
    pub(super) fn parse(args: &Value) -> Result<Self, Value> {
        let text = args["question"].as_str().unwrap_or("").trim();
        let options = args["options"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let bounded = |text: &str, limit: usize| {
            !text.is_empty() && text.encode_utf16().count() <= limit && !text.chars().any(char::is_control)
        };
        if !bounded(text, 280) || options.len() > OPTION_LIMIT {
            return Err(tools::error("Invalid question or too many options."));
        }
        let mut labels = Vec::<String>::new();
        for option in options {
            let label = option.as_str().unwrap_or("").trim();
            if !bounded(label, 48) || labels.iter().any(|old| old.to_lowercase() == label.to_lowercase()) {
                return Err(tools::error("Invalid or duplicate question option."));
            }
            labels.push(label.into());
        }
        Ok(Self { text: text.into(), options: labels })
    }

    fn display(&self) -> Value {
        let options: Vec<Value> = self.options.iter().map(|label| json!({"label":label})).collect();
        json!({"kind":"question","questions":[{"id":"question","question":self.text,"options":options}]})
    }

    fn notice(&self) -> String {
        let options = self.options.iter().map(|label| format!("「{label}」")).collect::<Vec<_>>().join(" ");
        format!("❓ 请回答：{}\n{options}\n（直接回复选项文字或输入你的回答）", self.text)
    }

    fn answer(&self, text: &str) -> String {
        self.options.iter().find(|label| label.to_lowercase() == text.to_lowercase())
            .cloned().unwrap_or_else(|| text.to_string())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    id: String,
    key: String,
    session_id: String,
    turn: String,
    epoch: String,
    expires_at: i64,
    display: Value,
}

impl Snapshot {
    fn current(&self, context: &RuntimeContext) -> bool {
        owner_current(context, &self.key, &self.epoch) && !self.session_id.is_empty()
            && super::turn_is_live(context, &self.turn)
    }
}

fn owner_current(context: &RuntimeContext, key: &str, epoch: &str) -> bool {
    crate::console::runtime_mode(context) == "closed-agent"
        && context.load_config().is_ok_and(|cfg| crate::chat::is_owner_key(&cfg, key))
        && crate::conversation_authority::token(context, key).is_ok_and(|token| {
            token.is_some_and(|token| crate::conversation_authority::generation(&token) == epoch)
        })
}

pub(crate) fn pending_view(context: &RuntimeContext) -> Result<Option<Value>, String> {
    let Some(text) = context.read_optional_text(RuntimeArea::Data, SNAPSHOT)? else {
        return Ok(None);
    };
    let snapshot: Option<Snapshot> = serde_json::from_str(&text)
        .map_err(|_| "owner interaction snapshot is invalid")?;
    Ok(snapshot.filter(|entry| {
        entry.expires_at > crate::parity_clock::now_ms() as i64 && entry.current(context)
    }).map(|entry| entry.display))
}

struct Pending {
    snapshot: Snapshot,
    expires: Instant,
    state: PendingState,
    wake: Option<oneshot::Sender<()>>,
    approval: bool,
}

enum PendingState {
    Delivering,
    Open,
    Accepted(String),
}

impl Pending {
    fn is_open(&self) -> bool {
        matches!(self.state, PendingState::Open)
    }
}

pub(crate) struct Mailbox {
    context: RuntimeContext,
    // If both are needed, the closed-agent gate precedes this mutex. The mutex covers
    // only the slot and its snapshot write. Release it before authority checks, waits,
    // conversation storage, or the send chain.
    pending: Mutex<Option<Pending>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Forward,
    Accepted,
    Notice(String),
    NoticeAndForward(String),
    NoticeIfNoLegacy(String),
}

impl Mailbox {
    pub(crate) fn start(context: &RuntimeContext) -> Result<Arc<Self>, String> {
        context.write_atomic(RuntimeArea::Data, SNAPSHOT, b"null")?;
        Ok(Arc::new(Self { context: context.clone(), pending: Mutex::new(None) }))
    }

    /// Ingress has already checked the actual sender. Claim exactly the request seen
    /// before waiting on the gate, even if expiry or reset ends it while waiting.
    pub(crate) async fn answer(&self, key: &str, text: &str, gate_wait: Duration) -> Answer {
        let text = text.trim();
        let mut words = text.split_whitespace();
        let command = words.next().filter(|word| matches!(*word, "/approve" | "/deny"));
        let id = words.next().filter(|_| words.next().is_none());
        let no_pending = || {
            if command.is_some() && key.starts_with("private:") {
                Answer::NoticeIfNoLegacy("没有待处理的请求。".into())
            } else {
                Answer::Forward
            }
        };
        let snapshot = self.pending.lock().ok().and_then(|slot| {
            let pending = slot.as_ref()?;
            (pending.snapshot.key == key && pending.is_open()).then(|| pending.snapshot.clone())
        });
        let Some(snapshot) = snapshot else { return no_pending() };
        let Ok(_gate) = crate::closed_agent::lock_wait(&self.context, gate_wait).await else {
            return Answer::Notice("⚠️ 系统繁忙，回答未提交，请稍后再回复一次。".into());
        };
        if !snapshot.current(&self.context) {
            return no_pending();
        }
        let Ok(mut slot) = self.pending.lock() else { return no_pending() };
        let Some(pending) = slot.as_mut().filter(|entry| entry.snapshot.id == snapshot.id && entry.is_open()) else {
            return no_pending();
        };
        if Instant::now() >= pending.expires {
            return no_pending();
        }
        let reply = if pending.approval {
            match command.filter(|_| id == Some(snapshot.id.as_str())) {
                Some(decision) => decision,
                None if text.starts_with('/') => return Answer::Notice(approval_prompt(&snapshot.id)),
                None => return Answer::NoticeAndForward(approval_prompt(&snapshot.id)),
            }
        } else if command.is_some() || text.is_empty() {
            return no_pending();
        } else if text.chars().take(ANSWER_LIMIT + 1).count() > ANSWER_LIMIT {
            return Answer::Notice(format!("⚠️ 回答太长，请缩短至 {ANSWER_LIMIT} 字以内再回复。"));
        } else {
            text
        };
        pending.state = PendingState::Accepted(reply.to_string());
        if let Some(wake) = pending.wake.take() {
            let _ = wake.send(());
        }
        Answer::Accepted
    }

    async fn reserve(
        self: &Arc<Self>,
        snapshot: Snapshot,
        expires: Instant,
        approval: bool,
    ) -> Result<(Lease, oneshot::Receiver<()>), String> {
        let _gate = crate::closed_agent::lock_wait(&self.context, crate::closed_agent::GATE_WAIT).await?;
        if !snapshot.current(&self.context) || Instant::now() >= expires {
            return Err("owner interaction expired or session changed".into());
        }
        let mut slot = self.pending.lock().map_err(|_| "owner interaction is unavailable")?;
        if slot.is_some() {
            return Err("another owner interaction is pending".into());
        }
        let (wake, receiver) = oneshot::channel();
        let lease = Lease { mailbox: self.clone(), id: snapshot.id.clone() };
        *slot = Some(Pending {
            snapshot,
            expires,
            state: PendingState::Delivering,
            wake: Some(wake),
            approval,
        });
        Ok((lease, receiver))
    }
}

struct Lease {
    mailbox: Arc<Mailbox>,
    id: String,
}

impl Lease {
    fn confirm_delivery(&self) -> Result<(), String> {
        let mut slot = self.mailbox.pending.lock().map_err(|_| "owner interaction is unavailable")?;
        let Some(pending) = slot.as_mut().filter(|entry| entry.snapshot.id == self.id) else {
            return Err("owner interaction is unavailable".into());
        };
        if Instant::now() < pending.expires {
            let bytes = serde_json::to_vec(&pending.snapshot).map_err(|_| "owner interaction cannot be displayed")?;
            self.mailbox.context.write_atomic(RuntimeArea::Data, SNAPSHOT, &bytes)?;
            pending.state = PendingState::Open;
        }
        Ok(())
    }

    fn take(&self) -> Option<Pending> {
        let mut slot = self.mailbox.pending.lock().ok()?;
        if slot.as_ref()?.snapshot.id != self.id {
            return None;
        }
        let pending = slot.take();
        if self.mailbox.context.write_atomic(RuntimeArea::Data, SNAPSHOT, b"null").is_err() {
            eprintln!("[owner interaction] snapshot cleanup failed");
        }
        pending
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.take();
    }
}

pub(super) struct Binding<'a> {
    pub mailbox: &'a Arc<Mailbox>,
    pub key: &'a str,
    pub session: &'a str,
    pub turn: &'a str,
    pub epoch: &'a str,
    pub deadline: Instant,
    pub deadline_generation: &'a OnceLock<String>,
}

impl Binding<'_> {
    pub async fn question(&self, args: &Value, cancelled: watch::Receiver<bool>) -> Result<Value, Failure> {
        let question = match Question::parse(args) {
            Ok(question) => question,
            Err(outcome) => return Ok(outcome),
        };
        let context = &self.mailbox.context;
        let mut known = record_credentials(context)?;
        if std::iter::once(&question.text).chain(&question.options).any(|text| {
            crate::activity::mask_log_text(text, &known) != *text
        }) {
            return Ok(tools::error("Question text contains private credentials."));
        }
        let reply = self.wait(question.display(), question.notice(), false, cancelled).await?;
        known.extend(record_credentials(context)?);
        let answer = crate::activity::mask_log_text(&question.answer(&reply), &known);
        Ok(tools::text(&answer, false))
    }

    pub async fn group_send(&self, args: &Value, cancelled: watch::Receiver<bool>) -> Result<Value, Failure> {
        let context = &self.mailbox.context;
        let Some(id) = args["groupId"].as_i64().or_else(|| args["groupId"].as_str()?.parse().ok()) else {
            return Ok(tools::error("Invalid group."));
        };
        if args["groupId"].as_str().is_some_and(|raw| raw != id.to_string()) {
            return Ok(tools::error("Invalid group."));
        }
        let key = format!("group:{id}");
        let text = args["message"].as_str().unwrap_or("");
        if text.encode_utf16().take(501).count() > 500 {
            return Ok(tools::error("Group message exceeds the approval preview limit."));
        }
        let message = crate::outbound_message::normalize(text);
        let cfg = context.load_config().map_err(Failure::from)?;
        let known = record_credentials(context)?;
        if !crate::console::approved_group_allowed(&cfg, &key, &message)
            || crate::activity::mask_log_text(&message, &known) != message
        {
            return Ok(tools::error("Group send refused by current settings."));
        }
        let decision = self.wait(
            json!({"kind":"approval","toolName":GROUP_SEND,"target":key,"message":message}),
            format!("🔐 请求向 {key} 发送以下消息：\n{message}"),
            true,
            cancelled.clone(),
        ).await?;
        if decision != "/approve" {
            return Ok(tools::error("The owner refused this send."));
        }
        let context = context.clone();
        let owner = self.key.to_string();
        let epoch = self.epoch.to_string();
        let turn = self.turn.to_string();
        let deadline = self.deadline;
        tokio::task::spawn_blocking(move || {
            let authorized = || !*cancelled.borrow() && Instant::now() < deadline
                && owner_current(&context, &owner, &epoch)
                && context.load_config().is_ok_and(|cfg| crate::console::approved_group_allowed(&cfg, &key, &message))
                && record_credentials(&context).is_ok_and(|known| crate::activity::mask_log_text(&message, &known) == message);
            if !authorized() {
                return tools::error("Group send cancelled.");
            }
            match crate::console::send_approved_group(&context, &key, &message, &turn, &authorized) {
                Ok((_, value)) if value["unknown"].as_array().is_some_and(|items| !items.is_empty()) => {
                    let mut outcome = tools::error("Group send delivery is outcome-unknown.");
                    outcome["_meta"] = json!({"herDelivery":"unknown"});
                    outcome
                }
                Ok((_, value)) if value["ok"] == true => tools::text("Group message sent.", false),
                _ => tools::error("Group send refused or not delivered."),
            }
        }).await.map_err(|_| Failure::Uncertain("Approved send worker did not settle.".into()))
    }

    async fn wait(&self, mut display: Value, mut notice: String, approval: bool, mut cancelled: watch::Receiver<bool>) -> Result<String, Failure> {
        let cfg = self.mailbox.context.load_config().map_err(Failure::from)?;
        let timeout = cfg["questionTimeoutMs"].as_u64().unwrap_or(300_000);
        let now = Instant::now();
        let remaining = self.deadline.saturating_duration_since(now);
        let wait = Duration::from_millis(timeout).min(remaining);
        let expires = now + wait;
        // At the turn cap the turn's timer revokes authority and wakes the waiter.
        let inner_expiry = (expires < self.deadline).then_some(expires);
        let id = crate::turn_ledger::mint_id("request-");
        if approval {
            display["requestId"] = json!(id);
            notice = format!("{}\n{notice}", approval_prompt(&id));
        }
        display["key"] = json!(self.key);
        display["sessionId"] = json!(self.session);
        let mut snapshot = Snapshot {
            id, key: self.key.into(), session_id: self.session.into(), turn: self.turn.into(),
            epoch: self.epoch.into(), expires_at: (crate::parity_clock::now_ms() as i64)
                .saturating_add(wait.as_millis() as i64), display,
        };
        let reservation = self.mailbox.reserve(snapshot.clone(), expires, approval);
        let (lease, receiver) = tokio::select! { biased;
            _ = cancelled.changed() => return Err(Failure::Cancelled),
            _ = wait_until_expiry(inner_expiry) => return Err(TIMED_OUT.into()),
            result = reservation => result?,
        };
        if !self.deliver(snapshot.clone(), notice, Some((expires, cancelled.clone())), false).await? {
            return if *cancelled.borrow() {
                Err(Failure::Cancelled)
            } else {
                Err("Owner interaction could not be delivered.".into())
            };
        }
        if !*cancelled.borrow() {
            lease.confirm_delivery()?;
        }
        let error = match wait_for_reply(&lease, receiver, inner_expiry, &mut cancelled).await {
            Ok(reply) => return Ok(reply),
            Err(error) => error,
        };
        if matches!(&error, Failure::Failed(_))
            && Instant::now() < self.deadline
            && self.deliver(snapshot.clone(), TIMEOUT_NOTICE.into(), Some((self.deadline, cancelled.clone())), true).await?
        {
            return Err(Failure::InteractionClosed);
        }
        if !approval {
            // A definite refusal under the old bound can coincide with the turn cap.
            // Wait for revocation before using the deadline's replacement generation.
            if Instant::now() >= self.deadline && !*cancelled.borrow() {
                let _ = cancelled.changed().await;
            }
            if let Some(generation) = self.deadline_generation.get() {
                snapshot.epoch.clone_from(generation);
                if self.deliver(snapshot, TIMEOUT_NOTICE.into(), None, true).await? {
                    return Err(Failure::InteractionClosed);
                }
            }
        }
        Err(error)
    }

    async fn deliver(
        &self,
        snapshot: Snapshot,
        text: String,
        bound: Option<(Instant, watch::Receiver<bool>)>,
        notice_only: bool,
    ) -> Result<bool, Failure> {
        let context = self.mailbox.context.clone();
        tokio::task::spawn_blocking(move || {
            let cfg = context.load_config().map_err(Failure::from)?;
            let ob = crate::chat::onebot_of(&context, &cfg)?;
            let purpose = if notice_only { "owner-interaction-timeout" } else { "owner-interaction" };
            let cancelled = || bound.as_ref().is_some_and(|(_, signal)| *signal.borrow());
            let report = crate::chat::send_recorded(&context, &ob, &snapshot.key, &snapshot.epoch, purpose, &text, 0, &|| {
                if !cancelled() && bound.as_ref().is_none_or(|(deadline, _)| Instant::now() < *deadline) && snapshot.current(&context) {
                    Ok(())
                } else {
                    Err("owner interaction ended".into())
                }
            });
            if report.unknown {
                return Err(Failure::Uncertain("Owner interaction delivery is outcome-unknown.".into()));
            }
            Ok(report.parts > 0 && report.confirmed == report.parts)
        }).await.map_err(|_| Failure::Uncertain("Owner interaction worker did not settle.".into()))?
    }
}

async fn wait_until_expiry(expires: Option<Instant>) {
    match expires {
        Some(expires) => tokio::time::sleep_until(expires).await,
        None => std::future::pending().await,
    }
}

async fn wait_for_reply(
    lease: &Lease,
    receiver: oneshot::Receiver<()>,
    expires: Option<Instant>,
    cancelled: &mut watch::Receiver<bool>,
) -> Result<String, Failure> {
    let interruption = tokio::select! { biased;
        _ = cancelled.changed() => Some(Failure::Cancelled),
        _ = wait_until_expiry(expires) => Some(Failure::Failed(TIMED_OUT.into())),
        _ = receiver => None,
    };
    // Wakeups only decide when to settle. Taking the slot orders settlement
    // against answer(), regardless of which wakeup arrived first.
    match lease.take().map(|pending| pending.state) {
        Some(PendingState::Accepted(reply)) => Ok(reply),
        _ => Err(interruption.unwrap_or(Failure::Cancelled)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    const OWNER_KEY: &str = "private:42";
    const TOKEN: &str = "synthetic-interaction-token";
    const REQUEST: &str = "request-fixture";
    const TURN: &str = "turn-fixture";

    struct Fixture {
        root: PathBuf,
        mailbox: Arc<Mailbox>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = crate::install_root::development_root(
                std::env::temp_dir().join(crate::turn_ledger::mint_id("her-mailbox-")),
            );
            for dir in ["config", "data", "resources"] {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            fs::write(
                root.join("config/config.json"),
                r#"{"ownerQQ":42,"consoleToken":"mailbox-fixture-console"}"#,
            ).unwrap();
            fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
            fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
            fs::write(
                root.join("dependencies.json"),
                r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
            ).unwrap();
            let context = RuntimeContext::from_root(&root).unwrap();
            let fixture = Self { root, mailbox: Mailbox::start(&context).unwrap() };
            fixture.set_token(TOKEN);
            fixture
        }

        fn set_token(&self, token: &str) {
            self.mailbox.context.write_atomic(
                RuntimeArea::Data,
                "social-v2.json",
                json!({"conversations":{OWNER_KEY:{"agentToken":token}}}).to_string().as_bytes(),
            ).unwrap();
        }

        async fn publish(&self, approval: bool) -> (fs::File, Lease, oneshot::Receiver<()>) {
            self.publish_until(approval, Instant::now() + Duration::from_secs(60)).await
        }

        async fn publish_until(
            &self,
            approval: bool,
            expires: Instant,
        ) -> (fs::File, Lease, oneshot::Receiver<()>) {
            let (owner, lease, reply) = self.reserve_until(approval, expires).await;
            lease.confirm_delivery().unwrap();
            (owner, lease, reply)
        }

        async fn reserve_until(
            &self,
            approval: bool,
            expires: Instant,
        ) -> (fs::File, Lease, oneshot::Receiver<()>) {
            let owner = super::super::turn_owner(&self.mailbox.context, TURN).unwrap();
            let snapshot = Snapshot {
                id: REQUEST.into(),
                key: OWNER_KEY.into(),
                session_id: "session-fixture".into(),
                turn: TURN.into(),
                epoch: crate::conversation_authority::generation(TOKEN),
                expires_at: crate::parity_clock::now_ms() as i64
                    + expires.saturating_duration_since(Instant::now()).as_millis() as i64,
                display: Value::Null,
            };
            let (lease, reply) = self.mailbox.reserve(snapshot, expires, approval).await.unwrap();
            (owner, lease, reply)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn an_accepted_reply_survives_the_inner_expiry_timer() {
        let fixture = Fixture::new();
        let expires = Instant::now() + Duration::from_secs(2);
        let (_owner, lease, reply) = fixture.publish_until(false, expires).await;
        let (_cancel, mut cancelled) = watch::channel(false);
        let wait = wait_for_reply(&lease, reply, Some(expires), &mut cancelled);
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        assert_eq!(fixture.mailbox.answer(OWNER_KEY, "Blue", Duration::ZERO).await, Answer::Accepted);
        tokio::time::sleep_until(expires + Duration::from_millis(1)).await;
        assert_eq!(wait.await.ok().as_deref(), Some("Blue"), "inner expiry discarded an accepted reply");
    }

    #[tokio::test]
    async fn an_accepted_reply_survives_turn_deadline_revocation() {
        let fixture = Fixture::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        let (_owner, lease, reply) = fixture.publish_until(false, deadline).await;
        let (cancel, cancelled) = watch::channel(false);
        let mut interaction_cancelled = cancelled.clone();
        let generation = OnceLock::new();
        let context = &fixture.mailbox.context;
        let tools = tools::Tools::capture(
            context, &context.load_config().unwrap(), "closed-agent", OWNER_KEY,
            &crate::conversation_authority::generation(TOKEN),
        ).unwrap();
        let turn = super::super::settle_turn(
            wait_for_reply(&lease, reply, None, &mut interaction_cancelled),
            Some(deadline), &cancel, cancelled, &tools, &generation,
        );
        tokio::pin!(turn);
        assert!(futures_util::poll!(&mut turn).is_pending());
        assert_eq!(fixture.mailbox.answer(OWNER_KEY, "Blue", Duration::ZERO).await, Answer::Accepted);
        tokio::time::sleep_until(deadline + Duration::from_millis(1)).await;
        let (result, interrupted) = turn.await;
        assert!(matches!(interrupted, Some(super::super::Status::Failed(reason)) if reason == super::super::DEADLINE_EXCEEDED));
        assert!(generation.get().is_some());
        assert_eq!(result.ok().as_deref(), Some("Blue"), "the turn deadline discarded an accepted reply");
    }

    #[tokio::test]
    async fn explicit_cancellation_preserves_an_accepted_reply_for_settlement() {
        let fixture = Fixture::new();
        let expires = Instant::now() + Duration::from_secs(60);
        let (_owner, lease, reply) = fixture.publish_until(true, expires).await;
        let (cancel, mut cancelled) = watch::channel(false);
        let wait = wait_for_reply(&lease, reply, Some(expires), &mut cancelled);
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        assert_eq!(
            fixture.mailbox.answer(OWNER_KEY, &format!("/deny {REQUEST}"), Duration::ZERO).await,
            Answer::Accepted,
        );
        cancel.send(true).unwrap();
        assert_eq!(wait.await.ok().as_deref(), Some("/deny"), "cancellation discarded an accepted decision");
    }

    #[tokio::test]
    async fn a_prompt_delivered_after_expiry_never_admits_an_answer() {
        let fixture = Fixture::new();
        let expires = Instant::now() + Duration::from_secs(2);
        let (_owner, lease, reply) = fixture.reserve_until(false, expires).await;
        let early = fixture.mailbox.answer(OWNER_KEY, "Blue", Duration::ZERO).await;
        assert_eq!(early, Answer::Forward, "accepted an answer before delivery was confirmed");
        assert_eq!(pending_view(&fixture.mailbox.context).unwrap(), None);
        tokio::time::sleep_until(expires).await;
        lease.confirm_delivery().unwrap();
        assert_eq!(fixture.mailbox.answer(OWNER_KEY, "Green", Duration::ZERO).await, Answer::Forward);
        let (_cancel, mut cancelled) = watch::channel(false);
        assert!(matches!(
            wait_for_reply(&lease, reply, Some(expires), &mut cancelled).await,
            Err(Failure::Failed(_)),
        ));
        assert_eq!(fixture.mailbox.answer(OWNER_KEY, "Green", Duration::ZERO).await, Answer::Forward);
        assert_eq!(pending_view(&fixture.mailbox.context).unwrap(), None);
        assert!(fixture.mailbox.pending.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn an_overlong_question_answer_gets_a_notice_and_can_be_retried() {
        let fixture = Fixture::new();
        let (_owner, lease, mut reply) = fixture.publish(false).await;
        let answer = "字".repeat(2_000);
        assert_eq!(
            fixture.mailbox.answer(OWNER_KEY, &format!("{answer}字"), Duration::ZERO).await,
            Answer::Notice("⚠️ 回答太长，请缩短至 2000 字以内再回复。".into()),
        );
        assert_eq!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        assert_eq!(fixture.mailbox.answer(OWNER_KEY, &answer, Duration::ZERO).await, Answer::Accepted);
        let (_cancel, mut cancelled) = watch::channel(false);
        assert_eq!(wait_for_reply(&lease, reply, None, &mut cancelled).await.ok(), Some(answer));
    }

    #[tokio::test]
    async fn busy_owner_interaction_gate_reports_and_leaves_the_answer_retryable() {
        let fixture = Fixture::new();
        let (_owner, lease, mut reply) = fixture.publish(false).await;
        let held = fixture.mailbox.context.open_locked_file(RuntimeArea::Data, "locks/closed-agent.lock").unwrap();
        assert_eq!(
            fixture.mailbox.answer(OWNER_KEY, "Blue", Duration::ZERO).await,
            Answer::Notice("⚠️ 系统繁忙，回答未提交，请稍后再回复一次。".into()),
            "a busy gate did not report that the answer was unsubmitted"
        );
        assert_eq!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        drop(held);
        assert_eq!(fixture.mailbox.answer(OWNER_KEY, "Blue", Duration::ZERO).await, Answer::Accepted);
        let (_cancel, mut cancelled) = watch::channel(false);
        assert_eq!(wait_for_reply(&lease, reply, None, &mut cancelled).await.ok().as_deref(), Some("Blue"));
    }

    #[tokio::test]
    async fn stale_owner_interaction_leaves_ordinary_chat_available() {
        let fixture = Fixture::new();
        let (_owner, _lease, mut reply) = fixture.publish(false).await;
        fixture.set_token("rotated-interaction-token");
        assert_eq!(
            fixture.mailbox.answer(OWNER_KEY, "ordinary chat", Duration::ZERO).await,
            Answer::Forward,
            "a stale request swallowed ordinary chat"
        );
        assert_eq!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    }

    #[tokio::test]
    async fn an_expired_owner_interaction_does_not_consume_late_chat_or_commands() {
        let fixture = Fixture::new();
        let (_owner, _lease, mut reply) = fixture.publish(true).await;
        fixture.mailbox.pending.lock().unwrap().as_mut().unwrap().expires = Instant::now();
        assert_eq!(
            fixture.mailbox.answer(OWNER_KEY, &format!("/approve {REQUEST}"), Duration::ZERO).await,
            Answer::NoticeIfNoLegacy("没有待处理的请求。".into()),
            "an expired request accepted a late approval"
        );
        assert_eq!(fixture.mailbox.answer(OWNER_KEY, "ordinary chat", Duration::ZERO).await, Answer::Forward);
        assert_eq!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    }

    #[tokio::test]
    async fn the_mailbox_binds_both_approval_commands_to_the_private_conversation_key() {
        let fixture = Fixture::new();
        for decision in ["/approve", "/deny"] {
            let (_owner, lease, mut reply) = fixture.publish(true).await;
            let command = format!("{decision} {REQUEST}");
            assert_eq!(
                fixture.mailbox.answer("group:43", &command, Duration::ZERO).await,
                Answer::Forward,
                "a matching group command claimed the private decision"
            );
            assert_eq!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            assert_eq!(fixture.mailbox.answer(OWNER_KEY, &command, Duration::ZERO).await, Answer::Accepted);
            let (_cancel, mut cancelled) = watch::channel(false);
            assert_eq!(wait_for_reply(&lease, reply, None, &mut cancelled).await.ok().as_deref(), Some(decision));
        }
    }

    #[test]
    fn questions_bound_options_and_reject_ambiguous_labels() {
        let question = "x".repeat(280);
        let label = "🎨".repeat(24);
        assert!(Question::parse(&json!({"question":question,"options":[label,"b","c","d"]})).is_ok());
        assert!(Question::parse(&json!({"question":question,"options":[label,"b","c","d","e"]})).is_err(), "accepted a fifth option");
        assert!(Question::parse(&json!({"question":format!("{question}x")})).is_err());
        assert!(Question::parse(&json!({"question":"pick","options":[format!("{label}x")]})).is_err());
        assert!(Question::parse(&json!({"question":"pick","options":[" Blue ","blue"]})).is_err());
    }
}
