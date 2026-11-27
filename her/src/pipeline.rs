//! P2-C 接线：消息 → 触发 → 会话（落盘复用）→ prompt → 等回复 → 发回 QQ。
//!
//! 设计约束：
//!   1. **默认不发送**。只有 `--live` 才真的发；否则只打日志（dry-run）。
//!   2. **出站哨兵**：会话令牌、Cookie、本机绝对路径一律拒绝发送。
//!   3. **会话映射落盘**（等价 JS 的 `state.sessions`），重启不丢。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::dsh::Rpc;
use crate::candidate_state::FailoverState;
use crate::failover;
use crate::recovery_log;
use crate::run_record::{Component, Outcome, Trigger};
use std::sync::{Arc, Mutex, OnceLock};
use crate::onebot::{OneBot, RateLimiter, SendTuning, Target};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

// The DSH turn adapter (`turn_engine::Adapter::Dsh`): its sessions, model failover, prompt
// submission, collector, cancellation and usage rows.
mod dsh_adapter;
pub(crate) use dsh_adapter::is_dsh_access_refusal;

/// The explicit per-conversation CLI route, using the same rules as turn dispatch.
pub(crate) fn pinned_cli_backend<'a>(cfg: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    cfg.pointer("/backends/sessions")
        .and_then(|sessions| sessions.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

struct WakePromptRefs {
    dsh: Option<Vec<serde_json::Value>>,
    cli: Option<Vec<serde_json::Value>>,
}

#[cfg(not(test))]
const SESSION_FILE: &str = crate::legacy_path!("state/rust-sessions.json");

#[cfg(test)]
// Keep legacy-wrapper tests off the fixed production path; an unscoped test call fails closed.
mod legacy_session_file_scope {
    use std::cell::RefCell;
    use std::path::PathBuf;

    std::thread_local! {
        static PATHS: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
    }

    struct ScopeGuard;

    impl Drop for ScopeGuard {
        fn drop(&mut self) {
            PATHS.with(|paths| {
                let popped = paths.borrow_mut().pop();
                debug_assert!(popped.is_some(), "legacy session file scope stack underflow");
            });
        }
    }

    pub(super) fn current() -> Option<PathBuf> {
        PATHS.with(|paths| paths.borrow().last().cloned())
    }

    pub(super) fn with_path<T>(path: PathBuf, action: impl FnOnce() -> T) -> T {
        PATHS.with(|paths| paths.borrow_mut().push(path));
        let _scope = ScopeGuard;
        action()
    }
}

#[cfg(test)]
fn session_store_file_path() -> PathBuf {
    legacy_session_file_scope::current()
        .expect("legacy SessionStore I/O in unit tests must use a scoped synthetic path")
}

#[cfg(not(test))]
fn session_store_file_path() -> PathBuf {
    PathBuf::from(SESSION_FILE)
}

/// 一个 QQ 会话在 DSH 侧的映射。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionRef {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "workspaceId", default)]
    pub workspace_id: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionStore {
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub sessions: HashMap<String, SessionRef>,
}

impl SessionStore {
    pub fn load() -> Self {
        std::fs::read_to_string(session_store_file_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn load_for_runtime(context: &RuntimeContext) -> Result<Self, String> {
        if !context.is_isolated() {
            return Ok(Self::load());
        }
        // The selected runtime shares one mapping with the admin console (`session_map`).
        let mut store = Self::default();
        store.workspace_id = crate::session_map::workspace_id(context)?.unwrap_or_default();
        for (key, session_id) in crate::session_map::all(context)? {
            store.sessions.insert(key, SessionRef { session_id, workspace_id: store.workspace_id.clone() });
        }
        Ok(store)
    }

    pub fn save(&self) -> Result<(), String> {
        let session_file = session_store_file_path();
        if let Some(dir) = session_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(session_file, text).map_err(|e| format!("写会话映射失败：{e}"))
    }

    pub fn save_for_runtime(&self, context: &RuntimeContext) -> Result<(), String> {
        if !context.is_isolated() {
            return self.save();
        }
        // Selected runtimes write through `session_map` (locked, merge-preserving); nothing to do.
        Ok(())
    }
}

/// 出站哨兵：这些内容绝不允许发到 QQ。
///
/// 与 JS 版 `sensitive.js` 的 `SENSITIVE_RE` 等价（那条规则曾把带路径的回复整条拦下）。
/// JS 的 `isSilentMarker`：整段只有 `[SILENT]`（大小写不敏感、允许首尾空白）才算静默。
/// 注意是**整段相等**，不是包含 —— 「他说 [SILENT] 这个词」不该被判成静默。
pub fn is_silent_marker(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("[SILENT]")
}
/// The four alternatives of the frozen JS `SENSITIVE_RE`, in alternation order: Windows path,
/// UNC path, Unix path and credential assignment. Two of them carry a JS lookbehind that this
/// engine lacks; callers check the character before a match themselves.
fn sensitive_patterns() -> &'static (regex::Regex, regex::Regex, regex::Regex, regex::Regex) {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<(Regex, Regex, Regex, Regex)> = OnceLock::new();
    RE.get_or_init(|| {
        (
            Regex::new(r#"[A-Za-z]:[\\/][^\s'"<>|]+"#).unwrap(),
            Regex::new(r#"\\\\[^\\\s'"<>|]+\\[^\s'"<>|]+"#).unwrap(),
            Regex::new(r#"(?i)(?:/home/|/Users/|/etc/|/var/|/root/)[^\s'"<>|]*"#).unwrap(),
            Regex::new(concat!(
                r#"(?i)(?:token|密码|密钥|口令|password|passwd|secret|api[_-]?key|"#,
                r#"authorization|bearer|access[_-]?key|credential)['"]?"#,
                r#"(?:\s*(?:[:=：]|是|为)\s*['"]?[^\s，。；、'"]{3,}"#,
                r#"|\s+[A-Za-z0-9_\-./]{3,})"#
            ))
            .unwrap(),
        )
    })
}

/// Sensitive patterns retain JS precedence; token and sensitive spans are matched
/// in the original text and masked together so neither can hide the other.
pub fn redact_sensitive_text(text: &str, known_tokens: &[String]) -> String {
    redact_sensitive_text_as(text, known_tokens, "***")
}

fn redact_sensitive_text_as(text: &str, known_tokens: &[String], mask: &str) -> String {
    let (win, unc, unix, cred) = sensitive_patterns();
    let boundary_ok = |s: &str, start: usize| -> bool {
        start == 0 || !s[..start].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric())
    };
    // First valid match of one alternative at or after `from`: (start, end).
    let first = |re: &regex::Regex, bounded: bool, from: usize| -> Option<(usize, usize)> {
        let mut pos = from;
        while pos <= text.len() {
            let m = re.find_at(text, pos)?;
            if !bounded || boundary_ok(text, m.start()) {
                return Some((m.start(), m.end()));
            }
            pos = m.start() + text[m.start()..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        None
    };
    let mut ranges = crate::activity::session_token_ranges(text, known_tokens);
    let mut from = 0usize;
    while from <= text.len() {
        let candidates = [first(win, true, from), first(unc, false, from), first(unix, true, from), first(cred, false, from)];
        let mut best: Option<(usize, usize)> = None;
        for c in candidates.into_iter().flatten() {
            if best.is_none_or(|b| c.0 < b.0) {
                best = Some(c);
            }
        }
        let Some((start, end)) = best else { break };
        if end == start {
            // JS would advance one code point past an empty match; none of the patterns can be empty.
            break;
        }
        ranges.push((start, end));
        from = end;
    }
    crate::activity::mask_ranges(text, ranges, mask)
}

/// Mask the full output before any cut, drop an initial reasoning block, and
/// keep one bounded diagnostic line. This is local logging, independent of DLP.
fn internal_output_preview(context: &RuntimeContext, text: &str) -> String {
    let safe = crate::activity::mask_session_tokens(text, &crate::activity::known_session_tokens(context));
    let normalized = safe.replace("\r\n", "\n");
    let trimmed = normalized.trim();
    let thought = trimmed.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("thought"))
        && trimmed[7..].chars().next().is_none_or(|c| c.is_whitespace() || c == ':');
    let visible = if thought {
        let mut end = 0;
        trimmed.split_inclusive('\n').find_map(|line| {
            end += line.len();
            line.trim().is_empty().then_some(trimmed[end..].trim())
        }).unwrap_or("[reasoning omitted]")
    } else { trimmed };
    let line = visible.replace(['\r', '\n'], " ");
    let mut preview: String = line.chars().take(80).collect();
    if line.chars().count() > 80 { preview.push('…'); }
    preview
}

/// `security.outboundGuard` (#154, a registry switch): only an explicit false disables outbound auditing; missing or
/// invalid values read as on.
pub fn outbound_guard_enabled(cfg: &serde_json::Value) -> bool {
    crate::settings::OUTBOUND_GUARD.switch(cfg)
}

pub fn configured_outbound_violation(
    cfg: &serde_json::Value,
    text: &str,
    secrets: &[String],
) -> Option<String> {
    // Audit the same canonical text that the shared send path persists and puts on the wire.
    // Removing face summaries can join otherwise harmless fragments into a path or known secret.
    outbound_guard_enabled(cfg)
        .then(|| {
            outbound_violation(text, secrets)
                .or_else(|| outbound_violation(&crate::outbound_message::normalize(text), secrets))
        })
        .flatten()
}

pub fn outbound_violation(text: &str, secrets: &[String]) -> Option<String> {
    // 与 JS `sensitive.js` 的 SENSITIVE_RE 等价。**这是红线的一部分**（出站哨兵零漏放）。
    // 原实现只查 `X:\` 一种形态，UNC 路径、Unix 路径、`token=xxx` 这类凭据赋值全漏 ——
    // 而那些恰恰是最该拦的东西。
    //
    // JS 用了 lookbehind `(?<![A-Za-z0-9])`，Rust 的 regex crate 不支持，
    // 所以拆成四条：带左边界的两条自己校验前一个字符，另外两条直接匹配。
    let (win, unc, unix, cred) = sensitive_patterns();
    // 「前面不是字母数字」——JS lookbehind 的语义
    let boundary_ok = |s: &str, start: usize| -> bool {
        if start == 0 {
            return true;
        }
        !s[..start]
            .chars()
            .next_back()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
    };
    // A candidate whose left neighbour fails the boundary must not hide a later start inside it
    // (`file:///C:/x`: the match from the `e` is refused, the one from `C` is not), so every start is tried.
    let bounded = |re: &regex::Regex| -> bool {
        let mut pos = 0usize;
        while pos <= text.len() {
            let Some(m) = re.find_at(text, pos) else { return false };
            if boundary_ok(text, m.start()) {
                return true;
            }
            pos = m.start() + text[m.start()..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        false
    };
    if bounded(win) {
        return Some("消息含本机绝对路径".to_string());
    }
    if unc.is_match(text) {
        return Some("消息含 UNC 路径".to_string());
    }
    if bounded(unix) {
        return Some("消息含本机绝对路径".to_string());
    }
    if cred.is_match(text) {
        return Some("消息含凭据".to_string());
    }
    for s in secrets {
        if !s.is_empty() && text.contains(s.as_str()) {
            return Some("消息含凭据".to_string());
        }
    }
    if text.contains("dsh-auth-") {
        return Some("消息含会话 Cookie".to_string());
    }
    None
}

fn now_ms() -> i64 {
    crate::parity_clock::now_ms() as i64
}

/// A model turn error (`turn/end` error) of a conversation, noted by the pipeline when it reports the error (activity line
/// and administrator message) so the caller that runs the turn on a task knows the error was already reported and that
/// the turn ended in a model error rather than a bridge failure. Taken once by `take_model_error`.
fn model_errors() -> &'static Mutex<HashMap<(String, String), String>> {
    static M: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn error_scope(context: &RuntimeContext) -> String {
    context.runtime_root().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "legacy".to_string())
}

fn note_model_error(context: &RuntimeContext, key: &str, message: &str) {
    model_errors().lock().unwrap_or_else(|p| p.into_inner()).insert((error_scope(context), key.to_string()), message.to_string());
}

/// The model's error message of the conversation's last turn when that turn ended in a model error (and only then).
pub fn take_model_error(context: &RuntimeContext, key: &str) -> Option<String> {
    model_errors().lock().unwrap_or_else(|p| p.into_inner()).remove(&(error_scope(context), key.to_string()))
}

/// The failover state of a runtime (JS `modelCandidateIndex`, `modelAppliedSessions`, `failoverRetries`): every turn
/// builds its own `Pipeline`, so the state that must outlive a turn lives here, one per selected root.
/// A DSH session left the conversation map (reset, mode switch, revocation, replacement): the failover memory forgets it, as
/// JS deletes `modelAppliedSessions` and `sessionModels` for it.
pub fn forget_failover_session(context: &RuntimeContext, session: &str) {
    failover_state_for(context).lock().unwrap_or_else(|p| p.into_inner()).forget_session(session);
}

fn failover_state_for(context: &RuntimeContext) -> Arc<Mutex<FailoverState>> {
    static STATES: OnceLock<Mutex<HashMap<String, Arc<Mutex<FailoverState>>>>> = OnceLock::new();
    let scope = context
        .runtime_root()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "legacy".to_string());
    STATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(scope)
        .or_default()
        .clone()
}

#[derive(Debug)]
pub(crate) enum ChatTurnOutcome {
    Completed,
    WaitForDsh,
    ParkSwitchUntil(i64),
}

enum TurnAdmissionError {
    Refused(String),
    ParkSwitchUntil(i64),
}

impl From<String> for TurnAdmissionError {
    fn from(error: String) -> Self { Self::Refused(error) }
}

impl From<TurnAdmissionError> for String {
    fn from(error: TurnAdmissionError) -> Self {
        match error {
            TurnAdmissionError::Refused(reason) => reason,
            TurnAdmissionError::ParkSwitchUntil(_) => "engine switch admission deferred".into(),
        }
    }
}

enum LedgerAdmissionError {
    Failed(String),
    Held { message: String, record: serde_json::Value },
}

impl From<String> for LedgerAdmissionError {
    fn from(error: String) -> Self { Self::Failed(error) }
}

impl From<LedgerAdmissionError> for String {
    fn from(error: LedgerAdmissionError) -> Self {
        match error {
            LedgerAdmissionError::Failed(message) | LedgerAdmissionError::Held { message, .. } => message,
        }
    }
}

pub(crate) type MintedEpoch = Arc<Mutex<Option<String>>>;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Reserved2Route {
    #[default]
    Dsh,
    Cli,
    PendingHarness,
    Harness,
    ExpiredBeforeAdmission,
}

pub(crate) type SharedReserved2Route = Arc<Mutex<Reserved2Route>>;

const RESERVED2_CAP_BEFORE_ADMISSION: &str = "reserved2 wake reached turnTimeout before admission";

/// Revoke an admitted simulation epoch and publish the exact replacement to the hub.
pub(crate) fn revoke_reserved2_epoch(
    context: &RuntimeContext, key: &str, token: &str, publication: Option<&MintedEpoch>,
) -> Result<(), String> {
    // Same order as the hub's epoch observation and DSH's own epoch publication.
    let mut minted = publication.map(|slot| slot.lock().unwrap_or_else(|p| p.into_inner()));
    let replacement = crate::social_state::revoke_if_current(context, key, token)?;
    if let (Some(slot), Some(token)) = (minted.as_mut(), replacement) {
        **slot = Some(token);
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct Reserved2Budget {
    pub(crate) deadline: i64,
    pub(crate) cap_ms: i64,
    pub(crate) ceiling: i64,
    pub(crate) began: i64,
}

impl Reserved2Budget {
    pub(crate) fn deadline_for(&self, conv: &serde_json::Value) -> i64 {
        crate::social_state::reserved2_deadline(
            self.deadline,
            self.cap_ms,
            self.ceiling,
            self.began,
            conv,
        )
    }

    pub(crate) fn expired_at(&self, now: i64, conv: &serde_json::Value) -> bool {
        now >= self.deadline_for(conv)
    }

    pub(crate) fn expired(&self, conv: &serde_json::Value) -> bool {
        self.expired_at(crate::turn_ledger::now_ms(), conv)
    }
}

pub struct Pipeline {
    /// Per-runtime failover memory shared by every pipeline of the process (see `failover_state_for`).
    failover: Arc<Mutex<FailoverState>>,
    pub rpc: Rpc,
    pub ob: OneBot,
    pub tune: SendTuning,
    pub limiter: RateLimiter,
    pub store: SessionStore,
    pub live: bool,
    pub preset_group: String,
    pub preset_owner: String,
    pub owner_qq: String,
    pub workspace_dir: String,
    pub secrets: Vec<String>,
    context: RuntimeContext,
    /// Turn lifecycle (ticket 10, selected runtime only): the ledger writer of the turn in flight,
    /// the agent token it runs under, and the inbound events it serves.
    cur_turn: Option<crate::turn_ledger::Recorder>,
    /// The ledger length captured before `cur_turn`'s `begun` record: its end folds only the records since.
    turn_ledger_offset: Option<u64>,
    turn_final: bool,
    turn_began: i64,
    turn_token: String,
    pub(crate) owner_interactions: Option<std::sync::Arc<crate::harness::interactions::Mailbox>>,
    turn_epoch: String,
    turn_epochs: Vec<String>,
    next_inbound: Vec<String>,
    next_reason: String,
    current_reason: String,
    dispatch_token: Option<String>,
    /// Where the scheduler learns which DSH session this turn uses (to cancel exactly that one).
    session_slot: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// Where the scheduler learns the epoch this turn minted when it abandoned an attempt.
    minted_slot: Option<MintedEpoch>,
    reserved2_route: Option<SharedReserved2Route>,
    reserved2_budget: Option<Reserved2Budget>,
    reserved2_admitted: Option<MintedEpoch>,
    #[cfg(test)]
    pub(crate) rotation_locked_hook: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    after_abandon_rotation: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    completion_observed_hook: Option<Box<dyn FnMut(&str) + Send>>,
    #[cfg(test)]
    pub(crate) harness_cap_poll_hook: Option<tokio::sync::oneshot::Sender<tokio::sync::oneshot::Sender<()>>>,
    /// The scripted adapter the next admitted turn runs on, instead of its selected engine.
    #[cfg(test)]
    scripted: Option<crate::turn_engine::Script>,
    /// A queued chat job's epoch check, run again right before the turn begins (after media).
    before_turn: Option<Box<dyn FnOnce() -> Option<String> + Send>>,
    dispatch_wake: Option<(i64, serde_json::Value)>,
    claimed_wake: Option<(i64, serde_json::Value)>,
    dispatch_deadline: Option<i64>,
    #[cfg(test)]
    pub(crate) admission_clock: Option<Box<dyn Fn() -> i64 + Send + Sync>>,
    #[cfg(test)]
    pub(crate) cap_clock: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,
    dispatch_unsubmitted: bool,
    // Set by the hub for its own wakes. Compatibility turns have no hub retry.
    reserved2_retries_remaining: Option<bool>,
    // Once the final unsubmitted failure is eligible for a line, the worker and hub
    // must use the same DSH parking decision even if health changes before collection.
    reserved2_failure_parks: Option<bool>,
    // Set by the reserved hub (`work.attempt < 3`) and synced for DSH parking.
    reserved_retries_remaining: Option<bool>,
    reserved_failure_parks: Option<bool>,
    chat_failure_defers: Option<bool>,
    /// Chat mode (ticket 12): the run state of the chat turn in flight.
    chat: Option<ChatRun>,
    /// Facts of the last finished turn, for the chat reply decision.
    last_report: Option<TurnReport>,
    failure_reported: bool,
    owner_interaction_closed: bool,
    active_candidate: Option<failover::Candidate>,
    carried_messages: Vec<serde_json::Value>,
    /// Memories the compatibility render of the turn in flight carries as recall: counted when that text
    /// itself is delivered (DSH without a per-attempt render, or a CLI fallback), never when merely rendered.
    compat_recall: Vec<i64>,
    refusals_recorded: std::collections::HashSet<(String, String, String)>,
    /// Agent preset of chat sessions (`agentPreset`, never the reserved2 one).
    preset_chat: String,
    preset_closed: String,
    /// Title of a session created by this pipeline, sent with `session/rename` once the first
    /// prompt is in DSH's hands (JS `renameSessionForUi` runs after the session exists, unawaited).
    pending_title: Option<(String, String)>,
    /// A session created by this pipeline whose title is not resolved yet: (session id, key).
    title_for: Option<(String, String)>,
}

/// State of a chat turn while it runs.
struct ChatRun {
    prompt: Option<crate::chat::Prompt>,
    /// The turn-engine adapter this turn runs on (captured before the turn begins).
    adapter: crate::turn_engine::Adapter,
    reserved: Option<crate::reserved::Authority>,
    mode: String,
    silent: bool,
    media_parts: Vec<serde_json::Value>,
    /// Acknowledgement to send once DSH accepted the prompt (sent at most once per message).
    ack: Option<String>,
    /// The owner sent the message that started this turn: only then may a DSH slash-command
    /// result be relayed to the conversation.
    owner_started: bool,
}

/// What the chat reply decision needs to know about a finished turn.
#[derive(Debug, Clone, Default)]
struct TurnReport {
    /// `completed`, `aborted`, ... (the `turn/end` reason kind), or `command`.
    end_kind: String,
    /// A send tool of this turn finished successfully: the model already spoke.
    send_tool_ok: bool,
    /// Result text of a DSH slash command that ran instead of a model turn.
    command: Option<String>,
}

/// Chat/first-generation collector budget (`socialV2.turnTimeoutSecs`, default 180).
fn turn_timeout_secs(cfg: &serde_json::Value) -> u64 {
    cfg.pointer("/socialV2/turnTimeoutSecs")
        .and_then(|v| v.as_u64())
        .map(|v| v.clamp(3, 900))
        .unwrap_or(180)
}

/// Covers an engaged turn up to its absolute ceiling; the hub's cap stays the enforcer.
fn reserved2_turn_timeout(cfg: &serde_json::Value) -> std::time::Duration {
    std::time::Duration::from_millis(crate::wake::reserved2_collector_ms(cfg))
}

/// What a cancellation did: the epoch it minted (when it rotated the token) and a failure to stop
/// DSH's work. The rotation stands even when the stop failed: the old epoch is revoked either way.
#[derive(Debug, Default)]
pub struct Cancelled {
    pub token: Option<String>,
    pub stop_error: Option<String>,
}

/// Prefix of the error `run_turn` returns when it refuses to start because the conversation has a
/// paused or unreconciled turn. The wake is recorded (`held`) and handed back once nothing blocks.
pub const HELD_ERROR_PREFIX: &str = "暂缓唤醒：";

/// An error that only says the turn was cancelled or held because its conversation changed (reset, revoked, mode switch,
/// a paused turn): not a dependency failure, so it raises no administrator alert.
pub fn is_cancellation_error(error: &str) -> bool {
    // The bridge's own wording only (`chat::stale_job` reasons, the turn validity checks): a model or DSH error that
    // happens to contain one of these words is a failure and still raises its alert.
    const OWN_PREFIXES: [&str; 8] = [
        "回合已取消",
        HELD_ERROR_PREFIX,
        "模式已变化",
        "会话或白名单已变化",
        "会话映射已变化",
        "会话令牌已变化",
        "创建对话过程中发生重置或权限变动",
        "候场等待期间对话发生重置或权限变动",
    ];
    OWN_PREFIXES.iter().any(|p| error.starts_with(p))
        || error.ends_with("丢弃排队的指令")
        || error.contains("token invalidated")
        || error.contains("mode or permission changed")
}

/// The send tools of the QQ MCP surface (JS `SEND_TOOL_RE`), with or without the server prefix.
pub fn is_send_tool(name: &str) -> bool {
    let bare = name.rsplit("__").next().unwrap_or(name);
    matches!(
        bare,
        "qq_send_group_message" | "qq_send_private_message" | "qq_reply" | "qq_send_burst" | "qq_send_message"
    )
}

/// Put `token` into the `【会话令牌】<token>（…` line of a wake prompt. The prompt is built from the
/// social state before the turn starts; the turn may have minted or rotated the token since.
pub fn patch_token_line(prompt: &str, token: &str) -> String {
    const MARK: &str = "【会话令牌】";
    if let Some(start) = prompt.find(MARK) {
        let after = start + MARK.len();
        if let Some(end) = prompt[after..].find('（') {
            return format!("{}{}{}", &prompt[..after], token, &prompt[after + end..]);
        }
    }
    prompt.to_string()
}

impl Pipeline {
    pub fn new_with_runtime(
        context: &RuntimeContext,
        cfg: &serde_json::Value,
        live: bool,
    ) -> Result<Self, String> {
        Self::build(context, cfg, live, true)
    }

    /// A pipeline for turns whose engine needs no DSH (`turn_engine::Engine::uses_dsh` is false;
    /// selected runtime only). It reads no DSH credential and needs no DSH dependency; every DSH
    /// call on it fails locally (`dsh_credential::UNBOUND`) without contacting DSH.
    pub fn new_without_dsh(
        context: &RuntimeContext,
        cfg: &serde_json::Value,
        live: bool,
    ) -> Result<Self, String> {
        Self::build(context, cfg, live, false)
    }

    fn build(
        context: &RuntimeContext,
        cfg: &serde_json::Value,
        live: bool,
        dsh: bool,
    ) -> Result<Self, String> {
        // Validate selected-root session state before constructing any live dependencies or
        // creating the candidate workspace. A corrupt file must not silently start a new session
        // and later overwrite the evidence needed to repair it.
        let store = SessionStore::load_for_runtime(context)?;
        let rpc = if !dsh {
            if !context.is_isolated() {
                return Err("a pipeline without DSH requires a selected runtime".to_string());
            }
            Rpc::unbound(context, context.dependency_url("dsh").unwrap_or_default())
        } else {
            let base = if context.is_isolated() {
                context
                    .dependency_url("dsh")
                    .ok_or_else(|| "runtime DSH dependency is unavailable".to_string())?
                    .to_string()
            } else {
                cfg
                .get("dsh")
                .and_then(|d| d.get("baseUrl"))
                .and_then(|b| b.as_str())
                .unwrap_or("http://127.0.0.1:3080")
                .to_string()
            };
            Rpc::connect_with_runtime(context, &base)?
        };

        let http_url = if context.is_isolated() {
            context
                .dependency_url("onebot")
                .ok_or_else(|| "runtime OneBot dependency is unavailable".to_string())?
                .to_string()
        } else { cfg
            .get("snowluma")
            .and_then(|s| s.get("httpUrl"))
            .and_then(|u| u.as_str())
            .unwrap_or("http://127.0.0.1:3000")
            .to_string() };
        let token = cfg
            .get("snowluma")
            .and_then(|s| s.get("accessToken"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        let mut secrets = vec![token.clone()];
        let console_token = context.effective_console_token(cfg);
        if !console_token.is_empty() {
            secrets.push(console_token);
        }

        let preset_group = cfg
            .get("socialV2")
            .and_then(|s| s.get("agentPreset"))
            .and_then(|p| p.as_str())
            .or_else(|| cfg.get("agentPreset").and_then(|p| p.as_str()))
            .unwrap_or("qq-chat-v2")
            .to_string();
        let preset_owner = cfg
            .get("socialV2")
            .and_then(|s| s.get("ownerDevPreset"))
            .and_then(|p| p.as_str())
            .unwrap_or(&preset_group)
            .to_string();
        let owner_qq = crate::chat::owner_qq(cfg);
        let workspace_dir = if context.is_isolated() {
            context
                .resolve_path(RuntimeArea::Data, "agents", PathIntent::CreateDirectory)
                .map_err(str::to_string)?
                .to_string_lossy()
                .to_string()
        } else {
            cfg.get("sessionCwd")
                .and_then(|c| c.as_str())
                .unwrap_or(crate::legacy_path!("state/agents"))
                .to_string()
        };

        let ob = if context.is_isolated() {
            OneBot::try_new_url(&http_url, token.clone())?
        } else {
            OneBot::new(&http_url, token.clone())
        };

        Ok(Self {
            failover: failover_state_for(context),
            rpc,
            ob,
            tune: SendTuning::from_config(cfg),
            limiter: RateLimiter::new(
                SendTuning::max_per_minute(cfg),
                SendTuning::max_per_hour(cfg),
            ),
            store,
            live,
            preset_group,
            preset_owner,
            owner_qq,
            workspace_dir,
            secrets,
            context: context.clone(),
            cur_turn: None,
            turn_ledger_offset: None,
            turn_final: true,
            turn_began: 0,
            turn_token: String::new(),
            owner_interactions: None,
            turn_epoch: String::new(),
            turn_epochs: Vec::new(),
            next_inbound: Vec::new(),
            next_reason: String::new(),
            current_reason: String::new(),
            dispatch_token: None,
            session_slot: None,
            minted_slot: None,
            reserved2_route: None,
            reserved2_budget: None,
            reserved2_admitted: None,
            #[cfg(test)]
            rotation_locked_hook: None,
            #[cfg(test)]
            after_abandon_rotation: None,
            #[cfg(test)]
            completion_observed_hook: None,
            #[cfg(test)]
            harness_cap_poll_hook: None,
            #[cfg(test)]
            scripted: None,
            before_turn: None,
            dispatch_wake: None,
            claimed_wake: None,
            dispatch_deadline: None,
            #[cfg(test)]
            admission_clock: None,
            #[cfg(test)]
            cap_clock: None,
            dispatch_unsubmitted: true,
            reserved2_retries_remaining: None,
            reserved2_failure_parks: None,
            reserved_retries_remaining: None,
            reserved_failure_parks: None,
            chat_failure_defers: None,
            chat: None,
            last_report: None,
            failure_reported: false,
            owner_interaction_closed: false,
            active_candidate: None,
            carried_messages: Vec::new(),
            compat_recall: Vec::new(),
            refusals_recorded: Default::default(),
            preset_chat: crate::chat::chat_preset(cfg),
            preset_closed: String::new(),
            pending_title: None,
            title_for: None,
        })
    }

    // ── 回合生命周期（ticket 10）──────────────────────────────────────────────────

    /// 主循环在 `run_turn` 之前告知：这一回合由哪些入站事件触发、原因是什么，
    /// 账本据此把输入、DSH 回合、工具和发送结果关联起来。
    pub fn note_turn(&mut self, reason: &str, inbound: &[String]) {
        self.next_reason = reason.to_string();
        self.next_inbound = inbound.to_vec();
    }


    pub fn expect_dispatch_token(&mut self, token: String) { self.dispatch_token = Some(token); }
    /// The scheduler's view of this turn's session (filled when the session is known).
    pub fn share_session(&mut self, slot: std::sync::Arc<std::sync::Mutex<Option<String>>>) { self.session_slot = Some(slot); }
    /// The scheduler's view of the token this turn mints when it abandons an attempt.
    pub fn share_minted_epoch(&mut self, slot: MintedEpoch) { self.minted_slot = Some(slot); }
    pub(crate) fn share_reserved2_route(&mut self, slot: SharedReserved2Route, admitted: MintedEpoch) {
        self.reserved2_route = Some(slot);
        self.reserved2_admitted = Some(admitted);
    }
    /// Run `check` right before the chat turn begins; `Some(reason)` drops the turn.
    pub fn recheck_before_turn(&mut self, check: Box<dyn FnOnce() -> Option<String> + Send>) { self.before_turn = Some(check); }

    pub fn expect_dispatch_wake(&mut self, at: i64, previous: serde_json::Value) { self.dispatch_wake = Some((at, previous)); }
    pub fn expect_dispatch_deadline(&mut self, at: i64) { self.dispatch_deadline = Some(at); }
    pub fn dispatch_definitely_unsubmitted(&self) -> bool { self.dispatch_unsubmitted }

    pub(crate) fn set_reserved2_retry_budget(&mut self, retries_remaining: bool) {
        self.reserved2_retries_remaining = Some(retries_remaining);
    }

    pub(crate) fn reserved2_failure_parks(&self) -> Option<bool> {
        self.reserved2_failure_parks
    }

    pub(crate) fn set_reserved_retry_budget(&mut self, retries_remaining: bool) {
        self.reserved_retries_remaining = Some(retries_remaining);
    }

    pub(crate) fn reserved_failure_parks(&self) -> Option<bool> {
        self.reserved_failure_parks
    }

    fn set_turn_token(&mut self, token: String) {
        self.turn_epoch = crate::send_outbox::token_epoch(&token);
        self.turn_epochs.push(self.turn_epoch.clone());
        self.turn_token = token;
        if let Some(turn) = &self.cur_turn {
            let _ = turn.epoch(&self.turn_epoch);
        }
    }

    fn finish_turn(&mut self, outcome: &str, reason: &str) {
        self.finish_attempt(outcome, reason, None, true);
    }

    fn finish_turn_with_retry(&mut self, outcome: &str, reason: &str, retry: bool) {
        self.finish_attempt(outcome, reason, None, retry);
    }

    fn finish_attempt(&mut self, outcome: &str, reason: &str, rpc_id: Option<&str>, retry: bool) {
        if self.turn_final {
            return;
        }
        self.turn_final = true;
        if let Some(turn) = &self.cur_turn {
            let budget_sleep = outcome == "completed"
                && (crate::social_state::turn_budget_ended(&self.context, &turn.key, &turn.id)
                    || (self.reserved2_budget.is_some()
                        && crate::social_state::turn_wait_limit_reached(
                            &self.context,
                            &turn.key,
                            &turn.id,
                            &self.turn_token,
                        )));
            let reason = if budget_sleep { "observationSleep" } else { reason };
            let writer = if retry { turn.clone() } else { turn.clone().without_retry() };
            let result = match rpc_id {
                Some(rpc_id) => writer.end_attempt(rpc_id, outcome, reason),
                None => writer.end(outcome, reason),
            };
            if let Err(error) = result {
                // Keep recovery's DSH/outbox evidence checks; never retry the turn or its sends here.
                eprintln!("[turn] 回合账本写入失败（{outcome}）：{error}；由恢复流程核对");
            }
        }
    }

    /// Publish every rotation owned by this turn, including unknown outcomes and
    /// replacement sessions. The mutex must cover token persistence and publication:
    /// otherwise tick could see an unowned persisted token and invalidate its queue.
    /// The separate turn-ledger epoch write needs no publication lock.
    fn rotate_turn_token(&mut self, key: &str) -> Result<(), String> {
        let shared = self.minted_slot.clone();
        let mut minted = shared.as_ref().map(|slot| slot.lock().unwrap_or_else(|p|p.into_inner()));
        #[cfg(test)]
        if let Some(hook) = self.rotation_locked_hook.take() { hook(); }
        let token = crate::social_state::ensure_or_rotate(&self.context, key, true)?;
        #[cfg(test)]
        if let Some(hook) = self.after_abandon_rotation.take() { hook(); }
        if let Some(slot) = minted.as_mut() { **slot = Some(token.clone()); }
        drop(minted);
        self.set_turn_token(token);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_rotate_turn_token(&mut self, key: &str) -> Result<(), String> {
        self.rotate_turn_token(key)
    }

    #[cfg(test)]
    pub(crate) fn pause_unknown_for_test(&mut self, key: &str) -> Result<String, String> {
        self.pause_unknown(key, "turn-unverified", "fixture failed abandonment stop")
    }

    /// 状态不明：撤换令牌（它此后迟到的工具调用与发送都会被拒）、记账本、报错。
    /// 不清空上下文、不重建会话、不重投。
    fn pause_unknown(&mut self, key: &str, reason: &str, detail: &str) -> Result<String, String> {
        match self.rotate_turn_token(key) {
            Ok(()) => {},
            Err(error) => eprintln!("[turn] 暂停回合时换令牌失败：{error}"),
        }
        self.turn_final = true;
        if let Some(turn) = &self.cur_turn {
            let _ = turn.unknown(reason, detail);
        }
        Err(format!("回合状态不明，已暂停并交管理员（{reason}）：{detail}"))
    }

    /// 这一轮是否仍然有效：会话映射仍指向该会话，令牌仍是这一轮在用的那个。
    fn turn_validity(&self, key: &str, sid: &str) -> Result<(), String> {
        if self.cur_turn.is_none() {
            return Ok(());
        }
        if let Some(authority) = self.chat.as_ref().and_then(|c|c.reserved.as_ref()) {
            authority.check_generation(&self.context,key)?;
        }
        if let Some(chat) = self.chat.as_ref().filter(|c| c.reserved.is_none()) {
            let cfg = self.context.load_config().map_err(|e| e.to_string())?;
            if crate::social_state::mode(&self.context) != chat.mode
                || !crate::chat::allowed_in_mode(&cfg, &chat.mode, key)
            {
                return Err("mode or permission changed".to_string());
            }
        }
        if self.chat.is_none() {
            let cfg = self.context.load_config().map_err(|e| e.to_string())?;
            if crate::social_state::mode(&self.context) != "reserved2"
                || !crate::social_state::inbound_admitted(&cfg, key)
            {
                return Err("mode or permission changed".to_string());
            }
        }
        // Only DSH turns are bound to a DSH session mapping.
        if self.chat.as_ref().is_none_or(|c| c.adapter.uses_dsh()) {
            match crate::session_map::lookup(&self.context, key) {
                Ok(Some(current)) if current == sid => {},
                Ok(_) => return Err("会话映射已变化（重置或替换）".to_string()),
                Err(error) => return Err(error),
            }
        }
        match crate::social_state::token(&self.context, key) {
            Ok(Some(token)) if crate::send_outbox::token_epoch(&token) == self.turn_epoch => Ok(()),
            Ok(_) => Err("会话令牌已变化（重置、取消或被替代）".to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    /// 发送账本里是否已有属于这一轮的批次（同会话、同令牌代际、回合开始之后）。
    fn turn_has_send_batch(&self, key: &str) -> bool {
        if self.cur_turn.is_none() {
            return false;
        }
        let Ok(records) = crate::send_outbox::read_records(&self.context) else { return true };
        records.iter().any(|r| {
            r.get("k").and_then(|v| v.as_str()) == Some("intent")
                // The chat acknowledgement is bridge text, not the model speaking.
                && r.get("purpose").and_then(|v| v.as_str()) != Some("ack")
                && r.get("key").and_then(|v| v.as_str()) == Some(key)
                && r.get("epoch")
                    .and_then(|v| v.as_str())
                    .is_some_and(|e| self.turn_epochs.iter().any(|x| x == e))
                && r.get("t").and_then(|v| v.as_i64()).unwrap_or(0) >= self.turn_began
        })
    }

/// 读某个会话的 `lastAiReplyAt`（毫秒）。读不到返回 0。
///
/// 用途：判断"这一轮里模型有没有自己调发送工具"。发送路径（MCP → console →
/// OneBot）成功后会更新这个字段，所以它比任何内存状态都可靠。
fn last_ai_reply_at(context: &RuntimeContext, key: &str) -> i64 {
    crate::conversation_state::ConversationState::for_runtime(context)
        .read_optional_document()
        .ok()
        .and_then(|doc| doc.conversation(key))
        .and_then(|conv| conv.field("lastAiReplyAt").and_then(|t| t.as_i64()))
        .unwrap_or(0)
}

/// 把一轮的分段耗时写进 logs/turn-timing.jsonl。
///
/// 只有写文件这一件事，失败就算了 —— 诊断数据不能影响主链路。
fn write_turn_timing(context: &RuntimeContext, rec: serde_json::Value) {
    let _ = Self::append_runtime_log(context, "logs/turn-timing.jsonl", &rec.to_string());
}

fn append_runtime_log(context: &RuntimeContext, relative: &str, line: &str) -> Result<(), String> {
    let safe = crate::activity::mask_log_text(line, &crate::activity::known_session_tokens(context));
    let mut bytes = safe.into_bytes();
    bytes.push(b'\n');
    if context.is_isolated() {
        context.append_bytes(RuntimeArea::Logs, relative, &bytes)
    } else {
        use std::io::Write as _;
        let name = relative.strip_prefix("logs/").unwrap_or(relative);
        let path = PathBuf::from(format!("{}/logs/{name}", crate::legacy_path!()));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())
    }
}

    fn report_backend_failure(&mut self, cfg: &serde_json::Value, key: &str, backend: &str, provider: &str, model: &str, code: &str, message: &str) {
        let failure = crate::model_failure::Failure::classify(code, message);
        note_model_error(&self.context, key, &failure.summary());
        self.alert_backend_failure(cfg, key, backend, provider, model, &failure, &[], None);
    }

    /// The owner's alert and refusal record of one failed backend request, without a model error note: a request that
    /// only failed over is no turn failure (`report_backend_failure` adds the note for the failure that ended the turn).
    #[allow(clippy::too_many_arguments)]
    fn alert_backend_failure(
        &mut self,
        cfg: &serde_json::Value,
        key: &str,
        backend: &str,
        provider: &str,
        model: &str,
        failure: &crate::model_failure::Failure,
        tool_refs: &[serde_json::Value],
        call: Option<crate::model_failure::RefusalCall<'_>>,
    ) {
        self.failure_reported = true;
        if failure.class == "refusal" && self.refusals_recorded.insert((backend.into(), provider.into(), model.into())) {
            let mut messages = self.carried_messages.clone();
            crate::model_failure::merge_refs(&mut messages, tool_refs.to_vec());
            let input = crate::model_failure::RefusalInput { messages: &messages, call };
            crate::model_failure::record_refusal(&self.context, key, backend, provider, model, failure, input);
        }
        crate::model_failure::notify(&self.context, cfg, key, backend, provider, model, failure);
    }

    /// JS `auditAndSend`: a reply stopped by the outbound sentinel is announced to the administrator (unless
    /// `security.interceptNotify` is `false`); the group sees nothing.
    fn alert_blocked_reply(&self, cfg: &serde_json::Value, key: &str) {
        let guard_cfg = self.context.load_config().unwrap_or_else(|_| cfg.clone());
        if !outbound_guard_enabled(&guard_cfg) || !crate::settings::INTERCEPT_NOTIFY.switch(cfg) {
            return;
        }
        crate::admin_alert::notify_admin(
            &self.context,
            cfg,
            "blocked-reply",
            &format!("⚠️ 外发内容触发安全校验（疑似含路径、凭据或会话令牌），已拦截未发。（{key}）"),
        );
        recovery_log::record(&self.context, Trigger::Turn, Component::Alert, "reply-blocked", Outcome::Skipped, &format!("{key}: a reply was stopped by the outbound sentinel"));
    }

    /// 只跑一轮，**不代发** —— 模型自己调 `qq_send_message` 发。
    ///
    /// 选定运行目录下，这一轮会写回合账本（`turn_ledger`）：开始、提交、结束都有记录，
    /// 并且处于「暂停/恢复中」的会话不再开新回合（见 `turn_recovery`）。
    pub async fn run_turn(
        &mut self,
        target: &Target,
        dsh_prompt: &str,
        cli_prompt: &str,
        cfg: &serde_json::Value,
    ) -> Result<usize, String> {
        // The recall the compatibility render carries; counted only if that text is delivered during this turn.
        self.compat_recall = crate::model_failure::take_prompt_refs(&self.context, &target.key(), "recall", dsh_prompt)
            .unwrap_or_default().iter().filter_map(serde_json::Value::as_i64).collect();
        let result = self.run_turn_ledgered(target, dsh_prompt, cli_prompt, cfg).await;
        self.compat_recall.clear();
        result
    }

    async fn run_turn_ledgered(
        &mut self,
        target: &Target,
        dsh_prompt: &str,
        cli_prompt: &str,
        cfg: &serde_json::Value,
    ) -> Result<usize, String> {
        self.refusals_recorded.clear();
        let key = target.key();
        let dsh_refs = crate::model_failure::take_prompt_refs(&self.context, &key, "dsh", dsh_prompt);
        let cli_refs = crate::model_failure::take_prompt_refs(&self.context, &key, "cli", cli_prompt)
            .or_else(|| crate::model_failure::snapshot_refs(cli_prompt));
        let refs = WakePromptRefs { dsh: dsh_refs, cli: cli_refs };
        if !self.context.is_isolated() {
            crate::social_state::begin_legacy_wait_turn(&self.context, &key)?;
            let deadline = self.dispatch_deadline.take().unwrap_or_else(||crate::turn_ledger::now_ms()
                .saturating_add(crate::wake::turn_budget(cfg).hard_cap_ms));
            crate::social_state::set_legacy_wait_deadline(&self.context, &key, deadline)?;
            self.carried_messages.clear();
            return self.run_turn_inner(target, dsh_prompt, cli_prompt, cfg, cfg, crate::turn_engine::Adapter::Dsh, refs).await;
        }
        let admission = self.turn_adapter(&key, "reserved2").await;
        if let Some(route) = &self.reserved2_route {
            let mut route = route.lock().unwrap_or_else(|p| p.into_inner());
            if *route == Reserved2Route::ExpiredBeforeAdmission
                || ((admission.is_ok() || *route == Reserved2Route::PendingHarness) && self.dispatch_expired())
            {
                *route = Reserved2Route::ExpiredBeforeAdmission;
                return Err(RESERVED2_CAP_BEFORE_ADMISSION.into());
            }
            if let Ok((adapter, _, admitted_cfg)) = &admission {
                let cli_cfg = admitted_cfg.as_ref().unwrap_or(cfg);
                *route = if matches!(adapter, crate::turn_engine::Adapter::Harness) {
                    Reserved2Route::Harness
                } else if adapter.uses_dsh() && pinned_cli_backend(cli_cfg, &key).is_some() {
                    Reserved2Route::Cli
                } else {
                    Reserved2Route::Dsh
                };
            }
        } else if admission.is_ok() && self.dispatch_expired() {
            return Err(RESERVED2_CAP_BEFORE_ADMISSION.into());
        }
        let (adapter, _admission, admitted_cfg) = admission?;
        let cli_cfg = admitted_cfg.as_ref().unwrap_or(cfg);
        let harness = matches!(&adapter, crate::turn_engine::Adapter::Harness);
        let token = match self.begin_ledger_turn_as(&key, adapter.id(), true) {
            Err(error) if (harness || matches!(error, LedgerAdmissionError::Held { .. })) && self.dispatch_expired() => {
                if let LedgerAdmissionError::Held { message, mut record } = error {
                    record["k"] = serde_json::json!("ended");
                    record["t"] = serde_json::json!(crate::turn_ledger::now_ms());
                    record["held"] = serde_json::json!(true);
                    record["outcome"] = serde_json::json!("cancelled");
                    record["reason"] = serde_json::json!("turnTimeout");
                    // Recovery still owns the hold if its durable end could not be written.
                    crate::turn_ledger::append_record(&self.context, &record)
                        .map_err(|error| format!("{message}: {error}"))?;
                }
                self.publish_expired_route();
                return Err(RESERVED2_CAP_BEFORE_ADMISSION.into());
            }
            result => result?,
        };
        if let Some(admitted) = &self.reserved2_admitted {
            *admitted.lock().unwrap_or_else(|p| p.into_inner()) = Some(token.clone());
        }
        let dispatched_at = self.dispatch_wake.as_ref().map(|(at, _)|*at).unwrap_or(self.turn_began);
        let cap_ms = crate::wake::turn_budget(cfg).hard_cap_ms;
        let deadline = self.dispatch_deadline.take().unwrap_or_else(||dispatched_at.saturating_add(cap_ms));
        // Engaged turns measure the cap from her last send (capMs), bounded by the dispatch ceiling.
        let ceiling = deadline.saturating_sub(cap_ms).saturating_add(crate::wake::turn_ceiling_ms(cfg)).max(deadline);
        self.reserved2_budget = Some(Reserved2Budget { deadline, cap_ms, ceiling, began: self.turn_began });
        let prepared = (|| {
            crate::turn_ledger::append_record(&self.context,
                &serde_json::json!({"k":"budget","turn":self.cur_turn.as_ref().unwrap().id,"t":self.turn_began,"deadline":deadline,
                    "capMs":cap_ms,"ceiling":ceiling}))?;
            if let Some((at, previous)) = self.dispatch_wake.take() {
                self.claimed_wake = Some((at, previous.clone()));
                let claim = serde_json::json!({"at":at,"previous":previous});
                self.cur_turn.as_ref().unwrap().wake_claim(&claim)?;
                crate::social_state::consume_reserved2_deadline(
                    &self.context,
                    &key,
                    &token,
                    at,
                    &previous,
                )?;
            }
            Ok(())
        })();
        let result = match prepared {
            Ok(()) => {
                let dsh_prompt = patch_token_line(dsh_prompt, &token);
                self.run_turn_inner(target, &dsh_prompt, cli_prompt, cfg, cli_cfg, adapter, refs).await
            }
            Err(error) => Err(error),
        };
        // Check failures from every preparation step, including a lost snapshot response.
        let result = if harness && result.is_err() && self.dispatch_unsubmitted && !self.turn_final
            && self.harness_admission_expired(&key)
        {
            Err(self.expire_unadmitted_harness(&key))
        } else {
            result
        };
        self.end_ledger_turn(result.as_ref().map(|_| ()).map_err(String::as_str));
        self.reserved2_budget = None;
        result
    }

    /// Open the ledger entry of a turn (selected runtime). Refuses, recording the wake as `held`,
    /// while the conversation has a paused or unreconciled turn. Returns the agent token in force.
    #[cfg(test)]
    fn begin_ledger_turn(&mut self, key: &str) -> Result<String, String> {
        self.begin_ledger_turn_as(key, crate::turn_engine::Engine::Dsh.id(), false).map_err(String::from)
    }

    /// Begin a durable turn for the adapter that will run it.
    fn begin_ledger_turn_as(&mut self, key: &str, backend: &str, reserved2: bool) -> Result<String, LedgerAdmissionError> {
        self.claimed_wake = None;
        let inbound = std::mem::take(&mut self.next_inbound);
        self.refusals_recorded.clear();
        self.carried_messages = inbound.iter().filter_map(|key| key.rsplit_once("|message:")
            .map(|(_, id)| serde_json::json!({"messageId":id}))).collect();
        let reason = std::mem::take(&mut self.next_reason);
        self.current_reason = reason.clone();
        let records = crate::turn_ledger::read_records(&self.context)?;
        let (turns, _) = crate::turn_ledger::fold(&records);
        if let Some(blocking) = crate::turn_ledger::blocking_turn(&turns, key) {
            let record = serde_json::json!({
                "k":"held", "turn":crate::turn_ledger::mint_id("held-"), "t":crate::turn_ledger::now_ms(),
                "key":key, "backend":backend, "inbound":inbound, "reason":reason, "blockedBy":blocking.id,
            });
            crate::turn_ledger::append_record(&self.context, &record)?;
            let message = format!("{HELD_ERROR_PREFIX}会话 {key} 有暂停或恢复中的回合 {}，本次唤醒已记录（管理端 /api/turns）", blocking.id);
            return Err(LedgerAdmissionError::Held { message, record });
        }
        let token = crate::social_state::ensure_or_rotate(&self.context, key, false)
            .map_err(String::from)?;
        if self.dispatch_token.take().is_some_and(|expected| expected != token) {
            return Err("queued wake token invalidated".to_string().into());
        }
        let epoch = crate::send_outbox::token_epoch(&token);
        let ledger_offset = crate::turn_ledger::ledger_len(&self.context).ok();
        let recorder = if reserved2 {
            crate::turn_ledger::Recorder::begin_reserved2(&self.context, key, backend, &reason, &epoch, &inbound)?
        } else {
            crate::turn_ledger::Recorder::begin(&self.context, key, backend, &reason, &epoch, &inbound, None)?
        };
        self.cur_turn = Some(recorder);
        self.turn_ledger_offset = ledger_offset;
        self.turn_final = false;
        self.turn_began = crate::turn_ledger::now_ms();
        self.turn_epoch = epoch.clone();
        self.turn_epochs = vec![epoch];
        self.turn_token = token.clone();
        Ok(token)
    }

    /// Close the ledger entry opened by `begin_ledger_turn`.
    fn end_ledger_turn(&mut self, outcome: Result<(), &str>) {
        match outcome {
            Ok(()) => self.finish_turn("completed", "ok"),
            Err(error) if is_dsh_access_refusal(error) => self.finish_turn("cancelled", error),
            Err(error) => self.finish_turn("failed", error),
        }
        self.cur_turn = None;
        self.turn_ledger_offset = None;
        self.claimed_wake = None;
    }

    // ── chat mode (ticket 12) ─────────────────────────────────────────────────────

    /// The checks JS `captureSendGuard` makes right before each part is sent: still chat mode,
    /// target still allowed, session and token of this turn still the current ones.
    fn chat_guard(&self, key: &str, sid: &str) -> Result<(), String> {
        let cfg = self.context.load_config().map_err(|e| e.to_string())?;
        if let Some(authority) = self.chat.as_ref().and_then(|c| c.reserved.as_ref()) {
            authority.check_generation(&self.context, key)?;
            if crate::social_state::role_state(&self.context)?["mode"] == "silent"
                && !crate::chat::is_owner_key(&cfg, key)
            {
                return Err("静默模式已启用".into());
            }
        } else if self
            .chat
            .as_ref()
            .is_none_or(|chat| crate::social_state::mode(&self.context) != chat.mode)
        {
            return Err("模式已变化".to_string());
        }
        let mode = crate::social_state::mode(&self.context);
        if !crate::chat::allowed_in_mode(&cfg, &mode, key) {
            return Err("会话或白名单已变化".to_string());
        }
        self.turn_validity(key, sid)
    }

    /// Send text to the conversation of the chat turn in flight (recorded, guarded).
    /// reserved2 never forwards the model's final text; JS records it (or the interception) in the
    /// activity log, skipping an empty text and the silent marker.
    fn log_internal_output(&self, key: &str, text: &str) {
        // Internal output is never sent to QQ. Redact locally before markdown
        // conversion or preview truncation; masking does not create owner alerts.
        // Literal *** replacements can form paired Markdown emphasis. Protect
        // them through conversion, then restore the fixed mask before logging.
        const MASK_MARKER: &str = "\u{e000}\u{e001}";
        let known = crate::activity::known_session_tokens(&self.context);
        let protected = redact_sensitive_text_as(text, &known, MASK_MARKER);
        let rendered = crate::chat::md_to_plain(&protected);
        let rendered = if protected != text { rendered.replace(MASK_MARKER, "***") } else { rendered };
        let plain = redact_sensitive_text(&rendered, &known);
        if plain.is_empty() || is_silent_marker(&plain) {
            return;
        }
        crate::activity::append(
            &self.context,
            &format!(
                "{key} [reserved2] 对话端本回合直接输出的这段文本并未外发（仅作内部草稿归档备查）：{}{}",
                crate::activity::slice(&plain, 80),
                if plain.encode_utf16().count() > 80 { "…" } else { "" }
            ),
        );
    }

    /// JS `appendActivity` for a reply that is about to be sent: the social (first-generation) form
    /// names the number of parts, the plain form marks a long text with `…`.
    fn log_reply_activity(&self, key: &str, plain: &str) {
        let safe = crate::activity::mask_session_tokens(plain, &crate::activity::known_session_tokens(&self.context));
        let plain = safe.as_str();
        let social = self.chat.as_ref().and_then(|c| c.reserved.as_ref()).is_some_and(|a| a.social());
        let parts = if social {
            self.context.load_config().ok().map(|c| crate::reserved::reply_parts(plain, &crate::reserved::config(&c)))
        } else {
            None
        };
        let line = match parts.as_ref().filter(|p| !p.is_empty()) {
            Some(parts) => {
                let first = &parts[0];
                format!(
                    "{key} agent 回复：{}{}{}",
                    crate::activity::slice(first, 80),
                    if parts.len() > 1 { format!("（分{}条）", parts.len()) } else { String::new() },
                    if first.encode_utf16().count() > 80 { "…" } else { "" }
                )
            }
            None => format!(
                "{key} agent 回复：{}{}",
                crate::activity::slice(plain, 80),
                if plain.encode_utf16().count() > 80 { "…" } else { "" }
            ),
        };
        crate::activity::append(&self.context, &line);
    }

    /// Chat sends of a turn run off the runtime worker: the chain wait, part delays and gateway
    /// calls must not starve ingress (issue #82).
    fn send_chat_text(
        &self,
        key: &str,
        sid: &str,
        purpose: &str,
        text: &str,
    ) -> crate::chat::SendReport {
        crate::recorded_send::off_worker(|| self.send_chat_text_inline(key, sid, purpose, text))
    }

    fn send_chat_text_inline(
        &self,
        key: &str,
        sid: &str,
        purpose: &str,
        text: &str,
    ) -> crate::chat::SendReport {
        let reserved = self
            .chat
            .as_ref()
            .and_then(|c| c.reserved.as_ref())
            .is_some_and(|a| a.social());
        let report = if reserved && purpose == "reply" {
            let cfg = match self.context.load_config() {
                Ok(c) => crate::reserved::config(&c),
                Err(e) => {
                    return crate::chat::SendReport {
                        error: Some(e.into()),
                        ..Default::default()
                    }
                }
            };
            let parts = crate::reserved::reply_parts(text, &cfg);
            let mut total = crate::chat::SendReport::default();
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    match crate::reserved::reply_delay(&self.context, &cfg) {
                        Ok(delay) => std::thread::sleep(std::time::Duration::from_millis(delay)),
                        Err(e) => {
                            total.error = Some(e);
                            break;
                        }
                    }
                }
                let report = crate::chat::send_recorded(
                    &self.context,
                    &self.ob,
                    key,
                    &self.turn_epoch,
                    purpose,
                    part,
                    0,
                    &|| self.chat_guard(key, sid),
                );
                total.parts += report.parts;
                total.confirmed += report.confirmed;
                total.delivery_unknown |= report.delivery_unknown;
                total.unknown |= report.unknown;
                if let Some(e) = report.error {
                    total.error = Some(e);
                }
            }
            if total.error.is_none() && total.confirmed > 0 {
                let _ = crate::reserved::record_reply(
                    &self.context,
                    key,
                    crate::parity_clock::now_ms(),
                );
            }
            total
        } else {
            let cfg = match self.context.load_config() {
                Ok(cfg) => cfg,
                Err(error) => return crate::chat::SendReport { error: Some(error.into()), ..Default::default() },
            };
            crate::chat::send_recorded(
                &self.context,
                &self.ob,
                key,
                &self.turn_epoch,
                purpose,
                text,
                crate::chat::send_delay_ms(&cfg),
                &|| self.chat_guard(key, sid),
            )
        };
        if let Some(error) = &report.error {
            println!("[chat] {key} 发送未完成：{error}");
        }
        report
    }

    /// A terminal backend failure may send one owner-authored line while the turn still owns its
    /// conversation. It uses the mode's send guards and the shared recorded text send.
    fn send_persona_failure_line(&mut self, key: &str, dsh_session: Option<&str>) {
        let Some(turn) = self.cur_turn.as_ref() else { return };
        let mode = self.chat.as_ref().map_or("reserved2", |chat| chat.mode.as_str());
        if !self.live || self.turn_final || mode == "closed-agent" || self.current_reason == "wakeReminder"
            || self.chat.as_ref().is_some_and(|chat| chat.silent)
        {
            return;
        }
        let configured_line = || crate::persona::failure_line(&self.context, key);
        let Some(line) = configured_line() else { return };
        if self.turn_has_send_batch(key) {
            return;
        }
        if self.dispatch_unsubmitted {
            match mode {
                "reserved2" => {
                    if let Some(retries_remaining) = self.reserved2_retries_remaining {
                        if retries_remaining {
                            return;
                        }
                        let Ok(cfg) = self.context.load_config() else { return };
                        let parks = self.reserved2_wake_parks(&cfg, key);
                        self.reserved2_failure_parks = Some(parks);
                        if parks {
                            return;
                        }
                    }
                }
                "reserved" => {
                    if self.reserved_retries_remaining == Some(true) {
                        return;
                    }
                    let parks = self.unsubmitted_dsh_deferred();
                    self.reserved_failure_parks = Some(parks);
                    if parks {
                        return;
                    }
                }
                "chat" if dsh_session.is_some() => {
                    let defers = self.unsubmitted_dsh_deferred();
                    self.chat_failure_defers = Some(defers);
                    if defers {
                        return;
                    }
                }
                _ => {}
            }
        }
        let guard = || {
            let cfg = self.context.load_config().map_err(str::to_string)?;
            if mode == "reserved2" {
                if !crate::social_state::inbound_admitted(&cfg, key) {
                    return Err("conversation access changed".into());
                }
                if self.reserved2_budget.is_some() && self.check_harness_turn(key)? {
                    return Err("turn budget expired".into());
                }
                crate::console::send_guard(&self.context, &cfg, key, &self.turn_epoch)
                    .map_err(|(_, reason)| reason)?;
                if let Some(session) = dsh_session {
                    self.turn_validity(key, session)?;
                }
            } else {
                self.chat_guard(key, dsh_session.unwrap_or_default())?;
                if crate::social_state::silent_blocks_send(&self.context, &cfg, key)
                    .map_err(|error| error.message().to_string())?
                {
                    return Err("silent mode".into());
                }
            }
            let offset = self.turn_ledger_offset.ok_or("missing turn ledger offset")?;
            let current = crate::turn_ledger::fold_turn_since(&self.context, offset, &turn.id)?
                .ok_or("missing turn ledger entry")?;
            if !current.state.is_open() || matches!(current.finalization_claim(), Some(("cancel-requested", _))) {
                return Err("turn ended or cancelled".into());
            }
            if configured_line().as_deref() != Some(line.as_str()) {
                return Err("persona failure line changed".into());
            }
            let records = crate::send_outbox::read_records(&self.context)?;
            if crate::send_outbox::blocks_persona_failure(&records, key, &self.turn_epochs, self.turn_began) {
                return Err("conversation already replied or has an unresolved send".into());
            }
            let mut secrets = self.secrets.clone();
            secrets.push(self.turn_token.clone());
            if configured_outbound_violation(&cfg, &line, &secrets).is_some() {
                return Err("persona failure line blocked by outbound policy".into());
            }
            Ok(())
        };
        let report = crate::chat::send_recorded(
            &self.context, &self.ob, key, &self.turn_epoch, crate::send_outbox::PERSONA_FAILURE_SEND, &line, 0, &guard,
        );
        if report.unknown {
            let _ = self.pause_unknown(key, "send-unresolved", "persona failure line delivery is unverified");
        }
    }

    /// One chat turn: ledger entry, turn on the chat preset session, then the automatic reply.
    pub(crate) async fn run_chat_turn(
        &mut self,
        key: &str,
        prompt: crate::chat::Prompt,
        ack: Option<String>,
        cfg: &serde_json::Value,
        inbound: &[String],
        media: &[serde_json::Value],
        mode: &str,
        owner_started: bool,
    ) -> Result<ChatTurnOutcome, String> {
        self.owner_interaction_closed = false;
        self.chat_failure_defers = None;
        if !self.context.is_isolated() {
            return Err("chat 模式只在选定运行环境中运行".to_string());
        }
        self.preset_chat = crate::chat::chat_preset(cfg);
        self.owner_qq = crate::chat::owner_qq(cfg);
        self.note_turn("chat", inbound);
        let (adapter, _admission, _) = match self.turn_adapter(key, mode).await {
            Ok(admission) => admission,
            Err(TurnAdmissionError::ParkSwitchUntil(at)) => return Ok(ChatTurnOutcome::ParkSwitchUntil(at)),
            Err(TurnAdmissionError::Refused(reason)) => return Err(reason),
        };
        let uses_dsh = adapter.uses_dsh();
        if uses_dsh && !self.prepare_dsh_chat(cfg, mode)? {
            return Ok(ChatTurnOutcome::WaitForDsh);
        }
        let context = self.context.clone();
        let media_cfg = cfg.clone();
        let media = media.to_vec();
        let media_parts = tokio::task::spawn_blocking(move || {
            crate::media::model_parts(&context, &media_cfg, &media)
        })
        .await
        .map_err(|_| "图片获取失败".to_string())?;
        // Recheck after media and admission, immediately before taking the current epoch.
        if let Some(reason) = self.before_turn.take().and_then(|check| check()) {
            return Err(reason);
        }
        self.begin_ledger_turn_as(key, adapter.id(), false)?;
        self.chat = Some(ChatRun {
            prompt: Some(prompt),
            adapter,
            reserved: None,
            mode: mode.to_string(),
            silent: false,
            ack,
            media_parts,
            owner_started,
        });
        let result = self.run_chat_inner(key, "", cfg).await;
        self.chat = None;
        self.end_ledger_turn(result.as_ref().map(|_| ()).map_err(String::as_str));
        if result.is_err() && uses_dsh && self.defer_unsubmitted_dsh_chat(key, cfg) {
            return Ok(ChatTurnOutcome::WaitForDsh);
        }
        result.map(|()| ChatTurnOutcome::Completed)
    }

    pub(crate) fn owner_interaction_closed(&self) -> bool {
        self.owner_interaction_closed
    }

    /// First-generation turns share session policy, media delivery, turn recovery and the outbox.
    pub async fn run_reserved_turn(
        &mut self,
        key: &str,
        prompt: &str,
        cfg: &serde_json::Value,
        inbound: &[String],
        media: &[serde_json::Value],
        silent: bool,
        authority: crate::reserved::Authority,
    ) -> Result<(), String> {
        authority.check(&self.context, key)?;
        self.reserved_failure_parks = None;
        self.expect_dispatch_token(authority.token.clone());
        self.preset_chat = crate::chat::chat_preset(cfg);
        self.note_turn(
            if silent {
                "reserved-summary"
            } else {
                "reserved"
            },
            inbound,
        );
        let context = self.context.clone();
        let media_cfg = cfg.clone();
        let media = media.to_vec();
        let media_parts = tokio::task::spawn_blocking(move || {
            crate::media::model_parts(&context, &media_cfg, &media)
        })
        .await
        .map_err(|_| "图片获取失败".to_string())?;
        let (adapter, _admission, _) = self.turn_adapter(key, "reserved").await?;
        authority.check(&self.context, key)?;
        self.begin_ledger_turn_as(key, adapter.id(), false)?;
        let ack = if authority.social() || silent {
            None
        } else {
            crate::chat::ack_message(cfg)
        };
        self.chat = Some(ChatRun {
            adapter,
            reserved: Some(authority),
            prompt: None,
            mode: "reserved".to_string(),
            silent,
            ack,
            media_parts,
            // A reserved batch has no single sender; only the owner's private conversation counts.
            owner_started: crate::chat::is_owner_key(cfg, key),
        });
        let result = self.run_chat_inner(key, prompt, cfg).await;
        self.chat = None;
        if result.is_err() && self.dispatch_definitely_unsubmitted() {
            if let Some(turn) = &self.cur_turn {
                // Includes definite prompt refusals that already have an attempt
                // record. Unknown/accepted submissions never receive this proof.
                turn.verified("unsubmitted", "reserved retry is safe")?;
            }
        }
        self.end_ledger_turn(result.as_ref().map(|_| ()).map_err(String::as_str));
        result
    }
    // ── the turn-engine seam (`turn_engine`) ─────────────────────────────────────

    /// The adapter admitted for the next turn, or the scripted adapter a test installed.
    async fn turn_adapter(&mut self, key: &str, mode: &str) -> Result<(
        crate::turn_engine::Adapter, Option<crate::engine_switch::TurnGates>, Option<serde_json::Value>,
    ), TurnAdmissionError> {
        if !self.context.is_isolated() {
            return Ok((crate::turn_engine::Adapter::Dsh, None, None));
        }
        let release = crate::release::turn_guard(&self.context).await?;
        let gate = crate::engine_switch::turn_gate(&self.context, key).await?;
        if self.reserved2_route.as_ref().is_some_and(|route| {
            *route.lock().unwrap_or_else(|p| p.into_inner()) == Reserved2Route::ExpiredBeforeAdmission
        }) {
            return Err(TurnAdmissionError::Refused(RESERVED2_CAP_BEFORE_ADMISSION.into()));
        }
        let gates = (release, gate);
        #[cfg(test)]
        if let Some(script) = self.scripted.take() {
            return Ok((crate::turn_engine::Adapter::Scripted(script), Some(gates), None));
        }
        let (context, key, mode) = (self.context.clone(), key.to_string(), mode.to_string());
        let simulation = mode == "reserved2";
        let expected = self.dispatch_token.clone().filter(|_| simulation);
        let publication = self.minted_slot.clone().filter(|_| simulation);
        // The blocking task retains both gates if its async caller is cancelled.
        let (engine, token, gates, cfg) = tokio::task::spawn_blocking(move || {
            use crate::engine_switch::TurnPreparation;
            let (preparation, cfg) =
                crate::engine_switch::prepare_turn(&context, &key, &mode, expected.as_deref(), publication.as_ref())?;
            match preparation {
                TurnPreparation::Ready(engine) => Ok((engine, None, gates, cfg)),
                TurnPreparation::Switched(engine, token) => Ok((engine, Some(token), gates, cfg)),
                TurnPreparation::ParkUntil(at) => Err(TurnAdmissionError::ParkSwitchUntil(at)),
            }
        }).await.map_err(|_| "engine admission worker stopped".to_string())??;
        if let Some(token) = token.filter(|_| simulation && self.dispatch_token.is_some()) {
            self.dispatch_token = Some(token);
        }
        Ok((crate::turn_engine::Adapter::for_engine(engine), Some(gates), simulation.then_some(cfg)))
    }

    /// Run one admitted turn on `adapter`. The pipeline already holds the turn's ledger entry and
    /// token; the adapter submits `prompt`, streams the turn, stops when it is cancelled and reports
    /// its facts (`last_report` and the turn timing record). Returns the model's final text.
    async fn run_admitted_turn(
        &mut self,
        adapter: &crate::turn_engine::Adapter,
        key: &str,
        prompt: &str,
        timeout: std::time::Duration,
        cfg: &serde_json::Value,
    ) -> Result<String, String> {
        match adapter {
            crate::turn_engine::Adapter::Dsh => self.run_dsh_turn(key, prompt, timeout, cfg).await,
            crate::turn_engine::Adapter::Harness => {
                // Gate waits and a handover can outlast the queued job's route configuration.
                let fresh = self.context.load_config().map_err(|error| error.to_string())?;
                self.ask_harness(key, prompt, timeout, &fresh).await
            }
            #[cfg(test)]
            crate::turn_engine::Adapter::Scripted(script) => self.run_scripted(script, key, prompt),
        }
    }

    /// Write the turn timing record (`logs/turn-timing.jsonl`) of an adapter's reported facts.
    fn report_facts(&self, key: &str, facts: &crate::turn_engine::Facts) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let turn = self.cur_turn.as_ref().map(|turn| turn.id.as_str());
        Self::write_turn_timing(&self.context, facts.timing_record(key, turn, ts));
    }

    /// The scripted test adapter: records the attempt, its acceptance and the thinking start like
    /// a live engine, answers with the script and reports facts. It never contacts DSH or a model.
    #[cfg(test)]
    fn run_scripted(&mut self, script: &crate::turn_engine::Script, key: &str, prompt: &str) -> Result<String, String> {
        let begun = std::time::Instant::now();
        let recorder = self.cur_turn.as_ref().ok_or("the scripted adapter requires a durable bridge turn")?;
        let attempt = format!("scripted:{}", recorder.id);
        recorder.attempt(&attempt, "")?;
        self.dispatch_unsubmitted = false;
        recorder.accepted(&attempt)?;
        recorder.started()?;
        script.seen.lock().unwrap_or_else(|p| p.into_inner()).push((key.to_string(), prompt.to_string()));
        if let Some(ack) = self.chat.as_mut().and_then(|c| c.ack.take()) {
            self.send_chat_text(key, "", "ack", &ack);
        }
        let ok = script.reply.is_ok();
        self.report_facts(key, &crate::turn_engine::Facts {
            ok,
            end_kind: Some(if ok { script.end_kind } else { "error" }.to_string()),
            cancel_reason: None,
            attempt,
            model: Some("scripted/fixture".to_string()),
            attempts: 1,
            prompt_chars: prompt.chars().count(),
            segments: Vec::new(),
            turn_ms: 0,
            total_ms: begun.elapsed().as_millis() as i64,
        });
        if ok {
            self.last_report = Some(TurnReport {
                end_kind: script.end_kind.to_string(),
                send_tool_ok: script.send_tool_ok,
                command: None,
            });
        }
        script.reply.clone()
    }

    fn check_harness_authority(&self, key: &str) -> Result<serde_json::Value, String> {
        if self.reserved2_budget.is_none() {
            return self.chat_guard(key, "").map(|()| serde_json::Value::Null);
        }
        let cfg = self.context.load_config().map_err(str::to_string)?;
        let doc = crate::conversation_state::ConversationState::for_runtime(&self.context)
            .read_document()
            .map_err(|(_, e)| e)?;
        let conv_token = doc
            .conversation(key)
            .and_then(|c| c.token().map(str::to_string));
        if crate::harness::Tools::current_mode(&self.context) != "reserved2"
            || !crate::social_state::inbound_admitted(&cfg, key)
            || cfg.pointer("/socialV2/enabled") == Some(&serde_json::json!(false))
            || doc.paused()
            || conv_token.as_deref() != Some(self.turn_token.as_str())
        {
            return Err("mode, permission or turn generation changed".into());
        }
        Ok(doc.into_value())
    }

    fn harness_now_ms(&self) -> i64 {
        #[cfg(test)]
        if let Some(clock) = &self.cap_clock {
            return clock.load(std::sync::atomic::Ordering::Acquire);
        }
        crate::turn_ledger::now_ms()
    }

    fn check_harness_turn(&self, key: &str) -> Result<bool, String> {
        let state = self.check_harness_authority(key)?;
        let now = self.harness_now_ms();
        Ok(self.reserved2_budget.is_some_and(|budget| budget.expired_at(now, &state["conversations"][key])))
    }

    fn harness_admission_expired(&self, key: &str) -> bool {
        let now = self.harness_now_ms();
        self.reserved2_budget.is_some_and(|budget| {
            match self.context.read_social_state() {
                Ok(state) => budget.expired_at(now, &state["conversations"][key]),
                // A failed state read cannot grant a pre-admission wake an extension.
                Err(_) => now >= budget.deadline,
            }
        })
    }

    fn dispatch_expired(&self) -> bool {
        self.dispatch_deadline.is_some_and(|deadline| {
            #[cfg(test)]
            if let Some(clock) = &self.admission_clock { return clock() >= deadline; }
            crate::turn_ledger::now_ms() >= deadline
        })
    }

    fn publish_expired_route(&self) {
        if let Some(route) = &self.reserved2_route {
            *route.lock().unwrap_or_else(|p| p.into_inner()) = Reserved2Route::ExpiredBeforeAdmission;
        }
    }

    fn expire_unadmitted_harness(&mut self, key: &str) -> String {
        self.publish_expired_route();
        if self.current_reason == "timeout" {
            if let Some((at, previous)) = self.claimed_wake.as_ref() {
                if previous["wakeConfig"]["sleepUntil"].is_string() {
                    if let Err(error) = crate::social_state::retire_reserved2_deadline(
                        &self.context,
                        key,
                        &self.turn_token,
                        *at,
                        previous,
                    ) {
                        eprintln!("[turn] expired timeout retirement failed: {error}");
                    }
                }
            }
        }
        if let Some(turn) = &self.cur_turn {
            if let Err(error) = turn.verified("expired-before-admission", "turnTimeout") {
                eprintln!("[turn] expired wake note failed: {error}");
            }
        }
        // The companion console may have granted snapshot receipts even if rendering failed.
        if let Err(error) = revoke_reserved2_epoch(&self.context, key, &self.turn_token, self.minted_slot.as_ref()) {
            eprintln!("[turn] expired wake revocation failed: {error}");
        }
        self.finish_turn("cancelled", "turnTimeout");
        crate::harness::DEADLINE_EXCEEDED.into()
    }

    fn revoke_harness_cap(&self, key: &str) {
        if let Err(error) = revoke_reserved2_epoch(&self.context, key, &self.turn_token, self.minted_slot.as_ref()) {
            eprintln!("[turn] simulation cap revocation failed: {error}");
        }
    }

    /// The harness adapter: admit the turn to HER's own harness, stream it, cancel it when the
    /// conversation's mode, permission or token changes, and report its facts.
    async fn ask_harness(
        &mut self,
        key: &str,
        prompt: &str,
        timeout: std::time::Duration,
        cfg: &serde_json::Value,
    ) -> Result<String, String> {
        use crate::harness::{Harness, ModelRoute, TurnRequest};
        // A model error noted by an earlier turn belongs to that turn: this turn's own outcome decides what its caller
        // is told (the DSH path clears it the same way).
        take_model_error(&self.context, key);
        let begun = std::time::Instant::now();
        if self.check_harness_turn(key)? {
            return Err(self.expire_unadmitted_harness(key));
        }
        // #162: the conversation's selected model route first, then the other routes in configured order.
        let route = ModelRoute::for_conversation(&self.context, cfg, key)?;
        let harness = Harness::open(&self.context)?;
        let recorder = self.cur_turn.clone().ok_or("harness requires a durable bridge turn")?;
        let attempt = format!("harness:{}", recorder.id);
        let mode = self.chat.as_ref().map_or("reserved2", |c| c.mode.as_str());
        let mut delivery = crate::harness::TurnDelivery {
            recorder: Some(recorder.clone()),
            reserved2_budget: self.reserved2_budget,
            prompt: Some(self.chat.as_ref().and_then(|chat| chat.prompt.clone())
                .map_or(crate::harness::PromptSource::Rendered { snapshot: false }, crate::harness::PromptSource::Chat)),
            ..Default::default()
        };
        let (prompt, layers) = if mode == "reserved2" && self.current_reason != "wakeReminder" {
            let mut record = harness.wake_briefing(key, &self.turn_epoch)?;
            let persona = crate::persona::briefing_identity(&self.context, key);
            let identity = serde_json::json!(persona);
            if record["installed"].is_object() && record["persona"] != identity {
                crate::briefing_delivery::next_generation(&mut record);
            }
            record["persona"] = identity;
            let plan = crate::briefing_delivery::plan(&record);
            let (context, config, wake_key, token, reason, prebuilt) = (
                self.context.clone(), cfg.clone(), key.to_owned(), self.turn_token.clone(),
                self.current_reason.clone(), prompt.to_owned(),
            );
            let turn = recorder.id.clone();
            let (wake, plan) = tokio::task::spawn_blocking(move || {
                let wake = crate::prompt::render_wake(&context, &config, &wake_key, &token, &reason, &prebuilt, &plan, false, &turn)?;
                if crate::persona::briefing_identity(&context, &wake_key) != persona {
                    return Err(crate::prompt::WAKE_BRIEFING_CHANGED.to_owned());
                }
                Ok::<_, String>((wake, plan))
            }).await.map_err(|_| "wake briefing worker stopped".to_owned())??;
            if !wake.hold(&mut record, &plan, &recorder.id) {
                return Err(crate::prompt::WAKE_BRIEFING_CHANGED.to_owned());
            }
            self.carried_messages.clear();
            crate::model_failure::merge_refs(&mut self.carried_messages,
                crate::model_failure::snapshot_refs(&wake.text).unwrap_or_default());
            let layers = serde_json::json!({"injected": wake.layers.to_json(), "cut": wake.cut.to_json()});
            // Harness history is keyed by the authority epoch, which also identifies its session.
            let authority_epoch = self.turn_epoch.as_str();
            let session_id = authority_epoch;
            crate::briefing::alert_empty(&self.context, cfg, key, session_id, authority_epoch, plan.generation, &wake.layers);
            delivery.briefing = Some(record);
            delivery.full_prompt = wake.full_text;
            delivery.prompt = Some(crate::harness::PromptSource::Rendered { snapshot: wake.has_snapshot });
            delivery.recalled = wake.recalled;
            (wake.text, Some(layers))
        } else if mode == "reserved2" {
            (prompt.lines().filter(|line| !line.starts_with("【会话令牌】")).collect::<Vec<_>>().join("\n"), None)
        } else {
            (prompt.to_owned(), None)
        };
        let prompt = if mode == "reserved2" {
            crate::activity::mask_session_tokens(&prompt, std::slice::from_ref(&self.turn_token))
        } else { prompt };
        if let Some(full) = &mut delivery.full_prompt {
            *full = crate::activity::mask_session_tokens(full, std::slice::from_ref(&self.turn_token));
        }
        let deadline = if let Some(budget) = self.reserved2_budget {
            if self.check_harness_turn(key)? { return Err(self.expire_unadmitted_harness(key)); }
            std::time::Duration::from_millis(budget.ceiling.saturating_sub(crate::turn_ledger::now_ms()).max(0) as u64)
        } else { timeout };
        let tools = crate::harness::Tools::capture(&self.context, cfg, mode, key, &self.turn_epoch)?
            .with_interactions(self.owner_interactions.clone());
        let prompt_chars = prompt.chars().count();
        recorder.attempt_with_layers(&attempt, "", layers.as_ref())?;
        let mut turn = harness.admit_with_delivery(TurnRequest {
            id: recorder.id.clone(), conversation: key.to_string(), generation: self.turn_epoch.clone(),
            prompt,
            media: self.chat.as_ref().map(|c| c.media_parts.clone()).unwrap_or_default(),
            profile: if matches!(mode, "closed-agent" | "reserved2") { mode.into() } else { self.preset_chat.clone() },
            route,
            limits: crate::harness::Limits::from_config(cfg),
            tools,
            deadline,
        }, delivery)?;
        self.dispatch_unsubmitted = false;
        let accepted = recorder.accepted(&attempt);
        if let Err(error) = accepted {
            turn.cancel();
            let _ = turn.wait().await;
            self.turn_final = matches!(turn.status(), crate::harness::Status::Paused { .. });
            return Err(error);
        }
        let started = std::time::Instant::now();
        if let Some(ack) = self.chat.as_mut().and_then(|c| c.ack.take()) {
            self.send_chat_text(key, "", "ack", &ack);
        }
        let mut invalidated = false;
        let mut capped = false;
        let result = loop {
            tokio::select! { biased;
                result = turn.wait() => break result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    #[cfg(test)]
                    if let Some(hook) = self.harness_cap_poll_hook.take() {
                        let (release, wait) = tokio::sync::oneshot::channel();
                        let _ = hook.send(release);
                        let _ = wait.await;
                    }
                    match self.check_harness_turn(key) {
                        Ok(false) => continue,
                        Ok(true) => {
                            if crate::social_state::observation_wait_turn(&self.context, key)
                                .ok().flatten().as_deref() == Some(recorder.id.as_str())
                            {
                                // Publish the sleep before revocation can release the handler.
                                turn.mark_observation_sleep();
                            }
                            self.revoke_harness_cap(key);
                            capped = true;
                        }
                        Err(_) => invalidated = true,
                    }
                    turn.cancel();
                    break turn.wait().await;
                }
            }
        };
        use crate::harness::Status;
        let mut status = turn.status();
        if !capped && self.reserved2_budget.is_some()
            && (matches!(status, Status::ObservationSleep)
                || result.as_ref().is_err_and(|error| error == crate::harness::DEADLINE_EXCEEDED))
        {
            // The runner can observe the cap while a response or tool dispatch beats our poll.
            self.revoke_harness_cap(key);
            capped = true;
        }
        let turn_ms = started.elapsed().as_millis() as i64;
        // The runner may have committed completion while the cap poll was checking authority.
        let settled_cap = match &status {
            Status::ObservationSleep => Some("observationSleep"),
            Status::Failed(_) | Status::Cancelled if capped => {
                status = Status::Failed(crate::harness::DEADLINE_EXCEEDED.into());
                Some("turnTimeout")
            }
            _ => None,
        };
        // The route the turn's last request went to, after any failover.
        let model = turn.model();
        self.report_facts(key, &crate::turn_engine::Facts::of_harness(
            &status,
            invalidated,
            attempt,
            format!("openai/{model}"),
            prompt_chars,
            turn_ms,
            begun.elapsed().as_millis() as i64,
        ));
        let result = if let Some(reason) = settled_cap {
            self.turn_final = true;
            let observation = reason == "observationSleep";
            let outcome = if observation { "completed" } else { "cancelled" };
            let _ = recorder.verified("cancel-requested", reason);
            if recorder.end_cancellation(outcome, reason).is_err() {
                eprintln!("[turn] simulation cap terminal write failed; recovery required");
            }
            if observation { return Ok(String::new()); }
            Err(crate::harness::DEADLINE_EXCEEDED.to_owned())
        } else { result };
        match &status {
            crate::harness::Status::Paused { .. } => self.turn_final = true,
            crate::harness::Status::Cancelled => self.finish_turn("cancelled", "harness turn cancelled"),
            _ => {}
        }
        // Each route request that failed over or ended the turn is an owner alert, as the DSH chain reports each failed
        // candidate. Only the one that ended a model turn is that turn's model error.
        let model_end = matches!(&result, Err(error) if error.starts_with("model/backend "));
        let failures = turn.route_failures();
        let count = failures.len();
        for (index, failure) in failures.iter().enumerate() {
            println!("[failover] {key} harness route {} failed ({})", failure.route, failure.summary);
            let classified = crate::model_failure::Failure::classify("", &failure.summary);
            if model_end && index + 1 == count {
                note_model_error(&self.context, key, &classified.summary());
            }
            let call = crate::model_failure::RefusalCall {
                turn_id: &recorder.id,
                index: Some(failure.model_call),
                rpc_id: None,
            };
            self.alert_backend_failure(cfg, key, "harness", "openai", &failure.model, &classified,
                &failure.message_refs, Some(call));
        }
        // When every route of the chain has failed, the owner is told once per outage, after the route reports above
        // (`harness::route_health`).
        if count > 0 {
            crate::harness::route_health::alert_if_owed(&self.context, cfg, key);
        }
        if matches!(&status, Status::Failed(_)) && turn.route_chain_exhausted() && !capped && !invalidated {
            self.send_persona_failure_line(key, None);
        }
        let text = match result {
            Ok(text) => text,
            Err(error) => {
                self.owner_interaction_closed = turn.owner_interaction_closed();
                if error == crate::harness::DEADLINE_EXCEEDED && !self.owner_interaction_closed {
                    self.report_backend_failure(cfg, key, "harness", "openai", &model, "", &error);
                }
                return Err(error);
            }
        };
        // A completed reserved2 turn can reach here after its cap poll revoked its token.
        // Reserved2 only logs the final text, and the hub checks epoch ownership before settling coverage.
        if !capped {
            self.check_harness_authority(key)?;
        }
        self.last_report = Some(TurnReport { end_kind: "completed".into(), send_tool_ok: turn.sent_tool_reply(), ..Default::default() });
        Ok(text)
    }

    async fn run_chat_inner(&mut self, key: &str, prompt: &str, cfg: &serde_json::Value) -> Result<(), String> {
        let adapter = self.chat.as_ref().map_or(crate::turn_engine::Adapter::Dsh, |c| c.adapter.clone());
        let at = crate::parity_clock::now_ms();
        let delivered = if let Some(chat_prompt) = self.chat.as_mut().and_then(|chat| chat.prompt.as_mut()) {
            let prefix = crate::prompt::wrap_for_delivery_with_runtime(&self.context, "", cfg, at);
            chat_prompt.prefix.insert_str(0, &prefix);
            let view = if adapter.uses_dsh() {
                crate::model_view::with_blacklist(&self.context, cfg)
            } else {
                serde_json::Value::Null
            };
            chat_prompt.render(&view)
        } else {
            crate::prompt::wrap_for_delivery_with_runtime(&self.context, prompt, cfg, at)
        };
        let timeout = std::time::Duration::from_secs(turn_timeout_secs(cfg));
        let text = self.run_admitted_turn(&adapter, key, &delivered, timeout, cfg).await?;
        let report = self.last_report.take().unwrap_or_default();
        if self.chat.as_ref().is_some_and(|c|c.silent) { return Ok(()); }
        let sid = crate::session_map::lookup(&self.context, key)?.unwrap_or_default();
        let reply_cfg = self.context.load_config().unwrap_or_else(|_| cfg.clone());
        let audit = outbound_guard_enabled(&reply_cfg)
            && (!self.chat.as_ref().is_some_and(|c| c.mode == "closed-agent") || !crate::chat::is_owner_key(cfg, key));
        let mut secrets = self.secrets.clone();
        if let Ok(Some(token)) = crate::social_state::token(&self.context, key) {
            secrets.push(token);
        }
        if let Some(command) = report.command {
            if !self.chat.as_ref().is_some_and(|c| c.owner_started) {
                // DSH ran a slash command for a turn the owner did not start: never relay its output.
                println!("[chat] {key} DSH 命令结果来自非管理员回合，未发出");
                return Ok(());
            }
            // `auditAndSend`: a command result passes the same sentinel as a reply.
            let plain = crate::chat::md_to_plain(&command);
            if plain.is_empty() {
                return Ok(());
            }
            match audit.then(|| configured_outbound_violation(&reply_cfg, &plain, &secrets)).flatten() {
                Some(why) => {
                    self.alert_blocked_reply(cfg, key);
                    println!("[chat] {key} 命令结果被安全策略拦截（{why}），未发出")
                }
                None => {
                    self.send_chat_text(key, &sid, "reply", &plain);
                }
            }
            return Ok(());
        }
        match crate::chat::reply_plan_with_audit(&report.end_kind, &text, report.send_tool_ok, &secrets, audit) {
            crate::chat::Reply::Silent(why) => println!("[chat] {key} 不发送（{why}）"),
            crate::chat::Reply::Blocked(why) => {
                let plain = crate::chat::md_to_plain(&text);
                let has_token = secrets.iter().any(|t| !t.is_empty() && plain.contains(t.as_str()));
                crate::activity::append(
                    &self.context,
                    &format!("{key} 对外发送已被安全规则拦下（含敏感片段{}）", if has_token { "或会话令牌" } else { "" }),
                );
                self.alert_blocked_reply(cfg, key);
                println!("[chat] {key} 外发回复未通过安全检查（{why}），已阻断")
            }
            crate::chat::Reply::Stopped => {
                println!("[chat] {key} 回合被中止");
                self.send_chat_text(key, &sid, "reply", "⏹️ 已停止");
            }
            crate::chat::Reply::Send(plain) => {
                let silent_for_members = crate::social_state::role_state(&self.context)
                    .ok()
                    .is_some_and(|state| state.get("mode").and_then(|m| m.as_str()) == Some("silent"))
                    && !crate::chat::is_owner_key(cfg, key);
                if silent_for_members {
                    crate::activity::append(&self.context, &format!("{key} 处于静默状态，截停在途回复"));
                    println!("[chat] {key} 处于静默状态，截停在途回复");
                } else {
                    self.log_reply_activity(key, &plain);
                    self.send_chat_text(key, &sid, "reply", &plain);
                }
            }
        }
        Ok(())
    }

    /// Relay pinned CLI drafts through the authenticated console within one durable turn.
    /// Only an empty initial draft retains DSH fallback; refusals and later silence are terminal.
    async fn run_cli_turn<F>(
        &mut self, target: &Target, cli_prompt: &str, cfg: &serde_json::Value, id: &str, mut provider: F,
    ) -> Result<Option<usize>, String>
    where F: FnMut(&str) -> Result<(String, String), String> + Send {
        let key = target.key();
        let token = if self.cur_turn.is_some() { self.turn_token.clone() }
            else { crate::social_state::ensure_or_rotate(&self.context, &key, false)? };
        let mut draft_prompt = cli_prompt.to_owned();
        // Match the shared hold's three revisions. The fourth draft uses its fixed deadline.
        for revision in 0..=3 {
            let started = std::time::Instant::now();
            self.dispatch_unsubmitted = false;
            let result = provider(&draft_prompt);
            let elapsed_ms = started.elapsed().as_millis() as i64;
            let record = serde_json::json!({
                "ts": crate::turn_ledger::now_ms(), "key": key, "backend": id, "mode": "text-only",
                "elapsedMs": elapsed_ms, "ok": result.is_ok(),
                "chars": result.as_ref().map(|(_,t)|t.trim().chars().count()).unwrap_or(0),
                "err": result.as_ref().err().map(|error| crate::model_failure::Failure::classify("", error).summary()).unwrap_or_default(),
            }).to_string();
            let _ = Self::append_runtime_log(&self.context,"logs/opencode-trial.jsonl",&record);
            let (used,text) = match result {
                Ok(value) => value,
                Err(note) => {
                    let failure = crate::model_failure::Failure::classify("", &note);
                    let (provider, model) = crate::model_failure::cli_identity(cfg,id);
                    self.report_backend_failure(cfg,&key,id,&provider,&model,"",&note);
                    if failure.class != "refusal" {
                        self.send_persona_failure_line(&key, None);
                    }
                    return Err(failure.summary());
                }
            };
            if text.trim().is_empty() { return Ok(if revision == 0 { None } else { Some(0) }); }
            if is_silent_marker(&text) { return Ok(Some(0)); }
            if let Some(turn) = &self.cur_turn {
                turn.attempt(&format!("cli:{}",crate::dsh::mint_request_id()),"")
                    .map_err(|e|format!("回合账本写入失败，不代发：{e}"))?;
            }
            let fresh = self.context.load_config().map_err(str::to_string)?;
            if let Some(why) = configured_outbound_violation(&fresh,&text,&self.secrets) {
                return Err(format!("被出站哨兵拦下：{why}"));
            }
            // Keep live ingress running while the companion console holds the draft.
            let (context, send_key, send_token, send_text) =
                (self.context.clone(),key.clone(),token.clone(),text.clone());
            let sent = tokio::task::spawn_blocking(move || {
                crate::qq_tools::send_reserved2_reply(&context,&send_key,&send_token,&send_text)
            }).await.map_err(|_|"CLI send worker did not complete".to_owned())?;
            let (status, response) = sent.map_err(|_| {
                let error = "CLI reply send failed: companion console unavailable";
                eprintln!("[pipeline] {error}");
                error.to_owned()
            })?;
            if status == 429 { return Ok(Some(0)); }
            if status == 403 && crate::group_mute::active(&self.context, &key).is_some() {
                // #46: the console refused locally because the group is muted: a clean sleep, not a failed send.
                println!("[pipeline] {key} 禁言中，CLI 回复未发送");
                return Ok(Some(0));
            }
            if !(200..300).contains(&status) || response["ok"] == false {
                return Err(format!("CLI reply send failed: HTTP {status}"));
            }
            if response["reviseRequired"] == true {
                if revision == 3 { return Err("CLI reply revision budget is exhausted".to_owned()); }
                crate::model_failure::merge_refs(&mut self.carried_messages,
                    crate::model_failure::message_refs(&response["newMessages"]));
                let response = crate::model_view::strip_chat_with_runtime(&self.context, &response, &fresh);
                draft_prompt = format!("{draft_prompt}\n\n上一条草稿没有发送：等待期间又有新消息。请据此重写回复，只输出要发到 QQ 的正文；无需回复就输出 [SILENT]。\n【本次消息快照】\n{response}");
                continue;
            }
            let sent = response["sent"].as_u64().ok_or("CLI send result is unavailable")?;
            println!("[pipeline] {key} CLI {used} ({elapsed_ms}ms): sent {sent}");
            return Ok(Some(if sent > 0 { text.chars().count() } else { 0 }));
        }
        unreachable!("bounded CLI revision loop always returns")
    }

    /// 回合本体（账本包装在 `run_turn`）。
    ///
    /// 这是 JS 的模式，也是更自然的那个：桥接只负责"叫醒 + 给上下文"，
    /// 说什么、什么时候说、发几条，全由模型决定。
    ///
    /// 为什么不能用 `handle_with_failover`：它会**把模型的输出文本再发一遍**。
    /// 模型自己调工具发了一条之后，那段"最终回答"（常常是"群里已经回了一句"
    /// 这类内心独白）就被桥接当成话发出去 —— 实测 2026-10-23 11:23:45/48
    /// 那对重复消息就是这么产生的（日志「已回复 9 字」正好对上群里那句）。
    #[allow(clippy::too_many_arguments)]
    async fn run_turn_inner(
        &mut self,
        target: &Target,
        dsh_prompt: &str,
        cli_prompt: &str,
        cfg: &serde_json::Value,
        cli_cfg: &serde_json::Value,
        adapter: crate::turn_engine::Adapter,
        refs: WakePromptRefs,
    ) -> Result<usize, String> {
        let key = target.key();
        let key: &str = &key;

        // ── 1) 纯文本后端（opencode 免费档）──
        //
        // 关键设计：给它 `build_text_only` 那份**自包含**提示词（消息、状态都塞进去），
        // 要求它只输出一句话、不调任何工具；拿到文本后**由 Rust 代发**。
        //
        // 为什么不能让它自己调工具：opencode v2 的 MCP 工具藏在 `execute` 代码沙箱里，
        // flash 档模型想不明白，实测会转去 `shell`/`grep`/`glob` 翻仓库，
        // 15 步烧 95 秒还一条消息没读到。改成纯文本后它没有别的路可走。
        if let Some(id) = pinned_cli_backend(cli_cfg, key).filter(|_| adapter.uses_dsh()) {
            let inbound_refs = refs.cli.map(|cli| std::mem::replace(&mut self.carried_messages, cli));
            let (context, backend_cfg, backend_id, backend_key) =
                (self.context.clone(), cli_cfg.clone(), id.to_owned(), key.to_owned());
            if let Some(sent) = self.run_cli_turn(target, cli_prompt, cli_cfg, id, move |prompt| {
                crate::backend::cli_fallback_by_id_with_runtime(
                    &context, &backend_cfg, &backend_id, &backend_key, prompt, 30,
                )
            }).await? { return Ok(sent); }
            if let Some(inbound_refs) = inbound_refs { self.carried_messages = inbound_refs; }
        }
        if adapter.uses_dsh() {
            if let Some(dsh) = refs.dsh { self.carried_messages = dsh; }
        }

        // ── 2) The turn engine: full prompt, the model sends through its own tools ──
        //
        // The DSH adapter drops `backends.sessions` from its configuration copy, so the pinned
        // CLI above never runs a second time inside its failover chain.
        //
        // **不要代发模型输出**。
        //
        // 2026-10-23 20:2x 我在这里加过一条"安全网"：模型没调发送工具就把它输出的文本代发出去。
        // 结果它把**内心独白**发进了群：
        //   「群里还是没动静（水位仍在 2016，未读为 0），我只把已读状态提交了一下，
        //     没多发消息，继续按原配置潜水。」
        // 原因很简单：模型决定**潜水**时也会输出一段推理，那段文字不是要说的话。
        // 在"代发"这一层无法可靠区分「要说的话」和「为什么不说」，所以这条路彻底不走。
        //
        // 正确的分工：说话 = 模型显式调 `qq_send_message`；不说话 = 什么都没发生。
        // 如果它该说而没说，那是**提示词**的问题（见 prompt::build 的唤醒上下文），
        // 要在提示词里把"谁说了什么、要不要接"说清楚，而不是在这里瞎补。
        let before = Self::last_ai_reply_at(&self.context, &key);
        // JS `withSlangContext`: every prompt handed to DSH starts with the clock line (and the
        // confirmed slang block) at the moment of delivery.
        let delivered = crate::prompt::wrap_for_delivery_with_runtime(
            &self.context,
            dsh_prompt,
            cfg,
            crate::parity_clock::now_ms(),
        );
        let timeout = if matches!(&adapter, crate::turn_engine::Adapter::Harness) {
            // The adapter reads the absolute dispatch budget itself.
            std::time::Duration::ZERO
        } else {
            reserved2_turn_timeout(cfg)
        };
        let reply = self.run_admitted_turn(&adapter, key, &delivered, timeout, cfg).await?;
        let after = Self::last_ai_reply_at(&self.context, &key);
        let trimmed = reply.trim();
        let n = trimmed.chars().count();
        // 记录模型到底输出了什么（截断）：既用于判断它有没有说话，也是排障的唯一线索。
        let preview = internal_output_preview(&self.context, trimmed);
        if crate::social_state::mode(&self.context) == "reserved2" {
            self.log_internal_output(key, trimmed);
        }
        println!(
            "[pipeline] {key}：{} 回合结束（输出 {n} 字，{}）｜{preview}",
            if adapter.uses_dsh() { "DSH" } else { adapter.id() },
            if after > before {
                "模型自己发过"
            } else {
                "模型选择不说"
            }
        );
        Ok(n)
    }

}

#[cfg(test)]
mod tests {
    include!("pipeline_cli_relay_tests.rs");
    include!("pipeline_completion_tests.rs");
    include!("pipeline_engine_tests.rs");
    use super::*;
    use super::dsh_adapter::{prompt_error_needs_probe, turn_records};
    use crate::dsh::TurnUsage;
    use serde_json::{json, Value};

    #[test]
    fn hotfix128_wrapped_fix1_structured_strings_are_masked_before_serialization() {
        let root = fake_runtime_root("wrapped-log-128");
        std::fs::write(root.join("data/social-v2.json"),
            r#"{"conversations":{"group:42":{"agentToken":"known-structured-token-128"}}}"#).unwrap();
        let context = crate::runtime::RuntimeContext::from_root(&root).unwrap();
        for newline in ["\n", "\r\n"] {
            let record = json!({"message":format!("prefix known-structured- {newline} token-128 suffix"),
                "nested":[{"detail":format!("known-structured-{newline}token-128"),"ok":true}],"count":2});
            Pipeline::append_runtime_log(&context, "logs/wrapped128.jsonl", &record.to_string()).unwrap();
        }
        let raw = std::fs::read_to_string(root.join("data/logs/wrapped128.jsonl" )).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        let values: Vec<Value> = raw.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(values, vec![json!({"message":"prefix *** suffix","nested":[{"detail":"***","ok":true}],"count":2}); 2]);
    }

    #[test]
    fn hotfix128_structured_pipeline_logs_mask_tokens_with_the_guard_off() {
        let root = fake_runtime_root("token-log-128");
        std::fs::write(root.join("data/social-v2.json"),
            r#"{"conversations":{"group:42":{"agentToken":"known-structured-token-128"}}}"#).unwrap();
        std::fs::write(root.join("config/config.json"),
            r#"{"consoleToken":"fixture-console-token-128","security":{"outboundGuard":false}}"#).unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let shape = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let record = json!({"err":format!("ordinary known-structured-token-128 {shape}"),"ok":false});
        Pipeline::append_runtime_log(&context, "logs/pipeline128.jsonl", &record.to_string()).unwrap();
        let raw = std::fs::read_to_string(root.join("data/logs/pipeline128.jsonl")).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(value, json!({"err":"ordinary *** ***","ok":false}), "{raw}");
    }

    static DSH_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct SessionStoreFixtureRoot(std::path::PathBuf);

    impl SessionStoreFixtureRoot {
        fn new(label: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock follows UNIX epoch")
                .as_nanos();
            let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-ticket04-session-store-{label}-{}-{stamp}",
                std::process::id()
            )));
            std::fs::create_dir(&root).expect("unique synthetic session root");
            let fixture = Self(root);
            std::fs::create_dir(fixture.0.join("data")).unwrap();
            fixture
        }

        fn session_file(&self) -> std::path::PathBuf {
            self.0.join("data/sessions.json")
        }
    }

    impl Drop for SessionStoreFixtureRoot {
        fn drop(&mut self) {
            let Ok(temp_root) = std::fs::canonicalize(std::env::temp_dir()) else {
                return;
            };
            let Ok(root) = std::fs::canonicalize(&self.0) else {
                return;
            };
            if root.parent() != Some(temp_root.as_path())
                || !root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("her-ticket04-session-store-"))
            {
                return;
            }
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn legacy_session_store_wrappers_load_and_save_only_scoped_ab_paths() {
        let root_a = SessionStoreFixtureRoot::new("a");
        let root_b = SessionStoreFixtureRoot::new("b");
        let path_a = root_a.session_file();
        let path_b = root_b.session_file();
        let bytes_a = br#"{
            "workspace_id":"workspace-a",
            "sessions":{"group:70001":{"sessionId":"sid-a","workspaceId":"session-workspace-a"}}
        }"#
        .to_vec();
        let bytes_b = br#"{
            "workspace_id":"workspace-b",
            "sessions":{"private:90002":{"sessionId":"sid-b","workspaceId":"session-workspace-b"}}
        }"#
        .to_vec();
        std::fs::write(&path_a, &bytes_a).unwrap();
        std::fs::write(&path_b, &bytes_b).unwrap();

        let mut store_a = legacy_session_file_scope::with_path(path_a.clone(), SessionStore::load);
        assert_eq!(store_a.workspace_id, "workspace-a");
        assert_eq!(store_a.sessions.len(), 1);
        assert_eq!(store_a.sessions["group:70001"].session_id, "sid-a");
        assert!(!store_a.sessions.contains_key("private:90002"));
        assert_eq!(std::fs::read(&path_a).unwrap(), bytes_a);
        assert_eq!(std::fs::read(&path_b).unwrap(), bytes_b);

        store_a.workspace_id = "workspace-a-updated".to_string();
        store_a.sessions.insert(
            "private:70002".to_string(),
            SessionRef {
                session_id: "sid-a-extra".to_string(),
                workspace_id: "session-workspace-a-extra".to_string(),
            },
        );
        legacy_session_file_scope::with_path(path_a.clone(), || store_a.save()).unwrap();
        let bytes_a_after_save = std::fs::read(&path_a).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes_a_after_save).unwrap(),
            json!({
                "workspace_id": "workspace-a-updated",
                "sessions": {
                    "group:70001": { "sessionId": "sid-a", "workspaceId": "session-workspace-a" },
                    "private:70002": { "sessionId": "sid-a-extra", "workspaceId": "session-workspace-a-extra" }
                }
            })
        );
        assert_eq!(std::fs::read(&path_b).unwrap(), bytes_b);

        let mut store_b = legacy_session_file_scope::with_path(path_b.clone(), SessionStore::load);
        assert_eq!(store_b.workspace_id, "workspace-b");
        assert_eq!(store_b.sessions.len(), 1);
        assert_eq!(store_b.sessions["private:90002"].session_id, "sid-b");
        assert!(!store_b.sessions.contains_key("group:70001"));
        assert_eq!(std::fs::read(&path_a).unwrap(), bytes_a_after_save);
        assert_eq!(std::fs::read(&path_b).unwrap(), bytes_b);

        store_b.workspace_id = "workspace-b-updated".to_string();
        store_b.sessions.insert(
            "group:90003".to_string(),
            SessionRef {
                session_id: "sid-b-extra".to_string(),
                workspace_id: "session-workspace-b-extra".to_string(),
            },
        );
        legacy_session_file_scope::with_path(path_b.clone(), || store_b.save()).unwrap();
        let bytes_b_after_save = std::fs::read(&path_b).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes_b_after_save).unwrap(),
            json!({
                "workspace_id": "workspace-b-updated",
                "sessions": {
                    "private:90002": { "sessionId": "sid-b", "workspaceId": "session-workspace-b" },
                    "group:90003": { "sessionId": "sid-b-extra", "workspaceId": "session-workspace-b-extra" }
                }
            })
        );
        assert_eq!(std::fs::read(&path_a).unwrap(), bytes_a_after_save);
    }

    struct RestoreDshHome(Option<std::ffi::OsString>);

    impl RestoreDshHome {
        fn set_to(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("DSH_HOME");
            std::env::set_var("DSH_HOME", path);
            Self(previous)
        }
    }

    impl Drop for RestoreDshHome {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                std::env::set_var("DSH_HOME", previous);
            } else {
                std::env::remove_var("DSH_HOME");
            }
        }
    }

    #[derive(Debug)]
    struct FakeDshRequest {
        path: String,
        cookie: Option<String>,
        body: String,
    }

    fn fake_dsh_accept(
        listener: &std::net::TcpListener,
        timeout: std::time::Duration,
    ) -> Option<(std::net::TcpStream, FakeDshRequest)> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Accepted sockets inherit the listener's nonblocking mode on Windows.
                    // Read the complete fixture request under the existing deadline instead.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let body_start;
                    let body_len;
                    loop {
                        let count = std::io::Read::read(&mut stream, &mut chunk).unwrap();
                        assert!(count > 0, "fake DSH client closed before sending a full request");
                        raw.extend_from_slice(&chunk[..count]);
                        let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let header = String::from_utf8_lossy(&raw[..header_end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if raw.len() >= header_end + 4 + length {
                            body_start = header_end + 4;
                            body_len = length;
                            break;
                        }
                    }
                    let header_end = body_start - 4;
                    let header = String::from_utf8_lossy(&raw[..header_end]);
                    let first_line = header.lines().next().unwrap_or("");
                    let path = first_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    assert_eq!(raw.len(), body_start + body_len, "unexpected bytes after request body");
                    let cookie = header.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("cookie").then(|| value.trim().to_string())
                    });
                    let body = String::from_utf8_lossy(&raw[body_start..]).to_string();
                    return Some((stream, FakeDshRequest { path, cookie, body }));
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(10035) =>
                {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("fake DSH accept failed: {error}"),
            }
        }
    }

    fn fake_dsh_respond(
        stream: &mut std::net::TcpStream,
        request: &FakeDshRequest,
        status: u16,
        value: serde_json::Value,
    ) {
        use std::io::Write as _;
        if status == 401 {
            stream
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            return;
        }
        let body = serde_json::json!({
            "type": "server-response",
            "rpcId": serde_json::from_str::<Value>(&request.body).unwrap()["rpcId"],
            "result": { "ok": true, "value": value }
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }


    #[test]
    fn hf85_port_ack_rpc_identity_and_explicit_request_id() {
        use std::io::Write as _;
        use std::net::TcpListener;
        for bounded in [false, true] {
            for explicit in [false, true] {
                let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                listener.set_nonblocking(true).unwrap();
                let root = fake_runtime_root("hf85-prompt-id");
                write_selected_secret(&root, [0x41; 32]);
                let (_context, pipeline) = pipeline_for_fake_dsh(&root, listener.local_addr().unwrap().port());
                let server = std::thread::spawn(move || {
                    let (mut stream, request) = fake_dsh_accept(&listener, std::time::Duration::from_secs(5)).unwrap();
                    let request: Value = serde_json::from_str(&request.body).unwrap();
                    let body = serde_json::json!({"type":"server-response", "rpcId":request["rpcId"],
                        "result":{"ok":true,"value":{"accepted":true}}}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    request
                });
                let mut payload = serde_json::json!({"sessionId":"s", "mode":"queue", "content":[{"type":"text","text":"synthetic briefing"}]});
                if explicit { payload["requestId"] = serde_json::json!("explicit-prompt-id"); }
                let result = if bounded {
                    pipeline.rpc.call_with_cancellation_as("session/prompt", payload, "hf85-prompt-request-id",
                        std::time::Instant::now() + std::time::Duration::from_secs(5), &|| false)
                } else {
                    pipeline.rpc.call_with_rpc_id("session/prompt", payload, "hf85-prompt-request-id")
                };
                let request = server.join().unwrap();
                std::fs::remove_dir_all(root).unwrap();
                assert_eq!(result.unwrap()["accepted"], true);
                assert_eq!(request["rpcId"], "hf85-prompt-request-id");
                assert_eq!(request["payload"]["args"]["request"]["requestId"],
                    if explicit { "explicit-prompt-id" } else { "hf85-prompt-request-id" },
                    "prompt correlation diverged: bounded={bounded}, explicit={explicit}");
            }
        }
    }

    fn selected_credentials_path(root: &std::path::Path) -> PathBuf {
        root.join("user/dsh/.credentials.yaml")
    }

    fn write_selected_secret(root: &std::path::Path, secret: [u8; 32]) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let encoded = URL_SAFE_NO_PAD.encode(secret);
        let path = selected_credentials_path(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("secret: \"{encoded}\"\n")).unwrap();
        encoded
    }

    fn cookie_is_signed_for(
        cookie: Option<&str>,
        secret: &[u8; 32],
        expected_authority: &str,
    ) -> bool {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let Some(cookie) = cookie else { return false };
        let Some((_name, value)) = cookie.split_once('=') else { return false };
        let mut parts = value.split('.');
        if parts.next() != Some("v1") {
            return false;
        }
        let Some(body) = parts.next() else { return false };
        let Some(signature) = parts.next() else { return false };
        if parts.next().is_some() {
            return false;
        }
        let Ok(payload_bytes) = URL_SAFE_NO_PAD.decode(body) else { return false };
        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) else {
            return false;
        };
        if payload.get("authority").and_then(|value| value.as_str()) != Some(expected_authority) {
            return false;
        }
        let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else { return false };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else { return false };
        mac.update(body.as_bytes());
        mac.verify_slice(&signature).is_ok()
    }
    fn pipeline_for_fake_dsh(
        root: &std::path::Path,
        port: u16,
    ) -> (RuntimeContext, Pipeline) {
        std::fs::write(
            root.join("dependencies.json"),
            format!(
                r#"{{"dsh":"http://127.0.0.1:{port}","model":"http://127.0.0.1:43202","onebot":"http://127.0.0.1:43203"}}"#
            ),
        )
        .unwrap();
        let context = RuntimeContext::from_root(root).unwrap();
        let pipeline = Pipeline::new_with_runtime(&context, &serde_json::json!({}), true).unwrap();
        (context, pipeline)
    }

    fn fake_runtime_root(tag: &str) -> PathBuf {
        let (root, _context) = isolated_fixture(tag);
        root
    }

    fn isolated_fixture(tag: &str) -> (PathBuf, RuntimeContext) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her ticket04 pipeline fixture 中文 {tag} {} {stamp}",
            std::process::id()
        )));
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("resources")).unwrap();
        // The fixture conversation `group:42` is allowed (JS `ensureSession` refuses others).
        std::fs::write(
            root.join("config/config.json"),
            r#"{"consoleToken":"fixture-token","allow":{"groups":[42]}}"#,
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{}}"#)
            .unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43201","model":"http://127.0.0.1:43202","onebot":"http://127.0.0.1:43203"}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (root, context)
    }

    fn cli_recall_fixture(tag: &str) -> (PathBuf, RuntimeContext, Pipeline, rusqlite::Connection) {
        let (root, context) = isolated_fixture(tag);
        let conn = crate::memory::open_ready_for_runtime(&context).unwrap();
        crate::memory::write_memory(&conn, &crate::memory::MemoryInput {
            owner_type: "session", owner_id: "group:42", scope: "group", kind: "fact", subject: None,
            content: "recalled fixture memory", tags: None, source: "fixture", importance: 0.5,
        }).unwrap();
        write_selected_secret(&root, [0x71; 32]);
        let (context, mut pipe) = pipeline_for_fake_dsh(&root, 1);
        pipe.compat_recall = vec![1];
        (root, context, pipe, conn)
    }

    fn recall_hits_of(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT SUM(hits) FROM memory", [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_cli_fallback_that_fails_delivers_nothing_and_counts_no_recall_hit() {
        let (root, _context, mut pipe, conn) = cli_recall_fixture("cli-recall-failed");
        let cfg = json!({"backends":{"failoverFallback":true,"list":[{"id":"fixture-cli","kind":"cli","cmd":"definitely-not-a-real-command-xyz"}]}});
        assert!(pipe.cli_fallback(&cfg, "group:42", "prompt", 5).is_err());
        assert!(pipe.cli_fallback(&json!({}), "group:42", "prompt", 5).is_err(), "the fallback is off by default");
        assert_eq!(recall_hits_of(&conn), 0);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hf136_fix1_intent_write_failure_still_stops_and_revokes() {
        let root = fake_runtime_root("hf136-fix1-intent-failure");
        write_selected_secret(&root, [0x13; 32]);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (context, mut pipe) = pipeline_for_fake_dsh(&root, listener.local_addr().unwrap().port());
        let key = "group:42";
        crate::session_map::bind(&context, key, "sid-136", None).unwrap();
        let before = crate::social_state::ensure_or_rotate(&context, key, false).unwrap();
        let epoch = crate::send_outbox::token_epoch(&before);
        let turn = crate::turn_ledger::Recorder::begin(&context, key, "dsh", "test", &epoch, &[], None).unwrap();
        turn.session("sid-136").unwrap();
        let server = std::thread::spawn(move || {
            let mut methods = vec![];
            while let Some((mut stream, req)) = fake_dsh_accept(&listener, std::time::Duration::from_secs(2)) {
                methods.push(req.path.trim_start_matches("/api/").to_string());
                fake_dsh_respond(&mut stream, &req, 200, serde_json::json!({"values":[]}));
                if methods.len() == 2 { break; }
            }
            methods
        });
        let failed = std::rc::Rc::new(std::cell::Cell::new(false));
        let injected = failed.clone();
        crate::runtime::APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move |stage| {
            if stage == "open" && !injected.replace(true) {
                return Err(std::io::Error::from_raw_os_error(33));
            }
            Ok(())
        })));
        let result = pipe.cancel_with_reason(key, "turnTimeout");
        crate::runtime::APPEND_IO_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
        let after = crate::social_state::token(&context, key).unwrap().unwrap();
        let methods = server.join().unwrap();
        let records = crate::turn_ledger::read_records(&context).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(failed.get(), "must exercise intent append failure");
        assert_ne!(after, before, "the cap must revoke authority even when intent persistence fails");
        assert_eq!(methods, ["session/projections", "session/cancel"]);
        assert_eq!(result.unwrap().token.as_deref(), Some(after.as_str()));
        assert!(records.iter().any(|r| r["k"] == "ended" && r["reason"] == "turnTimeout"), "retain terminal evidence when that write is available");
    }

    #[test]
    fn hf129_fix1_pausing_an_unknown_turn_publishes_its_rotated_epoch() {
        let root = fake_runtime_root("hf129-owned-pause");
        write_selected_secret(&root, [0x11;32]);
        let listener = std::net::TcpListener::bind(("127.0.0.1",0)).unwrap();
        let (context, mut pipe) = pipeline_for_fake_dsh(&root, listener.local_addr().unwrap().port());
        let key = "group:42";
        let before = crate::social_state::ensure_or_rotate(&context,key,false).unwrap();
        let minted: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        pipe.share_minted_epoch(minted.clone());
        let result = pipe.pause_unknown(key,"turn-unverified","synthetic unknown collector outcome");
        let after = crate::social_state::token(&context,key).unwrap().unwrap();
        let published = minted.lock().unwrap().clone();
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.is_err());
        assert_ne!(after,before);
        assert_eq!(published.as_deref(),Some(after.as_str()),"the turn must publish every token it owns, including unknown outcomes");
    }

    #[test]
    fn missing_candidate_session_store_is_a_noncreating_empty_store() {
        let (root, context) = isolated_fixture("missing-session");
        let store = SessionStore::load_for_runtime(&context).unwrap();
        assert!(store.sessions.is_empty());
        assert!(store.workspace_id.is_empty());
        assert!(!root.join("data/sessions.json").exists());
        assert!(!root.join("user/dsh").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_candidate_session_store_stops_pipeline_before_live_setup() {
        let (root, context) = isolated_fixture("corrupt-session");
        let session_file = root.join("data/sessions.json");
        let original = b"{this is deliberately invalid";
        std::fs::write(&session_file, original).unwrap();

        let error = match Pipeline::new_with_runtime(&context, &serde_json::json!({}), true) {
            Ok(_) => panic!("corrupt selected session data must not initialize a live pipeline"),
            Err(error) => error,
        };
        assert!(error.contains("候选会话映射 JSON 损坏"), "unexpected error: {error}");
        assert_eq!(std::fs::read(&session_file).unwrap(), original);
        assert!(
            !root.join("data/agents").exists(),
            "corrupt candidate state must not create a fresh workspace"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isolated_401_relogin_uses_rotated_selected_secret_and_preserves_other_context() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use std::net::TcpListener;

        let _env_lock = DSH_HOME_TEST_LOCK.lock().unwrap();
        let ambient_root = fake_runtime_root("ambient-secret");
        let ambient_secret = [0xa5; 32];
        write_selected_secret(&ambient_root, ambient_secret);
        let _restore_dsh_home = RestoreDshHome::set_to(&ambient_root.join("user/dsh"));
        let ambient_original = std::fs::read(selected_credentials_path(&ambient_root)).unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let root_a = fake_runtime_root("refresh-a");
        let old_secret = [0x11; 32];
        let new_secret = [0x22; 32];
        let old_encoded = write_selected_secret(&root_a, old_secret);
        let new_encoded = URL_SAFE_NO_PAD.encode(new_secret);
        let (context_a, mut pipeline_a) = pipeline_for_fake_dsh(&root_a, port);
        let old_cookie = pipeline_a.rpc.cookie();

        let root_b = fake_runtime_root("refresh-b");
        let b_secret = [0x33; 32];
        let _b_encoded = write_selected_secret(&root_b, b_secret);
        let (_context_b, pipeline_b) = pipeline_for_fake_dsh(&root_b, port);
        let b_cookie = pipeline_b.rpc.cookie();
        let b_credentials = std::fs::read(selected_credentials_path(&root_b)).unwrap();

        let rotate_path = selected_credentials_path(&root_a);
        let expected_old_cookie = old_cookie.clone();
        let rotated_credentials = new_encoded.clone();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            // A 401 now heals inside the call (one re-authentication and one repeat of that request), so the
            // verifying `agentPresets/list` probe of the old relogin path no longer appears between the two.
            for (index, expected_path) in [
                "/api/workspace/create",
                "/api/session/create",
                "/api/session/create",
                "/api/workspace/rename",
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, request) = fake_dsh_accept(&listener, std::time::Duration::from_secs(5))
                    .expect("pipeline should call the selected fake DSH endpoint");
                assert_eq!(request.path, expected_path, "request {index}: {request:?}");
                if index <= 1 {
                    assert_eq!(request.cookie.as_deref(), Some(expected_old_cookie.as_str()));
                } else {
                    assert_ne!(request.cookie.as_deref(), Some(expected_old_cookie.as_str()));
                }
                if index == 1 {
                    std::fs::write(&rotate_path, format!("secret: \"{rotated_credentials}\"\n")).unwrap();
                    fake_dsh_respond(&mut stream, &request, 401, serde_json::Value::Null);
                } else {
                    let value = match request.path.as_str() {
                        "/api/workspace/create" => serde_json::json!({
                            "workspace": { "workspaceId": "ws-fixture-a" }
                        }),
                        "/api/agentPresets/list" => serde_json::json!([]),
                        "/api/session/create" => serde_json::json!({ "sessionId": "sid-fixture-a" }),
                        _ => serde_json::json!({}),
                    };
                    fake_dsh_respond(&mut stream, &request, 200, value);
                }
                seen.push(request);
            }
            seen
        });

        let sid = pipeline_a.ensure_session("group:42").unwrap();
        assert_eq!(sid, "sid-fixture-a");
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].cookie.as_deref(), Some(old_cookie.as_str()));
        assert_eq!(seen[1].cookie.as_deref(), Some(old_cookie.as_str()));
        let authority = format!("127.0.0.1:{port}");
        assert!(cookie_is_signed_for(seen[0].cookie.as_deref(), &old_secret, &authority));
        assert!(cookie_is_signed_for(seen[1].cookie.as_deref(), &old_secret, &authority));
        let refreshed_cookie = seen[2].cookie.as_ref().unwrap();
        assert!(cookie_is_signed_for(Some(refreshed_cookie), &new_secret, &authority));
        assert!(!cookie_is_signed_for(Some(refreshed_cookie), &old_secret, &authority));
        assert!(!cookie_is_signed_for(Some(refreshed_cookie), &ambient_secret, &authority));
        assert!(!cookie_is_signed_for(Some(refreshed_cookie), &b_secret, &authority));
        assert_ne!(refreshed_cookie, &old_cookie);
        assert_eq!(seen[3].cookie.as_ref(), Some(refreshed_cookie));
        assert_eq!(pipeline_a.rpc.cookie(), *refreshed_cookie);
        assert_eq!(std::fs::read(selected_credentials_path(&root_a)).unwrap(), format!("secret: \"{new_encoded}\"\n").as_bytes());
        assert!(!std::fs::read(root_a.join("data/sessions.json")).unwrap().is_empty());
        assert_eq!(std::fs::read(selected_credentials_path(&root_b)).unwrap(), b_credentials);
        assert_eq!(pipeline_b.rpc.cookie(), b_cookie, "refresh in context A must not mutate context B");
        assert!(cookie_is_signed_for(Some(&b_cookie), &b_secret, &authority));
        assert!(!cookie_is_signed_for(Some(&b_cookie), &new_secret, &authority));
        assert_eq!(std::fs::read(selected_credentials_path(&ambient_root)).unwrap(), ambient_original);
        assert_eq!(old_encoded, URL_SAFE_NO_PAD.encode([0x11; 32]));
        let canonical_root_a = std::fs::canonicalize(&root_a).unwrap();
        assert_eq!(context_a.runtime_root(), Some(canonical_root_a.as_path()));

        drop(pipeline_a);
        drop(pipeline_b);
        std::fs::remove_dir_all(root_a).unwrap();
        std::fs::remove_dir_all(root_b).unwrap();
        std::fs::remove_dir_all(ambient_root).unwrap();
    }

    /// Ticket 25 AC3: every RPC heals a 401 once, `session/prompt` included. The 401 is refused before the request is
    /// processed, so the same request (same JSON-RPC id) is repeated once with a cookie signed by the credentials
    /// as they are now.
    #[test]
    fn ticket25_a_401_on_session_prompt_reauthenticates_once_and_repeats_the_same_request_once() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = fake_runtime_root("t25-prompt-401");
        write_selected_secret(&root, [0x41; 32]);
        let (_context, pipeline) = pipeline_for_fake_dsh(&root, port);
        let rotate = selected_credentials_path(&root);
        let new_secret = [0x42; 32];
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for index in 0..2 {
                let (mut stream, request) = fake_dsh_accept(&listener, std::time::Duration::from_secs(5))
                    .expect("the prompt must be sent twice: refused, then repeated");
                assert_eq!(request.path, "/api/session/prompt");
                if index == 0 {
                    std::fs::write(&rotate, format!("secret: \"{}\"\n", URL_SAFE_NO_PAD.encode(new_secret))).unwrap();
                    fake_dsh_respond(&mut stream, &request, 401, serde_json::Value::Null);
                } else {
                    fake_dsh_respond(&mut stream, &request, 200, serde_json::json!({"accepted": true}));
                }
                seen.push(request);
            }
            // nothing else may arrive
            assert!(fake_dsh_accept(&listener, std::time::Duration::from_millis(700)).is_none(), "a third request was made");
            seen
        });
        let answer = pipeline
            .rpc
            .call_with_rpc_id("session/prompt", serde_json::json!({"sessionId": "s", "mode": "queue", "content": []}), "rpc-fixed-25")
            .expect("the repeated request is accepted");
        assert_eq!(answer["accepted"], true);
        let seen = server.join().unwrap();
        let authority = format!("127.0.0.1:{port}");
        assert!(cookie_is_signed_for(seen[0].cookie.as_deref(), &[0x41; 32], &authority));
        assert!(cookie_is_signed_for(seen[1].cookie.as_deref(), &new_secret, &authority), "repeat uses the re-read credentials");
        let rpc_id = |r: &FakeDshRequest| serde_json::from_str::<Value>(&r.body).unwrap()["rpcId"].clone();
        assert_eq!(rpc_id(&seen[0]), "rpc-fixed-25");
        assert_eq!(rpc_id(&seen[0]), rpc_id(&seen[1]), "the repeat is the same request");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ticket25_a_second_401_is_not_retried_again() {
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = fake_runtime_root("t25-401-bounded");
        write_selected_secret(&root, [0x51; 32]);
        let (_context, pipeline) = pipeline_for_fake_dsh(&root, port);
        let server = std::thread::spawn(move || {
            let mut count = 0;
            while let Some((mut stream, request)) = fake_dsh_accept(&listener, std::time::Duration::from_millis(1500)) {
                assert_eq!(request.path, "/api/settings/describe");
                fake_dsh_respond(&mut stream, &request, 401, serde_json::Value::Null);
                count += 1;
            }
            count
        });
        let error = pipeline.rpc.call("settings/describe", serde_json::json!({})).unwrap_err();
        assert!(error.contains("401"), "{error}");
        assert_eq!(server.join().unwrap(), 2, "one request and exactly one repeat, never a loop");
        std::fs::remove_dir_all(root).unwrap();
    }

    fn fake_dsh_refuse(stream: &mut std::net::TcpStream, request: &FakeDshRequest, code: &str, message: &str) {
        use std::io::Write as _;
        let body = serde_json::json!({
            "type": "server-response",
            "rpcId": serde_json::from_str::<Value>(&request.body).unwrap()["rpcId"],
            "result": { "ok": false, "error": { "code": code, "message": message } }
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    fn selected(body: &str) -> String {
        let v: Value = serde_json::from_str(body).unwrap();
        format!("{}/{}", v["payload"]["args"]["request"]["provider"].as_str().unwrap_or("?"), v["payload"]["args"]["request"]["model"].as_str().unwrap_or("?"))
    }

    /// Ticket 25 AC3: the frozen JS applies the model once per session (`modelAppliedSessions`); a second turn on the same
    /// session sends no `session/selectModel`.
    #[test]
    fn ticket25_the_model_is_selected_once_per_session_and_not_again_on_the_next_turn() {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = fake_runtime_root("t25-select-once");
        write_selected_secret(&root, [0x61; 32]);
        let (_context, mut pipeline) = pipeline_for_fake_dsh(&root, port);
        let chain = failover::model_chain(&json!({"dsh": {"provider": "pa", "model": "ma", "fallbacks": [{"provider": "pb", "model": "mb"}]}}));
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            while let Some((mut stream, request)) = fake_dsh_accept(&listener, std::time::Duration::from_millis(1500)) {
                assert_eq!(request.path, "/api/session/selectModel");
                fake_dsh_respond(&mut stream, &request, 200, serde_json::json!({"selected": {}}));
                bodies.push(request.body);
            }
            bodies
        });
        let cfg = json!({});
        assert_eq!(pipeline.apply_model(&cfg, "group:42", "sid-once", &chain).unwrap(), 0);
        assert_eq!(pipeline.apply_model(&cfg, "group:42", "sid-once", &chain).unwrap(), 0, "second turn: no request");
        let bodies = server.join().unwrap();
        assert_eq!(bodies.len(), 1, "exactly one selectModel for the session: {bodies:?}");
        assert_eq!(selected(&bodies[0]), "pa/ma");
        let v: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(v["payload"]["args"]["request"]["reasoningEffort"], "max", "default effort is max, as JS");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Ticket 25 AC3: after a failover step the next selection starts at the degraded candidate (never the broken primary),
    /// and a primary that refuses twice is left after exactly two attempts (JS `ensureChatModel`).
    #[test]
    fn ticket25_selection_starts_at_the_sticky_candidate_and_gives_each_candidate_two_attempts() {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = fake_runtime_root("t25-select-sticky");
        write_selected_secret(&root, [0x62; 32]);
        let (_context, mut pipeline) = pipeline_for_fake_dsh(&root, port);
        let chain = failover::model_chain(&json!({"dsh": {"provider": "pa", "model": "ma", "fallbacks": [{"provider": "pb", "model": "mb"}, {"provider": "pc", "model": "mc"}]}}));
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            while let Some((mut stream, request)) = fake_dsh_accept(&listener, std::time::Duration::from_millis(4000)) {
                let which = selected(&request.body);
                if which == "pa/ma" {
                    fake_dsh_refuse(&mut stream, &request, "not-found", "model not found");
                } else {
                    fake_dsh_respond(&mut stream, &request, 200, serde_json::json!({"selected": {}}));
                }
                seen.push(which);
            }
            seen
        });
        let cfg = json!({});
        // 1) primary refuses twice (1 s pause between), the first fallback accepts
        assert_eq!(pipeline.apply_model(&cfg, "group:42", "sid-sticky", &chain).unwrap(), 1);
        // 2) a failed turn on candidate 1: the failover step records candidate 2; selecting again starts there
        {
            let mut state = pipeline.failover_state();
            assert_eq!(state.plan("group:42", "sid-sticky", chain.len(), false), failover::FailoverAction::Retry { idx: 2 });
            state.on_failover("group:42", "sid-sticky", 2);
        }
        assert_eq!(pipeline.apply_model(&cfg, "group:42", "sid-sticky", &chain).unwrap(), 2);
        let seen = server.join().unwrap();
        assert_eq!(seen, vec!["pa/ma", "pa/ma", "pb/mb", "pc/mc"], "two attempts on the primary, then the fallback, then straight to the sticky candidate");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Ticket 25 AC3 (decision Q5): the retry cap is per conversation and lives across messages, one memory per runtime.
    #[test]
    fn ticket25_the_failover_retry_cap_holds_across_messages_of_one_conversation() {
        let (_root, context) = isolated_fixture("t25-cap");
        let state = failover_state_for(&context);
        let mut s = state.lock().unwrap();
        s.mark_applied("sid-cap", 0);
        assert_eq!(s.plan("group:42", "sid-cap", 3, false), failover::FailoverAction::Retry { idx: 1 });
        s.on_failover("group:42", "sid-cap", 1);
        s.mark_applied("sid-cap", 1);
        assert_eq!(s.plan("group:42", "sid-cap", 3, false), failover::FailoverAction::Retry { idx: 2 });
        s.on_failover("group:42", "sid-cap", 2);
        s.mark_applied("sid-cap", 2);
        drop(s);
        // another pipeline of the same runtime (the next message) sees the same budget
        let again = failover_state_for(&context);
        assert!(Arc::ptr_eq(&state, &again), "one failover memory per runtime, shared by every turn's pipeline");
        assert_eq!(again.lock().unwrap().retries("group:42"), 2);
    }

    /// Ticket 25 review 11: without a ledger turn (a legacy context) an error of `session/prompt` is not a refusal unless
    /// DSH said so: a timeout or a dropped connection leaves the outcome unknown, is checked with DSH and never fails over.
    #[test]
    fn ticket25_a_transport_error_without_a_ledger_turn_is_checked_not_assumed_refused() {
        for has_turn in [true, false] {
            assert!(prompt_error_needs_probe(has_turn, "HTTP 连接失败：session/prompt"), "has_turn={has_turn}");
            assert!(prompt_error_needs_probe(has_turn, "响应不是 JSON（session/prompt）：eof"), "has_turn={has_turn}");
            assert!(!prompt_error_needs_probe(has_turn, "session/prompt 被拒：invalid: bad"), "DSH refused: definite");
            assert!(!prompt_error_needs_probe(has_turn, "401 未授权：session/prompt"), "a 401 is refused before processing");
        }
    }

    /// Ticket 25 review (minor): only the bridge's own cancellation wording is a cancellation; a model or DSH error that happens
    /// to contain "丢弃" or "已变化" still raises its alert.
    #[test]
    fn ticket25_cancellation_errors_are_recognised_by_the_bridges_own_wording_only() {
        for real in [
            "回合已取消：会话映射已变化（重置或替换）",
            "暂缓唤醒：该对话存在尚未收尾的暂停回合",
            "模式已变化",
            "会话或白名单已变化",
            "会话令牌已变化（重置、取消或被替代）",
            "创建对话过程中发生重置或权限变动，放弃新建会话",
            "模式已切换，丢弃排队的指令",
            "会话已被重置或授权变更，丢弃排队的指令",
        ] {
            assert!(is_cancellation_error(real), "{real}");
        }
        for failure in [
            "回合失败（upstream 429 请丢弃旧的请求）",
            "provider error: 配额已变化",
            "HTTP 连接失败：session/prompt",
        ] {
            assert!(!is_cancellation_error(failure), "{failure}");
        }
    }

    #[test]
    fn reserved2_collector_budget_tracks_effective_observation_and_cap() {
        // A ceiling below the cap is the cap itself: default cap 515000 (observation 300000 +
        // quiet extension 125000 + 90000) plus 30000.
        assert_eq!(reserved2_turn_timeout(&serde_json::json!({"socialV2":{"wake":{"turnCeilingMs":1}}})).as_millis(), 545000);
        assert_eq!(reserved2_turn_timeout(&serde_json::json!({"socialV2":{"wake":{"turnCeilingMs":1,"preSleepWaitMs":450000}}})).as_millis(), 695000);
        assert_eq!(reserved2_turn_timeout(&serde_json::json!({"socialV2":{"wake":{"turnCeilingMs":1,"turnHardCapMs":500123}}})).as_millis(), 530123);
        assert_eq!(reserved2_turn_timeout(&serde_json::json!({"socialV2":{"wake":{"turnCeilingMs":1,"turnHardCapMs":"bad"}}})).as_millis(), 545000);
        // An engaged turn may run until its absolute ceiling (default 30 minutes); the collector covers it.
        assert_eq!(reserved2_turn_timeout(&serde_json::json!({})).as_millis(), 1_830_000);
        assert_eq!(reserved2_turn_timeout(&serde_json::json!({"socialV2":{"wake":{"turnCeilingMs":2_400_000}}})).as_millis(), 2_430_000);
        for invalid in [serde_json::json!("bad"), serde_json::json!(0), serde_json::json!(-5), serde_json::json!(1.5)] {
            assert_eq!(reserved2_turn_timeout(&serde_json::json!({"socialV2":{"wake":{"turnCeilingMs":invalid}}})).as_millis(), 1_830_000);
        }
        assert_eq!(turn_timeout_secs(&serde_json::json!({})),180,"chat keeps its own collector setting");
    }

    /// Ticket 25 review 7: the 401 re-authentication is recorded against the runtime the call belongs to, not the process default.
    #[test]
    fn ticket25_a_reauthenticated_401_is_recorded_on_the_calls_own_run_record() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = fake_runtime_root("t25-401-record");
        write_selected_secret(&root, [0x43; 32]);
        let (context, pipeline) = pipeline_for_fake_dsh(&root, port);
        let rotate = selected_credentials_path(&root);
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, request) = fake_dsh_accept(&listener, std::time::Duration::from_secs(5)).expect("refused, then repeated");
                if index == 0 {
                    std::fs::write(&rotate, format!("secret: \"{}\"\n", URL_SAFE_NO_PAD.encode([0x44u8; 32]))).unwrap();
                    fake_dsh_respond(&mut stream, &request, 401, serde_json::Value::Null);
                } else {
                    fake_dsh_respond(&mut stream, &request, 200, serde_json::json!({"accepted": true}));
                }
            }
        });
        pipeline.rpc.call("session/prompt", serde_json::json!({"sessionId": "s", "mode": "queue", "content": []})).expect("healed");
        server.join().unwrap();
        let page = crate::recovery_log::page(&context, &crate::run_record::Query::default()).unwrap();
        assert!(
            page.records.iter().any(|r| r.event == "reauth-401"),
            "the re-authentication is on this runtime's record: {:?}",
            page.records.iter().map(|r| &r.event).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Ticket 25 review (minor, JS `modelAppliedSessions.delete` on reset, mode switch and revocation): when a session leaves the
    /// conversation map, the failover memory forgets it, so nothing about a retired session lingers.
    #[test]
    fn ticket25_retiring_a_session_from_the_map_forgets_its_failover_memory() {
        let (root, context) = isolated_fixture("t25-forget-retire");
        crate::session_map::bind(&context, "group:42", "sid-retired", None).unwrap();
        {
            let state = failover_state_for(&context);
            let mut s = state.lock().unwrap();
            s.note_chain("sid-retired", "chain-a");
            s.mark_applied("sid-retired", 2);
            assert!(!s.needs_apply("sid-retired"));
            assert_eq!(s.selected_index("sid-retired", 3), 2);
        }
        assert!(crate::session_map::retire_if(&context, "group:42", "sid-retired").unwrap());
        let state = failover_state_for(&context);
        let s = state.lock().unwrap();
        assert!(s.needs_apply("sid-retired"), "a retired session is selected again if it ever returns");
        assert_eq!(s.selected_index("sid-retired", 3), 0, "its degraded candidate is forgotten");
        drop(s);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Ticket 24 S3: an access change (revocation or whole reset) that lands while DSH is creating a
    /// conversation's session must not let the new session be bound; it is archived instead (JS
    /// `ensureSession` re-checks epoch, policy and mode after creation).
    #[tokio::test]
    async fn ticket24_a_session_created_across_a_revocation_or_reset_is_archived_not_bound() {
        use std::net::TcpListener;
        for (tag, path, body) in [
            ("t24-create-revoke", "/api/whitelist", serde_json::json!({"allow": {"groups": []}, "confirm": true})),
            ("t24-create-reset", "/api/socialV2/reset", serde_json::json!({})),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let root = fake_runtime_root(tag);
            std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","allow":{"groups":[42]}}"#).unwrap();
            std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
            std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{"group:42":{"agentToken":"fixture-group-token-abcdefghij"}}}"#).unwrap();
            write_selected_secret(&root, [0x24; 32]);
            let (context, mut pipeline) = pipeline_for_fake_dsh(&root, port);
            let server_context = context.clone();
            let server = std::thread::spawn(move || {
                let mut seen = Vec::new();
                while let Some((mut stream, request)) = fake_dsh_accept(&listener, std::time::Duration::from_millis(1500)) {
                    let value = match request.path.as_str() {
                        "/api/workspace/create" => serde_json::json!({ "workspace": { "workspaceId": "ws-t24" } }),
                        "/api/session/create" => {
                            // The administrator acts while DSH is still creating the session.
                            let (status, reply) = crate::console::admin_request(&server_context, "POST", path, Some(&body));
                            assert_eq!(status, 200, "{path}: {reply}");
                            serde_json::json!({ "sessionId": "sid-created-across-change" })
                        }
                        _ => serde_json::json!({}),
                    };
                    fake_dsh_respond(&mut stream, &request, 200, value);
                    seen.push(request.path);
                }
                seen
            });
            let cfg = context.load_config().unwrap();
            let result = pipeline.ask_with_failover("group:42", "fixture prompt", 5, &cfg).await;
            let seen = server.join().unwrap();
            assert!(is_dsh_access_refusal(&result.unwrap_err()), "{tag}: the authority change must cancel admission");
            assert!(take_model_error(&context, "group:42").is_none() && !pipeline.failure_reported);
            assert!(!seen.iter().any(|p| p == "/api/session/prompt" || p == "/api/session/selectModel"));
            assert_eq!(crate::session_map::lookup(&context, "group:42").unwrap(), None, "{tag}: the new session was bound");
            assert!(seen.iter().any(|p| p == "/api/workspace/archiveSession"), "{tag}: the new session must be archived: {seen:?}");
            drop(pipeline);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn t48_session_admission_is_not_a_model_error() {
        use std::io::Write as _;
        use std::net::TcpListener;
        for (allow, deny, admitted) in [
            (serde_json::json!([]), serde_json::json!([]), false),
            (serde_json::json!([42]), serde_json::json!([42]), false),
            (serde_json::json!([42]), serde_json::json!([]), true),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let root = fake_runtime_root("t48-session-admission");
            let config = serde_json::json!({"consoleToken":"fixture-token","ownerQQ":"7001",
                "allow":{"groups":allow},"deny":{"groups":deny},"allowAllWhenEmpty":true});
            std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
            std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
            write_selected_secret(&root, [0x48; 32]);
            let dependencies = serde_json::json!({
                "dsh":format!("http://127.0.0.1:{port}"),
                "model":format!("http://127.0.0.1:{port}"),
                "onebot":format!("http://127.0.0.1:{port}")
            });
            std::fs::write(root.join("dependencies.json"), dependencies.to_string()).unwrap();
            let context = RuntimeContext::from_root(&root).unwrap();
            let cfg = context.load_config().unwrap();
            let mut pipeline = Pipeline::new_with_runtime(&context, &cfg, true).unwrap();
            let server = std::thread::spawn(move || {
                let mut paths = Vec::new();
                while let Some((mut stream, request)) =
                    fake_dsh_accept(&listener, std::time::Duration::from_secs(2))
                {
                    if request.path == "/send_private_msg" {
                        let body = r#"{"status":"ok","retcode":0,"data":{"message_id":123}}"#;
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    } else {
                        // DSH returns a malformed workspace result. This is a genuine backend failure.
                        fake_dsh_respond(&mut stream, &request, 200, serde_json::json!({}));
                    }
                    paths.push(request.path);
                }
                paths
            });
            let error = pipeline.ask_with_failover("group:42", "fixture prompt", 5, &cfg).await.unwrap_err();
            let note = take_model_error(&context, "group:42");
            let reported = pipeline.failure_reported;
            let paths = server.join().unwrap();
            drop(pipeline);
            std::fs::remove_dir_all(root).unwrap();
            if admitted {
                assert!(note.is_some() && reported, "backend failure must be reported: {error}");
                assert_eq!(paths, ["/api/workspace/create", "/send_private_msg"]);
            } else {
                assert!(note.is_none(), "session admission must not leave a model error note: {note:?}");
                assert!(!reported && paths.is_empty(), "session admission must not call DSH or alert the owner: {paths:?}");
                assert!(is_cancellation_error(&error), "{error}");
            }
        }
    }

    /// Ticket 24 second fix round (F3 / Codex 15): a conversation that is not allowed when the
    /// session is requested never gets one (JS `ensureSession` checks `isSessionAllowedInCurrentMode`
    /// first), even when nothing changes while DSH creates it.
    #[test]
    fn ticket24_fix2_a_conversation_denied_before_creation_gets_no_session() {
        use std::net::TcpListener;
        let _env_lock = DSH_HOME_TEST_LOCK.lock().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = fake_runtime_root("t24-fix2-denied");
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","allow":{"groups":[]}}"#).unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
        std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{"group:42":{"agentToken":"fixture-group-token-abcdefghij"}}}"#).unwrap();
        write_selected_secret(&root, [0x25; 32]);
        let (context, mut pipeline) = pipeline_for_fake_dsh(&root, port);
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            while let Some((mut stream, request)) = fake_dsh_accept(&listener, std::time::Duration::from_millis(1500)) {
                let value = match request.path.as_str() {
                    "/api/workspace/create" => serde_json::json!({ "workspace": { "workspaceId": "ws-t24" } }),
                    "/api/session/create" => serde_json::json!({ "sessionId": "sid-denied" }),
                    _ => serde_json::json!({}),
                };
                fake_dsh_respond(&mut stream, &request, 200, value);
                seen.push(request.path);
            }
            seen
        });
        let result = pipeline.ensure_session("group:42");
        let seen = server.join().unwrap();
        assert!(result.is_err(), "a denied conversation got a session: {result:?}");
        assert!(!seen.iter().any(|p| p == "/api/session/create"), "DSH was asked to create a session for a denied conversation: {seen:?}");
        assert_eq!(crate::session_map::lookup(&context, "group:42").unwrap(), None);
        drop(pipeline);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Ticket 24 second fix round (Codex 15): a new session that cannot be bound is archived, and
    /// a discarded session's archive failure is reported, not swallowed.
    #[test]
    fn ticket24_fix2_unbound_or_discarded_sessions_are_archived_and_failures_reported() {
        use std::net::TcpListener;
        let _env_lock = DSH_HOME_TEST_LOCK.lock().unwrap();
        for (tag, refuse_archive) in [("t24-fix2-bind-fails", false), ("t24-fix2-archive-fails", true)] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let root = fake_runtime_root(tag);
            std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-token","allow":{"groups":[42]}}"#).unwrap();
            std::fs::write(root.join("data/mode.json"), r#"{"mode":"reserved2"}"#).unwrap();
            std::fs::write(root.join("data/social-v2.json"), r#"{"conversations":{"group:42":{"agentToken":"fixture-group-token-abcdefghij"}}}"#).unwrap();
            write_selected_secret(&root, [0x26; 32]);
            let (context, mut pipeline) = pipeline_for_fake_dsh(&root, port);
            let server_context = context.clone();
            let server_root = root.clone();
            let server = std::thread::spawn(move || {
                let mut seen = Vec::new();
                while let Some((mut stream, request)) = fake_dsh_accept(&listener, std::time::Duration::from_millis(1500)) {
                    seen.push(request.path.clone());
                    let value = match request.path.as_str() {
                        "/api/workspace/create" => serde_json::json!({ "workspace": { "workspaceId": "ws-t24" } }),
                        "/api/session/create" => {
                            if refuse_archive {
                                // The administrator revokes while DSH creates the session.
                                let (status, reply) = crate::console::admin_request(&server_context, "POST", "/api/whitelist", Some(&serde_json::json!({"allow": {"groups": []}, "confirm": true})));
                                assert_eq!(status, 200, "{reply}");
                            } else {
                                // The mapping store cannot be written any more: binding fails.
                                std::fs::remove_file(server_root.join("data/sessions.json")).ok();
                                std::fs::create_dir_all(server_root.join("data/sessions.json")).unwrap();
                            }
                            serde_json::json!({ "sessionId": "sid-unbound" })
                        }
                        "/api/workspace/archiveSession" if refuse_archive => {
                            let body = serde_json::json!({"type": "server-response", "rpcId": serde_json::from_str::<Value>(&request.body).unwrap()["rpcId"], "result": {"ok": false, "error": {"code": "fixture-archive-refused", "message": "synthetic"}}}).to_string();
                            use std::io::Write as _;
                            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                            continue;
                        }
                        _ => serde_json::json!({}),
                    };
                    fake_dsh_respond(&mut stream, &request, 200, value);
                }
                seen
            });
            let result = pipeline.ensure_session("group:42");
            let seen = server.join().unwrap();
            let error = String::from(result.expect_err(&format!("{tag}: the session was used")));
            assert!(seen.iter().any(|p| p == "/api/workspace/archiveSession"), "{tag}: the new session must be archived: {seen:?}");
            if refuse_archive {
                assert!(error.contains("fixture-archive-refused"), "{tag}: the archive failure was swallowed: {error}");
            }
            drop(pipeline);
            let _ = std::fs::remove_dir_all(root.join("data/sessions.json"));
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn isolated_401_relogin_fails_closed_when_selected_secret_is_missing_or_invalid() {
        use std::net::TcpListener;

        let _env_lock = DSH_HOME_TEST_LOCK.lock().unwrap();
        let ambient_root = fake_runtime_root("ambient-valid-secret");
        write_selected_secret(&ambient_root, [0xa6; 32]);
        let _restore_dsh_home = RestoreDshHome::set_to(&ambient_root.join("user/dsh"));
        let ambient_original = std::fs::read(selected_credentials_path(&ambient_root)).unwrap();

        for (tag, replacement) in [
            ("missing-after-401", None),
            ("invalid-after-401", Some(b"secret: invalid\n".as_slice())),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let root = fake_runtime_root(tag);
            write_selected_secret(&root, [0x44; 32]);
            let (_context, mut pipeline) = pipeline_for_fake_dsh(&root, port);
            let selected_path = selected_credentials_path(&root);
            let server_selected_path = selected_path.clone();

            let server = std::thread::spawn(move || {
                let mut seen = Vec::new();
                for index in 0..2 {
                    let (mut stream, request) = fake_dsh_accept(&listener, std::time::Duration::from_secs(5))
                        .expect("pipeline should call the selected fake DSH endpoint");
                    let expected = if index == 0 { "/api/workspace/create" } else { "/api/session/create" };
                    assert_eq!(request.path, expected);
                    if index == 1 {
                        if let Some(contents) = replacement {
                            std::fs::write(&server_selected_path, contents).unwrap();
                        } else {
                            std::fs::remove_file(&server_selected_path).unwrap();
                        }
                        fake_dsh_respond(&mut stream, &request, 401, serde_json::Value::Null);
                    } else {
                        fake_dsh_respond(
                            &mut stream,
                            &request,
                            200,
                            serde_json::json!({ "workspace": { "workspaceId": "ws-before-401" } }),
                        );
                    }
                    seen.push(request);
                }

                // If isolated relogin consulted ambient DSH_HOME, it would issue agentPresets/list
                // with the synthetic ambient credential. Observe and answer any such request so
                // the test can verify the complete fail-closed boundary without hanging the client.
                while let Some((mut stream, request)) =
                    fake_dsh_accept(&listener, std::time::Duration::from_millis(700))
                {
                    let value = match request.path.as_str() {
                        "/api/agentPresets/list" => serde_json::json!([]),
                        "/api/session/create" => serde_json::json!({ "sessionId": "sid-ambient-fallback" }),
                        _ => serde_json::json!({}),
                    };
                    fake_dsh_respond(&mut stream, &request, 200, value);
                    seen.push(request);
                }
                seen
            });

            let error = String::from(pipeline.ensure_session("group:42").unwrap_err());
            assert!(error.contains("重新登录也失败"), "unexpected refresh error: {error}");
            let seen = server.join().unwrap();
            assert_eq!(seen.len(), 2, "isolated refresh must not use ambient DSH_HOME: {seen:?}");
            assert!(seen.iter().all(|request| request.path != "/api/agentPresets/list"));
            match replacement {
                Some(contents) => assert_eq!(std::fs::read(&selected_path).unwrap(), contents),
                None => assert!(!selected_path.exists()),
            }
            drop(pipeline);
            std::fs::remove_dir_all(root).unwrap();
        }

        assert_eq!(std::fs::read(selected_credentials_path(&ambient_root)).unwrap(), ambient_original);
        std::fs::remove_dir_all(ambient_root).unwrap();
    }

    #[test]
    fn silent_marker_is_exact_and_case_insensitive() {
        // 与 JS `/^\s*\[SILENT\]\s*$/i` 一致：整段相等才算
        assert!(is_silent_marker("[SILENT]"));
        assert!(is_silent_marker("  [silent]  "));
        assert!(is_silent_marker("[Silent]"));
        assert!(!is_silent_marker("他说 [SILENT] 这个词"));
        assert!(!is_silent_marker("[SILENT] 后面还有字"));
        assert!(!is_silent_marker(""));
        assert!(!is_silent_marker("[SILENTX]"));
    }
    #[test]
    fn outbound_sentinel_matches_js_categories() {
        // 四类必须拦：本机路径（Win/UNC/Unix）+ 凭据赋值
        assert!(outbound_violation(r"C:\qqbot\config.json", &[]).is_some()); // layout-literal: sanitizer fixture
        assert!(outbound_violation(r"\\192.168.1.5\share\x.txt", &[]).is_some());
        assert!(outbound_violation("/home/me/.dsh/config", &[]).is_some());
        assert!(outbound_violation("/etc/passwd", &[]).is_some());
        assert!(outbound_violation("token=abcd1234", &[]).is_some());
        assert!(outbound_violation("密码：qwerty123", &[]).is_some());
        assert!(outbound_violation("api_key=sk-aaaaaaaaaa", &[]).is_some());
        assert!(outbound_violation("authorization: Bearer abc.def", &[]).is_some());

        // 必须放过：常见误伤面
        assert!(outbound_violation("今天天气不错", &[]).is_none());
        assert!(outbound_violation("C:盘满了", &[]).is_none(), "盘符后不是斜杠不该拦");
        assert!(outbound_violation("12:30 吃午饭", &[]).is_none());
        assert!(outbound_violation("他说 token 这个词本身没问题", &[]).is_none());
        assert!(outbound_violation("https://example.com/path/to/file", &[]).is_none());
        assert!(
            outbound_violation("md5 是 d41d8cd98f00b204e9800998ecf8427e", &[]).is_none(),
            "32 位十六进制不该被当令牌拦下（表情 md5/图片 URL 都是这种形态）"
        );

        // 精确名单仍然有效
        assert!(outbound_violation("随便一句话", &["随便".to_string()]).is_some());
    }


    #[test]
    fn sentinel_blocks_paths() {
        let secrets = vec!["TOPSECRET".to_string()];
        assert!(outbound_violation("看 C:\\qqbot\\docs 里的文件", &secrets).is_some()); // layout-literal: sanitizer fixture
        assert!(outbound_violation("路径 C:/qqbot/x", &secrets).is_some()); // layout-literal: sanitizer fixture
        assert!(outbound_violation("token 是 TOPSECRET", &secrets).is_some());
        assert!(outbound_violation("cookie dsh-auth-abc=def", &secrets).is_some());
    }

    #[test]
    fn sentinel_allows_normal_chat() {
        let secrets = vec!["TOPSECRET".to_string()];
        assert!(outbound_violation("今天天气不错，出去走走？", &secrets).is_none());
        assert!(outbound_violation("我记得你说过喜欢鲸鱼", &secrets).is_none());
    }

    #[test]
    fn store_roundtrip_shape() {
        let mut s = SessionStore::default();
        s.workspace_id = "ws-1".into();
        s.sessions.insert(
            "group:1".into(),
            SessionRef { session_id: "s-1".into(), workspace_id: "ws-1".into() },
        );
        let text = serde_json::to_string(&s).unwrap();
        let back: SessionStore = serde_json::from_str(&text).unwrap();
        assert_eq!(back.workspace_id, "ws-1");
        assert_eq!(back.sessions.get("group:1").unwrap().session_id, "s-1");
        // 字段名必须是 JS 兼容的 camelCase
        assert!(text.contains("\"sessionId\""));
    }

    #[test]
    fn one_turn_appends_exactly_one_ledger_line() {
        // 验收 T5：跑完一轮 → 账本文件正好多一行，且 Ledger 能把它读回来。
        let at = 1_792_700_000_000i64;
        let sample = TurnUsage {
            turn: 7,
            step: 2,
            retry: 0,
            seq: 42,
            t: at,
            model: Some("deepseek-flash".into()),
            provider: Some("deepseek".into()),
            usage: serde_json::json!({
                "cacheMiss": 10, "cacheHit": 20, "cacheWrite": 0, "output": 5, "reasoning": 0
            }),
        };
        let records = turn_records(
            "group:1",
            "sid-test-1",
            &[sample],
            42,
            Some(7),
            &Some("deepseek-flash".into()),
            &Some("deepseek".into()),
            at,
        );
        assert_eq!(records.len(), 1, "一条 usage 对应一条账本记录");
        let rec = &records[0];
        assert_eq!(rec["k"], "turn");
        assert_eq!(rec["sid"], "sid-test-1");
        assert_eq!(rec["key"], "group:1");
        assert_eq!(rec["seq"], 42);
        assert_eq!(rec["turn"], 7);
        assert_eq!(rec["step"], 2);
        assert_eq!(rec["model"], "deepseek-flash");
        assert_eq!(rec["provider"], "deepseek");

        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-ledger-test-{}-{}",
            std::process::id(),
            now_ms()
        )));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token-usage.jsonl");
        let path = path.to_string_lossy().to_string();
        crate::ledger::append_record_to(&path, rec).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1, "一轮必须只多一行");
        let ledger = crate::ledger::Ledger::load(&path);
        assert_eq!(ledger.dropped_lines, 0, "写出去的格式必须能被账本读回");
        assert_eq!(ledger.samples.len(), 1);
        let back = ledger.samples.values().next().unwrap();
        assert_eq!(back.key.as_deref(), Some("group:1"));
        assert_eq!(back.buckets.cache_hit, 20.0);
        assert_eq!(back.buckets.output, 5.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_usage_writes_null_buckets_not_fake_numbers() {
        let records = turn_records(
            "group:1",
            "sid-test-2",
            &[],
            99,
            Some(3),
            &None,
            &None,
            1_792_700_000_000,
        );
        assert_eq!(records.len(), 1, "回合跑过但没有 usage：仍要留一行证据");
        assert!(records[0]["b"].is_null(), "拿不到明细必须写 b:null");
        assert_eq!(records[0]["step"], 0, "step=0 表示无明细，不冒充正常 step");
        // 连 turn/start 都没看到（prompt 未被接受）：不要凭空造一行
        assert!(turn_records("group:1", "sid-test-3", &[], 0, None, &None, &None, 0).is_empty());
    }

    struct FakeUnsubmittedDsh {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        server: Option<std::thread::JoinHandle<()>>,
        port: u16,
    }

    impl FakeUnsubmittedDsh {
        fn start(unreachable_prompt: bool) -> Self {
            use std::io::{Read, Write};
            use std::sync::atomic::Ordering;
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopping = stop.clone();
            let server = std::thread::spawn(move || {
                let mut clients = vec![];
                while !stopping.load(Ordering::SeqCst) {
                    let Ok((stream, _)) = listener.accept() else {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    };
                    clients.push(std::thread::spawn(move || {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                            .unwrap();
                        let mut head = [0; 4];
                        if stream.peek(&mut head).is_err() {
                            return;
                        }
                        if &head[..3] == b"GET" {
                            use tokio_tungstenite::tungstenite::{accept, Message};
                            let Ok(mut ws) = accept(stream) else { return };
                            let Ok(msg) = ws.read() else { return };
                            let Ok(text) = msg.to_text() else { return };
                            let Ok(open) = serde_json::from_str::<Value>(text) else { return };
                            let id = &open["streamId"];
                            let _ = ws.send(Message::Text(
                                serde_json::json!({
                                    "type": "item",
                                    "streamId": id,
                                    "value": {"type": "snapshot", "records": [], "hasMore": false}
                                })
                                .to_string(),
                            ));
                            let _ = ws.read();
                        } else {
                            let mut stream = stream;
                            let mut bytes = vec![];
                            let (path, body) = loop {
                                let mut chunk = [0; 4096];
                                let Ok(n) = stream.read(&mut chunk) else { return };
                                if n == 0 {
                                    return;
                                }
                                bytes.extend_from_slice(&chunk[..n]);
                                let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                                    continue;
                                };
                                let head = String::from_utf8_lossy(&bytes[..end]);
                                let len = head
                                    .lines()
                                    .find_map(|line| {
                                        let (k, v) = line.split_once(':')?;
                                        k.eq_ignore_ascii_case("content-length")
                                            .then(|| v.trim().parse::<usize>().unwrap_or(0))
                                    })
                                    .unwrap_or(0);
                                if bytes.len() >= end + 4 + len {
                                    break (
                                        head.lines()
                                            .next()
                                            .unwrap()
                                            .split_whitespace()
                                            .nth(1)
                                            .unwrap()
                                            .to_string(),
                                        String::from_utf8_lossy(&bytes[end + 4..end + 4 + len])
                                            .to_string(),
                                    );
                                }
                            };
                            if path == "/api/session/prompt" && unreachable_prompt {
                                let _ = write!(
                                    stream,
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot json"
                                );
                            } else {
                                let value = match path.as_str() {
                                    "/api/session/prompt" => serde_json::json!({"accepted": false}),
                                    "/api/session/projections" => serde_json::json!({"values": []}),
                                    _ => serde_json::json!({}),
                                };
                                fake_dsh_respond(
                                    &mut stream,
                                    &FakeDshRequest {
                                        path,
                                        cookie: None,
                                        body,
                                    },
                                    200,
                                    value,
                                );
                            }
                        }
                    }));
                }
                for client in clients {
                    let _ = client.join();
                }
            });
            Self {
                stop,
                server: Some(server),
                port,
            }
        }
    }

    impl Drop for FakeUnsubmittedDsh {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
        }
    }

    struct FailureLineFixture {
        root: PathBuf,
        context: RuntimeContext,
        cfg: serde_json::Value,
        ob_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ob_reqs: std::sync::Arc<std::sync::Mutex<Vec<FakeDshRequest>>>,
        ob_server: Option<std::thread::JoinHandle<()>>,
    }

    impl FailureLineFixture {
        fn finish(mut self) -> (Vec<Value>, Vec<FakeDshRequest>) {
            self.ob_stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(server) = self.ob_server.take() {
                server.join().unwrap();
            }
            let outbox = crate::send_outbox::read_records(&self.context).unwrap();
            let reqs = self.ob_reqs.lock().unwrap().drain(..).collect();
            std::fs::remove_dir_all(&self.root).unwrap();
            (outbox, reqs)
        }
    }

    fn setup_failure_line_fixture(
        tag: &str,
        mode: &str,
        dsh_port: u16,
        line: &str,
    ) -> FailureLineFixture {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        let root = fake_runtime_root(tag);
        write_selected_secret(&root, [0x48; 32]);
        let onebot = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        onebot.set_nonblocking(true).unwrap();
        let onebot_port = onebot.local_addr().unwrap().port();
        let ob_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ob_reqs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ob_stopping = ob_stop.clone();
        let ob_seen = ob_reqs.clone();
        let ob_server = std::thread::spawn(move || {
            while !ob_stopping.load(Ordering::SeqCst) {
                if let Some((mut stream, req)) =
                    fake_dsh_accept(&onebot, std::time::Duration::from_millis(20))
                {
                    ob_seen.lock().unwrap().push(req);
                    let body = r#"{"status":"ok","retcode":0,"data":{"message_id":123}}"#;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            }
        });
        let cfg = serde_json::json!({
            "consoleToken": "fixture-token",
            "allow": {"groups": [42]},
            "dsh": {"provider": "p1", "model": "m1"}
        });
        std::fs::write(root.join("config/config.json"), cfg.to_string()).unwrap();
        std::fs::write(
            root.join("data/mode.json"),
            serde_json::json!({"mode": mode}).to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            serde_json::json!({
                "dsh": format!("http://127.0.0.1:{dsh_port}"),
                "model": "http://127.0.0.1:43202",
                "onebot": format!("http://127.0.0.1:{onebot_port}")
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("data/personas.json"),
            serde_json::json!({
                "format": 1, "nextId": 2, "default": "p1", "overrides": {},
                "personas": {"p1": {"id": "p1", "activeVersion": 1, "versions": [
                    {"version": 1, "approved": true, "name": "Fixture", "core": "Speak plainly.", "failureLine": line}
                ]}}
            })
            .to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        crate::session_map::bind(&context, "group:42", "sid-48", None).unwrap();
        FailureLineFixture {
            root,
            context,
            cfg,
            ob_stop,
            ob_reqs,
            ob_server: Some(ob_server),
        }
    }

    #[tokio::test]
    async fn t48_chat_unsubmitted_dsh_deferral_suppresses_failure_line_until_terminal() {
        let line = "A moment, please.";
        for (case, unreachable_prompt, expect_wait_for_dsh, expect_send) in [
            ("deferred-wait-for-dsh", true, true, false),
            ("terminal-prompt-refused", false, false, true),
        ] {
            let dsh = FakeUnsubmittedDsh::start(unreachable_prompt);
            let fx = setup_failure_line_fixture(&format!("t48-chat-{case}"), "chat", dsh.port, line);
            let mut pipe = Pipeline::new_with_runtime(&fx.context, &fx.cfg, true).unwrap();
            let result = pipe
                .run_chat_turn(
                    "group:42",
                    "hello chat".into(),
                    None,
                    &fx.cfg,
                    &["e1".into()],
                    &[],
                    "chat",
                    false,
                )
                .await;
            drop(pipe);
            drop(dsh);
            let (outbox, reqs) = fx.finish();

            if expect_wait_for_dsh {
                assert!(
                    matches!(result, Ok(ChatTurnOutcome::WaitForDsh)),
                    "{case}: expected WaitForDsh, got {result:?}"
                );
            } else {
                assert!(result.is_err(), "{case}: expected terminal Err, got {result:?}");
            }
            let sent_count = reqs.len();
            assert_eq!(
                sent_count,
                usize::from(expect_send),
                "{case}: onebot failure-line send count"
            );
            assert_eq!(
                outbox
                    .iter()
                    .filter(|r| r["k"] == "intent" && r["purpose"] == "persona-failure")
                    .count(),
                usize::from(expect_send),
                "{case}: durable failure-line intent count"
            );
            if expect_send {
                assert_eq!(reqs[0].path, "/send_group_msg");
                let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
                assert_eq!(body["message"], serde_json::json!([{"type": "text", "data": {"text": line}}]));
            }
        }
    }

    #[tokio::test]
    async fn t48_reserved_unsubmitted_dsh_retries_and_parking_suppress_failure_line_until_terminal() {
        let line = "A moment, please.";
        for (case, retries_remaining, unreachable_prompt, expect_parks, expect_send) in [
            ("retryable-attempt", true, false, None, false),
            ("parked-on-dsh-down", false, true, Some(true), false),
            ("terminal-retries-exhausted", false, false, Some(false), true),
        ] {
            let dsh = FakeUnsubmittedDsh::start(unreachable_prompt);
            let fx =
                setup_failure_line_fixture(&format!("t48-reserved-{case}"), "reserved", dsh.port, line);
            let authority = crate::reserved::authority_for_test(&fx.context, "group:42").unwrap();
            let mut pipe = Pipeline::new_with_runtime(&fx.context, &fx.cfg, true).unwrap();
            pipe.set_reserved_retry_budget(retries_remaining);
            let result = pipe
                .run_reserved_turn(
                    "group:42",
                    "hello reserved",
                    &fx.cfg,
                    &["e1".into()],
                    &[],
                    false,
                    authority,
                )
                .await;
            let parks = pipe.reserved_failure_parks();
            let unsubmitted = pipe.dispatch_definitely_unsubmitted();
            drop(pipe);
            drop(dsh);
            let (outbox, reqs) = fx.finish();

            assert!(result.is_err(), "{case}: turn failed");
            assert!(unsubmitted, "{case}: prompt must stay unsubmitted");
            assert_eq!(parks, expect_parks, "{case}: reserved_failure_parks");
            assert_eq!(
                reqs.len(),
                usize::from(expect_send),
                "{case}: onebot failure-line send count"
            );
            assert_eq!(
                outbox
                    .iter()
                    .filter(|r| r["k"] == "intent" && r["purpose"] == "persona-failure")
                    .count(),
                usize::from(expect_send),
                "{case}: durable failure-line intent count"
            );
            if expect_send {
                assert_eq!(reqs[0].path, "/send_group_msg");
                let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
                assert_eq!(body["message"], serde_json::json!([{"type": "text", "data": {"text": line}}]));
            }
        }
    }
}
