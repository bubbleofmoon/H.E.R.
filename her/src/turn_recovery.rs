//! Restart reconciliation of unfinished turns (ticket 10).
//!
//! At startup the ledger (`turn_ledger`) lists turns that never reached an end record, and the
//! inbound journal (`inbound`) lists events found `interrupted`. Before anything is re-run, each
//! turn is checked against what DSH actually holds (`dsh::probe_prompt`, keyed by the JSON-RPC id
//! DSH stored as `source.rpcId`) and against the send outbox (`send_outbox`, linked by agent-token
//! epoch). The verdict is one of:
//!
//! * finished in DSH (completed, failed with side effects, cancelled): recorded, never redone;
//! * still running or queued in DSH: re-attached, waiting for DSH to finish it, never resubmitted;
//! * provably never executed and still valid: its events go back to the normal replay path;
//! * provably never executed but no longer valid (session reset, token changed, chat removed):
//!   abandoned with the reason;
//! * DSH cannot be asked: paused as `dsh-unreachable` and retried; it never resumes unchecked;
//! * anything else unprovable: paused for the administrator, the agent token is rotated so late
//!   results are refused, and nothing is cleared, rebuilt or resubmitted.
//!
//! Console, private owner commands and expiry share one paused-turn resolution path; this task applies it.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::dsh::{PromptProbe, Rpc, TurnFacts};
use crate::inbound::{Journal, Replayable};
use crate::runtime::RuntimeContext;
use crate::send_outbox::Batch;
use crate::turn_ledger::{self, Recorder, State, Turn};

const PASS_INTERVAL: Duration = Duration::from_secs(2);
const UNREACHABLE_RETRY_MS: i64 = 10_000;
const DEFAULT_WAIT_SECS: u64 = 900;

/// Anchor expiry to the first durable pause, never to a recovery pass or process start.
fn auto_abandon_at(cfg: &Value, turn: &Turn) -> Option<i64> {
    let minutes = cfg
        .pointer("/socialV2/pausedTurnAutoAbandonMinutes")
        .and_then(Value::as_u64)
        .unwrap_or(10);
    if minutes == 0 {
        return None;
    }
    let delay = minutes.saturating_mul(60_000).min(i64::MAX as u64) as i64;
    turn.paused_at.map(|at| at.saturating_add(delay))
}

/// Console and live recovery are separate processes. Serialize decisions and notice claims,
/// with fresh reads after this gate, so only one decision wins.
fn resolution_gate(context: &RuntimeContext) -> Result<std::fs::File, (u16, String)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match context.open_locked_file(
            crate::runtime::RuntimeArea::Data,
            "locks/turn-resolution.lock",
        ) {
            Ok(gate) => return Ok(gate),
            Err(error) if error == "runtime file is already locked" => {
                if std::time::Instant::now() >= deadline {
                    return Err((503, "回合处理正忙，稍后重试".to_string()));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err((503, error)),
        }
    }
}

/// All three resolution sources share the console's validation and revocation path.
pub fn resolve(
    context: &RuntimeContext,
    id: &str,
    action: &str,
    confirmed: bool,
    by: &str,
) -> Result<Value, (u16, String)> {
    if id.is_empty() || !matches!(action, "abandon" | "retry") {
        return Err((
            400,
            "需要 turnId，且 action 为 abandon 或 retry".to_string(),
        ));
    }
    let _gate = resolution_gate(context)?;
    let records = turn_ledger::read_records(context).map_err(|e| (503, e))?;
    let (turns, _) = turn_ledger::fold(&records);
    let turn = turns
        .iter()
        .find(|t| t.id == id)
        .ok_or((404, "回合不存在".to_string()))?;
    if turn.state != State::Paused {
        return Err((
            409,
            format!(
                "回合当前状态为 {}，只有暂停的回合可以处理",
                turn.state.label()
            ),
        ));
    }
    if by == "auto" {
        let cfg = context.load_config().map_err(|e| (503, e.to_string()))?;
        if action != "abandon" || !auto_abandon_at(&cfg, turn).is_some_and(|at| now() >= at) {
            return Err((409, "尚未到自动放弃时间".to_string()));
        }
    }
    if action == "retry" && turn.pause_reason == "send-unresolved" && !confirmed {
        return Err((409, "该回合有发送结果未知的消息，重试可能重复发送；确认可以接受后请带 confirm:true 再次提交".to_string()));
    }
    if action == "abandon" && turn.pause_reason == "send-unresolved" {
        crate::social_state::ensure_or_rotate(context, &turn.key, true)
            .map_err(|e| (500, format!("会话令牌未能撤销，未放弃回合：{e}")))?;
    }
    Recorder::attach(context, id, &turn.key)
        .resolved_by(action, by)
        .map_err(|e| (500, e))?;
    Ok(serde_json::json!({"ok":true,"turnId":id,"action":action,"confirmed":confirmed}))
}

/// Recovery notices have their own bounded worker: a burst of paused turns must not consume
/// the JS alert budget or be lost to its hourly cap. Unknown delivery is never retried.
fn private_notice(
    context: &RuntimeContext,
    cfg: &Value,
    text: String,
    claim: impl FnOnce() -> Result<(), (u16, String)>,
) -> Result<(), (u16, String)> {
    type Job = (
        RuntimeContext,
        Value,
        i64,
        String,
        std::sync::mpsc::Receiver<()>,
    );
    static SENDER: OnceLock<std::sync::mpsc::SyncSender<Job>> = OnceLock::new();
    let Some(owner) = crate::admin_alert::owner_id(cfg) else {
        return Ok(());
    };
    let body = crate::admin_alert::prepare_body(&text, |t| crate::activity::redact(context, t));
    let sender = SENDER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(32);
        std::thread::Builder::new()
            .name("turn-notice".into())
            .spawn(move || {
                while let Ok((context, cfg, owner, body, committed)) = rx.recv() {
                    if committed.recv().is_err() {
                        continue;
                    }
                    let result = crate::console::media_onebot(&context, &cfg)
                        .and_then(|ob| ob.send_one(&crate::onebot::Target::Private(owner), &body));
                    if let Err(error) = result {
                        crate::safe_println!(
                            "[recovery] 私聊通知失败：{}",
                            crate::activity::redact(&context, &error)
                        );
                    }
                }
            })
            .expect("turn notice worker starts");
        tx
    });
    // Reserve bounded queue space before the durable claim. A full queue keeps the notice
    // pending for the next pass; a failed claim drops the permit without sending anything.
    let (commit, committed) = std::sync::mpsc::channel();
    sender
        .try_send((context.clone(), cfg.clone(), owner, body, committed))
        .map_err(|_| (503, "私聊通知队列正忙".to_string()))?;
    claim()?;
    let _ = commit.send(());
    Ok(())
}

/// Claim a notice durably before sending. A restart never repeats a potentially delivered notice.
fn notify_turn(
    context: &RuntimeContext,
    cfg: &Value,
    id: &str,
    resolved: bool,
) -> Result<(), (u16, String)> {
    if crate::admin_alert::owner_id(cfg).is_none() {
        return Ok(());
    }
    let _gate = resolution_gate(context)?;
    let (turns, _) = turn_ledger::fold(&turn_ledger::read_records(context).map_err(|e| (503, e))?);
    let Some(turn) = turns.iter().find(|t| t.id == id) else {
        return Ok(());
    };
    let body = if resolved {
        if !turn.state.is_final() || turn.paused_at.is_none() || turn.resolution_notified {
            return Ok(());
        }
        let by = if turn.state == State::Resolved {
            turn.resolution_by.as_str()
        } else {
            "recovery"
        };
        let action = turn.resolution.as_deref().unwrap_or(turn.state.label());
        format!(
            "[{}] 回合 {} 已处理：{}（{}）",
            turn.key,
            turn.code(),
            action,
            by
        )
    } else {
        if turn.paused_at.is_none() || turn.pause_notified {
            return Ok(());
        }
        let expiry = auto_abandon_at(cfg, turn)
            .map(|at| {
                let zone = crate::local_time::configured_zone(cfg);
                let time = crate::local_time::at(at, &zone);
                let deadline = if jiff::Timestamp::from_millisecond(at).is_ok() {
                    crate::local_time::display(&time)
                } else {
                    format!("时间戳 {at}ms")
                };
                format!("{}（{}）自动放弃", deadline, crate::local_time::label(&time))
            })
            .unwrap_or_else(|| "自动放弃已禁用".to_string());
        format!(
            "[{}] 回合暂停：{}；回合码 {}；{}。私聊命令：放弃 {} / 重试 {}",
            turn.key,
            turn.last_pause_reason,
            turn.code(),
            expiry,
            turn.code(),
            turn.code()
        )
    };
    private_notice(context, cfg, body, || {
        Recorder::attach(context, id, &turn.key)
            .notified(resolved)
            .map_err(|e| (500, e))
    })
}

/// A recovery command is exactly `<verb> <code>` or `<retry verb> <code> <confirm word>`, where the
/// verbs are `放弃`/`abandon`/`重试`/`retry`, the code is the eight hex digits shown in the pause
/// alert and the confirm word is `确认`/`confirm`. Any other message, including one that merely
/// starts like a command ("retry later", "重试 20261109 的任务"), is ordinary input.
fn command_action(words: &[&str]) -> Option<&'static str> {
    let [verb, code, tail @ ..] = words else { return None };
    if code.len() != 8 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let action = match *verb {
        word if word == "放弃" || word.eq_ignore_ascii_case("abandon") => "abandon",
        word if word == "重试" || word.eq_ignore_ascii_case("retry") => "retry",
        _ => return None,
    };
    match tail {
        [] => Some(action),
        [word] if action == "retry" && (*word == "确认" || word.eq_ignore_ascii_case("confirm")) => Some(action),
        _ => None,
    }
}

/// Whether `text` is exactly an owner recovery command (`放弃 <code>`, `重试 <code> [确认]`, ASCII forms).
pub(crate) fn is_recovery_command(text: &str) -> bool {
    command_action(&text.split_whitespace().collect::<Vec<_>>()).is_some()
}

/// Consume owner recovery commands before any mode-specific ingress or model dispatch.
pub async fn owner_command(
    context: &RuntimeContext,
    journal: &crate::inbound::SharedJournal,
    ev: &crate::Event,
    raw: &str,
    replayed: bool,
) -> Result<bool, String> {
    if !context.is_isolated() || ev.post_type != "message" || ev.message_type != "private" {
        return Ok(false);
    }
    let (plain, _, _) = crate::plain_text(&ev.message, ev.self_id.unwrap_or(0));
    let body = if plain.is_empty() {
        ev.raw_message.as_str()
    } else {
        plain.as_str()
    };
    let words: Vec<&str> = body.split_whitespace().collect();
    let Some(action) = command_action(&words) else {
        return Ok(false);
    };
    let cfg = context.load_config().map_err(str::to_string)?;
    let Some(owner) = crate::admin_alert::owner_id(&cfg) else {
        return Ok(false);
    };
    if ev.user_id != Some(owner)
        || ev
            .sender
            .as_ref()
            .and_then(|s| s.user_id)
            .is_some_and(|id| id != owner)
    {
        return Ok(false);
    }
    let key = format!("private:{owner}");
    let event = crate::inbound::event_key(
        &key,
        &ev.post_type,
        crate::event_message_id(ev).as_deref(),
        raw,
    );
    if !replayed
        && journal
            .record_received(event.clone(), key, raw.to_string(), now())
            .await?
            == crate::inbound::Receipt::Duplicate
    {
        return Ok(true);
    }
    // Mark consumed before executing: neither a crash nor reconnect may send it to a model.
    journal.mark_handled(vec![event], now()).await?;
    let result = (|| -> Result<Option<String>, (u16, String)> {
        let confirmed = words.len() == 3;
        let records = turn_ledger::read_records(context).map_err(|e| (503, e))?;
        let (turns, _) = turn_ledger::fold(&records);
        let mut matches = turns
            .iter()
            .filter(|turn| turn.code().eq_ignore_ascii_case(words[1]));
        let turn = matches.next();
        let turn = match turn {
            Some(turn) if matches.next().is_none() && turn.state == State::Paused => turn,
            _ => return Ok(Some("回合码不存在或已失效。".to_string())),
        };
        if action == "retry" && turn.pause_reason == "send-unresolved" && !confirmed {
            return Ok(Some(format!(
                "发送结果未知，重试可能重复发送。确认请发：重试 {} 确认",
                turn.code()
            )));
        }
        resolve(context, &turn.id, action, confirmed, "chat")?;
        // The recovery task sends the one resolution notice after the durable decision.
        Ok(None)
    })();
    let reply = match result {
        Ok(reply) => reply,
        Err((status, error)) => {
            crate::safe_println!(
                "[recovery] 私聊命令处理失败：{}",
                crate::activity::redact(context, &error)
            );
            Some(if matches!(status, 404 | 409) {
                "回合码不存在或已失效。".to_string()
            } else {
                "处理失败，请稍后重试。".to_string()
            })
        }
    };
    if let Some(reply) = reply {
        if let Err((_, error)) = private_notice(context, &cfg, reply, || Ok(())) {
            crate::safe_println!("[recovery] 私聊回复失败：{error}");
        }
    }
    Ok(true)
}

fn resume_queue() -> &'static Mutex<VecDeque<Replayable>> {
    static QUEUE: OnceLock<Mutex<VecDeque<Replayable>>> = OnceLock::new();
    QUEUE.get_or_init(Default::default)
}

fn active_turns() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

/// Events that reconciliation handed back for a fresh turn; the main loop replays them through
/// its normal path (decision, batching, wake), so every current validity check applies again.
pub fn take_resumable() -> Vec<Replayable> {
    let mut queue = resume_queue().lock().unwrap_or_else(|p| p.into_inner());
    queue.drain(..).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendState {
    /// The turn has no send batch.
    None,
    /// Every batch item is settled.
    Settled,
    /// Items that may or may not have been delivered, or a half-finished batch.
    Unresolved(String),
    /// The send ledger could not be read, so what was sent is unknown.
    Unreadable(String),
}

pub fn send_state(batches: &[Batch]) -> SendState {
    if batches.is_empty() {
        return SendState::None;
    }
    let mut problems = Vec::new();
    for batch in batches {
        let unknown = batch.indices_where(|s| s.needs_verification());
        let open = batch.indices_where(|s| s.resendable());
        if !unknown.is_empty() || (batch.confirmed_count() > 0 && !open.is_empty()) {
            problems.push(format!(
                "批次 {} 未结清（待核实 {:?}，未发出 {:?}）；用 resumeBatchId 处理",
                batch.id, unknown, open
            ));
        }
    }
    if problems.is_empty() {
        SendState::Settled
    } else {
        SendState::Unresolved(problems.join("；"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Never executed and still valid: hand its events back.
    Resume(String),
    /// Never executed and no longer valid, or ended by a decision: close it.
    Abandon(String),
    Complete(String),
    Fail(String),
    Cancel(String),
    /// Running or queued in DSH: wait for DSH.
    Reattach,
    /// DSH cannot be asked right now: keep, retry later.
    Defer(String),
    Pause { reason: String, detail: String, rotate: bool },
}

fn kind_of_end(ended: &Value) -> &str {
    ended.pointer("/reason/kind").and_then(Value::as_str).unwrap_or("")
}

fn pause(reason: &str, detail: impl Into<String>, rotate: bool) -> Verdict {
    Verdict::Pause { reason: reason.to_string(), detail: detail.into(), rotate }
}

/// The decision table. `validity` is `Ok` while the turn may still run (same session mapping,
/// same agent token, conversation still allowed); `probe` is `None` for a turn that never
/// reached the point of calling DSH.
pub fn decide(turn: &Turn, probe: Option<&PromptProbe>, sends: &SendState, validity: Result<(), String>) -> Verdict {
    preserve_finalization_claim(turn, decide_evidence(turn, probe, sends, validity))
}

/// Reconciliation still proves external work/sends before claiming a terminal. Once proven,
/// a handler lost to a crash cannot lose its first claim or replay its already claimed input.
fn preserve_finalization_claim(turn: &Turn, verdict: Verdict) -> Verdict {
    if !matches!(verdict, Verdict::Resume(_) | Verdict::Abandon(_) | Verdict::Complete(_) | Verdict::Fail(_) | Verdict::Cancel(_)) {
        return verdict;
    }
    match turn.finalization_claim() {
        Some(("cancel-requested", "observationSleep")) => Verdict::Complete("observationSleep".into()),
        Some(("cancel-requested", reason)) => Verdict::Cancel(reason.into()),
        Some(("completion-observed", _)) => match verdict {
            Verdict::Complete(_) | Verdict::Fail(_) => verdict,
            _ => Verdict::Complete("DSH completion observed before handler loss".into()),
        },
        _ => verdict,
    }
}

fn decide_evidence(turn: &Turn, probe: Option<&PromptProbe>, sends: &SendState, validity: Result<(), String>) -> Verdict {
    if turn.attempts.iter().any(|a| a.rpc_id.starts_with("cli:")) {
        return pause(
            "cli-send-unverifiable",
            "文本后端的代发可能已发出，无法核实，不重跑",
            true,
        );
    }
    let not_executed = |validity: Result<(), String>| match (sends, validity) {
        (SendState::None, Ok(())) => Verdict::Resume("未执行且仍有效".to_string()),
        (SendState::None, Err(why)) => Verdict::Abandon(format!("未执行但已失效：{why}")),
        (SendState::Unreadable(detail), _) => pause("send-unresolved", detail.clone(), true),
        (_, _) => pause("inconsistent-state", "回合未提交到 DSH，却已有发送批次；状态矛盾，交管理员", true),
    };
    if turn.attempts.is_empty() {
        return not_executed(validity);
    }
    let Some(probe) = probe else {
        return Verdict::Defer("尚未核对".to_string());
    };
    match probe {
        PromptProbe::Unreachable(why) => Verdict::Defer(why.clone()),
        PromptProbe::NotFound { proved: false } => pause(
            "dsh-history-incomplete",
            "DSH 日志被截断且队列里没有这条 prompt，无法证明它没有执行",
            true,
        ),
        PromptProbe::NotFound { proved: true } | PromptProbe::SessionGone | PromptProbe::RemovedFromQueue => {
            not_executed(validity)
        }
        PromptProbe::Queued => Verdict::Reattach,
        PromptProbe::Turn(TurnFacts { ended: None, .. }) => Verdict::Reattach,
        PromptProbe::Turn(TurnFacts { ended: Some(ended), tools, .. }) => {
            match sends {
                SendState::Unresolved(detail) => return pause("send-unresolved", detail.clone(), false),
                SendState::Unreadable(detail) => return pause("send-unresolved", detail.clone(), true),
                _ => {}
            }
            match kind_of_end(ended) {
                "completed" => Verdict::Complete("DSH 已完成该回合".to_string()),
                "error" => {
                    if tools.is_empty() && *sends == SendState::None {
                        match validity {
                            Ok(()) => Verdict::Resume("DSH 回合出错收尾，没有工具调用和发送，可安全重做".to_string()),
                            Err(why) => Verdict::Abandon(format!("回合出错收尾且已失效：{why}")),
                        }
                    } else {
                        Verdict::Fail("DSH 回合出错收尾，且已有工具调用或发送，不重做".to_string())
                    }
                }
                other => Verdict::Cancel(format!("DSH 回合以 {other} 收尾")),
            }
        }
    }
}

fn dsh_base(context: &RuntimeContext) -> Result<String, String> {
    context
        .dependency_url("dsh")
        .map(str::to_string)
        .ok_or_else(|| "runtime DSH dependency is unavailable".to_string())
}

fn wait_secs(cfg: &Value) -> u64 {
    cfg.pointer("/socialV2/recovery/waitSecs")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(1, 3600))
        .unwrap_or(DEFAULT_WAIT_SECS)
}

/// Same session mapping, same token epoch, conversation still allowed.
fn validity(context: &RuntimeContext, cfg: &Value, turn: &Turn) -> Result<(), String> {
    if !crate::social_state::inbound_admitted(cfg, &turn.key) {
        return Err("会话已不在允许范围内".to_string());
    }
    if !turn.session_id.is_empty() {
        match crate::session_map::lookup(context, &turn.key)? {
            Some(current) if current == turn.session_id => {}
            _ => return Err("会话映射已变化（重置或替换）".to_string()),
        }
    }
    match crate::social_state::token(context, &turn.key)? {
        Some(token) if turn.epochs.iter().any(|e| *e == crate::send_outbox::token_epoch(&token)) => Ok(()),
        _ => Err("会话令牌已变化（重置、取消或被替代）".to_string()),
    }
}

/// The ledger recorded something at or before `at_ms`, so a turn started then would be in it.
pub fn ledger_covers(records: &[Value], at_ms: i64) -> bool {
    records.iter().filter_map(|r| r.get("t").and_then(Value::as_i64)).min().is_some_and(|first| first <= at_ms)
}

/// 恢复核对时把 DSH 账本事实（快照里的历史 usage 与基线）补写进账本：已记过的按序号跳过，不重复计费。
fn record_harvest(context: &crate::runtime::RuntimeContext, key: &str, session: &str, harvest: &crate::dsh::UsageHarvest) {
    let rows = crate::ledger::harvest_candidates(Some(key), session, harvest, now());
    if let Err(error) = crate::ledger::append_planned(context, &rows) {
        println!("[recovery] 回合核对时补记账本失败：{error}");
    }
}

fn now() -> i64 {
    turn_ledger::now_ms()
}

fn settle(journal: &Journal, keys: &[String], to: &str) -> bool {
    if keys.is_empty() {
        return true;
    }
    match journal.settle_events(keys, to, now()) {
        Ok(()) => true,
        Err(error) => {
            println!("[recovery] ❌ 事件改为 {to} 失败：{error}");
            false
        }
    }
}

/// Return events to the normal replay path. They are queued only after the stage change was
/// stored, and an event already queued is never queued again.
/// A reset of `key` (revocation, session reset, whole reset or policy narrowing) was recorded after
/// turn `id` began.
fn epoch_ended_since(records: &[Value], id: &str, key: &str) -> bool {
    records
        .iter()
        .skip_while(|r| !(r.get("k").and_then(Value::as_str) == Some("begun") && r.get("turn").and_then(Value::as_str) == Some(id)))
        .any(|r| turn_ledger::is_reset_of(r, key))
}

fn hand_back(journal: &Journal, keys: &[String]) {
    if !settle(journal, keys, "received") {
        return;
    }
    match journal.events_by_keys(keys) {
        Ok(events) => {
            let mut queue = resume_queue().lock().unwrap_or_else(|p| p.into_inner());
            for event in events {
                if !queue.iter().any(|queued| queued.event_key == event.event_key) {
                    queue.push_back(event);
                }
            }
        }
        Err(error) => println!("[recovery] ❌ 读取待重放事件失败：{error}"),
    }
}

fn allowed(cfg: &Value, event: &Replayable) -> bool {
    crate::social_state::inbound_admitted(cfg, &event.conversation)
}

fn close_interrupted_harness(
    context: &RuntimeContext,
    turn: &Turn,
    status: &crate::harness::Status,
    reason: Option<&str>,
) -> Result<bool, String> {
    if turn.backend == "harness" {
        return crate::harness::resolve_interrupted_with(context, &turn.id, status, reason);
    }
    Ok(false)
}

fn rearm_reserved2_if_needed(
    context: &RuntimeContext,
    cfg: &Value,
    turn: &Turn,
    live_cutoff: Option<i64>,
) -> Result<(), String> {
    if turn.backend == "harness" && (turn.reserved2 || turn.wake_claim.is_some()) {
        crate::social_state::rearm_interrupted_reserved2_wake(
            context,
            cfg,
            &turn.key,
            turn.wake_claim.as_ref(),
            Some(turn.begun_at),
            Some(live_cutoff.unwrap_or_else(now)),
        )?;
    }
    Ok(())
}

/// Apply one verdict. The ledger record comes first and is the commit point: if it cannot be
/// written nothing is settled or queued for replay (fail closed), and the next pass retries.
fn apply(context: &RuntimeContext, cfg: &Value, journal: &Journal, turn: &Turn, verdict: &Verdict, facts: Option<&TurnFacts>) {
    apply_at(context, cfg, journal, turn, verdict, facts, None);
}

fn apply_at(
    context: &RuntimeContext,
    cfg: &Value,
    journal: &Journal,
    turn: &Turn,
    verdict: &Verdict,
    facts: Option<&TurnFacts>,
    live_cutoff: Option<i64>,
) {
    // A DSH probe can finish after the owner or expiry resolved this turn. Serialize the
    // commit and its hand-back with resolutions; a stale verdict must never replay it.
    let current = resolution_gate(context).and_then(|gate| {
        let records = turn_ledger::read_records(context).map_err(|e| (503, e))?;
        let (turns, _) = turn_ledger::fold(&records);
        let latest = turns.into_iter().find(|t| t.id == turn.id);
        Ok((gate, latest))
    });
    let (_gate, latest) = match current {
        Ok(current) => current,
        Err((_, error)) => {
            crate::safe_println!("[recovery] 无法核对最新回合状态，稍后重试：{error}");
            return;
        }
    };
    let Some(turn) = latest.as_ref().filter(|t| !t.state.is_final()) else { return; };
    let verdict = preserve_finalization_claim(turn, verdict.clone());
    let recorder = Recorder::attach(context, &turn.id, &turn.key);
    let log = |text: &str| println!("[recovery] 回合 {} ({}) {text}", turn.id, turn.key);
    let commit = |result: Result<(), String>| -> bool {
        match result {
            Ok(()) => true,
            Err(error) => {
                println!("[recovery] ❌ 回合 {} 的核对结果写不进账本，不处理事件，稍后重试：{error}", turn.id);
                false
            }
        }
    };
    let finalize_handled = |store_status: crate::harness::Status,
                            store_reason: Option<&str>,
                            outcome: &str,
                            ledger_reason: &str,
                            message: String| {
        if !commit(
            close_interrupted_harness(context, turn, &store_status, store_reason).map(|_| ()),
        ) {
            return;
        }
        if !commit(rearm_reserved2_if_needed(context, cfg, turn, live_cutoff)) {
            return;
        }
        if commit(recorder.end_reconciled(outcome, ledger_reason)) {
            settle(journal, &turn.inbound, "handled");
            log(&message);
        }
    };
    if let Some(facts) = facts {
        let _ = recorder.tools(facts.tools.iter().map(|t| t.to_json()).collect());
        let _ = recorder.dsh_turn(&facts.rpc_id, facts.dsh_turn);
    }
    // A crash can leave cancellation intent without its handler's terminal write.
    // Only a proven terminal verdict can take over; unknown work/sends stay held.
    if matches!(verdict, Verdict::Resume(_) | Verdict::Abandon(_) | Verdict::Complete(_) | Verdict::Fail(_) | Verdict::Cancel(_)) {
        if let Some(("cancel-requested", reason)) = turn.finalization_claim() {
            if !commit(
                crate::social_state::ensure_or_rotate(context, &turn.key, true)
                    .map(|_| ())
                    .map_err(String::from),
            ) {
                return;
            }
            // A clean observation sleep is completed work owned by the cancellation claim.
            let (outcome, store_status) = if reason == "observationSleep" {
                ("completed", crate::harness::Status::ObservationSleep)
            } else {
                ("cancelled", crate::harness::Status::Cancelled)
            };
            finalize_handled(store_status, Some(reason), outcome, reason, format!("已核实取消：{reason}"));
            return;
        }
    }
    match &verdict {
        Verdict::Resume(why) => {
            if !commit(
                close_interrupted_harness(
                    context,
                    turn,
                    &crate::harness::Status::Failed(why.clone()),
                    Some("interrupted"),
                )
                .map(|_| ()),
            ) {
                return;
            }
            // A timer-only turn has no inbound event to hand back. Restore its durable deadline
            // before closing recovery, so another crash at either boundary remains recoverable.
            if let Some(claim) = &turn.wake_claim {
                if !commit(
                    crate::social_state::rollback_reserved2_wake(
                        context,
                        &turn.key,
                        claim["at"].as_i64().unwrap_or(0),
                        &claim["previous"],
                    )
                    .map_err(String::from),
                ) {
                    return;
                }
            } else if !commit(rearm_reserved2_if_needed(context, cfg, turn, live_cutoff)) {
                return;
            }
            let _ = recorder.verified("not-executed", why);
            if commit(recorder.end_reconciled("resumed", why)) {
                // Fault seam for the crash test: the ledger says "resumed" but the events were not handed back yet.
                #[cfg(feature = "parity-clock")]
                if std::env::var("HER_FAULT_EXIT_AFTER_RESUMED").is_ok_and(|v| v == "1") {
                    std::process::exit(86);
                }
                hand_back(journal, &turn.inbound);
                log(&format!("未执行，事件交回重处理：{why}"));
            }
        }
        Verdict::Abandon(why) => {
            // Re-arm any consumed wake deadline BEFORE committing `abandoned` so a transient rearm
            // failure keeps the turn open (even without a SQLite row) for recovery to retry, and
            // commit `abandoned` before closing SQLite so a mid-close crash never flips to Cancelled.
            if !commit(rearm_reserved2_if_needed(context, cfg, turn, live_cutoff)) {
                return;
            }
            if commit(recorder.end_reconciled("abandoned", why)) {
                if !commit(
                    close_interrupted_harness(
                        context,
                        turn,
                        &crate::harness::Status::Cancelled,
                        Some("abandoned"),
                    )
                    .map(|_| ()),
                ) {
                    return;
                }
                settle(journal, &turn.inbound, "handled");
                log(&format!("已放弃：{why}"));
            }
        }
        Verdict::Complete(why) => {
            let store_status = if why == "observationSleep" {
                crate::harness::Status::ObservationSleep
            } else {
                crate::harness::Status::Completed(String::new())
            };
            finalize_handled(store_status, Some(why), "completed", why, format!("已核实完成，不重做：{why}"));
        }
        Verdict::Fail(why) => {
            finalize_handled(
                crate::harness::Status::Failed(why.clone()),
                Some("interrupted"),
                "failed",
                why,
                format!("已核实失败，不重做：{why}"),
            );
        }
        Verdict::Cancel(why) => {
            finalize_handled(
                crate::harness::Status::Cancelled,
                Some(why),
                "cancelled",
                why,
                format!("已核实取消：{why}"),
            );
        }
        Verdict::Reattach | Verdict::Defer(_) => {}
        Verdict::Pause { reason, detail, rotate } => {
            if *rotate {
                if let Err(error) = crate::social_state::ensure_or_rotate(context, &turn.key, true)
                {
                    println!("[recovery] ⚠️ 暂停时换令牌失败 {}：{error}", turn.key);
                }
            }
            if commit(recorder.unknown(reason, detail)) {
                log(&format!("状态不明，已暂停交管理员（{reason}）：{detail}"));
            }
        }
    }
}

struct Pass<'a> {
    context: &'a RuntimeContext,
    cfg: &'a Value,
}

impl Pass<'_> {
    fn sends_of(&self, turn: &Turn, turns: &[Turn], outbox: &Result<Vec<Value>, String>) -> SendState {
        let outbox = match outbox {
            Ok(records) => records,
            Err(error) => return SendState::Unreadable(format!("发送账本无法读取，已发送内容未知：{error}")),
        };
        let next_begin = turns.iter().filter(|t| t.key == turn.key && t.begun_at > turn.begun_at).map(|t| t.begun_at).min();
        send_state(&turn_ledger::batches_of_turn(turn, next_begin, outbox))
    }

    /// Reconcile an interrupted harness turn without DSH probing, failing closed on uncertain
    /// outbox sends or unsettled conversation-store tool calls.
    fn reconcile_harness(&self, turn: &Turn, sends: &SendState) -> Verdict {
        if let SendState::Unresolved(detail) | SendState::Unreadable(detail) = sends {
            return pause("send-unresolved", detail.clone(), true);
        }
        let stored = crate::harness::store::Store::open(self.context).and_then(|store| {
            let Some(status) = store.status(&turn.id)? else {
                return Ok(None);
            };
            let unsettled = store.has_unsettled_tools(&turn.id)?;
            let has_tools = store.has_tool_runs(&turn.id)?;
            Ok(Some((status, unsettled, has_tools)))
        });
        let is_reserved2 = turn.reserved2 || turn.wake_claim.is_some();
        if is_reserved2 {
            if let Err(error) = crate::social_state::mode_checked(self.context) {
                return pause("send-unresolved", error.to_string(), true);
            }
        }
        let (status, unsettled, has_tools) = match stored {
            Ok(Some(row)) => row,
            Ok(None) if is_reserved2 && matches!(sends, SendState::None) && turn.attempts.iter().all(|a| !a.accepted) => {
                let decided = match validity(self.context, self.cfg, turn) {
                    Ok(()) => Verdict::Resume("Harness reserved2 wake interrupted before admission; resuming".into()),
                    Err(why) => Verdict::Abandon(format!("Harness reserved2 wake interrupted and no longer valid: {why}")),
                };
                return preserve_finalization_claim(turn, decided);
            }
            Ok(None) => return pause("send-unresolved", "harness interrupted turn is missing from conversation storage", true),
            Err(detail) => return pause("send-unresolved", detail, true),
        };
        if unsettled {
            return pause("send-unresolved", "Harness tool effect could not be confirmed; administrator confirmation required before retry", true);
        }
        if let crate::harness::Status::Paused { reason, detail } = status {
            return pause(&reason, detail, true);
        }
        if !is_reserved2 {
            return pause("harness-interrupted", "Harness turn interrupted; administrator resolution required", true);
        }
        let decided = match status {
            crate::harness::Status::Completed(_) => {
                Verdict::Complete("Harness reserved2 wake completed before restart".into())
            }
            crate::harness::Status::ObservationSleep => {
                Verdict::Complete("observationSleep".into())
            }
            crate::harness::Status::Cancelled => {
                Verdict::Cancel("Harness reserved2 wake cancelled before restart".into())
            }
            crate::harness::Status::Running | crate::harness::Status::Failed(_) => {
                if !has_tools && matches!(sends, SendState::None) {
                    match validity(self.context, self.cfg, turn) {
                        Ok(()) => Verdict::Resume("Harness reserved2 wake interrupted before tool calls or sends; resuming".into()),
                        Err(why) => Verdict::Abandon(format!("Harness reserved2 wake interrupted and no longer valid: {why}")),
                    }
                } else {
                    Verdict::Fail("Harness reserved2 wake interrupted after tool or send effects; keeping confirmed sends without replay".into())
                }
            }
            crate::harness::Status::Paused { .. } => unreachable!(),
        };
        preserve_finalization_claim(turn, decided)
    }

    #[cfg(test)]
    async fn reconcile(&self, journal: &Journal, turn: &Turn, turns: &[Turn], outbox: &Result<Vec<Value>, String>) {
        self.reconcile_at(journal, turn, turns, outbox, None).await;
    }

    async fn reconcile_at(
        &self,
        journal: &Journal,
        turn: &Turn,
        turns: &[Turn],
        outbox: &Result<Vec<Value>, String>,
        live_cutoff: Option<i64>,
    ) {
        // Intent can survive a crash before the cancelling handler revokes authority.
        // Revoke before any probe/wait, without claiming that the work has ended.
        if turn.notes.iter().any(|note| note["verdict"] == "cancel-requested") {
            let revoke = crate::social_state::token(self.context, &turn.key).and_then(|token| {
                if token.is_some_and(|token| turn.epochs.contains(&crate::send_outbox::token_epoch(&token))) {
                    crate::social_state::ensure_or_rotate(self.context, &turn.key, true).map(|_| ())
                } else { Ok(()) }
            });
            if let Err(error) = revoke {
                println!("[recovery] ❌ 取消回合 {} 换令牌失败，稍后重试：{error}", turn.id);
                return;
            }
        }
        let sends = self.sends_of(turn, turns, outbox);
        if turn.backend == "harness" {
            let verdict = self.reconcile_harness(turn, &sends);
            apply_at(self.context, self.cfg, journal, turn, &verdict, None, live_cutoff);
            return;
        }
        let check = validity(self.context, self.cfg, turn);
        let probe = if turn.attempts.is_empty() || turn.attempts.iter().any(|a| a.rpc_id.starts_with("cli:")) {
            None
        } else {
            let session = turn.attempts.last().map(|a| a.session_id.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| turn.session_id.clone());
            let rpc_ids: Vec<String> = turn.attempts.iter().map(|a| a.rpc_id.clone()).collect();
            match dsh_base(self.context).and_then(|base| Rpc::connect_with_runtime(self.context, &base)) {
                Ok(rpc) => {
                    let (probe, harvest) = crate::dsh::probe_prompt_harvest(&rpc, &session, &rpc_ids).await;
                    record_harvest(self.context, &turn.key, &session, &harvest);
                    Some(probe)
                }
                Err(error) => Some(PromptProbe::Unreachable(error)),
            }
        };
        let verdict = decide(turn, probe.as_ref(), &sends, check);
        let facts = match &probe {
            Some(PromptProbe::Turn(facts)) => Some(facts.clone()),
            _ => None,
        };
        match &verdict {
            Verdict::Reattach => self.reattach(turn),
            Verdict::Defer(why) => {
                if !(turn.state == State::Paused && turn.pause_reason == "dsh-unreachable") {
                    let recorder = Recorder::attach(self.context, &turn.id, &turn.key);
                    let _ = recorder.unknown("dsh-unreachable", why);
                    println!("[recovery] 回合 {} 暂时无法向 DSH 核对，稍后重试：{why}", turn.id);
                }
            }
            other => apply(self.context, self.cfg, journal, turn, other, facts.as_ref()),
        }
    }

    /// Wait for DSH to finish a turn that was running or queued when the bridge went down.
    fn reattach(&self, turn: &Turn) {
        {
            let mut active = active_turns().lock().unwrap_or_else(|p| p.into_inner());
            if !active.insert(turn.id.clone()) {
                return;
            }
        }
        let recorder = Recorder::attach(self.context, &turn.id, &turn.key);
        let _ = recorder.reattach();
        let context = self.context.clone();
        let cfg = self.cfg.clone();
        let turn = turn.clone();
        println!("[recovery] 回合 {} ({}) 仍在 DSH 运行或排队，重新接入等待，不重投", turn.id, turn.key);
        tokio::spawn(async move {
            let session = turn.attempts.last().map(|a| a.session_id.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| turn.session_id.clone());
            let rpc_ids: Vec<String> = turn.attempts.iter().map(|a| a.rpc_id.clone()).collect();
            let outcome = match dsh_base(&context).and_then(|base| Rpc::connect_with_runtime(&context, &base)) {
                Ok(rpc) => crate::dsh::wait_prompt_outcome(&rpc, &session, &rpc_ids, Duration::from_secs(wait_secs(&cfg)))
                    .await
                    .map(|(probe, harvest)| {
                        record_harvest(&context, &turn.key, &session, &harvest);
                        probe
                    }),
                Err(error) => Err(error),
            };
            if let Ok(journal) = Journal::open(&context) {
                let sends = reattach_sends(&context, &cfg, &turn);
                let facts = match &outcome {
                    Ok(PromptProbe::Turn(facts)) => Some(facts.clone()),
                    _ => None,
                };
                let verdict = match outcome {
                    Ok(probe) => decide(&turn, Some(&probe), &sends, validity(&context, &cfg, &turn)),
                    Err(error) => pause("dsh-turn-unresolved", format!("重新接入后仍未等到回合结束：{error}"), true),
                };
                apply(&context, &cfg, &journal, &turn, &verdict, facts.as_ref());
            }
            active_turns().lock().unwrap_or_else(|p| p.into_inner()).remove(&turn.id);
        });
    }

    /// Act on administrator decisions: `retry` hands the paused turn's events back, `abandon`
    /// closes them. Events held meanwhile are released by `release_held`.
    fn resolutions(&self, journal: &Journal, turns: &[Turn], records: &[Value]) {
        for turn in turns.iter().filter(|t| t.state == State::Resolved && !t.resolution_applied) {
            let action = turn.resolution.clone().unwrap_or_default();
            if turn.backend == "harness" {
                if let Err(error) = crate::harness::resolve_interrupted(self.context, &turn.id) {
                    println!("[recovery] ❌ Harness resolution could not be stored; retrying later: {error}");
                    continue;
                }
            }
            if let Err(error) = Recorder::attach(self.context, &turn.id, &turn.key).applied() {
                println!("[recovery] ❌ 管理员决定写不进账本，稍后重试：{error}");
                continue;
            }
            // A revocation or reset after the turn began ended its epoch: the retry would run the
            // old input under the authority granted afterwards.
            let voided = epoch_ended_since(records, &turn.id, &turn.key);
            if action == "retry" && !voided {
                hand_back(journal, &turn.inbound);
            } else {
                settle(journal, &turn.inbound, "handled");
            }
            if action == "retry" && voided {
                println!("[recovery] 回合 {} 的重试已作废：会话在此之后已撤权或重置", turn.id);
            } else {
                println!("[recovery] 管理员对回合 {} 选择 {action}", turn.id);
            }
        }
    }

    /// Wakes that were refused while a conversation had a paused or recovering turn go back to the
    /// normal replay path once nothing blocks the conversation any more.
    fn release_held(&self, journal: &Journal, turns: &[Turn], records: &[Value]) {
        let keys: HashSet<String> = records
            .iter()
            .filter(|r| r.get("k").and_then(Value::as_str) == Some("held"))
            .filter_map(|r| r.get("key").and_then(Value::as_str).map(str::to_string))
            .collect();
        for key in keys {
            let held = turn_ledger::held_events(records, &key);
            if held.is_empty() || turn_ledger::blocking_turn(turns, &key).is_some() {
                continue;
            }
            if turns.iter().any(|t| t.key == key && t.state == State::Resolved && !t.resolution_applied) {
                continue;
            }
            // The main loop marks a refused wake's events handled one iteration later; wait for it.
            let settled = held.iter().all(|k| {
                journal.stage_of(k).is_ok_and(|stage| !matches!(stage.as_deref(), Some("received" | "processing")))
            });
            if !settled {
                continue;
            }
            // The worker persists a held wake's expiry before the hub settles its journal entry.
            // Our pass may have read the hold before that end. Recheck it after observing settlement.
            let Ok(fresh) = turn_ledger::read_records(self.context) else { continue };
            if turn_ledger::held_events(&fresh, &key) != held { continue; }
            if let Err(error) = turn_ledger::append_record(
                self.context,
                &serde_json::json!({"k": "requeued", "t": now(), "key": key, "inbound": held}),
            ) {
                println!("[recovery] ❌ 暂缓唤醒的交回记录写不进账本，稍后重试：{error}");
                continue;
            }
            hand_back(journal, &held);
            println!("[recovery] {key} 不再被阻塞，{} 条暂缓的唤醒交回重处理", held.len());
        }
    }

    /// Interrupted events that no turn ever started for were never executed, but only when the
    /// ledger was already running when the event went into processing. An event interrupted by a
    /// build that had no ledger proves nothing and stays reported for the administrator.
    fn orphans(&self, journal: &Journal, turns: &[Turn], records: &[Value]) {
        let Ok(interrupted) = journal.interrupted_events() else { return };
        for event in interrupted {
            if records.iter().any(|r| turn_ledger::ended_hold(r) && r["key"] == event.conversation
                && r["inbound"].as_array().is_some_and(|keys| keys.iter().any(|k| k.as_str() == Some(event.event_key.as_str())))) {
                // A crash can interrupt the journal mark after the durable expiry of a held wake.
                settle(journal, std::slice::from_ref(&event.event_key), "handled");
                continue;
            }
            if turns.iter().any(|t| t.inbound.contains(&event.event_key)) {
                // Handled by the owning turn's verdict (finished turns settle in `finished`).
                continue;
            }
            let at = journal.updated_ms_of(&event.event_key).ok().flatten().unwrap_or(0);
            if !ledger_covers(records, at) {
                continue;
            }
            if allowed(self.cfg, &event) {
                println!("[recovery] 事件 {} 没有对应回合，未执行，交回重处理", event.event_key);
                hand_back(journal, std::slice::from_ref(&event.event_key));
            } else {
                settle(journal, std::slice::from_ref(&event.event_key), "handled");
            }
        }
    }

    /// A turn ended `resumed` ("never executed, events handed back") but the process died before the hand-back: its events
    /// are still `interrupted` and nothing will ever hand them back. They are never executed twice and are not replayed
    /// here either (a replay needs the administrator's decision); the administrator is told once per event which
    /// conversation is affected (no message text) and the run record keeps the fact.
    fn report_stuck_resumed(&self, journal: &Journal, turns: &[Turn]) {
        for turn in turns.iter().filter(|t| t.state == State::Resumed) {
            for key in &turn.inbound {
                if journal.stage_of(key).ok().flatten().as_deref() != Some("interrupted")
                    || turns.iter().any(|t| t.id != turn.id && t.begun_at >= turn.begun_at && t.inbound.contains(key))
                {
                    continue;
                }
                if !first_reported(self.context, key) {
                    continue;
                }
                let detail = format!(
                    "{}: turn {} was proven not executed (ended resumed) but the process stopped before its inbound event was handed back; the event stays interrupted and is not re-run",
                    turn.key, turn.id
                );
                println!("[recovery] ⚠️ {detail}");
                crate::recovery_log::record(self.context, crate::run_record::Trigger::Recovery, crate::run_record::Component::Queue, "resumed-not-handed-back", crate::run_record::Outcome::Unknown, &detail);
                crate::admin_alert::notify_notice(
                    self.context,
                    self.cfg,
                    &format!("[{}] 上次运行在回合被判定「未执行」后、交回消息前停止；该消息停在 interrupted，不会被重复执行也不会自动重放，需要时请在管理端处理", turn.key),
                    "resumed-interrupted",
                );
            }
        }
    }

    /// Interrupted events of turns that already ended (crash between turn end and the stage mark).
    fn finished(&self, journal: &Journal, turns: &[Turn]) {
        for turn in turns.iter().filter(|t| t.state.is_final() && t.state != State::Resumed) {
            if turn.state == State::Resolved && !turn.resolution_applied {
                continue;
            }
            if turn.reason == "reserved"
                && turn.state == State::Failed
                && (turn.attempts.is_empty() || turn.notes.iter().any(|n| n["verdict"] == "unsubmitted"))
            {
                // The live reserved driver owns its delayed retries. Leave processing input
                // recoverable until it settles the job; only a restart interrupts that input.
                let keys: Vec<String> = turn.inbound.iter()
                    .filter(|key| {
                        journal.stage_of(key).ok().flatten().as_deref() == Some("interrupted")
                            && turns.iter().rev().find(|t| t.inbound.contains(key)).map(|t| &t.id)
                                == Some(&turn.id)
                    })
                    .cloned()
                    .collect();
                if keys.is_empty() {
                    continue;
                }
                if crate::social_state::mode(self.context) == "reserved"
                    && validity(self.context, self.cfg, turn).is_ok()
                {
                    if Recorder::attach(self.context, &turn.id, &turn.key)
                        .verified("resume", "reserved pre-submit failure interrupted during retry delay")
                        .is_ok()
                    {
                        hand_back(journal, &keys);
                    }
                } else {
                    settle(journal, &keys, "handled");
                }
                continue;
            }
            if turn.backend == "harness" && turn.state == State::Abandoned {
                if let Err(error) = close_interrupted_harness(
                    self.context,
                    turn,
                    &crate::harness::Status::Cancelled,
                    Some("abandoned"),
                ) {
                    crate::safe_println!("[recovery] 关闭已放弃回合 {} 失败：{error}", turn.id);
                    continue;
                }
            }
            settle(journal, &turn.inbound, "handled");
        }
    }
}

/// Events whose stuck-`resumed` state was already told to the administrator, kept under the data directory so a restart
/// does not tell it again (the event stays `interrupted` until the administrator acts, so every start would find it).
const REPORTED_FILE: &str = "recovery-reported.json";
const REPORTED_MAX: usize = 500;

/// `true` the first time `event_key` is seen by this data directory (in this process or an earlier one); the sighting is
/// remembered. A store that cannot be written still remembers in memory, so a failing disk never repeats the message.
fn first_reported(context: &RuntimeContext, event_key: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    first_reported_in(SEEN.get_or_init(|| Mutex::new(HashSet::new())), context, event_key)
}

/// `first_reported` over an explicit in-memory set (a process's own memory; tests use one per simulated process).
fn first_reported_in(seen: &Mutex<HashSet<String>>, context: &RuntimeContext, event_key: &str) -> bool {
    let scoped = format!("{}|{event_key}", context.data_dir().display());
    if !seen.lock().unwrap_or_else(|p| p.into_inner()).insert(scoped) {
        return false;
    }
    let mut stored: Vec<String> = context
        .read_optional_text(crate::runtime::RuntimeArea::Data, REPORTED_FILE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if stored.iter().any(|k| k == event_key) {
        return false;
    }
    stored.push(event_key.to_string());
    if stored.len() > REPORTED_MAX {
        stored.drain(..stored.len() - REPORTED_MAX);
    }
    if let Ok(text) = serde_json::to_string(&stored) {
        if let Err(error) = context.write_atomic(crate::runtime::RuntimeArea::Data, REPORTED_FILE, text.as_bytes()) {
            println!("[recovery] 无法记录已通知的事件：{error}");
        }
    }
    true
}

/// What a re-attached turn has sent. An unreadable turn ledger or send ledger leaves the sends
/// unknown (the turn pauses as `send-unresolved`); an empty read must never stand in for a failure.
fn reattach_sends(context: &RuntimeContext, cfg: &Value, turn: &Turn) -> SendState {
    let records = match turn_ledger::read_records(context) {
        Ok(records) => records,
        Err(error) => return SendState::Unreadable(format!("回合账本无法读取，已发送内容未知：{error}")),
    };
    let (turns, _) = turn_ledger::fold(&records);
    let outbox = crate::send_outbox::read_records(context);
    Pass { context, cfg }.sends_of(turn, &turns, &outbox)
}

/// Turns that were left open (or waiting for DSH) by an earlier process: begun at or before `cutoff`,
/// the instant this process started. A turn begun later belongs to the live process.
fn startup_turns(records: &[Value], cutoff: i64) -> HashSet<String> {
    turn_ledger::fold(records)
        .0
        .into_iter()
        .filter(|t| t.begun_at <= cutoff)
        .filter(|t| t.state.is_open() || (t.state == State::Paused && t.pause_reason == "dsh-unreachable"))
        .map(|t| t.id)
        .collect()
}

/// Start the recovery task once per process. Only a selected runtime has the ledger.
pub fn spawn(context: RuntimeContext) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if !context.is_isolated() || STARTED.set(()).is_err() {
        return;
    }
    // An unreadable ledger must not read as "nothing was open": the loop takes the startup set from the
    // first successful read instead, still limited to turns begun before this process started.
    let started_at = now();
    let startup: Option<HashSet<String>> = match turn_ledger::read_records(&context) {
        Ok(records) => Some(startup_turns(&records, started_at)),
        Err(error) => {
            println!("[recovery] ❌ 回合账本无法读取，恢复暂停并重试：{error}");
            None
        }
    };
    if let Some(startup) = startup.as_ref().filter(|s| !s.is_empty()) {
        println!("[recovery] 启动时有 {} 个未完成回合，先向 DSH 与发送账本核对", startup.len());
    }
    // Own thread and runtime: the journal connection is not `Sync`, so this loop cannot live on the
    // main multi-thread runtime. Re-attached turns spawn onto this runtime's own handle.
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() else {
            println!("[recovery] ❌ 无法创建恢复任务的运行时");
            return;
        };
        runtime.block_on(recovery_loop(context, startup, started_at));
    });
}

/// Re-arm interrupted terminal `reserved2` harness wakes once per startup when the previous process
/// committed a terminal outcome before the hub could schedule or re-arm the next wake deadline.
fn rearm_startup_terminal_sleeps(context: &RuntimeContext, cfg: &Value, turns: &[Turn], cutoff: i64) -> bool {
    let mut latest_by_key: std::collections::HashMap<&str, &Turn> = Default::default();
    let mut keys_with_live_turn: std::collections::HashSet<&str> = Default::default();
    for turn in turns {
        if turn.begun_at <= cutoff {
            latest_by_key.insert(turn.key.as_str(), turn);
        } else {
            keys_with_live_turn.insert(turn.key.as_str());
        }
    }
    let mut ok = true;
    for (key, turn) in latest_by_key {
        if keys_with_live_turn.contains(key) {
            continue;
        }
        if (turn.reserved2 || turn.wake_claim.is_some())
            && turn.backend == "harness"
            && matches!(
                turn.state,
                State::Completed | State::Failed | State::Cancelled | State::Abandoned
            )
        {
            if let Err(error) = crate::social_state::rearm_interrupted_reserved2_wake(
                context,
                cfg,
                &turn.key,
                turn.wake_claim.as_ref(),
                Some(turn.begun_at),
                Some(cutoff),
            ) {
                crate::safe_println!("[recovery] startup terminal sleep rearm failed for {}: {error}", turn.key);
                ok = false;
            }
        }
    }
    ok
}

async fn recovery_loop(context: RuntimeContext, mut startup: Option<HashSet<String>>, started_at: i64) {
    {
        let mut last_try: std::collections::HashMap<String, i64> = Default::default();
        let mut terminal_rearm_done = false;
        loop {
            if let (Ok(cfg), Ok(journal), Ok(records)) =
                (context.load_config(), Journal::open(&context), turn_ledger::read_records(&context))
            {
                let startup = startup.get_or_insert_with(|| {
                    let set = startup_turns(&records, started_at);
                    if !set.is_empty() {
                        println!("[recovery] 启动时有 {} 个未完成回合，先向 DSH 与发送账本核对", set.len());
                    }
                    set
                });
                let (turns, _) = turn_ledger::fold(&records);
                if !terminal_rearm_done && rearm_startup_terminal_sleeps(&context, &cfg, &turns, started_at) {
                    terminal_rearm_done = true;
                }
                let mut expiry_applied = false;
                for turn in turns.iter().filter(|t| t.paused_at.is_some() && (t.state == State::Paused || t.notification_intent)) {
                    let pause_ready = if turn.pause_notified {
                        true
                    } else {
                        match notify_turn(&context, &cfg, &turn.id, false) {
                            Ok(()) => true,
                            Err((_, error)) => {
                                crate::safe_println!("[recovery] 暂停通知记录失败：{error}");
                                false
                            }
                        }
                    };
                    if turn.state == State::Paused && auto_abandon_at(&cfg, turn).is_some_and(|at| now() >= at) {
                        match resolve(&context, &turn.id, "abandon", false, "auto") {
                            Ok(_) => expiry_applied = true,
                            Err((status, error)) if status != 409 => {
                                crate::safe_println!("[recovery] 自动放弃失败：{error}");
                            }
                            Err(_) => {}
                        }
                    }
                    if pause_ready && turn.state.is_final() && !turn.resolution_notified {
                        if let Err((_, error)) = notify_turn(&context, &cfg, &turn.id, true) {
                            crate::safe_println!("[recovery] 处理通知记录失败：{error}");
                        }
                    }
                }
                // Expiry may just have resolved a turn. Re-read before any reconciliation/replay.
                let (records, turns) = if expiry_applied {
                    let Ok(records) = turn_ledger::read_records(&context) else { continue };
                    let (turns, _) = turn_ledger::fold(&records);
                    (records, turns)
                } else {
                    (records, turns)
                };
                let outbox = crate::send_outbox::read_records(&context);
                let pass = Pass { context: &context, cfg: &cfg };
                for turn in &turns {
                    let working = startup.contains(&turn.id)
                        && (turn.state.is_open() || (turn.state == State::Paused && turn.pause_reason == "dsh-unreachable"));
                    if !working {
                        continue;
                    }
                    if active_turns().lock().unwrap_or_else(|p| p.into_inner()).contains(&turn.id) {
                        continue;
                    }
                    if turn.state == State::Paused {
                        let tried = last_try.get(&turn.id).copied().unwrap_or(0);
                        if now() - tried < UNREACHABLE_RETRY_MS {
                            continue;
                        }
                    }
                    last_try.insert(turn.id.clone(), now());
                    pass.reconcile_at(&journal, turn, &turns, &outbox, Some(started_at)).await;
                }
                pass.resolutions(&journal, &turns, &records);
                pass.release_held(&journal, &turns, &records);
                pass.finished(&journal, &turns);
                pass.report_stuck_resumed(&journal, &turns);
                // Orphans only after every startup turn has a verdict that is not "still open".
                let unsettled = turns.iter().any(|t| startup.contains(&t.id) && t.state.is_open());
                if !unsettled {
                    pass.orphans(&journal, &turns, &records);
                }
            }
            tokio::time::sleep(PASS_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_ledger::fold;
    use serde_json::json;

    fn turn(records: Vec<Value>) -> Turn {
        fold(&records).0.remove(0)
    }

    fn begun() -> Value {
        json!({"k":"begun","turn":"t1","t":10,"key":"group:1","epoch":"e1","inbound":["group:1|message:7"],"reason":"atMention"})
    }

    #[test]
    fn recovery_commands_need_a_verb_and_a_turn_code() {
        let parse = |text: &str| command_action(&text.split_whitespace().collect::<Vec<_>>());
        assert_eq!(parse("放弃 0a1b2c3d"), Some("abandon"));
        assert_eq!(parse("ABANDON 0A1B2C3D"), Some("abandon"));
        assert_eq!(parse("重试 0a1b2c3d 确认"), Some("retry"));
        assert_eq!(parse("retry 0a1b2c3d confirm"), Some("retry"));
        for ordinary in [
            "retry", "放弃", "retry later please", "放弃 吧", "重试 一下", "abandon unknown84", "retry 0a1b2c3", "retry 0a1b2c3d4",
            "retry 0a1b2c3g", "再重试 0a1b2c3d", "",
            "retry 0a1b2c3d 吗", "重试 20261109 的任务", "放弃 0a1b2c3d 吧", "abandon 0a1b2c3d confirm", "retry 0a1b2c3d confirm now",
            "放弃 0a1b2c3d。", "retry 0a1b2c3d 确认 谢谢",
        ] {
            assert_eq!(parse(ordinary), None, "{ordinary:?} is ordinary input");
        }
    }

    #[test]
    fn recovery_finishes_a_pending_cancellation_before_settling_its_input() {
        for verdict in [Verdict::Complete("finished".into()), Verdict::Resume("not executed".into()),
            Verdict::Fail("failed".into()), Verdict::Abandon("invalidated".into()), Verdict::Cancel("aborted".into())] {
            let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id("her-cancel-recovery-")));
            for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
            std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token","allow":{"groups":[1]}}"#).unwrap();
            std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
            std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{"group:1":{"agentToken":"old-token"}}}"#).unwrap();
            let context = RuntimeContext::from_root(&root).unwrap();
            let journal = Journal::open(&context).unwrap();
            let input = "group:1|message:7".to_string();
            journal.record_received(&input, "group:1", "{}", 1).unwrap();
            journal.mark_processing(&[input.clone()], 2).unwrap();
            journal.recover_interrupted(3).unwrap();
            let recorder = Recorder::begin(&context, "group:1", "dsh", "atMention", "e1", &[input.clone()], None).unwrap();
            recorder.verified("cancel-requested", "turnTimeout").unwrap();
            let pending = turn(crate::turn_ledger::read_records(&context).unwrap());
            let cfg = context.load_config().unwrap();
            apply(&context, &cfg, &journal, &pending, &verdict, None);
            let records = crate::turn_ledger::read_records(&context).unwrap();
            let ended = turn(records.clone());
            assert_eq!(ended.state, State::Cancelled, "{verdict:?}: {records:?}");
            assert_eq!(ended.outcome_reason, "turnTimeout");
            assert_eq!(records.iter().filter(|row| row["k"] == "ended").count(), 1);
            assert_ne!(
                crate::social_state::token(&context, "group:1")
                    .unwrap()
                    .as_deref(),
                Some("old-token")
            );
            assert_eq!(journal.stage_of(&input).unwrap().as_deref(), Some("handled"));
            drop(journal);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn ticket159_expired_held_inbound_stays_handled_after_resolution() {
        let key = "group:42";
        let cfg = json!({"consoleToken":"synthetic-held-cap","engineDefault":"harness","allow":{"groups":[42]}});
        let root = crate::runtime::tests::runtime_fixture(std::env::temp_dir().join(turn_ledger::mint_id("her-held-cap-")),
            &cfg, &json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}));
        std::fs::write(root.0.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.0.join("data/social-v2.json"), json!({"conversations":{key:{"agentToken":"held-cap-token"}}}).to_string()).unwrap();
        let context = RuntimeContext::from_root(&root.0).unwrap();
        let journal = Journal::open(&context).unwrap();
        let input = "group:42|message:159".to_owned();
        journal.record_received(&input, key, "{}", 1).unwrap();
        journal.mark_processing(std::slice::from_ref(&input), 2).unwrap();
        let paused = Recorder::begin(&context, key, "dsh", "atMention", "earlier", &[], None).unwrap();
        paused.unknown("send-unresolved", "fixture").unwrap();
        let mut pipe = crate::pipeline::Pipeline::new_without_dsh(&context, &cfg, true).unwrap();
        pipe.note_turn("atMention", std::slice::from_ref(&input));
        pipe.expect_dispatch_deadline(10);
        let clock_context = context.clone();
        pipe.admission_clock = Some(Box::new(move || {
            if turn_ledger::read_records(&clock_context).unwrap().iter().any(|r| r["k"] == "held") { 10 } else { 9 }
        }));
        let err = pipe.run_turn(&crate::onebot::Target::Group(42), "", "", &cfg).await.unwrap_err();
        assert!(err.contains("turnTimeout"), "{err}");
        // Lose the process after the durable expiry and before the hub marks the input handled.
        assert_eq!(journal.recover_interrupted(now()).unwrap(), vec![input.clone()]);
        paused.resolved("abandon").unwrap();
        let pass = Pass { context: &context, cfg: &cfg };
        let records = turn_ledger::read_records(&context).unwrap();
        pass.resolutions(&journal, &fold(&records).0, &records);
        let records = turn_ledger::read_records(&context).unwrap();
        assert!(fold(&records).0.iter().any(|t| t.id == paused.id && t.resolution_applied));
        pass.orphans(&journal, &fold(&records).0, &records);
        assert_eq!(journal.stage_of(&input).unwrap().as_deref(), Some("handled"));
        // A pass that observed resolution and the hold before its terminal write must also refuse replay.
        let stale: Vec<_> = records.iter().filter(|r| !turn_ledger::ended_hold(r)).cloned().collect();
        pass.release_held(&journal, &fold(&stale).0, &stale);
        pass.release_held(&journal, &fold(&records).0, &records);
        assert_eq!(journal.stage_of(&input).unwrap().as_deref(), Some("handled"), "an expired held wake must never replay");
        let records = turn_ledger::read_records(&context).unwrap();
        assert!(records.iter().any(|r| r["k"] == "ended" && r["reason"] == "turnTimeout" && r["outcome"] == "cancelled"
            && r["inbound"] == json!([input])));
    }

    #[test]
    fn hf84_failed_notification_sync_prevents_delivery() {
        use std::io::{BufRead, Read, Write};
        let root = crate::install_root::development_root(std::env::temp_dir().join(turn_ledger::mint_id("her-notice-sync-")));
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cfg = json!({"ownerQQ":8808,"consoleToken":"synthetic-notice-token"});
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), json!({"onebot":url,"dsh":url,"model":url}).to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let recorder = Recorder::begin(&context, "group:1", "dsh", "atMention", "epoch", &[], None).unwrap();
        recorder.unknown("dsh-history-incomplete", "synthetic").unwrap();

        crate::runtime::APPEND_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(|stage, bytes| {
            let record: Value = serde_json::from_slice(bytes).unwrap();
            if stage == "sync" && matches!(record["k"].as_str(), Some("pause_notified" | "resolution_notified")) {
                return Err("injected notification sync failure".into());
            }
            Ok(())
        })));
        let pause = notify_turn(&context, &cfg, &recorder.id, false);
        recorder.resolved("abandon").unwrap();
        let resolution = notify_turn(&context, &cfg, &recorder.id, true);
        crate::runtime::APPEND_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);

        // This worker is FIFO. Observing the barrier proves both earlier jobs were processed;
        // no elapsed quiet period is used as evidence that a failed claim did not send.
        private_notice(&context, &cfg, "notification-sync-barrier".into(), || Ok(())).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("notice worker did not reach the gateway: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" { break; }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let reply = r#"{"status":"ok","retcode":0,"data":{"message_id":1}}"#;
        write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(String::from_utf8(body).unwrap().contains("notification-sync-barrier"), "a failed claim released delivery");
        for result in [pause, resolution] {
            assert_eq!(result, Err((500, "injected notification sync failure".into())), "a notice claim must sync before delivery");

        }
    }

    #[test]
    fn review_never_submitted_timer_restores_deadline_before_recovery_is_final() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id("her-timer-recovery-")));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token","allow":{"groups":[1]}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let previous = json!({"wakeConfig":{"confirmedAt":1,"sleepUntil":"2000-01-01T00:00:00.000Z","lastWakeAt":0,"wakeCount":0},"lastWakeReason":""});
        std::fs::write(root.join("data/social-v2.json"), json!({"conversations":{"group:1":{"agentToken":"fixture","wakeTimes":[10],"wakeConfig":{"confirmedAt":1,"sleepUntil":null,"lastWakeAt":10,"wakeCount":1}}}}).to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = Journal::open(&context).unwrap();
        let begin = json!({"k":"begun","turn":"t1","t":10,"key":"group:1","epoch":"e1","inbound":[],"reason":"timeout"});
        let lease = json!({"k":"wake_claim","turn":"t1","t":10,"claim":{"at":10,"previous":previous}});
        crate::turn_ledger::append_record(&context, &begin).unwrap();
        crate::turn_ledger::append_record(&context, &lease).unwrap();
        let t = turn(vec![begin, lease]);
        let cfg = context.load_config().unwrap();
        apply(&context, &cfg, &journal, &t, &Verdict::Resume("never submitted".into()), None);
        assert_eq!(context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"], previous["wakeConfig"]["sleepUntil"], "timer-only recovery has no inbound event to replay");
        assert_eq!(fold(&crate::turn_ledger::read_records(&context).unwrap()).0[0].state, State::Resumed);
        drop(journal);
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn recover_reserved2_before_admission(claimed: bool, with_budget: bool) -> (Turn, Value) {
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id("her-harness-wake-recovery-")));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _root = Cleanup(root.clone());
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token","allow":{"groups":[1]}}"#).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let token = "fixture-token";
        let deadline = "2000-01-01T00:00:00.000Z";
        let previous = json!({"wakeConfig":{"confirmedAt":1,"sleepUntil":deadline,"lastWakeAt":0,"wakeCount":0},"lastWakeReason":""});
        let mut wake = previous["wakeConfig"].clone();
        // Dispatch records bookkeeping before begun. Only a durable claim consumes sleepUntil.
        wake["lastWakeAt"] = json!(10);
        wake["wakeCount"] = json!(1);
        if claimed { wake["sleepUntil"] = Value::Null; }
        std::fs::write(root.join("data/social-v2.json"), json!({"conversations":{"group:1":{"agentToken":token,"wakeTimes":[10],"wakeConfig":wake}}}).to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let cfg = context.load_config().unwrap();
        let journal = Journal::open(&context).unwrap();
        let epoch = crate::send_outbox::token_epoch(token);
        let recorder = if claimed {
            // Keep coverage for older claimed wakes whose begun record has no mode.
            Recorder::begin(&context, "group:1", "harness", "timeout", &epoch, &[], None)
        } else {
            Recorder::begin_reserved2(&context, "group:1", "harness", "timeout", &epoch, &[])
        }.unwrap();
        if with_budget {
            turn_ledger::append_record(&context, &json!({"k":"budget","turn":recorder.id,"t":10,"deadline":20,"capMs":10,"ceiling":30})).unwrap();
        }
        if claimed { recorder.wake_claim(&json!({"at":10,"previous":previous})).unwrap(); }
        let turns = fold(&turn_ledger::read_records(&context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&context);
        Pass { context: &context, cfg: &cfg }.reconcile(&journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&context).unwrap()).0.remove(0);
        let restored = context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"].clone();
        (recovered, restored)
    }

    #[tokio::test]
    async fn ticket159_reserved2_recovery_resumes_a_harness_wake_before_admission() {
        let (recovered, restored) = recover_reserved2_before_admission(true, true).await;
        assert_eq!(recovered.state, State::Resumed,
            "a harness wake with no attempt or store row must resume, got pause reason {:?}", recovered.pause_reason);
        assert_eq!(restored, "2000-01-01T00:00:00.000Z", "the resumed timer's wake claim must be rolled back");
    }

    #[tokio::test]
    async fn ticket159_reserved2_recovery_resumes_before_the_wake_claim() {
        let begun = recover_reserved2_before_admission(false, false).await;
        let budget = recover_reserved2_before_admission(false, true).await;
        assert_eq!([begun.0.state, budget.0.state], [State::Resumed; 2],
            "both pre-claim crash boundaries must resume the reserved2 wake, got {:?} and {:?}",
            begun.0.pause_reason, budget.0.pause_reason);
        for (turn, sleep_until) in [begun, budget] {
            assert!(turn.wake_claim.is_none());
            assert_eq!(sleep_until, "2000-01-01T00:00:00.000Z", "the unconsumed timer deadline remains due");
        }
    }

    struct SubmittedHarnessFixture {
        _root: crate::runtime::tests::FixtureRoot,
        context: RuntimeContext,
        cfg: Value,
        journal: Journal,
        store: crate::harness::store::Store,
        turn_id: String,
        epoch: String,
        input: String,
    }

    fn setup_submitted_harness_turn(
        label: &str,
        mode: &str,
        reserved2: bool,
        wake_claim: Option<Value>,
        with_inbound: bool,
    ) -> SubmittedHarnessFixture {
        let key = "group:1";
        let token = "fixture-token-297";
        let cfg = json!({
            "consoleToken": "synthetic-review-token-297",
            "allow": {"groups": [1]},
            "harness": {"model": "fake-chat"},
            "socialV2": {"wake": {"recommendedSleepMinMs": 60_000, "recommendedSleepMaxMs": 60_000}},
        });
        let root = crate::runtime::tests::runtime_fixture(
            std::env::temp_dir().join(crate::turn_ledger::mint_id(label)),
            &cfg,
            &json!({"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}),
        );
        std::fs::write(root.0.join("config/harness-api-key"), "synthetic-harness-key").unwrap();
        std::fs::write(root.0.join("data/mode.json"), json!({"mode": mode}).to_string()).unwrap();
        let wake = wake_claim
            .as_ref()
            .map(|_| {
                json!({
                    "mode": "diving",
                    "infinite": false,
                    "sleepUntil": null,
                    "triggers": {"atMention": true},
                    "confirmedAt": 1,
                    "lastWakeAt": 10,
                    "wakeCount": 1
                })
            })
            .unwrap_or_else(|| {
                json!({
                    "mode": "diving",
                    "infinite": true,
                    "sleepUntil": null,
                    "triggers": {"atMention": true},
                    "confirmedAt": 1,
                    "lastWakeAt": 10,
                    "wakeCount": 1
                })
            });
        std::fs::write(
            root.0.join("data/social-v2.json"),
            json!({"conversations": {key: {"agentToken": token, "wakeTimes": [10], "wakeConfig": wake}}}).to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root.0).unwrap();
        let journal = Journal::open(&context).unwrap();
        let input = format!("{key}|message:{}", crate::turn_ledger::mint_id("msg-"));
        let inbound = if with_inbound {
            journal.record_received(&input, key, "{}", 1).unwrap();
            journal.mark_processing(std::slice::from_ref(&input), 2).unwrap();
            journal.recover_interrupted(3).unwrap();
            vec![input.clone()]
        } else {
            Vec::new()
        };
        let epoch = crate::send_outbox::token_epoch(token);
        let recorder = if reserved2 {
            Recorder::begin_reserved2(&context, key, "harness", "atMention", &epoch, &inbound)
        } else {
            Recorder::begin(&context, key, "harness", "atMention", &epoch, &inbound, None)
        }
        .unwrap();
        if let Some(claim) = wake_claim {
            recorder.wake_claim(&claim).unwrap();
        }
        let attempt_rpc = format!("harness:{}", recorder.id);
        recorder.attempt(&attempt_rpc, "").unwrap();
        recorder.accepted(&attempt_rpc).unwrap();

        let store = crate::harness::store::Store::open(&context).unwrap();
        let request = crate::harness::TurnRequest {
            id: recorder.id.clone(),
            conversation: key.into(),
            generation: epoch.clone(),
            prompt: "fixture prompt".into(),
            media: Vec::new(),
            profile: mode.into(),
            route: crate::harness::ModelRoute::from_runtime(&context, &cfg).unwrap(),
            deadline: Duration::from_secs(10),
            tools: crate::harness::Tools::capture(&context, &cfg, mode, key, &epoch).unwrap(),
            limits: crate::harness::Limits::from_config(&cfg),
        };
        store.admit(&request).unwrap();

        SubmittedHarnessFixture {
            _root: root,
            context,
            cfg,
            journal,
            store,
            turn_id: recorder.id,
            epoch,
            input,
        }
    }

    fn record_settled_tool(store: &crate::harness::store::Store, turn_id: &str, name: &str, state: &str) {
        let call = genai::chat::ToolCall {
            call_id: crate::turn_ledger::mint_id("call-"),
            fn_name: name.into(),
            fn_arguments: json!({}),
            thought_signatures: None,
        };
        let run = store.tool_start(turn_id, &get_running_attempt(store, turn_id), &call).unwrap();
        if state != "running" {
            let outcome = match state {
                "uncertain" => json!({"isError": true, "_meta": {"herDelivery": "unknown"}}),
                "refused" => json!({"isError": true}),
                _ => json!({"content": [{"type": "text", "text": "ok"}]}),
            };
            let msg = genai::chat::ChatMessage::tool(genai::chat::ToolResponse::from_tool_call(&call, "ok"));
            store.tool_result(turn_id, &run, &msg, &outcome).unwrap();
        }
    }

    fn get_running_attempt(store: &crate::harness::store::Store, turn_id: &str) -> String {
        let assistant = crate::harness::Event::Assistant {
            text: String::new(),
            raw: json!({}),
            message: genai::chat::ChatMessage::assistant(String::new()),
        };
        store.complete_step(turn_id, &assistant).unwrap()
    }

    fn admit_followup(fx: &SubmittedHarnessFixture, id: &str) -> Result<(), String> {
        let token = crate::social_state::token(&fx.context, "group:1")?.unwrap();
        let generation = crate::send_outbox::token_epoch(&token);
        let request = crate::harness::TurnRequest {
            id: id.into(),
            conversation: "group:1".into(),
            generation: generation.clone(),
            prompt: "next wake".into(),
            media: Vec::new(),
            profile: "reserved2".into(),
            route: crate::harness::ModelRoute::from_runtime(&fx.context, &fx.cfg).unwrap(),
            deadline: Duration::from_secs(10),
            tools: crate::harness::Tools::capture(&fx.context, &fx.cfg, "reserved2", "group:1", &generation).unwrap(),
            limits: crate::harness::Limits::from_config(&fx.cfg),
        };
        fx.store.admit(&request).map(|_| ())
    }

    #[tokio::test]
    async fn ticket297_reserved2_reconciles_confirmed_send_without_replay_and_schedules_next_wake() {
        let previous = json!({
            "wakeConfig": {"confirmedAt": 1, "sleepUntil": "2000-01-01T00:00:00.000Z", "lastWakeAt": 0, "wakeCount": 0},
            "lastWakeReason": ""
        });
        let fx = setup_submitted_harness_turn(
            "her-297-send-",
            "reserved2",
            true,
            Some(json!({"at": 10, "previous": previous})),
            true,
        );
        record_settled_tool(&fx.store, &fx.turn_id, "qq_send_message", "completed");
        let turns = fold(&turn_ledger::read_records(&fx.context).unwrap()).0;
        let send_t = turns[0].begun_at;
        let outbox_lines = format!(
            "{}\n{}\n{}\n",
            json!({"k":"intent","batch":"b297","key":"group:1","epoch":fx.epoch,"t":send_t,"messages":["hello"]}),
            json!({"k":"attempt","batch":"b297","idx":0,"t":send_t}),
            json!({"k":"confirmed","batch":"b297","idx":0,"t":send_t}),
        );
        std::fs::write(fx._root.0.join("data/send-outbox.jsonl"), outbox_lines).unwrap();

        let outbox = crate::send_outbox::read_records(&fx.context);
        Pass { context: &fx.context, cfg: &fx.cfg }.reconcile(&fx.journal, &turns[0], &turns, &outbox).await;

        let recovered = fold(&turn_ledger::read_records(&fx.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Failed, "got {:?}", recovered.pause_reason);
        assert!(matches!(fx.store.status(&fx.turn_id).unwrap(), Some(crate::harness::Status::Failed(_))));
        assert_eq!(fx.journal.stage_of(&fx.input).unwrap().as_deref(), Some("handled"), "confirmed send must never replay");
        assert!(
            !resume_queue().lock().unwrap().iter().any(|r| r.event_key == fx.input),
            "handled inbound must not enter resume_queue"
        );
        let wake = &fx.context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"];
        let sleep_until = wake["sleepUntil"].as_str().expect("consumed finite deadline must be re-armed");
        assert!(sleep_until > "2000-01-01T00:00:00.000Z", "{wake}");
        assert_eq!(wake["sleepRangeMs"], json!([60_000, 60_000]));
        admit_followup(&fx, "followup-297-send").expect("next wake must admit without admin resolution");
    }

    #[tokio::test]
    async fn ticket297_reserved2_reconciles_settled_non_send_tool_without_replay() {
        let fx = setup_submitted_harness_turn("her-297-tool-", "reserved2", true, None, true);
        record_settled_tool(&fx.store, &fx.turn_id, "qq_mark_read", "completed");

        let turns = fold(&turn_ledger::read_records(&fx.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx.context);
        Pass { context: &fx.context, cfg: &fx.cfg }.reconcile(&fx.journal, &turns[0], &turns, &outbox).await;

        let recovered = fold(&turn_ledger::read_records(&fx.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Failed);
        assert_eq!(fx.journal.stage_of(&fx.input).unwrap().as_deref(), Some("handled"));
        admit_followup(&fx, "followup-297-tool").expect("next wake must admit cleanly");
    }

    #[tokio::test]
    async fn ticket297_reserved2_resumes_running_turn_without_tools_or_sends_and_closes_store() {
        let previous = json!({
            "wakeConfig": {"confirmedAt": 1, "sleepUntil": "2000-01-01T00:00:00.000Z", "lastWakeAt": 0, "wakeCount": 0},
            "lastWakeReason": ""
        });
        let fx = setup_submitted_harness_turn(
            "her-297-resume-",
            "reserved2",
            true,
            Some(json!({"at": 10, "previous": previous})),
            true,
        );

        let turns = fold(&turn_ledger::read_records(&fx.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx.context);
        Pass { context: &fx.context, cfg: &fx.cfg }.reconcile(&fx.journal, &turns[0], &turns, &outbox).await;

        let recovered = fold(&turn_ledger::read_records(&fx.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Resumed, "got {:?}", recovered.pause_reason);
        assert!(matches!(fx.store.status(&fx.turn_id).unwrap(), Some(crate::harness::Status::Failed(_))));
        assert_eq!(
            fx.context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"],
            "2000-01-01T00:00:00.000Z"
        );
        assert_eq!(fx.journal.stage_of(&fx.input).unwrap().as_deref(), Some("received"));
        let mut queue = resume_queue().lock().unwrap();
        assert!(queue.iter().any(|r| r.event_key == fx.input));
        queue.retain(|r| r.event_key != fx.input);
        drop(queue);
        admit_followup(&fx, "followup-297-resume").expect("resumed wake must admit cleanly");
    }

    #[tokio::test]
    async fn ticket297_reserved2_reconciles_finished_store_and_preserves_store_pauses() {
        // Store already completed with observationSleep before ledger ended.
        let previous = json!({
            "wakeConfig": {"confirmedAt": 1, "sleepUntil": "2000-01-01T00:00:00.000Z", "lastWakeAt": 0, "wakeCount": 0},
            "lastWakeReason": ""
        });
        let fx = setup_submitted_harness_turn(
            "her-297-obs-",
            "reserved2",
            true,
            Some(json!({"at": 10, "previous": previous})),
            true,
        );
        fx.store.finish(&fx.turn_id, &crate::harness::Status::ObservationSleep, None).unwrap();
        let turns = fold(&turn_ledger::read_records(&fx.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx.context);
        Pass { context: &fx.context, cfg: &fx.cfg }.reconcile(&fx.journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&fx.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Completed);
        assert_eq!(recovered.outcome_reason, "observationSleep");
        assert_eq!(fx.journal.stage_of(&fx.input).unwrap().as_deref(), Some("handled"));
        assert!(fx.context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"].as_str().is_some());

        // Store explicitly marked Paused must still pause in the ledger.
        let fx_paused = setup_submitted_harness_turn("her-297-store-pause-", "reserved2", true, None, true);
        fx_paused.store.finish(&fx_paused.turn_id, &crate::harness::Status::Paused {
            reason: "send-unresolved".into(),
            detail: "uncertain interaction".into(),
        }, None).unwrap();
        let turns = fold(&turn_ledger::read_records(&fx_paused.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx_paused.context);
        Pass { context: &fx_paused.context, cfg: &fx_paused.cfg }.reconcile(&fx_paused.journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&fx_paused.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Paused);
        assert_eq!(recovered.pause_reason, "send-unresolved");
    }

    #[tokio::test]
    async fn ticket297_reserved2_respects_cancel_claims_and_revoked_validity() {
        // Pending cancel-requested with observationSleep closes store as ObservationSleep and rotates token.
        let fx_obs = setup_submitted_harness_turn("her-297-cancel-obs-", "reserved2", true, None, true);
        Recorder::attach(&fx_obs.context, &fx_obs.turn_id, "group:1")
            .verified("cancel-requested", "observationSleep")
            .unwrap();
        let turns = fold(&turn_ledger::read_records(&fx_obs.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx_obs.context);
        Pass { context: &fx_obs.context, cfg: &fx_obs.cfg }.reconcile(&fx_obs.journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&fx_obs.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Completed);
        assert_eq!(recovered.outcome_reason, "observationSleep");
        assert!(matches!(fx_obs.store.status(&fx_obs.turn_id).unwrap(), Some(crate::harness::Status::ObservationSleep)));
        assert_ne!(crate::social_state::token(&fx_obs.context, "group:1").unwrap().as_deref(), Some("fixture-token-297"));

        // Disallowed conversation abandons instead of resuming.
        let mut fx_revoked = setup_submitted_harness_turn("her-297-revoked-", "reserved2", true, None, true);
        fx_revoked.cfg["allow"] = json!({"groups": []});
        let turns = fold(&turn_ledger::read_records(&fx_revoked.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx_revoked.context);
        Pass { context: &fx_revoked.context, cfg: &fx_revoked.cfg }.reconcile(&fx_revoked.journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&fx_revoked.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Abandoned);
        assert!(matches!(fx_revoked.store.status(&fx_revoked.turn_id).unwrap(), Some(crate::harness::Status::Cancelled)));
        assert_eq!(fx_revoked.journal.stage_of(&fx_revoked.input).unwrap().as_deref(), Some("handled"));
    }

    #[tokio::test]
    async fn ticket297_unsettled_tools_unresolved_sends_and_non_reserved2_still_pause() {
        // Running tool intent in reserved2 still pauses as send-unresolved.
        let fx_unsettled = setup_submitted_harness_turn("her-297-unsettled-", "reserved2", true, None, true);
        record_settled_tool(&fx_unsettled.store, &fx_unsettled.turn_id, "qq_send_message", "running");
        let turns = fold(&turn_ledger::read_records(&fx_unsettled.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx_unsettled.context);
        Pass { context: &fx_unsettled.context, cfg: &fx_unsettled.cfg }.reconcile(&fx_unsettled.journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&fx_unsettled.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Paused);
        assert_eq!(recovered.pause_reason, "send-unresolved");

        // Unresolved outbox send in reserved2 still pauses as send-unresolved.
        let fx_send = setup_submitted_harness_turn("her-297-unres-send-", "reserved2", true, None, true);
        let turns = fold(&turn_ledger::read_records(&fx_send.context).unwrap()).0;
        let send_t = turns[0].begun_at;
        std::fs::write(
            fx_send._root.0.join("data/send-outbox.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"k":"intent","batch":"b297u","key":"group:1","epoch":fx_send.epoch,"t":send_t,"messages":["hi"]}),
                json!({"k":"attempt","batch":"b297u","idx":0,"t":send_t}),
            ),
        )
        .unwrap();
        let outbox = crate::send_outbox::read_records(&fx_send.context);
        Pass { context: &fx_send.context, cfg: &fx_send.cfg }.reconcile(&fx_send.journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&fx_send.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Paused);
        assert_eq!(recovered.pause_reason, "send-unresolved");

        // Non-reserved2 (chat) harness turn with settled tools still pauses as harness-interrupted.
        let fx_chat = setup_submitted_harness_turn("her-297-chat-", "chat", false, None, true);
        record_settled_tool(&fx_chat.store, &fx_chat.turn_id, "web_search", "completed");
        let turns = fold(&turn_ledger::read_records(&fx_chat.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx_chat.context);
        Pass { context: &fx_chat.context, cfg: &fx_chat.cfg }.reconcile(&fx_chat.journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&fx_chat.context).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Paused);
        assert_eq!(recovered.pause_reason, "harness-interrupted");
    }

    #[tokio::test]
    async fn ticket297_covers_unaccepted_attempt_and_observation_sleep_crash_windows() {
        // Crash between `attempt` (unaccepted) and `conversations.db` admission reconciles safely without pause.
        let root_unaccepted = crate::install_root::development_root(
            std::env::temp_dir().join(crate::turn_ledger::mint_id("her-297-unaccepted-")),
        );
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root_unaccepted.join(dir)).unwrap();
        }
        std::fs::write(
            root_unaccepted.join("config/config.json"),
            r#"{"consoleToken":"synthetic-review-token","allow":{"groups":[1]}}"#,
        )
        .unwrap();
        std::fs::write(root_unaccepted.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(
            root_unaccepted.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        let tok = "fixture-unaccepted-tok";
        let deadline = crate::sticker::iso_at(crate::turn_ledger::now_ms() + 600_000);
        let prev = json!({"wakeConfig":{"confirmedAt":1,"infinite":false,"sleepUntil":deadline},"lastWakeReason":""});
        std::fs::write(
            root_unaccepted.join("data/social-v2.json"),
            json!({"conversations":{"group:1":{"agentToken":tok,"wakeConfig":{"confirmedAt":1,"infinite":false,"lastWakeAt":10,"sleepUntil":null}}}}).to_string(),
        )
        .unwrap();
        let ctx = RuntimeContext::from_root(&root_unaccepted).unwrap();
        let cfg = ctx.load_config().unwrap();
        let journal = Journal::open(&ctx).unwrap();
        let rec = Recorder::begin_reserved2(&ctx, "group:1", "harness", "timeout", &crate::send_outbox::token_epoch(tok), &[]).unwrap();
        rec.wake_claim(&json!({"at":10,"previous":prev})).unwrap();
        rec.attempt(&format!("harness:{}", rec.id), "").unwrap();
        let turns = fold(&turn_ledger::read_records(&ctx).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&ctx);
        Pass { context: &ctx, cfg: &cfg }.reconcile(&journal, &turns[0], &turns, &outbox).await;
        let recovered = fold(&turn_ledger::read_records(&ctx).unwrap()).0.remove(0);
        assert_eq!(recovered.state, State::Resumed, "unaccepted attempt before store admission must resume");
        assert_eq!(ctx.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"], deadline);
        drop(journal);
        let _ = std::fs::remove_dir_all(&root_unaccepted);

        // Crash right after `end_cancellation("completed", "observationSleep")` re-arms consumed deadline at startup.
        let mut fx_obs = setup_submitted_harness_turn("her-297-obs-rearm-", "reserved2", true, None, true);
        fx_obs.cfg["socialV2"]["wake"] = json!({"recommendedSleepMinMs": 60_000, "recommendedSleepMaxMs": 60_000});
        let past_deadline = "2000-01-01T00:00:00.000Z";
        let obs_claim = json!({"at": 10, "previous": {"wakeConfig": {"confirmedAt": 1, "sleepUntil": past_deadline}}});
        turn_ledger::Recorder::attach(&fx_obs.context, &fx_obs.turn_id, "group:1")
            .wake_claim(&obs_claim)
            .unwrap();
        turn_ledger::Recorder::attach(&fx_obs.context, &fx_obs.turn_id, "group:1")
            .end_cancellation("completed", "observationSleep")
            .unwrap();
        crate::conversation_state::ConversationState::for_runtime(&fx_obs.context)
            .update("group:1", |conv| {
                let mut wc = conv.field("wakeConfig").cloned().unwrap_or_else(|| json!({}));
                wc["sleepUntil"] = Value::Null;
                wc["confirmedAt"] = json!(1);
                wc["infinite"] = json!(false);
                conv.set_field("wakeConfig", wc);
                Ok(())
            })
            .unwrap();
        let turns = fold(&turn_ledger::read_records(&fx_obs.context).unwrap()).0;
        assert!(rearm_startup_terminal_sleeps(&fx_obs.context, &fx_obs.cfg, &turns, crate::turn_ledger::now_ms() + 1000));
        let sleep_until = fx_obs.context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"]
            .as_str()
            .and_then(crate::wake::parse_iso_ms);
        assert!(sleep_until.is_some(), "interrupted observationSleep must re-arm finite sleepUntil at startup");

        // Ordinary `completed` (`ok`) before the hub schedules the next wake also re-arms at startup.
        let mut fx_ok = setup_submitted_harness_turn("her-297-ok-rearm-", "reserved2", true, None, true);
        fx_ok.cfg["socialV2"]["wake"] = json!({"recommendedSleepMinMs": 60_000, "recommendedSleepMaxMs": 60_000});
        turn_ledger::Recorder::attach(&fx_ok.context, &fx_ok.turn_id, "group:1")
            .wake_claim(&obs_claim)
            .unwrap();
        turn_ledger::Recorder::attach(&fx_ok.context, &fx_ok.turn_id, "group:1")
            .end("completed", "ok")
            .unwrap();
        crate::conversation_state::ConversationState::for_runtime(&fx_ok.context)
            .update("group:1", |conv| {
                let mut wc = conv.field("wakeConfig").cloned().unwrap_or_else(|| json!({}));
                wc["sleepUntil"] = Value::Null;
                wc["confirmedAt"] = json!(1);
                wc["infinite"] = json!(false);
                conv.set_field("wakeConfig", wc);
                Ok(())
            })
            .unwrap();
        let ok_turns = fold(&turn_ledger::read_records(&fx_ok.context).unwrap()).0;
        assert!(rearm_startup_terminal_sleeps(&fx_ok.context, &fx_ok.cfg, &ok_turns, crate::turn_ledger::now_ms() + 1000));
        assert!(
            fx_ok.context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"]
                .as_str()
                .and_then(crate::wake::parse_iso_ms)
                .is_some(),
            "ordinary completed reserved2 wake must re-arm finite sleepUntil at startup"
        );

        // Pre-admission abandonment keeps the turn open if rearm fails transiently (`mode.json` corrupt),
        // then re-arms before committing `abandoned` once recovered; mid-close crash is finished by `finished()`.
        let mut fx_abandoned = setup_submitted_harness_turn("her-297-abandoned-sqlite-", "reserved2", true, None, true);
        fx_abandoned.cfg["socialV2"]["wake"] = json!({"recommendedSleepMinMs": 60_000, "recommendedSleepMaxMs": 60_000});
        let abandoned_rec = turn_ledger::Recorder::attach(&fx_abandoned.context, &fx_abandoned.turn_id, "group:1");
        abandoned_rec.wake_claim(&obs_claim).unwrap();
        abandoned_rec.end_reconciled("abandoned", "revoked before sqlite close").unwrap();
        let turns = fold(&turn_ledger::read_records(&fx_abandoned.context).unwrap()).0;
        Pass { context: &fx_abandoned.context, cfg: &fx_abandoned.cfg }.finished(&fx_abandoned.journal, &turns);
        assert!(matches!(fx_abandoned.store.status(&fx_abandoned.turn_id).unwrap(), Some(crate::harness::Status::Cancelled)));

        let root_pre_abandon = crate::install_root::development_root(
            std::env::temp_dir().join(crate::turn_ledger::mint_id("her-297-pre-abandon-")),
        );
        for dir in ["config", "data", "resources", "user/dsh"] {
            std::fs::create_dir_all(root_pre_abandon.join(dir)).unwrap();
        }
        std::fs::write(
            root_pre_abandon.join("config/config.json"),
            r#"{"consoleToken":"synthetic-review-token","allow":{"groups":[1]},"socialV2":{"wake":{"recommendedSleepMinMs":60000,"recommendedSleepMaxMs":60000}}}"#,
        )
        .unwrap();
        std::fs::write(root_pre_abandon.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(
            root_pre_abandon.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        std::fs::write(
            root_pre_abandon.join("data/social-v2.json"),
            json!({"conversations":{"group:1":{"agentToken":"tok-pre-abandon","wakeConfig":{"confirmedAt":1,"infinite":false,"lastWakeAt":10,"sleepUntil":null}}}}).to_string(),
        )
        .unwrap();
        let pre_ctx = RuntimeContext::from_root(&root_pre_abandon).unwrap();
        let pre_cfg = pre_ctx.load_config().unwrap();
        let pre_journal = Journal::open(&pre_ctx).unwrap();
        let pre_rec = Recorder::begin_reserved2(&pre_ctx, "group:1", "harness", "timeout", "stale-epoch", &[]).unwrap();
        pre_rec.wake_claim(&obs_claim).unwrap();
        // Force a transient rearm failure before ledger abandonment: apply() keeps the turn open, then retries once recovered.
        std::fs::write(root_pre_abandon.join("data/mode.json"), "{broken").unwrap();
        let pre_turns = fold(&turn_ledger::read_records(&pre_ctx).unwrap()).0;
        apply(&pre_ctx, &pre_cfg, &pre_journal, &pre_turns[0], &Verdict::Abandon("token changed".into()), None);
        assert_eq!(
            fold(&turn_ledger::read_records(&pre_ctx).unwrap()).0.remove(0).state,
            State::Begun,
            "transient rearm failure must keep pre-admission turn open for retry"
        );
        std::fs::write(root_pre_abandon.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        let pre_outbox = crate::send_outbox::read_records(&pre_ctx);
        Pass { context: &pre_ctx, cfg: &pre_cfg }.reconcile(&pre_journal, &pre_turns[0], &pre_turns, &pre_outbox).await;
        let pre_done = fold(&turn_ledger::read_records(&pre_ctx).unwrap()).0.remove(0);
        assert_eq!(pre_done.state, State::Abandoned);
        assert!(
            pre_ctx.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"]
                .as_str()
                .and_then(crate::wake::parse_iso_ms)
                .is_some(),
            "pre-admission abandon must re-arm consumed finite sleepUntil before ending"
        );
        drop(pre_journal);
        let _ = std::fs::remove_dir_all(&root_pre_abandon);

        // Unreadable mode.json fails closed as send-unresolved instead of rolling back and resuming a claimed wake.
        let fx_bad_mode = setup_submitted_harness_turn("her-297-bad-mode-", "reserved2", true, None, true);
        std::fs::write(fx_bad_mode._root.0.join("data/mode.json"), "{broken").unwrap();
        let bad_rec = turn_ledger::Recorder::attach(&fx_bad_mode.context, &fx_bad_mode.turn_id, "group:1");
        bad_rec.wake_claim(&obs_claim).unwrap();
        let turns = fold(&turn_ledger::read_records(&fx_bad_mode.context).unwrap()).0;
        let outbox = crate::send_outbox::read_records(&fx_bad_mode.context);
        Pass { context: &fx_bad_mode.context, cfg: &fx_bad_mode.cfg }.reconcile(&fx_bad_mode.journal, &turns[0], &turns, &outbox).await;
        let paused = fold(&turn_ledger::read_records(&fx_bad_mode.context).unwrap()).0.remove(0);
        assert_eq!(paused.state, State::Paused);
        assert_eq!(paused.pause_reason, "send-unresolved");

        // An already-closed historical unclaimed abandoned turn must not re-arm a live wake's timer.
        let fx_unclaimed_abandoned = setup_submitted_harness_turn("her-297-unclaimed-aband-", "reserved2", true, None, false);
        turn_ledger::Recorder::attach(&fx_unclaimed_abandoned.context, &fx_unclaimed_abandoned.turn_id, "group:1")
            .end_reconciled("abandoned", "revoked")
            .unwrap();
        fx_unclaimed_abandoned.store.finish(&fx_unclaimed_abandoned.turn_id, &crate::harness::Status::Cancelled, None).unwrap();
        crate::conversation_state::ConversationState::for_runtime(&fx_unclaimed_abandoned.context)
            .update("group:1", |conv| {
                let mut wc = conv.field("wakeConfig").cloned().unwrap_or_else(|| json!({}));
                wc["lastWakeAt"] = json!(crate::turn_ledger::now_ms() + 100_000);
                wc["confirmedAt"] = json!(crate::turn_ledger::now_ms() + 100_000);
                wc["sleepUntil"] = Value::Null;
                wc["infinite"] = json!(false);
                conv.set_field("wakeConfig", wc);
                Ok(())
            })
            .unwrap();
        let unclaimed_turns = fold(&turn_ledger::read_records(&fx_unclaimed_abandoned.context).unwrap()).0;
        assert!(unclaimed_turns[0].wake_claim.is_none());
        Pass { context: &fx_unclaimed_abandoned.context, cfg: &fx_unclaimed_abandoned.cfg }
            .finished(&fx_unclaimed_abandoned.journal, &unclaimed_turns);
        assert!(
            fx_unclaimed_abandoned.context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"].is_null(),
            "historical unclaimed closed abandoned turn must not overwrite live wake timer"
        );

        // Provisional claim (lastWakeAt bumped before cutoff without a ledger turn) still allows startup repair,
        // and socialV2.enabled=false at restart still re-arms consumed finite deadlines.
        let mut fx_prov = setup_submitted_harness_turn("her-297-provisional-", "reserved2", true, None, true);
        fx_prov.cfg["socialV2"]["enabled"] = json!(false);
        fx_prov.cfg["socialV2"]["wake"] = json!({"recommendedSleepMinMs": 60_000, "recommendedSleepMaxMs": 60_000});
        turn_ledger::Recorder::attach(&fx_prov.context, &fx_prov.turn_id, "group:1")
            .wake_claim(&obs_claim)
            .unwrap();
        turn_ledger::Recorder::attach(&fx_prov.context, &fx_prov.turn_id, "group:1")
            .end("completed", "ok")
            .unwrap();
        let startup_cutoff = crate::turn_ledger::now_ms() + 500;
        crate::conversation_state::ConversationState::for_runtime(&fx_prov.context)
            .update("group:1", |conv| {
                let mut wc = conv.field("wakeConfig").cloned().unwrap_or_else(|| json!({}));
                wc["lastWakeAt"] = json!(startup_cutoff - 10);
                wc["confirmedAt"] = json!(1);
                wc["sleepUntil"] = Value::Null;
                wc["infinite"] = json!(false);
                conv.set_field("wakeConfig", wc);
                Ok(())
            })
            .unwrap();
        let prov_turns = fold(&turn_ledger::read_records(&fx_prov.context).unwrap()).0;
        assert!(rearm_startup_terminal_sleeps(&fx_prov.context, &fx_prov.cfg, &prov_turns, startup_cutoff));
        assert!(
            fx_prov.context.read_social_state().unwrap()["conversations"]["group:1"]["wakeConfig"]["sleepUntil"]
                .as_str()
                .and_then(crate::wake::parse_iso_ms)
                .is_some(),
            "provisional claim before startup cutoff and socialV2.enabled=false must still re-arm finite sleepUntil"
        );

        // Non-harness (DSH) reserved2 turns are unaffected by harness re-arming during `apply`.
        let mut dsh_turn = turns[0].clone();
        dsh_turn.backend = "dsh".into();
        dsh_turn.reserved2 = true;
        dsh_turn.wake_claim = Some(obs_claim.clone());
        std::fs::write(fx_abandoned._root.0.join("data/mode.json"), "{broken").unwrap();
        assert!(rearm_reserved2_if_needed(&fx_abandoned.context, &fx_abandoned.cfg, &dsh_turn, None).is_ok());
    }

    /// Ticket 24 second fix round (Codex 7): a revocation (or reset) voids work of the old epoch
    /// that recovery still holds: an administrator's pending `retry` of a paused turn and wakes held
    /// while that turn blocked the conversation. Re-granting access must not hand them back.
    #[test]
    fn ticket24_fix2_a_revocation_voids_a_pending_retry_and_held_wakes() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id("her-t24-recovery-")));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token-abcdefghij","allow":{"groups":[1]}}"#).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), json!({"conversations":{"group:1":{"agentToken":"fixture-token-abcdefghij"}}}).to_string()).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let journal = Journal::open(&context).unwrap();
        let (retried, held) = ("group:1|message:7".to_string(), "group:1|message:8".to_string());
        for event in [&retried, &held] {
            journal.record_received(event, "group:1", "{}", 1).unwrap();
            journal.mark_handled(&[event.clone()], 2).unwrap();
        }
        let turn = crate::turn_ledger::Recorder::begin(&context, "group:1", "dsh", "atMention", "epoch", &[retried.clone()], None).unwrap();
        turn.unknown("send-unresolved", "synthetic").unwrap();
        crate::turn_ledger::append_record(&context, &json!({"k":"held","t":crate::turn_ledger::now_ms(),"key":"group:1","inbound":[held.clone()],"reason":"atMention","blockedBy":turn.id})).unwrap();
        turn.resolved("retry").unwrap();
        // The administrator revokes the conversation and re-grants it before recovery's next pass.
        let (status, reply) = crate::console::admin_request(&context, "POST", "/api/whitelist", Some(&json!({"allow": {"groups": []}, "confirm": true})));
        assert_eq!(status, 200, "{reply}");
        let (_, recent) = crate::console::admin_request(&context, "GET", "/api/audit/recent", None);
        let id = recent["entries"][0]["id"].clone();
        let (status, reply) = crate::console::admin_request(&context, "POST", "/api/audit/undo", Some(&json!({"id": id, "confirm": true})));
        assert_eq!(status, 200, "{reply}");
        let cfg = context.load_config().unwrap();
        let pass = Pass { context: &context, cfg: &cfg };
        let records = crate::turn_ledger::read_records(&context).unwrap();
        pass.resolutions(&journal, &fold(&records).0, &records);
        let records = crate::turn_ledger::read_records(&context).unwrap();
        pass.release_held(&journal, &fold(&records).0, &records);
        let stages: Vec<Option<String>> = [&retried, &held].iter().map(|e| journal.stage_of(e).unwrap()).collect();
        let queued: Vec<String> = resume_queue().lock().unwrap().iter().map(|r| r.event_key.clone()).filter(|k| *k == retried || *k == held).collect();
        drop(journal);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(stages[0].as_deref(), Some("handled"), "the pending retry of the revoked epoch was handed back");
        assert_eq!(stages[1].as_deref(), Some("handled"), "the wake held under the revoked epoch was handed back");
        assert!(queued.is_empty(), "work of the revoked epoch was queued for replay: {queued:?}");
    }

    fn attempt(rpc: &str) -> Value {
        json!({"k":"attempt","turn":"t1","t":11,"rpcId":rpc,"sessionId":"s1"})
    }

    fn facts(ended: Option<Value>, tools: usize) -> PromptProbe {
        PromptProbe::Turn(TurnFacts {
            rpc_id: "r1".into(),
            dsh_turn: 1,
            ended,
            tools: (0..tools)
                .map(|i| crate::dsh::ToolFact { call_id: i.to_string(), name: "x".into(), done: true, ok: true, step: None })
                .collect(),
            text: String::new(),
        })
    }

    #[test]
    fn a_turn_that_never_called_dsh_resumes_only_while_valid() {
        let t = turn(vec![begun()]);
        assert!(matches!(decide(&t, None, &SendState::None, Ok(())), Verdict::Resume(_)));
        assert!(matches!(decide(&t, None, &SendState::None, Err("reset".into())), Verdict::Abandon(_)));
    }

    #[test]
    fn hf136_fix1_recovery_preserves_the_first_claim_after_a_crash() {
        for (verdict, detail, expected) in [
            ("completion-observed", "DSH completed", Verdict::Complete("ok".into())),
            ("cancel-requested", "turnTimeout", Verdict::Cancel("turnTimeout".into())),
            ("cancel-requested", "observationSleep", Verdict::Complete("observationSleep".into())),
        ] {
            let t = turn(vec![begun(), attempt("r1"), json!({"k":"verified","turn":"t1","t":12,"verdict":verdict,"detail":detail})]);
            // Completion and definite non-execution must both close the claimed turn,
            // without replay. Unresolved evidence still pauses/defers normally.
            let done = facts(Some(json!({"turn":1,"reason":{"kind":"completed"}})), 0);
            for probe in [&done, &PromptProbe::SessionGone] {
                let actual = decide(&t, Some(probe), &SendState::None, Ok(()));
                if verdict == "completion-observed" { assert!(matches!(actual, Verdict::Complete(_)), "observed completion cannot replay: {actual:?}"); }
                else { assert_eq!(actual, expected); }
            }
            assert!(matches!(decide(&t, Some(&done), &SendState::Unresolved("unknown".into()), Ok(())), Verdict::Pause { .. }));
            assert!(matches!(decide(&t, Some(&PromptProbe::Unreachable("offline".into())), &SendState::None, Ok(())), Verdict::Defer(_)));
        }
    }

    #[test]
    fn a_completed_dsh_turn_is_never_redone() {
        let t = turn(vec![begun(), attempt("r1")]);
        let done = facts(Some(json!({"turn":1,"reason":{"kind":"completed"}})), 1);
        assert!(matches!(decide(&t, Some(&done), &SendState::Settled, Ok(())), Verdict::Complete(_)));
    }

    #[test]
    fn a_failed_turn_is_redone_only_without_tools_or_sends() {
        let t = turn(vec![begun(), attempt("r1")]);
        let err = json!({"turn":1,"reason":{"kind":"error","error":{"code":"x","message":"y"}}});
        assert!(matches!(decide(&t, Some(&facts(Some(err.clone()), 0)), &SendState::None, Ok(())), Verdict::Resume(_)));
        assert!(matches!(decide(&t, Some(&facts(Some(err.clone()), 2)), &SendState::None, Ok(())), Verdict::Fail(_)));
        assert!(matches!(decide(&t, Some(&facts(Some(err), 0)), &SendState::Settled, Ok(())), Verdict::Fail(_)));
    }

    #[test]
    fn running_or_queued_turns_are_reattached_not_resubmitted() {
        let t = turn(vec![begun(), attempt("r1")]);
        assert_eq!(decide(&t, Some(&facts(None, 0)), &SendState::None, Ok(())), Verdict::Reattach);
        assert_eq!(decide(&t, Some(&PromptProbe::Queued), &SendState::None, Ok(())), Verdict::Reattach);
    }

    #[test]
    fn unprovable_or_unreachable_states_never_resume() {
        let t = turn(vec![begun(), attempt("r1")]);
        assert!(matches!(
            decide(&t, Some(&PromptProbe::NotFound { proved: false }), &SendState::None, Ok(())),
            Verdict::Pause { rotate: true, .. }
        ));
        assert!(matches!(
            decide(&t, Some(&PromptProbe::Unreachable("down".into())), &SendState::None, Ok(())),
            Verdict::Defer(_)
        ));
        assert!(matches!(
            decide(&t, Some(&PromptProbe::NotFound { proved: true }), &SendState::None, Ok(())),
            Verdict::Resume(_)
        ));
    }

    #[test]
    fn unresolved_sends_pause_even_a_finished_turn() {
        let t = turn(vec![begun(), attempt("r1")]);
        let done = facts(Some(json!({"turn":1,"reason":{"kind":"completed"}})), 1);
        let unresolved = SendState::Unresolved("batch b1".into());
        assert!(matches!(decide(&t, Some(&done), &unresolved, Ok(())), Verdict::Pause { reason, .. } if reason == "send-unresolved"));
    }

    #[test]
    fn gap_ledger_an_unreadable_turn_ledger_makes_the_reattached_turn_sends_unknown() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(crate::turn_ledger::mint_id("her-reattach-unreadable-")));
        for dir in ["config", "data", "resources", "user/dsh"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-review-token-abcdefghij","allow":{"groups":[1]}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let cfg = context.load_config().unwrap();
        let t = turn(vec![begun(), attempt("r1")]);
        crate::turn_ledger::append_record(&context, &begun()).unwrap();
        assert!(matches!(reattach_sends(&context, &cfg, &t), SendState::None), "an intact ledger has no sends for this turn");
        let path = root.join("data").join(crate::turn_ledger::ISOLATED_LEDGER_FILE);
        std::fs::write(&path, format!("{}\nnot json\n{}\n", begun(), begun())).unwrap();
        assert!(matches!(reattach_sends(&context, &cfg, &t), SendState::Unreadable(_)), "a damaged ledger must not read as no sends");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gap_ledger_startup_turns_are_taken_from_records_begun_before_the_cutoff() {
        let old = json!({"k":"begun","turn":"old","t":10,"key":"group:1","epoch":"e1","inbound":[],"reason":"atMention"});
        let live = json!({"k":"begun","turn":"live","t":500,"key":"group:2","epoch":"e1","inbound":[],"reason":"atMention"});
        let set = startup_turns(&[old, live], 100);
        assert_eq!(set.into_iter().collect::<Vec<_>>(), vec!["old".to_string()]);
    }

    #[test]
    fn an_unreadable_send_ledger_pauses_as_send_unresolved_and_rotates_the_token() {
        let unreadable = SendState::Unreadable("ledger torn".into());
        let is_rotating_pause = |v: Verdict| matches!(v, Verdict::Pause { reason, rotate: true, .. } if reason == "send-unresolved");
        let t = turn(vec![begun(), attempt("r1")]);
        let done = facts(Some(json!({"turn":1,"reason":{"kind":"completed"}})), 1);
        assert!(is_rotating_pause(decide(&t, Some(&done), &unreadable, Ok(()))), "a finished turn is not closed");
        let err = json!({"turn":1,"reason":{"kind":"error","error":{"code":"x","message":"y"}}});
        assert!(is_rotating_pause(decide(&t, Some(&facts(Some(err), 0)), &unreadable, Ok(()))), "an errored turn without tools is not redone");
        assert!(is_rotating_pause(decide(&t, Some(&PromptProbe::NotFound { proved: true }), &unreadable, Ok(()))));
        let never_sent = turn(vec![begun()]);
        assert!(is_rotating_pause(decide(&never_sent, None, &unreadable, Ok(()))), "a turn that never reached DSH is not resumed");
        assert_eq!(decide(&t, Some(&facts(None, 0)), &unreadable, Ok(())), Verdict::Reattach, "a running turn is still waited for");
    }

    #[test]
    fn cli_turns_with_an_attempt_are_paused() {
        let t = turn(vec![begun(), attempt("cli:abc")]);
        assert!(matches!(decide(&t, None, &SendState::None, Ok(())), Verdict::Pause { reason, .. } if reason == "cli-send-unverifiable"));
    }

    #[test]
    fn an_event_without_a_turn_is_only_provably_unexecuted_when_the_ledger_was_running() {
        let records = vec![begun()];
        assert!(ledger_covers(&records, 10), "event marked processing when the ledger already had a record");
        assert!(ledger_covers(&records, 5_000));
        assert!(!ledger_covers(&records, 9), "event interrupted before the ledger existed proves nothing");
        assert!(!ledger_covers(&[], 5_000), "no ledger at all proves nothing");
    }

    #[test]
    fn send_state_flags_unknown_and_half_finished_batches() {
        let outbox = vec![
            json!({"k":"intent","batch":"b1","key":"group:1","epoch":"e1","t":12,"messages":["a","b"]}),
            json!({"k":"attempt","batch":"b1","idx":0,"t":13}),
            json!({"k":"confirmed","batch":"b1","idx":0,"t":14}),
        ];
        let t = turn(vec![begun()]);
        let batches = turn_ledger::batches_of_turn(&t, None, &outbox);
        assert!(matches!(send_state(&batches), SendState::Unresolved(_)));
        assert_eq!(send_state(&[]), SendState::None);
    }

    #[test]
    fn a_stuck_resumed_event_is_reported_once_across_restarts() {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-reported-{}", crate::turn_ledger::mint_id(""))));
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token-abcdefghij"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let first_process = Mutex::new(HashSet::new());
        assert!(first_reported_in(&first_process, &context, "evt-1"), "the first sighting is reported");
        assert!(!first_reported_in(&first_process, &context, "evt-1"), "not twice in one process");
        // A restart: nothing remembered in memory, the same data directory.
        let second_process = Mutex::new(HashSet::new());
        assert!(!first_reported_in(&second_process, &context, "evt-1"), "not again after a restart");
        assert!(first_reported_in(&second_process, &context, "evt-2"), "another event is still reported");
        for i in 0..(REPORTED_MAX + 20) {
            first_reported_in(&second_process, &context, &format!("bulk-{i}"));
        }
        let kept = std::fs::read_to_string(root.join("data").join(REPORTED_FILE)).unwrap();
        assert!(serde_json::from_str::<Vec<String>>(&kept).unwrap().len() <= REPORTED_MAX, "the memory is bounded");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
