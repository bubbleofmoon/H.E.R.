//! Closed-agent remote questions and approvals. The live process owns the pending requests;
//! the separately running console reads a filtered, selected-runtime snapshot.

use crate::runtime::{RuntimeArea, RuntimeContext};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

const SNAPSHOT: &str = "pending-interactions.json";
const OWNER: &str = "closed-agent-owner.json";

/// Serialize a receipt against console mode changes and session resets in another process.
pub fn lock(context: &RuntimeContext) -> Result<std::fs::File, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match context.open_locked_file(RuntimeArea::Data, "locks/closed-agent.lock") {
            Ok(file) => return Ok(file),
            Err(e) if e == "runtime file is already locked" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(format!("closed-agent interaction lock unavailable: {e}")),
        }
    }
}

/// How long a live-process answer or registration waits for the gate. A console mode write holds
/// the gate through DSH's settings write-through, whose read timeout is 30 s.
const GATE_WAIT: Duration = Duration::from_secs(45);

fn try_lock(context: &RuntimeContext) -> Result<Option<std::fs::File>, String> {
    match context.open_locked_file(RuntimeArea::Data, "locks/closed-agent.lock") {
        Ok(file) => Ok(Some(file)),
        Err(e) if e == "runtime file is already locked" => Ok(None),
        Err(e) => Err(format!("closed-agent interaction lock unavailable: {e}")),
    }
}

/// Like `lock`, but waits without blocking the async executor.
async fn lock_wait(context: &RuntimeContext, wait: Duration) -> Result<std::fs::File, String> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(file) = try_lock(context)? {
            return Ok(file);
        }
        if Instant::now() >= deadline {
            return Err("closed-agent interaction lock unavailable: runtime file is already locked".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Clone)]
struct Pending {
    key: String,
    session: String,
    turn: String,
    epoch: String,
    client: String,
    event: String,
    kind: String,
    questions: Value,
    tool: Value,
    outcome_unknown: bool,
    expires: Instant,
    expires_ms: i64,
}

impl Pending {
    fn display(&self) -> Value {
        let mut item = json!({"key":self.key,"kind":self.kind,"sessionId":self.session});
        if self.kind == "question" {
            item["questions"] = self.questions.clone();
        } else {
            // The frozen bridge sends the reason in the QQ notification but
            // registers only toolName in its pending approval entry.
            item["toolName"] = self.tool.clone();
        }
        if self.outcome_unknown {
            item["status"] = json!("outcome-unknown");
        }
        item
    }

    fn valid(&self, context: &RuntimeContext) -> bool {
        if Instant::now() >= self.expires {
            return false;
        }
        if self.outcome_unknown {
            valid_identity(context, &self.key, &self.session, &self.turn, &self.epoch)
        } else {
            // JS keeps the request across turn end; only the session policy,
            // an answer, a replacement, a reset, or its timer retires it.
            valid_session(context, &self.key, &self.session, &self.epoch)
        }
    }
}

fn valid_session(context: &RuntimeContext, key: &str, session: &str, epoch: &str) -> bool {
    if crate::console::runtime_mode(context) != "closed-agent" {
        return false;
    }
    let Ok(cfg) = context.load_config() else {
        return false;
    };
    if !crate::chat::is_owner_key(&cfg, key) {
        return false;
    }
    if crate::session_map::lookup(context, key)
        .ok()
        .flatten()
        .as_deref()
        != Some(session)
    {
        return false;
    }
    let token = crate::console::conversation_token(context, key)
        .ok()
        .flatten()
        .unwrap_or_default();
    if token.is_empty() || crate::send_outbox::token_epoch(&token) != epoch {
        return false;
    }
    true
}

fn valid_identity(
    context: &RuntimeContext,
    key: &str,
    session: &str,
    turn: &str,
    epoch: &str,
) -> bool {
    if !valid_session(context, key, session, epoch) {
        return false;
    }
    let Ok(records) = crate::turn_ledger::read_records(context) else {
        return false;
    };
    let (turns, _) = crate::turn_ledger::fold(&records);
    turns.iter().any(|t| {
        t.id == turn
            && t.key == key
            && t.session_id == session
            && t.state.is_open()
            && t.epochs.iter().any(|e| e == epoch)
    })
}

/// Console view omits entries whose session authorization or deadline has changed.
pub fn pending_view(context: &RuntimeContext) -> Result<Value, String> {
    let Some(text) = context
        .read_optional_text(RuntimeArea::Data, SNAPSHOT)
        .map_err(str::to_string)?
    else {
        return Ok(json!({"pending":[]}));
    };
    let entries: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("pending state is invalid: {e}"))?;
    let now = crate::parity_clock::now_ms() as i64;
    let mut list = Vec::new();
    for entry in entries {
        let get = |name| entry.get(name).and_then(Value::as_str).unwrap_or("");
        let current = valid_session(context, get("key"), get("sessionId"), get("epoch"));
        if entry["expiresAt"].as_i64().unwrap_or(0) > now && current {
            if let Some(value) = entry.get("display") {
                list.push(value.clone());
            }
        }
    }
    Ok(json!({"pending":list}))
}

struct State {
    entries: HashMap<String, Pending>,
    seen: HashSet<(String, String)>,
}

// Inert unless the opt-in test supplies its private per-run directory.
#[cfg(feature = "deadlock-regression")]
macro_rules! deadlock_probe_armed {
    ($armed:ident) => {
        $armed
    };
}
#[cfg(feature = "deadlock-regression")]
mod deadlock_probe {
    use std::{future::Future, path::PathBuf, task::Poll, time::Duration};
    fn directory() -> Option<PathBuf> {
        std::env::var_os("HER_DEADLOCK_PROBE").map(PathBuf::from)
    }
    pub fn enabled() -> bool {
        directory().is_some()
    }
    pub fn signal(name: &str) {
        if let Some(dir) = directory() {
            std::fs::write(dir.join(name), b"witness").expect("deadlock probe witness");
        }
    }
    pub async fn control(armed: bool) -> bool {
        loop {
            if let Some(dir) = directory() {
                if !armed && dir.join("arm").exists() {
                    signal("armed");
                    return false;
                }
                if armed && dir.join("maintenance").exists() {
                    std::fs::remove_file(dir.join("arm")).expect("remove arm");
                    std::fs::remove_file(dir.join("maintenance")).expect("remove maintenance");
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn poll_reap<F: Future<Output = ()>>(future: F) {
        tokio::pin!(future);
        let mut first = true;
        std::future::poll_fn(|cx| {
            let result = future.as_mut().poll(cx);
            if first {
                first = false;
                signal(match result {
                    Poll::Pending => "maintenance-pending",
                    Poll::Ready(()) => "maintenance-ready",
                });
            }
            result
        })
        .await
    }
}

pub struct Hub {
    context: RuntimeContext,
    state: tokio::sync::Mutex<State>,
}

impl Hub {
    pub fn start(context: &RuntimeContext) -> Result<Arc<Self>, String> {
        let hub = Arc::new(Self {
            context: context.clone(),
            state: tokio::sync::Mutex::new(State {
                entries: HashMap::new(),
                seen: HashSet::new(),
            }),
        });
        context.write_atomic(RuntimeArea::Data, SNAPSHOT, b"[]")?;
        context.write_atomic(RuntimeArea::Data, OWNER, b"{}")?;
        hub.refresh_owner();
        tokio::spawn(hub.clone().pump());
        Ok(hub)
    }

    fn save(&self, state: &State) -> Result<(), String> {
        let entries: Vec<Value> = state.entries.values().map(|p| json!({"key":p.key,"sessionId":p.session,"turn":p.turn,"epoch":p.epoch,"expiresAt":p.expires_ms,"outcomeUnknown":p.outcome_unknown,"display":p.display()})).collect();
        self.context.write_atomic(
            RuntimeArea::Data,
            SNAPSHOT,
            serde_json::to_string(&entries)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
    }

    fn rpc(&self) -> Result<crate::dsh::Rpc, String> {
        let base = self
            .context
            .dependency_url("dsh")
            .ok_or("runtime DSH dependency is unavailable")?;
        crate::dsh::Rpc::connect_with_runtime(&self.context, base)
    }

    fn refresh_owner(&self) {
        let Ok(observed) = self
            .context
            .read_optional_text(RuntimeArea::Data, "mode.json")
        else {
            return;
        };
        let Ok(rpc) = self.rpc() else { return };
        let Ok(settings) = rpc.call("settings/describe", json!({})) else {
            return;
        };
        let Some(ns) = settings["namespaces"]
            .as_array()
            .and_then(|items| items.iter().find(|n| n["ns"] == "qq-mode"))
        else {
            return;
        };
        let Some(mode) = ns
            .get("user")
            .and_then(|v| v.get("mode"))
            .or_else(|| ns.get("value").and_then(|v| v.get("mode")))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(mode) = crate::config::normalize_mode(mode) else {
            return;
        };
        match crate::console::apply_dsh_mode(&self.context, mode, observed) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                eprintln!("[closed-agent] DSH mode update failed: {error}");
                return;
            }
        }
        let Some(owner) = ns
            .get("user")
            .and_then(|v| v.get("ownerQQ"))
            .or_else(|| ns.get("value").and_then(|v| v.get("ownerQQ")))
        else {
            return;
        };
        if owner.is_null() || owner.as_str().is_some_and(|s| s.trim().is_empty()) {
            return;
        }
        let Ok((normalized, _)) = crate::config::canonical_config(&json!({"ownerQQ":owner}))
        else {
            return;
        };
        if let Err(error) =
            crate::console::apply_closed_agent_owner(&self.context, &normalized["ownerQQ"])
        {
            eprintln!("[closed-agent] DSH owner policy update failed: {error}");
        }
    }

    fn respond(&self, p: &Pending, value: Value) -> Result<(), String> {
        self.rpc()?.call("$events/result", json!({"clientId":p.client,"eventId":p.event,"outcome":{"kind":"result","value":value}})).map(|_| ())
    }

    fn cancel(&self, p: &Pending) {
        let result = if p.kind == "question" {
            json!({"answers":[]})
        } else {
            json!("rejected")
        };
        if let Err(e) = self.respond(p, result) {
            eprintln!("[closed-agent] cancellation receipt failed: {e}");
        }
    }

    fn say(&self, p: &Pending, text: &str) {
        self.say_guarded(p, text, false, false);
    }

    fn say_guarded(&self, p: &Pending, text: &str, allow_expired: bool, allow_ended: bool) {
        let Ok(cfg) = self.context.load_config() else {
            return;
        };
        let Some(url) = self.context.dependency_url("onebot") else {
            return;
        };
        let token = cfg
            .pointer("/snowluma/accessToken")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Ok(ob) = crate::onebot::OneBot::try_new_url(url, token.to_string()) else {
            return;
        };
        let report = crate::chat::send_recorded(
            &self.context,
            &ob,
            &p.key,
            &p.epoch,
            "interaction",
            text,
            crate::chat::send_delay_ms(&cfg),
            &|| {
                let current = if allow_ended {
                    valid_session(&self.context, &p.key, &p.session, &p.epoch)
                } else {
                    valid_identity(&self.context, &p.key, &p.session, &p.turn, &p.epoch)
                };
                if (allow_expired || Instant::now() < p.expires) && current {
                    Ok(())
                } else {
                    Err("interaction is no longer valid".into())
                }
            },
        );
        if let Some(e) = report.error {
            eprintln!("[closed-agent] interaction delivery failed: {e}");
        }
    }

    async fn register(&self, client: &str, frame: &Value) {
        let Some(session) = frame.get("agentId").and_then(Value::as_str) else {
            return;
        };
        let Some(event) = frame
            .get("eventId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let kind = match frame.get("event").and_then(Value::as_str) {
            Some("user-questions/request") => "question",
            Some("approval/request") => "approval",
            _ => return,
        };
        let Ok(mapping) = crate::session_map::all(&self.context) else {
            return;
        };
        let Some((key, _)) = mapping.into_iter().find(|(_, sid)| sid == session) else {
            return;
        };
        let Ok(records) = crate::turn_ledger::read_records(&self.context) else {
            return;
        };
        let (turns, _) = crate::turn_ledger::fold(&records);
        let Some(turn) = turns
            .into_iter()
            .rev()
            .find(|t| t.key == key && t.session_id == session && t.state.is_open())
        else {
            return;
        };
        let epoch = turn.epoch.clone();
        if !valid_identity(&self.context, &key, session, &turn.id, &epoch) {
            return;
        }
        let Ok(cfg) = self.context.load_config() else {
            return;
        };
        let timeout = cfg
            .get("questionTimeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(300_000);
        let questions = frame
            .pointer("/request/questions")
            .cloned()
            .unwrap_or_else(|| json!([]));
        if kind == "question" && !questions.is_array() {
            return;
        }
        let p = Pending {
            key: key.clone(),
            session: session.into(),
            turn: turn.id,
            epoch,
            client: client.into(),
            event: event.into(),
            kind: kind.into(),
            questions,
            tool: frame
                .pointer("/request/toolName")
                .cloned()
                .unwrap_or(Value::Null),
            outcome_unknown: false,
            expires: Instant::now() + Duration::from_millis(timeout),
            expires_ms: (crate::parity_clock::now_ms() as i64)
                .saturating_add(timeout.min(i64::MAX as u64) as i64),
        };
        #[cfg(feature = "deadlock-regression")]
        let mut state = if event == "question-next" && deadlock_probe::enabled() {
            let future = self.state.lock();
            tokio::pin!(future);
            let mut first = true;
            std::future::poll_fn(|cx| {
                let result = std::future::Future::poll(future.as_mut(), cx);
                if first {
                    first = false;
                    deadlock_probe::signal(if result.is_pending() {
                        "register-pending"
                    } else {
                        "register-ready"
                    });
                }
                result
            })
            .await
        } else {
            self.state.lock().await
        };
        #[cfg(not(feature = "deadlock-regression"))]
        let mut state = self.state.lock().await;
        // A busy gate must not drop the question: wait for it, and settle it explicitly if it stays busy.
        let _gate = match lock_wait(&self.context, GATE_WAIT).await {
            Ok(gate) => gate,
            Err(e) => {
                eprintln!("[closed-agent] question not registered: {e}");
                self.cancel(&p);
                return;
            }
        };
        if !p.valid(&self.context) {
            return;
        }
        if !state.seen.insert((p.client.clone(), p.event.clone())) {
            return;
        }
        if let Some(old) = state.entries.remove(&key) {
            if !old.outcome_unknown {
                self.cancel(&old);
            }
        }
        state.entries.insert(key, p.clone());
        if let Err(e) = self.save(&state) {
            state.entries.remove(&p.key);
            self.cancel(&p);
            eprintln!("[closed-agent] pending state write failed: {e}");
            return;
        }
        let text = if kind == "question" {
            let lines = p
                .questions
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, q)| {
                    let mut line = format!("{}. {}", i + 1, q["question"].as_str().unwrap_or(""));
                    if let Some(options) = q["options"].as_array().filter(|o| !o.is_empty()) {
                        line.push_str("\n   ");
                        line.push_str(
                            &options
                                .iter()
                                .map(|o| format!("「{}」", o["label"].as_str().unwrap_or("")))
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("❓ agent 需要你回答：\n{lines}\n（直接回复选项文字或输入你的回答）")
        } else {
            let reason = frame
                .pointer("/request/reason")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .map(|r| format!("\n理由：{r}"))
                .unwrap_or_default();
            format!(
                "🔐 agent 请求审批：{}{reason}\n回复「通过」或「拒绝」",
                p.tool.as_str().unwrap_or("")
            )
        };
        self.say(&p, &text);
    }

    /// Returns true if the inbound message was consumed as a pending answer.
    pub async fn answer(&self, key: &str, text: &str) -> bool {
        let mut state = self.state.lock().await;
        let Some(p) = state.entries.get(key).cloned() else {
            return false;
        };
        // A busy gate must not swallow the answer: wait for it, and tell the owner if it stays busy.
        let _gate = match lock_wait(&self.context, GATE_WAIT).await {
            Ok(gate) => gate,
            Err(e) => {
                eprintln!("[closed-agent] answer not submitted: {e}");
                self.say_guarded(&p, &format!("⚠️ 系统繁忙，回答未提交（{key}），请稍后再回复一次。"), false, true);
                return true;
            }
        };
        if !p.valid(&self.context) {
            state.entries.remove(key);
            let _ = self.save(&state);
            if !p.outcome_unknown {
                self.cancel(&p);
            }
            return false;
        }
        if p.outcome_unknown {
            self.say(&p, "⚠️ 回执结果不明，请先在 DSH 核对或重置会话。");
            return true;
        }
        let value = if p.kind == "question" {
            let answers: Vec<Value> = p
                .questions
                .as_array()
                .unwrap()
                .iter()
                .map(|q| {
                    let hit = q["options"].as_array().and_then(|opts| {
                        opts.iter().find_map(|o| {
                            o["label"].as_str().filter(|label| {
                                label.trim().to_lowercase() == text.trim().to_lowercase()
                            })
                        })
                    });
                    match hit {
                        Some(label) => json!({"id":q["id"],"selected":[label]}),
                        None => json!({"id":q["id"],"selected":[],"custom":text}),
                    }
                })
                .collect();
            json!({"answers":answers})
        } else {
            let t = text.trim().to_lowercase();
            let result = if ["通过", "同意", "允许", "批准", "yes", "y", "approve", "ok"]
                .contains(&t.as_str())
            {
                "allowed-once"
            } else if [
                "拒绝",
                "不同意",
                "不允许",
                "驳回",
                "no",
                "n",
                "reject",
                "deny",
            ]
            .contains(&t.as_str())
            {
                "rejected"
            } else {
                self.say(
                    &p,
                    &format!("请回复「通过」或「拒绝」来决定这个审批（{key}）"),
                );
                return true;
            };
            json!(result)
        };
        // Claim before the external call: an ambiguous transport failure must not send a second result.
        state.entries.remove(key);
        if let Err(e) = self.save(&state) {
            state.entries.insert(key.into(), p);
            eprintln!("[closed-agent] pending state write failed: {e}");
            return true;
        }
        if let Err(e) = self.respond(&p, value.clone()) {
            if e.starts_with("$events/result 被拒：") && p.valid(&self.context) {
                state.entries.insert(key.into(), p.clone());
                if let Err(error) = self.save(&state) {
                    eprintln!("[closed-agent] pending state write failed: {error}");
                }
                let message = if p.kind == "question" {
                    format!("⚠️ 回答提交失败（{key}），请再回复一次。")
                } else {
                    format!("⚠️ 审批回执提交失败（{key}），请再回复一次「通过」或「拒绝」。")
                };
                self.say(&p, &message);
            } else {
                eprintln!("[closed-agent] answer outcome unknown: {e}");
                let mut unknown = p.clone();
                unknown.outcome_unknown = true;
                if Instant::now() < p.expires
                    && valid_session(&self.context, &p.key, &p.session, &p.epoch)
                {
                    state.entries.insert(key.into(), unknown);
                    if let Err(error) = self.save(&state) {
                        eprintln!("[closed-agent] pending state write failed: {error}");
                    }
                    self.say_guarded(
                        &p,
                        "⚠️ 回执结果不明，请先在 DSH 核对或重置会话。",
                        true,
                        true,
                    );
                }
            }
        } else if p.kind == "approval" {
            // Frozen JS fails to call its out-of-scope notifier before it can
            // remove an answered approval. Its timer and reset still remove it.
            let retained = p.clone();
            if retained.valid(&self.context) {
                state.entries.insert(key.into(), retained);
                if let Err(error) = self.save(&state) {
                    eprintln!("[closed-agent] pending state write failed: {error}");
                }
            }
        }
        true
    }

    async fn reap(&self) {
        // The stream may already be queued for this mutex in register().
        // Waiting here would suspend the stream that must acquire it first.
        // A later tick retries maintenance after the current holder releases it.
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if state.entries.is_empty() {
            return;
        }
        // Maintenance never waits: a later tick retries while the gate holder finishes.
        let Ok(Some(_gate)) = try_lock(&self.context) else {
            return;
        };
        let keys: Vec<String> = state
            .entries
            .iter()
            .filter(|(_, p)| {
                if p.outcome_unknown {
                    Instant::now() >= p.expires
                        || !valid_session(&self.context, &p.key, &p.session, &p.epoch)
                } else {
                    !p.valid(&self.context)
                }
            })
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return;
        }
        for key in keys {
            if let Some(p) = state.entries.remove(&key) {
                if p.outcome_unknown {
                    continue;
                }
                if Instant::now() >= p.expires {
                    self.say_guarded(
                        &p,
                        &format!("⏰ 等待回答超时，已取消该请求（{key}）"),
                        true,
                        // Pending requests outlive their turn; JS notifies regardless.
                        true,
                    );
                }
                self.cancel(&p);
            }
        }
        if let Err(e) = self.save(&state) {
            eprintln!("[closed-agent] pending state write failed: {e}");
        }
    }

    async fn pump(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut settings_tick = tokio::time::interval(Duration::from_secs(5));
        #[cfg(feature = "deadlock-regression")]
        let mut probe_armed = false;
        // JS event-stream reconnect: 3 s doubling to 30 s, restarted only by a stream that delivered frames, one warning
        // at the third failure in a row (a long DSH outage must not become a request storm).
        let mut backoff = crate::dependency::ReconnectBackoff::event_stream();
        let traffic = std::sync::atomic::AtomicBool::new(false);
        loop {
            traffic.store(false, std::sync::atomic::Ordering::SeqCst);
            let mut stream_error = String::from("stream ended");
            let stream = self.stream(&traffic);
            tokio::pin!(stream);
            loop {
                #[cfg(feature = "deadlock-regression")]
                tokio::select! {
                    result = &mut stream => {
                        if let Err(e) = result {
                            eprintln!("[closed-agent] DSH event stream: {e}");
                            stream_error = e;
                        }
                        break;
                    }
                    trigger = deadlock_probe::control(probe_armed) => {
                        probe_armed = !trigger;
                        if trigger { deadlock_probe::poll_reap(self.reap()).await; }
                    }
                    _ = tick.tick(), if !deadlock_probe_armed!(probe_armed) => self.reap().await,
                    _ = settings_tick.tick() => self.refresh_owner(),
                }
                #[cfg(not(feature = "deadlock-regression"))]
                tokio::select! {
                    result = &mut stream => {
                        if let Err(e) = result {
                            eprintln!("[closed-agent] DSH event stream: {e}");
                            stream_error = e;
                        }
                        break;
                    }
                    _ = tick.tick() => self.reap().await,
                    _ = settings_tick.tick() => self.refresh_owner(),
                }
            }
            if traffic.load(std::sync::atomic::Ordering::SeqCst) {
                backoff.on_traffic();
            }
            let wait_ms = backoff.on_failure(crate::dependency::jitter_unit(crate::parity_clock::now_ms(), u64::from(backoff.streak())));
            crate::recovery_log::record(
                &self.context,
                crate::run_record::Trigger::Timer,
                crate::run_record::Component::Dsh,
                "event-stream-closed",
                crate::run_record::Outcome::Failed,
                &format!("closed-agent DSH event stream ended: {stream_error}; reconnecting in {} s", wait_ms / 1000),
            );
            if backoff.should_warn() {
                eprintln!("[closed-agent] ⚠️ DSH 事件流已连续 {} 次断开，继续按退避重连", backoff.streak());
            }
            let retry_at = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
            loop {
                #[cfg(feature = "deadlock-regression")]
                tokio::select! {
                    _ = tokio::time::sleep_until(retry_at) => break,
                    trigger = deadlock_probe::control(probe_armed) => {
                        probe_armed = !trigger;
                        if trigger { deadlock_probe::poll_reap(self.reap()).await; }
                    }
                    _ = tick.tick(), if !deadlock_probe_armed!(probe_armed) => self.reap().await,
                    _ = settings_tick.tick() => self.refresh_owner(),
                }
                #[cfg(not(feature = "deadlock-regression"))]
                tokio::select! {
                    _ = tokio::time::sleep_until(retry_at) => break,
                    _ = tick.tick() => self.reap().await,
                    _ = settings_tick.tick() => self.refresh_owner(),
                }
            }
        }
    }

    async fn stream(&self, traffic: &std::sync::atomic::AtomicBool) -> Result<(), String> {
        let rpc = self.rpc()?;
        let url = format!(
            "{}/api/remote.mux",
            rpc.base.replacen("http://", "ws://", 1)
        );
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| e.to_string())?;
        request.headers_mut().insert(
            "Cookie",
            rpc.cookie().parse().map_err(|_| "invalid DSH cookie")?,
        );
        let (mut ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| "websocket handshake timed out".to_string())?
        .map_err(|e| e.to_string())?;
        let stream = crate::dsh::mint_request_id();
        ws.send(Message::Text(
            json!({"type":"open","streamId":stream,"endpoint":"$events","payload":{"args":{}}})
                .to_string(),
        ))
        .await
        .map_err(|e| e.to_string())?;
        let mut client = String::new();
        loop {
            tokio::select! {
                item = ws.next() => {
                    let text = match item {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Err("stream closed".into()),
                        Some(Ok(_)) => continue,
                    };
                    let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                    if frame["streamId"] != stream || frame["type"] != "item" { continue; }
                    let value = &frame["value"];
                    traffic.store(true, std::sync::atomic::Ordering::SeqCst);
                    if value["type"] == "ready" { client = value["clientId"].as_str().unwrap_or("").to_string(); }
                    else if value["type"] == "waterfall" && !client.is_empty() { self.register(&client, value).await; }
                }
            }
        }
    }
}
