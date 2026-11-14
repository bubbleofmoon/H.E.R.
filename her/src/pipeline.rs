//! P2-C 接线：消息 → 触发 → 会话（落盘复用）→ prompt → 等回复 → 发回 QQ。
//!
//! 设计约束：
//!   1. **默认不发送**。只有 `--live` 才真的发；否则只打日志（dry-run）。
//!   2. **出站哨兵**：会话令牌、Cookie、本机绝对路径一律拒绝发送。
//!   3. **会话映射落盘**（等价 JS 的 `state.sessions`），重启不丢。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::dsh::{FollowStream, Rpc, TurnUsage};
use crate::candidate_state::FailoverState;
use crate::dependency::{Clock, Idempotency, RetryDecision, RetryPolicy, SystemClock};
use crate::failover;
use crate::recovery_log;
use crate::run_record::{Component, Outcome, Trigger};
use std::sync::{Arc, Mutex, OnceLock};
use crate::onebot::{OneBot, RateLimiter, SendTuning, Target};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};

/// The explicit per-conversation CLI route, using the same rules as turn dispatch.
pub(crate) fn pinned_cli_backend<'a>(cfg: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    cfg.pointer("/backends/sessions")
        .and_then(|sessions| sessions.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

#[cfg(not(test))]
const SESSION_FILE: &str = "C:/qqbot/state/rust-sessions.json";

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

/// Explicit false disables outbound auditing; missing or invalid values retain the default.
pub fn outbound_guard_enabled(cfg: &serde_json::Value) -> bool {
    cfg.pointer("/security/outboundGuard") != Some(&serde_json::Value::Bool(false))
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

/// 把一轮的 usage 采样转成账本行（字段与 JS `token-ledger.js` 的 appendLine 一致）。
///
/// 为什么一条 usage 一行、而不是整轮合并成一行：JS 也是按 `(turn, step, retry)` 落账。
/// 保持同样的粒度，两边同时写时才能靠 `seq` 幂等折叠；合并成一行会和多步的 JS 记录
/// 叠在一起，把同一笔 token 算两遍。
fn turn_records(
    key: &str,
    sid: &str,
    samples: &[TurnUsage],
    seq: i64,
    turn: Option<i64>,
    model: &Option<String>,
    provider: &Option<String>,
    at: i64,
) -> Vec<serde_json::Value> {
    if let Some(turn) = turn {
        if samples.is_empty() {
            // 回合确实跑过，但事件里没有 token 明细：写 `b:null` 留证据，账本按 0 计。
            // **不编造数字**；`step:0` 表示「无明细」，不会和正常 step 撞键。
            return vec![serde_json::json!({
                "k": "turn", "t": at, "sid": sid, "key": key, "seq": seq,
                "turn": turn, "step": 0, "retry": 0,
                "model": model, "provider": provider, "b": serde_json::Value::Null,
            })];
        }
    }
    samples
        .iter()
        .map(|s| {
            serde_json::json!({
                "k": "turn", "t": s.t, "sid": sid, "key": key, "seq": s.seq,
                "turn": s.turn, "step": s.step, "retry": s.retry,
                "model": s.model.clone().or_else(|| model.clone()),
                "provider": s.provider.clone().or_else(|| provider.clone()),
                "b": s.usage,
            })
        })
        .collect()
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
    turn_epoch: String,
    turn_epochs: Vec<String>,
    next_inbound: Vec<String>,
    next_reason: String,
    current_reason: String,
    dispatch_token: Option<String>,
    /// Where the scheduler learns which DSH session this turn uses (to cancel exactly that one).
    session_slot: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// Where the scheduler learns the epoch this turn minted when it abandoned an attempt.
    minted_slot: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    #[cfg(test)]
    pub(crate) rotation_locked_hook: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    after_abandon_rotation: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    completion_observed_hook: Option<Box<dyn FnMut() + Send>>,
    /// A queued chat job's epoch check, run again right before the turn begins (after media).
    before_turn: Option<Box<dyn FnOnce() -> Option<String> + Send>>,
    dispatch_wake: Option<(i64, serde_json::Value)>,
    dispatch_deadline: Option<i64>,
    dispatch_unsubmitted: bool,
    /// Chat mode (ticket 12): the run state of the chat turn in flight.
    chat: Option<ChatRun>,
    /// Facts of the last finished turn, for the chat reply decision.
    last_report: Option<TurnReport>,
    failure_reported: bool,
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
    engine: crate::harness::Engine,
    reserved: Option<crate::reserved::Authority>,
    mode: String,
    silent: bool,
    media_parts: Vec<serde_json::Value>,
    /// Acknowledgement to send once DSH accepted the prompt (sent at most once per message).
    ack: Option<String>,
    delay_ms: u64,
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

/// Outcome of a `session/prompt` submission that did not end in acceptance.
enum Submit {
    /// DSH refused it or it never reached DSH: safe to try another candidate.
    Refused(String),
    /// The request may have been accepted and nothing proves otherwise: never resubmit.
    Unverified(String),
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
        "会话创建期间已重置或权限已变化",
        "等待期间会话已重置或权限已变化",
    ];
    OWN_PREFIXES.iter().any(|p| error.starts_with(p))
        || error.ends_with("丢弃排队的指令")
        || error.contains("token invalidated")
        || error.contains("mode or permission changed")
}

/// Whether an error of `session/prompt` leaves the outcome open (check with DSH) rather than being a definite refusal.
/// Only DSH's own refusal (or a 401, refused before processing) is definite, with or without a ledger turn: a timeout or a
/// dropped connection may still have been accepted, so the prompt is never assumed unsent and never fails over blind.
fn prompt_error_needs_probe(_has_turn: bool, error: &str) -> bool {
    !definite_refusal(error)
}

/// DSH answered with a refusal (or HTTP 401): the prompt was definitely not accepted.
pub(crate) fn definite_refusal(error: &str) -> bool {
    error.contains(" 被拒：") || error.starts_with("401")
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
    pub fn new(cfg: &serde_json::Value, live: bool) -> Result<Self, String> {
        Self::new_with_runtime(&RuntimeContext::legacy(), cfg, live)
    }

    pub fn new_with_runtime(
        context: &RuntimeContext,
        cfg: &serde_json::Value,
        live: bool,
    ) -> Result<Self, String> {
        // Validate selected-root session state before constructing any live dependencies or
        // creating the candidate workspace. A corrupt file must not silently start a new session
        // and later overwrite the evidence needed to repair it.
        let store = SessionStore::load_for_runtime(context)?;
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
        let rpc = Rpc::connect_with_runtime(context, &base)?;

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
                .unwrap_or("C:/qqbot/state/agents")
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
            turn_epoch: String::new(),
            turn_epochs: Vec::new(),
            next_inbound: Vec::new(),
            next_reason: String::new(),
            current_reason: String::new(),
            dispatch_token: None,
            session_slot: None,
            minted_slot: None,
            #[cfg(test)]
            rotation_locked_hook: None,
            #[cfg(test)]
            after_abandon_rotation: None,
            #[cfg(test)]
            completion_observed_hook: None,
            before_turn: None,
            dispatch_wake: None,
            dispatch_deadline: None,
            dispatch_unsubmitted: true,
            chat: None,
            last_report: None,
            failure_reported: false,
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

    fn preset_for(&self, key: &str) -> String {
        if self.context.is_isolated() && crate::console::runtime_mode(&self.context) == "closed-agent" {
            return self.preset_closed.clone();
        }
        // Chat mode has one preset for every conversation (JS `modePreset`).
        if self.context.is_isolated() && matches!(crate::console::runtime_mode(&self.context).as_str(), "chat" | "reserved") {
            return self.preset_chat.clone();
        }
        let owner_key = format!("private:{}", self.owner_qq);
        if !self.owner_qq.is_empty() && key == owner_key {
            self.preset_owner.clone()
        } else {
            self.preset_group.clone()
        }
    }

    fn ensure_workspace(&mut self) -> Result<String, String> {
        if self.context.is_isolated() {
            if let Some(id) = crate::session_map::workspace_id(&self.context)? {
                self.store.workspace_id = id.clone();
                return Ok(id);
            }
        } else if !self.store.workspace_id.is_empty() {
            return Ok(self.store.workspace_id.clone());
        }
        if self.context.is_isolated() {
            let selected = self
                .context
                .resolve_path(RuntimeArea::Data, "agents", PathIntent::CreateDirectory)
                .map_err(str::to_string)?;
            if selected.to_string_lossy() != self.workspace_dir {
                return Err("候选 workspace path changed after initialization".to_string());
            }
        } else {
            let _ = std::fs::create_dir_all(&self.workspace_dir);
        }
        let ws = self
            .rpc
            .call("workspace/create", serde_json::json!({ "path": self.workspace_dir }))?;
        let id = ws
            .get("workspace")
            .and_then(|w| w.get("workspaceId"))
            .and_then(|i| i.as_str())
            .ok_or_else(|| format!("workspace/create 没返回 workspaceId：{ws}"))?
            .to_string();
        self.store.workspace_id = id.clone();
        if self.context.is_isolated() {
            crate::session_map::set_workspace_id(&self.context, &id)?;
        } else {
            self.store.save_for_runtime(&self.context)?;
        }
        Ok(id)
    }

    /// 取（或新建）某个 QQ 会话对应的 DSH sessionId。
    pub fn ensure_session(&mut self, key: &str) -> Result<String, String> {
        if key.starts_with("ab:") {
            return Err("shadow isolation refused: stored shadow sessions cannot use the live session path".into());
        }
        // JS `ensureSession`: the access epoch seen before DSH creates a session must still hold when
        // it is bound; a revocation, reset or mode change in between archives the new session.
        let epoch = if self.context.is_isolated() { Some(self.access_epoch(key)) } else { None };
        if epoch.as_ref().is_some_and(|e| !matches!(e.allowed, Ok(true))) {
            return Err(format!("当前模式不允许会话 {key}"));
        }
        if self.context.is_isolated() {
            // Fresh read: the console may have reset or replaced this mapping since the last turn.
            match crate::session_map::lookup(&self.context, key)? {
                Some(sid) => {
                    // A session made under another mode must not carry this mode's messages
                    // (JS `isCurrentSession` policy check): retire it and start a new one.
                    if let Some(reason) = self.stale_policy(key)? {
                        return self.replace_session(key, &sid, reason);
                    }
                    self.store.sessions.insert(
                        key.to_string(),
                        SessionRef { session_id: sid.clone(), workspace_id: self.store.workspace_id.clone() },
                    );
                    return Ok(sid);
                }
                None => {
                    self.store.sessions.remove(key);
                }
            }
        } else if let Some(s) = self.store.sessions.get(key) {
            if !s.session_id.is_empty() {
                return Ok(s.session_id.clone());
            }
        }
        let ws_id = self.ensure_workspace()?;
        let preset = self.preset_for(key);
        // A conversation of a QQ group must never fall back to DSH's default preset (it carries local
        // tools): chat refuses to create a session unless DSH offers the configured preset.
        if self.context.is_isolated() && matches!(crate::console::runtime_mode(&self.context).as_str(), "chat" | "reserved") {
            self.require_preset(&preset)?;
        }
        let create = |rpc: &crate::dsh::Rpc| {
            let mut request = serde_json::json!({ "workspaceId": ws_id });
            if !preset.is_empty() { request["agentPreset"] = serde_json::json!(preset); }
            rpc.call(
                "session/create",
                request,
            )
        };
        // 401 = DSH 换了签名密钥（重启）。重读密钥 + 重新铸 cookie 再试一次 ——
        // 不这么做的话，DSH 每重启一次，Rust 就得人工重启才能恢复。
        let created = match create(&self.rpc) {
            Ok(v) => v,
            Err(e) if e.contains("401") => {
                println!("[dsh] session/create 401，重读密钥后重试");
                self.rpc.relogin().map_err(|re| format!("{e}；重新登录也失败：{re}"))?;
                create(&self.rpc)?
            }
            Err(e) => return Err(e),
        };
        let sid = created
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or_else(|| format!("session/create 没返回 sessionId：{created}"))?
            .to_string();

        if self.context.is_isolated() && matches!(crate::console::runtime_mode(&self.context).as_str(), "chat" | "reserved") {
            // Chat: the JS title (`renameSessionForUi`) is resolved after the model is selected and
            // set once the first prompt is in DSH's hands.
            self.title_for = Some((sid.clone(), key.to_string()));
        } else {
            // 起个好认的名字，方便在 DSH 里区分是 Rust 建的
            let _ = self.rpc.call(
                "workspace/rename",
                serde_json::json!({ "sessionId": sid, "title": format!("Rust · {key}") }),
            );
        }

        if let Some(epoch) = &epoch {
            if !self.access_epoch(key).still(epoch) {
                return Err(self.discard_created_session(key, &sid, false));
            }
        }
        self.store.sessions.insert(
            key.to_string(),
            SessionRef { session_id: sid.clone(), workspace_id: ws_id },
        );
        if self.context.is_isolated() {
            let policy = self.session_policy(key);
            if let Err(error) = crate::session_map::bind(&self.context, key, &sid, Some(&policy)) {
                // Never leave a created session running unbound in DSH.
                let discarded = self.discard_created_session(key, &sid, false);
                return Err(format!("绑定新会话失败：{error}；{discarded}"));
            }
        } else {
            self.store.save_for_runtime(&self.context)?;
        }
        // A change between the check and the binding retires the mapping again (JS re-checks after
        // `saveState`).
        if let Some(epoch) = &epoch {
            if !self.access_epoch(key).still(epoch) {
                return Err(self.discard_created_session(key, &sid, true));
            }
        }
        Ok(sid)
    }

    /// What a session's authority depends on: mode file, current allowance, the ledger's reset
    /// count, the conversation token and the session policy.
    fn access_epoch(&self, key: &str) -> AccessEpoch {
        let mode = crate::console::runtime_mode(&self.context);
        AccessEpoch {
            mode_stamp: crate::reserved::mode_stamp(&self.context),
            allowed: self.context.load_config().map(|cfg| crate::console::access_allowed(&cfg, &mode, key)).map_err(str::to_string),
            resets: crate::turn_ledger::reset_count(&self.context, key),
            token: crate::console::conversation_token(&self.context, key),
            policy: self.session_policy(key),
        }
    }

    /// A session created across an access change is never used: unbind it if it was bound, archive
    /// it in DSH and refuse the turn.
    fn discard_created_session(&mut self, key: &str, sid: &str, bound: bool) -> String {
        let mut failures = Vec::new();
        if bound {
            if let Err(error) = crate::session_map::retire_if(&self.context, key, sid) {
                failures.push(format!("解除映射失败：{error}"));
            }
        }
        self.store.sessions.remove(key);
        if let Err(error) = crate::dsh::archive_session(&self.rpc, sid) {
            failures.push(format!("归档新会话 {sid} 失败：{error}"));
        }
        println!("[session] 会话创建期间 {key} 已重置或权限已变化，丢弃新会话 {sid}");
        if failures.is_empty() {
            "会话创建期间已重置或权限已变化，丢弃新会话".to_string()
        } else {
            println!("[session] ⚠️ 丢弃新会话 {sid} 未完成：{}", failures.join("；"));
            format!("会话创建期间已重置或权限已变化，丢弃新会话；{}", failures.join("；"))
        }
    }

    /// JS `resolvePresetName(strict)`: the preset must be in DSH's list; an unknown list refuses too.
    fn require_preset(&self, preset: &str) -> Result<(), String> {
        let listed = self.rpc.call("agentPresets/list", serde_json::json!({}))?;
        let ids: Vec<String> = listed
            .get("presets")
            .and_then(|p| p.as_array())
            .map(|items| items.iter().filter_map(|p| p.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect())
            .unwrap_or_default();
        if !preset.is_empty() && ids.iter().any(|id| id == preset) {
            return Ok(());
        }
        Err(format!(
            "拒绝创建会话：模式 chat 需要 preset \"{preset}\"，但它不在 DSH 可用清单中（{}）；回退到 DSH 默认 preset 会把本地工具暴露给 QQ 群",
            ids.join(", ")
        ))
    }

    fn closed_preset(&self) -> Result<String, String> {
        let local = self.context.read_optional_text(RuntimeArea::Data, "mode.json")?
            .map(|s| serde_json::from_str::<serde_json::Value>(&s).map_err(|e| e.to_string()))
            .transpose()?.unwrap_or_default();
        let wanted = local.get("closedAgentPreset").and_then(|v| v.as_str()).unwrap_or("").trim();
        let settings = self.rpc.call("settings/describe", serde_json::json!({}))?;
        let default = settings.get("namespaces").and_then(|v| v.as_array()).and_then(|ns| ns.iter().find(|n| n["ns"] == "agent-presets"))
            .and_then(|n| n.get("user").and_then(|v| v.get("default")).or_else(|| n.get("value").and_then(|v| v.get("default"))))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let listed = self.rpc.call("agentPresets/list", serde_json::json!({}))?;
        let presets = listed.get("presets").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let default = if default.is_empty() { presets.iter().find(|p| p["isDefault"] == true).and_then(|p| p["id"].as_str()).unwrap_or("").to_string() } else { default };
        if wanted.is_empty() { return Ok(default); }
        if presets.is_empty() || presets.iter().any(|p| p["id"].as_str() == Some(wanted)) { Ok(wanted.to_string()) } else { Ok(default) }
    }

    /// Resolve the title of a freshly created session (group name through OneBot).
    fn prepare_title(&mut self) {
        let Some((sid, key)) = self.title_for.take() else { return };
        let group_name = key
            .strip_prefix("group:")
            .map(|id| {
                self.ob
                    .request("get_group_list", serde_json::json!({}))
                    .and_then(|v| v.get("data").cloned())
                    .and_then(|list| {
                        list.as_array().and_then(|items| {
                            items
                                .iter()
                                .find(|g| g.get("group_id").map(|v| v.to_string().trim_matches('"').to_string()).as_deref() == Some(id))
                                .and_then(|g| g.get("group_name").and_then(|n| n.as_str()).map(str::to_string))
                        })
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        self.pending_title = Some((sid, crate::chat::session_title(&key, &group_name)));
    }

    /// Send the pending session title (failure never matters).
    fn flush_title(&mut self) {
        if let Some((sid, title)) = self.pending_title.take() {
            let _ = self.rpc.call("session/rename", serde_json::json!({ "sessionId": sid, "title": title }));
        }
    }

    /// Cancel the turn of one known session (a scheduler cleanup after a reset or mode change):
    /// stop its DSH work; only while it is still the conversation's mapping does the conversation
    /// lose its token and every open turn (as `cancel`). A session that a newer one replaced only
    /// has its own open turns ended: the replacement keeps running.
    pub fn cancel_session(&mut self, key: &str, sid: &str) -> Result<(), String> {
        let stopped = crate::dsh::stop_session_work(&self.rpc, sid);
        if crate::session_map::lookup(&self.context, key)?.as_deref() == Some(sid) {
            crate::console::ensure_or_rotate_agent_token(&self.context, key, true)?;
        }
        let current = crate::session_map::lookup(&self.context, key)?;
        if let Ok(records) = crate::turn_ledger::read_records(&self.context) {
            let (turns, _) = crate::turn_ledger::fold(&records);
            for turn in turns.iter().filter(|t| {
                t.key == key && t.state.is_open() && (t.session_id == sid || (t.session_id.is_empty() && current.is_none()))
            }) {
                let _ = crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry().end("cancelled", "superseded");
            }
        }
        stopped.map(|_| ()).map_err(|e| format!("取消回合失败：{e}"))
    }

    /// 取消这个会话正在跑的回合（对应 DSH 的 `session/cancel`）。
    ///
    /// 用途：新来的是确定性触发（@ / 关键词 / 提问），而当前回合还没说出口、
    /// 又已经跑够久 —— 与其让 @ 排队 3 分钟（JS 那边实测 10:58→11:01），
    /// 不如把这一轮作废，用新消息重开。已经说过话的回合不取消（见 wake.rs 的判定）。
    pub fn cancel(&mut self, key: &str) -> Result<(), String> {
        match self.cancel_with_reason(key, "superseded")?.stop_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Report exactly the epoch minted by this cancellation so the scheduler can retain
    /// buffered work without adopting a later reset or revocation's token. A failed DSH stop is
    /// reported beside the minted epoch, never instead of it.
    pub fn cancel_with_reason(&mut self, key: &str, reason: &str) -> Result<Cancelled, String> {
        let sid = if self.context.is_isolated() {
            crate::session_map::lookup(&self.context, key)?
        } else {
            self.store.sessions.get(key).map(|r| r.session_id.clone())
        };
        let Some(sid) = sid else {
            return Ok(Cancelled::default()); // 还没建会话，没什么可取消的
        };
        if self.context.is_isolated() {
            // 被取消/被替代的回合不能再有迟到的结果：先清 inbox（DSH 的 cancel 保留队列）并取消，
            // 再换令牌，之后它的工具调用与发送都因令牌不符被拒；账本里把它记为已取消。
            // A diagnostic intent preserves the reason if DSH ends before cancel returns.
            // It does not finalize the turn: crash recovery must still reconcile live work.
            let open = crate::turn_ledger::read_records(&self.context).ok()
                .map(|records| crate::turn_ledger::fold(&records).0).unwrap_or_default();
            let pending_wait = crate::console::observation_wait_turn(&self.context, key).unwrap_or(None);
            let sleeping = reason == "turnTimeout" && open.iter().any(|t|t.key == key && t.state.is_open()
                && pending_wait.as_deref() == Some(t.id.as_str()));
            let terminal_reason = if sleeping { "observationSleep" } else { reason };
            for turn in open.iter().filter(|t| t.key == key && t.state.is_open()) {
                if crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry()
                    .verified("cancel-requested", terminal_reason).is_err() {
                    eprintln!("[turn] cancellation intent write failed; enforcing stop and revocation; recovery required");
                }
            }
            let stopped = crate::dsh::stop_session_work(&self.rpc, &sid);
            let token = crate::console::ensure_or_rotate_agent_token(&self.context, key, true)?;
            for turn in open.iter().filter(|t| t.key == key && t.state.is_open()) {
                let recorder = crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry();
                let result = if sleeping && stopped.is_ok() {
                    recorder.end_cancellation("completed", terminal_reason)
                } else if sleeping {
                    recorder.unknown("dsh-unreachable", "observation stop unconfirmed")
                } else { recorder.end_cancellation("cancelled", terminal_reason) };
                if result.is_err() { eprintln!("[turn] terminal ledger write failed; recovery required"); }
            }
            return Ok(Cancelled { token: Some(token), stop_error: stopped.err().map(|e| format!("取消回合失败：{e}")) });
        }
        match self.rpc.call(
            "session/cancel",
            serde_json::json!({ "sessionId": sid }),
        ) {
            Ok(_) => Ok(Cancelled::default()),
            Err(e) => Err(format!("取消回合失败：{e}")),
        }
    }

    /// Mode retirement already revoked the old epoch. Stale driver cleanup must
    /// never resolve the new mapping or rotate a replacement turn's token.
    pub fn cancel_reserved_epoch(&self, key: &str, token: &str) -> Result<(), String> {
        let epoch = crate::send_outbox::token_epoch(token);
        let (turns, _) =
            crate::turn_ledger::fold(&crate::turn_ledger::read_records(&self.context)?);
        let mut sessions = std::collections::BTreeSet::new();
        for turn in turns
            .iter()
            .filter(|t| t.key == key && t.epochs.contains(&epoch) && t.state.is_open())
        {
            if !turn.session_id.is_empty() {
                sessions.insert(turn.session_id.clone());
            }
            crate::turn_ledger::Recorder::attach(&self.context, &turn.id, key).without_retry()
                .end("cancelled", "reserved mode retired")?;
        }
        for sid in sessions {
            crate::dsh::stop_session_work(&self.rpc, &sid)?;
        }
        Ok(())
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
    pub fn share_minted_epoch(&mut self, slot: std::sync::Arc<std::sync::Mutex<Option<String>>>) { self.minted_slot = Some(slot); }
    /// Run `check` right before the chat turn begins; `Some(reason)` drops the turn.
    pub fn recheck_before_turn(&mut self, check: Box<dyn FnOnce() -> Option<String> + Send>) { self.before_turn = Some(check); }

    fn note_session(&self, sid: &str) {
        if let Some(turn) = &self.cur_turn {
            let _ = turn.session(sid);
        }
        if let Some(slot) = &self.session_slot {
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(sid.to_string());
        }
    }
    pub fn expect_dispatch_wake(&mut self, at: i64, previous: serde_json::Value) { self.dispatch_wake = Some((at, previous)); }
    pub fn expect_dispatch_deadline(&mut self, at: i64) { self.dispatch_deadline = Some(at); }
    pub fn dispatch_definitely_unsubmitted(&self) -> bool { self.dispatch_unsubmitted }

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
            let budget_sleep = outcome == "completed" && crate::console::turn_budget_ended(&self.context, &turn.key, &turn.id);
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
        let token = crate::console::ensure_or_rotate_agent_token(&self.context, key, true)?;
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

    /// 作废当前这次尝试：先清 inbox 并取消 DSH 里的回合，再换令牌。
    ///
    /// Returns the reason when DSH's work could not be stopped: then nothing proves the abandoned attempt is dead.
    fn abandon_attempt(&mut self, key: &str, sid: &str, why: &str) -> Result<(), String> {
        let stopped = crate::dsh::stop_session_work(&self.rpc, sid).map(|_| ());
        if let Err(error) = &stopped {
            println!("[turn] ⚠️ 停止 DSH 回合失败 {key}：{error}");
        }
        if let Err(error) = self.rotate_turn_token(key) {
            println!("[turn] ⚠️ 换令牌失败 {key}：{error}");
        }
        if stopped.is_ok() {
            if let Some(turn) = &self.cur_turn {
                let _ = turn.verified("attempt-abandoned", why);
            }
        }
        stopped
    }

    #[cfg(test)]
    pub(crate) fn abandon_attempt_for_test(&mut self, key: &str, sid: &str, after_rotation: impl FnOnce() + Send + 'static) -> Result<(), String> {
        self.after_abandon_rotation = Some(Box::new(after_rotation));
        self.abandon_attempt(key, sid, "fixture collector timeout")
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
            if crate::console::runtime_mode(&self.context) != chat.mode || !crate::chat::allowed_in_mode(&cfg, &chat.mode, key) {
                return Err("mode or permission changed".to_string());
            }
        }
        if self.chat.is_none() {
            let cfg = self.context.load_config().map_err(|e|e.to_string())?;
            if crate::console::runtime_mode(&self.context) != "reserved2" || !crate::console::key_allowed(&cfg,key) {
                return Err("mode or permission changed".to_string());
            }
        }
        if !self.chat.as_ref().is_some_and(|c| c.engine == crate::harness::Engine::Harness) {
            match crate::session_map::lookup(&self.context, key) {
                Ok(Some(current)) if current == sid => {},
                Ok(_) => return Err("会话映射已变化（重置或替换）".to_string()),
                Err(error) => return Err(error),
            }
        }
        match crate::console::conversation_token(&self.context, key) {
            Ok(Some(token)) if crate::send_outbox::token_epoch(&token) == self.turn_epoch => Ok(()),
            Ok(_) => Err("会话令牌已变化（重置、取消或被替代）".to_string()),
            Err(error) => Err(error),
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

    /// JS 的会话策略串：`JSON.stringify([mode, preset, closed-agent ? ownerQQ : null])`。
    /// 一并写入映射，回滚到 JS 后它认得这些会话而不是整体退役。未解析的 preset 与 JS 一样序列化为 `""`。
    fn session_policy(&self, key: &str) -> String {
        let mode = crate::console::runtime_mode(&self.context);
        let owner = if mode == "closed-agent" {
            match self.owner_qq.parse::<i64>() {
                Ok(n) => serde_json::json!(n),
                Err(_) => serde_json::json!(self.owner_qq),
            }
        } else {
            serde_json::Value::Null
        };
        let preset = self.preset_for(key);
        serde_json::json!([mode, preset, owner]).to_string()
    }

    /// The stored session policy names another mode than the current one (or, in chat mode, another
    /// preset): the session was made for different rules and must be replaced. `None` when there is
    /// no stored policy (older records) or it still fits. Reserved2 compares the mode only, because
    /// its preset depends on DSH's preset list.
    fn stale_policy(&self, key: &str) -> Result<Option<&'static str>, String> {
        let doc = crate::session_map::read(&self.context)?;
        let Some(stored) = doc.get("sessionPolicies").and_then(|p| p.get(key)).and_then(|p| p.as_str()) else {
            return Ok(None);
        };
        let mode = crate::console::runtime_mode(&self.context);
        let stored_mode = serde_json::from_str::<serde_json::Value>(stored)
            .ok()
            .and_then(|v| v.get(0).and_then(|m| m.as_str()).map(str::to_string));
        if stored_mode.as_deref() != Some(mode.as_str()) {
            return Ok(Some("mode-changed"));
        }
        if matches!(mode.as_str(), "chat" | "reserved" | "closed-agent") && stored != self.session_policy(key) {
            return Ok(Some("policy-changed"));
        }
        Ok(None)
    }

    /// 会话明确失效（DSH 不认它了）：撤销映射、停掉/归档旧会话、作废旧令牌、换新会话。
    fn replace_session(&mut self, key: &str, old: &str, reason: &str) -> Result<String, String> {
        crate::session_map::retire_if(&self.context, key, old)?;
        let _ = crate::dsh::stop_session_work(&self.rpc, old);
        let _ = crate::dsh::archive_session(&self.rpc, old);
        self.rotate_turn_token(key)?;
        let _ = crate::console::mark_conversation_unbootstrapped(&self.context, key);
        self.store.sessions.remove(key);
        self.failover_state().forget_session(old);
        let new = self.ensure_session(key)?;
        crate::turn_ledger::record_replacement(&self.context, key, old, &new, reason)?;
        self.note_session(&new);
        println!("[session] {key} 的 DSH 会话 {old} 已失效（{reason}），换用 {new}");
        Ok(new)
    }

    fn failover_state(&self) -> std::sync::MutexGuard<'_, FailoverState> {
        self.failover.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Give the session its model, once per session (JS `ensureChatModel`/`modelAppliedSessions`).
    ///
    /// The selection is applied again only when this session has none yet, after a failover step, or when the configured
    /// chain changed. It starts at the sticky candidate (the one a failed turn fell back to), tries each candidate under
    /// `RetryPolicy::SELECT_MODEL` (two attempts, one second apart, as JS) and moves on to the next; a 401 re-reads the
    /// credentials once per call without using up an attempt. Returns the candidate DSH now has selected.
    fn apply_model(
        &mut self,
        cfg: &serde_json::Value,
        key: &str,
        sid: &str,
        chain: &[failover::Candidate],
    ) -> Result<usize, String> {
        let signature = chain
            .iter()
            .map(|c| format!("{}/{}/{}", c.provider, c.model, c.effort))
            .collect::<Vec<_>>()
            .join("|");
        let start = {
            let mut state = self.failover_state();
            state.note_chain(sid, &signature);
            if !state.needs_apply(sid) {
                return Ok(state.selected_index(sid, chain.len()));
            }
            state.start_index(sid, chain.len())
        };
        let clock = SystemClock;
        let mut last_err = String::new();
        let mut relogged = false;
        for idx in start..chain.len() {
            let cand = &chain[idx];
            self.active_candidate = Some(cand.clone());
            let mut retry = RetryPolicy::SELECT_MODEL.start(clock.now_ms());
            loop {
                let attempt = retry.failures() + 1;
                match self.rpc.call(
                    "session/selectModel",
                    serde_json::json!({
                        "sessionId": sid,
                        "provider": cand.provider,
                        "model": cand.model,
                        "reasoningEffort": cand.effort
                    }),
                ) {
                    Ok(_) => {
                        self.failover_state().mark_applied(sid, idx);
                        if idx > 0 {
                            println!("[failover] 已降级到候选 {}/{}：{}", idx + 1, chain.len(), cand.label());
                            recovery_log::record(
                                &self.context,
                                Trigger::Turn,
                                Component::ModelChain,
                                "degraded",
                                Outcome::Ok,
                                &format!("{key} runs on candidate {}/{}: {}", idx + 1, chain.len(), cand.label()),
                            );
                        }
                        return Ok(idx);
                    }
                    Err(e) => {
                        last_err = format!("{}：{e}", cand.label());
                        println!(
                            "[failover] 设置模型失败 {}（第 {}/{} 次）：{}",
                            cand.label(),
                            attempt,
                            RetryPolicy::SELECT_MODEL.max_attempts,
                            crate::activity::redact(&self.context, &e)
                        );
                        recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "select-model-failed",
                            Outcome::Failed,
                            &format!("{}: {e}", cand.label()),
                        );
                        crate::dsh_health::note_call_failure(&self.context, cfg, &e);
                        // 401 = DSH 换了签名密钥（重启）：重读密钥后再试同一个候选，一次调用只做一次，不占尝试次数。
                        if e.contains("401") && !relogged {
                            relogged = true;
                            println!("[failover] 命中 401，重读密钥后重试");
                            match self.rpc.relogin() {
                                Ok(()) => continue,
                                Err(re) => last_err = format!("{}：{e}；重新登录也失败：{re}", cand.label()),
                            }
                        }
                        let now = clock.now_ms();
                        match retry.on_failure(now, Idempotency::Safe, crate::dependency::jitter_unit(now, idx as u64)) {
                            RetryDecision::Retry { after_ms, .. } => {
                                std::thread::sleep(std::time::Duration::from_millis(after_ms));
                            }
                            RetryDecision::GiveUp(_) => break,
                        }
                    }
                }
            }
            println!("[failover] {} 不可用，降级到下一个候选", cand.label());
        }
        recovery_log::record(
            &self.context,
            Trigger::Turn,
            Component::ModelChain,
            "chain-exhausted-at-select",
            Outcome::Failed,
            &format!("{key}: no candidate accepted the selection: {last_err}"),
        );
        Err(format!("所有候选模型都设置失败：{last_err}"))
    }

    /// 带 failover 的一轮：选模型 → 投 prompt → 失败换候选重投一次。
    ///
    /// **只在还没发出任何字节时重投**：这里发送发生在回合成功之后，
    /// 所以重投不会重复发言；一旦将来改成流式边说边发，这个前提就得重新审。
/// 读某个会话的 `lastAiReplyAt`（毫秒）。读不到返回 0。
///
/// 用途：判断"这一轮里模型有没有自己调发送工具"。发送路径（MCP → console →
/// OneBot）成功后会更新这个字段，所以它比任何内存状态都可靠。
fn last_ai_reply_at(context: &RuntimeContext, key: &str) -> i64 {
    let state = if context.is_isolated() {
        context.read_social_state().ok()
    } else {
        std::fs::read_to_string("C:/qqbot/qq-bridge/state/social-v2.json")
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    };
    state
        .and_then(|v| {
            v.get("conversations")
                .and_then(|c| c.get(key))
                .and_then(|c| c.get("lastAiReplyAt"))
                .and_then(|t| t.as_i64())
        })
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
        let path = PathBuf::from(format!("C:/qqbot/logs/{name}"));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())
    }
}

    pub async fn ask_with_failover(
        &mut self,
        key: &str,
        prompt: &str,
        timeout_secs: u64,
        cfg: &serde_json::Value,
    ) -> Result<String, String> {
        self.ask_with_failover_with_deadline(key, prompt, std::time::Duration::from_secs(timeout_secs), cfg).await
    }

    async fn ask_with_failover_with_deadline(
        &mut self,
        key: &str,
        prompt: &str,
        timeout: std::time::Duration,
        cfg: &serde_json::Value,
    ) -> Result<String, String> {
        self.failure_reported = false;
        self.refusals_recorded.clear();
        self.active_candidate = None;
        let result = self.ask_with_failover_inner(key, prompt, timeout, cfg).await;
        if let Err(error) = &result {
            if !self.failure_reported && !is_cancellation_error(error)
                && !error.contains("preset") && !error.contains("预设") {
                let candidate = self.active_candidate.clone().unwrap_or_else(|| failover::model_chain_for(cfg, key)[0].clone());
                self.report_backend_failure(cfg, key, "dsh", &candidate.provider, &candidate.model, "", error);
            }
        }
        // A failure to reach DSH marks it down (queued work waits for recovery); a call that went through marks it up.
        match &result {
            Ok(_) => crate::dsh_health::note_call_success(&self.context, cfg),
            Err(error) => crate::dsh_health::note_call_failure(&self.context, cfg, error),
        }
        result
    }

    fn report_backend_failure(&mut self, cfg: &serde_json::Value, key: &str, backend: &str, provider: &str, model: &str, code: &str, message: &str) {
        let failure = crate::model_failure::Failure::classify(code, message);
        self.failure_reported = true;
        if failure.class == "refusal" && self.refusals_recorded.insert((backend.into(), provider.into(), model.into())) {
            crate::model_failure::record_refusal(&self.context, key, backend, provider, model, &failure, &self.carried_messages);
        }
        note_model_error(&self.context, key, &failure.summary());
        crate::model_failure::notify(&self.context, cfg, key, backend, provider, model, &failure);
    }

    fn cli_fallback(&mut self, cfg: &serde_json::Value, key: &str, prompt: &str, timeout_secs: u64) -> Result<(String, String), String> {
        let context = self.context.clone();
        let result = crate::backend::cli_fallback_with_runtime_reporting(&context, cfg, key, prompt, timeout_secs, |id, error| {
            let (provider, model) = crate::model_failure::cli_identity(cfg, id);
            self.report_backend_failure(cfg, key, id, &provider, &model, "", error);
        });
        // The backend took the compatibility render: its recall was delivered.
        if result.is_ok() {
            crate::prompt::record_recall_hits(&self.context, &self.compat_recall);
        }
        result
    }

    /// JS `shouldAuditKey`: every conversation is audited except the owner's private chat in closed-agent mode.
    fn should_audit_key(&self, cfg: &serde_json::Value, key: &str) -> bool {
        !(crate::console::runtime_mode(&self.context) == "closed-agent" && crate::chat::is_owner_key(cfg, key))
    }

    /// JS `shouldAuditKey(key) && SENSITIVE_RE.test(text) ? '（含敏感信息，已隐藏）' : text`.
    fn hide_if_sensitive(&self, cfg: &serde_json::Value, key: &str, text: &str) -> String {
        let guard_cfg = self.context.load_config().unwrap_or_else(|_| cfg.clone());
        if self.should_audit_key(cfg, key) && configured_outbound_violation(&guard_cfg, text, &[]).is_some() {
            "（含敏感信息，已隐藏）".to_string()
        } else {
            text.to_string()
        }
    }

    /// JS `auditAndSend`: a reply stopped by the outbound sentinel is announced to the administrator (unless
    /// `security.interceptNotify` is `false`); the group sees nothing.
    fn alert_blocked_reply(&self, cfg: &serde_json::Value, key: &str) {
        let guard_cfg = self.context.load_config().unwrap_or_else(|_| cfg.clone());
        if !outbound_guard_enabled(&guard_cfg) || cfg.pointer("/security/interceptNotify") == Some(&serde_json::Value::Bool(false)) {
            return;
        }
        crate::admin_alert::notify_admin(
            &self.context,
            cfg,
            "blocked-reply",
            &format!("⚠️ 本条回复因疑似包含敏感信息（路径/凭据/会话令牌）被安全策略拦截，未发出。（{key}）"),
        );
        recovery_log::record(&self.context, Trigger::Turn, Component::Alert, "reply-blocked", Outcome::Skipped, &format!("{key}: a reply was stopped by the outbound sentinel"));
    }

    /// Retain the local activity/recovery diagnostics; the shared failure reporter owns the private alert.
    fn report_turn_error(&mut self, cfg: &serde_json::Value, key: &str, message: &str) {
        let safe = self.hide_if_sensitive(cfg, key, &crate::activity::slice(message, 500));
        println!("[turn] ⚠️ agent 处理出错 ({key})：{}", crate::activity::redact(&self.context, &safe));
        crate::activity::append(&self.context, &format!("{key} agent 处理出错：{}", crate::activity::slice(&safe, 80)));
        recovery_log::record(
            &self.context,
            Trigger::Turn,
            Component::ModelChain,
            "turn-error",
            Outcome::Failed,
            &format!("{key}: {safe}"),
        );
    }

    async fn ask_with_failover_inner(
        &mut self,
        key: &str,
        prompt: &str,
        timeout: std::time::Duration,
        cfg: &serde_json::Value,
    ) -> Result<String, String> {
        // Work on a delivery copy, after chat logging/storage and before either backend.
        let model_prompt = crate::model_view::strip_text_with_runtime(&self.context, prompt, cfg);
        let prompt = model_prompt.as_str();
        // CLI adapters accept seconds; the DSH collector retains the exact millisecond budget.
        let timeout_secs = timeout.as_secs().saturating_add(u64::from(timeout.subsec_nanos() > 0));
        if key.starts_with("ab:") {
            return Err("shadow isolation refused: shadow cannot use live or CLI failover".into());
        }
        self.last_report = None;
        take_model_error(&self.context, key);
        // ── 分段计时 ──
        //
        // 为什么加：用户反馈"刚唤醒也要反应半天"，但原来的日志只有裸 println、没有时间戳，
        // 算不出时间花在哪。这里把一轮拆成 6 段落盘（logs/turn-timing.jsonl）：
        //   cli    试 CLI 后端（opencode 免费档）
        //   sess   建/复用 DSH 会话
        //   model  session/selectModel（候选链逐个设）
        //   open   FollowStream 的 WS 握手
        //   accept session/prompt 被接受
        //   turn   模型真正干活到回合结束（含工具调用）
        // 不加这个就只能猜，而"猜"今天已经错过好几次了。
        let t0 = std::time::Instant::now();
        let mut seg_cli: i64 = 0;
        let mut seg_sess: i64 = 0;
        let mut seg_model: i64 = 0;
        let mut seg_open: i64 = 0;
        let mut seg_accept: i64 = 0;
        let mut seg_turn: i64 = 0;
        let mut attempts: usize = 0;
        let mut used_model: Option<String> = None;
        let mut prompt_chars = prompt.chars().count();
        // 会话级**后端**覆盖：`backends.sessions["group:xxx"] = "opencode-free"` 时，
        // 这个会话直接走 CLI 后端，不经过 DSH。
        //
        // 为什么需要它：opencode 的免费档只允许在它自己的客户端里用（直连 API 返回
        // FreeTierError），所以想白嫖就必须让 opencode CLI 当后端 —— 而 CLI 后端
        // 原本只在"DSH 全挂"时兜底，正常路径永远走不到。
        if let Some(id) = pinned_cli_backend(cfg, key) {
            // 灰度观测：CLI 后端的调用**不进 DSH 账本**（不经过 session/prompt），
            // 所以要单独记一行，否则"这个群花了多少、多快"完全看不见。
            //
            // 超时**单独收紧**：CLI 后端的热态是 2.7s，但偶尔会顶上 180s（实测一次端到端
            // 就是 180s 超时后回落 DSH，用户等了 296 秒）。CLI 是"能快就快、慢就让路"的
            // 角色，不该占满整个回合预算 —— 这里最多等 25s，超了立刻回落 DSH。
            // CLI 后端（opencode 免费档）在这个环境里的实测：
            //   · 简单问答 4.5~6.6s ✅
            //   · 需要调工具的提示词 25s 都不够（直接跑要 51s，且只烧 137 token
            //     —— 时间全在 agent 运行时开销上）
            // 所以给它 **10 秒**：够跑简单问答，跑不完就立刻回落 DSH，
            // 别让用户白等 30 秒。注意 backend.rs 里的 clamp 下限已改成 5 秒，
            // 否则这里设的数字会被抬回 30。
            let cli_timeout = timeout_secs.min(10);
            let started = std::time::Instant::now();
            let result = crate::backend::cli_fallback_by_id_with_runtime(
                &self.context, cfg, id, key, prompt, cli_timeout,
            );
            let elapsed_ms = started.elapsed().as_millis() as i64;
            let record = serde_json::json!({
                "ts": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                "key": key,
                "backend": id,
                "elapsedMs": elapsed_ms,
                "ok": result.is_ok(),
                "chars": result.as_ref().map(|(_, t)| t.chars().count()).unwrap_or(0),
                "err": result.as_ref().err().map(|error| crate::model_failure::Failure::classify("", error).summary()).unwrap_or_default(),
            })
            .to_string();
            let _ = Self::append_runtime_log(&self.context, "logs/opencode-trial.jsonl", &record);
            match result {
                Ok((used, text)) => {
                    println!("[backend] {key} 指定走 CLI 后端 {used}（来自 backends.sessions，{elapsed_ms}ms）");
                    return Ok(text);
                }
                Err(note) => {
                    let (provider, model) = crate::model_failure::cli_identity(cfg, id);
                    self.report_backend_failure(cfg, key, id, &provider, &model, "", &note);
                    return Err(note);
                }
            }
        }
        // A pinned CLI may run independently, but its fallback must not contact a DSH already
        // known to be down. Unpinned callers can safely park this unsubmitted turn for recovery.
        if crate::dsh_health::is_down(&self.context) {
            return Err("DSH 不可用，未提交到 DSH".to_string());
        }
        // 按会话取候选链：这个群配了独立模型就用它的，否则回落全局。
        seg_cli = t0.elapsed().as_millis() as i64;
        let chain = failover::model_chain_for(cfg, key);
        let mut sid = self.ensure_session(key)?;
        self.note_session(&sid);
        seg_sess = t0.elapsed().as_millis() as i64 - seg_cli;
        let mut prompt_text = prompt.to_string();
        // The memories the text in `prompt_text` carries as recall, counted when a submission of it is accepted.
        let mut delivered_recall = self.compat_recall.clone();
        // 连模型都设不上（DSH 不可达 / provider 全坏）：同样给 CLI 后端一次机会，
        // 否则这条最常见的"DSH 挂了"路径根本走不到下面的 failover 分支。
        //
        // DSH 明确说会话已不存在（重启丢失、归档）是「明确失效」：撤销映射、换新会话，
        // 旧会话的令牌随之作废；这与「状态不明」不同，后者绝不重建（ticket 10）。
        // The follow stream is opened right after the session exists and before the model is
        // selected (as the JS bridge does), and reused by the first attempt below.
        let mut pre_follow = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            FollowStream::open(&self.rpc.base, &self.rpc.cookie(), &sid),
        )
        .await
        {
            Ok(Ok(f)) => Some(f),
            _ => None,
        };
        let applied = match self.apply_model(cfg, key, &sid, &chain) {
            Err(e) if self.cur_turn.is_some() && crate::dsh::is_session_gone(&e) => {
                sid = self.replace_session(key, &sid, "dsh-not-found")?;
                pre_follow = None;
                prompt_text = patch_token_line(&prompt_text, &self.turn_token);
                self.apply_model(cfg, key, &sid, &chain)
            }
            other => other,
        };
        let mut idx = match applied {
            Ok(i) => i,
            Err(e) => {
                let candidate = self.active_candidate.clone().unwrap_or_else(|| chain[0].clone());
                self.report_backend_failure(cfg, key, "dsh", &candidate.provider, &candidate.model, "", &e);
                match self.cli_fallback(cfg, key, prompt, timeout_secs) {
                    Ok((backend_id, text)) => {
                        println!("[failover] 模型全设不上（{e}），改用 CLI 后端 {backend_id} 拿到回复");
                        return Ok(text);
                    }
                    Err(note) => return Err(format!("{e}；CLI 后端也没成：{note}")),
                }
            },
        };
        self.prepare_title();
        seg_model = t0.elapsed().as_millis() as i64 - seg_cli - seg_sess;

        loop {
            attempts += 1;
            // A model error noted by an earlier attempt belongs to that attempt: this attempt's own outcome decides what the
            // caller is told, so a stale note can never hide a different failure.
            take_model_error(&self.context, key);
            used_model = Some(format!("{}/{}", chain[idx].provider, chain[idx].model));
            self.active_candidate = Some(chain[idx].clone());
            // WS 握手也要有超时：`FollowStream::open` 里没有超时，
            // DSH 换密钥/重启的那一瞬间握手会一直挂着 —— 外面套的兜底超时也救不了，
            // 那层只能作用于 await 点，握手中卡住就一直不返回。
            let mut follow = match pre_follow.take() {
                Some(f) => f,
                None => match tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    FollowStream::open(&self.rpc.base, &self.rpc.cookie(), &sid),
                )
                .await
                {
                    Ok(Ok(f)) => f,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err("FollowStream 握手超时（20s）".to_string()),
                },
            };
            seg_open += t0.elapsed().as_millis() as i64 - seg_cli - seg_sess - seg_model;
            // Persist the tracked prompt id before submission. Rpc defaults requestId to it;
            // DSH echoes requestId in data.source.rpcId for exact recovery. A failed write refuses submission.
            let rpc_id = crate::dsh::mint_request_id();
            self.turn_validity(key, &sid)?;
            if self.context.is_isolated() && self.cur_turn.is_some() && self.chat.is_none() {
                follow.track_briefing(&self.context, key, &sid, &self.turn_token)?;
                if self.current_reason != "wakeReminder" {
                    (prompt_text, delivered_recall) = crate::prompt::prepare_dsh_wake(&self.context, cfg, key, &sid, &self.turn_token, &self.current_reason, &rpc_id, prompt)?;
                    // Refusal provenance follows the single packet actually delivered, not the
                    // compatibility full render that was built before session resolution.
                    self.carried_messages.clear();
                    crate::model_failure::merge_refs(&mut self.carried_messages,
                        crate::model_failure::snapshot_refs(&prompt_text).unwrap_or_default());
                    prompt_chars = prompt_text.chars().count();
                }
            }
            if let Some(turn) = &self.cur_turn {
                turn.attempt(&rpc_id, &sid)
                    .map_err(|e| format!("回合账本写入失败，不提交 prompt：{e}"))?;
            }
            let mut content = vec![serde_json::json!({"type": "text", "text": prompt_text})];
            if let Some(chat) = &self.chat { content.extend(chat.media_parts.iter().cloned()); }
            let sent = self.rpc.call_with_rpc_id(
                "session/prompt",
                serde_json::json!({
                    "sessionId": sid,
                    "mode": "queue",
                    "content": content
                }),
                &rpc_id,
            );
            // Chat mode: a slash command DSH runs itself answers directly and starts no model turn.
            if self.chat.is_some() {
                if let Some(command) = sent.as_ref().ok().and_then(|v| v.get("command")).filter(|c| c.is_object()) {
                    let text = command.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    follow.close().await;
                    if let Some(turn) = &self.cur_turn {
                        let _ = turn.accepted(&rpc_id);
                    }
                    self.last_report = Some(TurnReport { end_kind: "command".to_string(), send_tool_ok: false, command: Some(text) });
                    self.finish_turn("completed", "dsh-command");
                    return Ok(String::new());
                }
            }
            // 提交结果三种：被接受；明确没提交；不知道（连接断了、响应读不到）。
            // 第三种先去 DSH 核对，核对不了就暂停，绝不整轮重投。
            let submitted: Result<(), Submit> = match sent {
                Ok(v) if v.get("accepted").and_then(|a| a.as_bool()) == Some(true) => Ok(()),
                Ok(v) => Err(Submit::Refused(format!("prompt 未被接受：{v}"))),
                Err(e) if !prompt_error_needs_probe(self.cur_turn.is_some(), &e) => {
                    Err(Submit::Refused(format!("prompt 调用失败：{e}")))
                }
                Err(e) => match crate::dsh::probe_prompt(&self.rpc, &sid, std::slice::from_ref(&rpc_id)).await {
                    crate::dsh::PromptProbe::Turn(_) | crate::dsh::PromptProbe::Queued => Ok(()),
                    crate::dsh::PromptProbe::NotFound { proved: true }
                    | crate::dsh::PromptProbe::SessionGone
                    | crate::dsh::PromptProbe::RemovedFromQueue => {
                        Err(Submit::Refused(format!("prompt 调用失败（已向 DSH 核实未提交）：{e}")))
                    }
                    crate::dsh::PromptProbe::NotFound { proved: false } => Err(Submit::Unverified(format!(
                        "prompt 调用失败（{e}），DSH 日志不足以证明它没有被接受"
                    ))),
                    crate::dsh::PromptProbe::Unreachable(why) => Err(Submit::Unverified(format!(
                        "prompt 调用失败（{e}），核对 DSH 也失败：{why}"
                    ))),
                },
            };
            // 明确标注类型：所有分支都只产出 Err（成功分支直接 return），
            // 不标注的话编译器会推断出 `Result<!, String>`，后面的 unwrap_or_default 就炸。
            if !matches!(&submitted, Err(Submit::Refused(_))) { self.dispatch_unsubmitted = false; }
            let mut may_still_run = false;
            let mut had_send = false;
            // An attempt that ended in `turn/end` error: (code, detail, the model's message).
            let mut model_failure: Option<(String, String, String)> = None;
            let failed: Result<(), String> = match submitted {
                Err(Submit::Unverified(why)) => {
                    follow.close().await;
                    return self.pause_unknown(key, "prompt-unverified", &why);
                }
                Err(Submit::Refused(why)) => {
                    follow.close().await;
                    // The shared failure reporter below owns the owner-private notification.
                    Err(why)
                }
                Ok(()) => {
                    if let Some(turn) = &self.cur_turn {
                        let _ = turn.accepted(&rpc_id);
                    }
                    // DSH holds the prompt: the memories it recalled were used, once per accepted submission.
                    crate::prompt::record_recall_hits(&self.context, &delivered_recall);
                    // Chat mode: acknowledge once the prompt is in DSH's hands (not again on a retry).
                    if let Some(ack) = self.chat.as_mut().and_then(|c| c.ack.take()) {
                        self.send_chat_text(key, &sid, "ack", &ack);
                    }
                    // JS starts the group-title lookup without awaiting it. Preserve the live
                    // group-turn order: acknowledgement, pending UI rename, final reply.
                    self.flush_title();
                    let start_context = self.context.clone();
                    let start_turn = self.cur_turn.as_ref().map(|turn|turn.id.clone());
                    let start_key = key.to_string();
                    let on_start = move || {
                        if let Some(id)=start_turn.as_deref() {
                            crate::turn_ledger::Recorder::attach(&start_context,id,&start_key).started()?;
                        }
                        Ok(())
                    };
                    let completed_turn = self.cur_turn.clone();
                    let completed_rpc = rpc_id.clone();
                    let on_end = move |ended: &serde_json::Value| {
                        if ended.pointer("/reason/kind").and_then(|v| v.as_str()) == Some("completed") {
                            if let Some(turn) = &completed_turn {
                                turn.observe_completion(&completed_rpc)?;
                            }
                        }
                        Ok(())
                    };
                    let mut retired = None;
                    let reply = if self.chat.as_ref().is_some_and(|c| c.mode == "closed-agent") {
                        // A reset or mode change can retire the session even if DSH never sends
                        // turn/end. Do not hold the conversation queue until the turn timeout.
                        let completion = follow.next_turn_with_lifecycle(timeout, on_start, on_end);
                        tokio::pin!(completion);
                        let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
                        loop {
                            tokio::select! {
                                result = &mut completion => break result,
                                _ = tick.tick() => {
                                    // Mode/reset handlers hold this gate until retirement completes.
                                    // On contention keep waiting; do not race their partial state.
                                    if let Ok(_gate) = crate::closed_agent::lock(&self.context) {
                                        if let Err(reason) = self.turn_validity(key, &sid) {
                                            let already_ended = self.cur_turn.as_ref().and_then(|turn| {
                                                crate::turn_ledger::read_records(&self.context).ok().map(|records| {
                                                    let (turns, _) = crate::turn_ledger::fold(&records);
                                                    turns.iter().any(|entry| entry.id == turn.id && !entry.state.is_open())
                                                })
                                            }).unwrap_or(false);
                                            if already_ended { self.turn_final = true; }
                                            else { self.finish_turn_with_retry("cancelled", &reason, false); }
                                            retired = Some(reason);
                                            break Err("closed-agent turn retired".to_string());
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        follow.next_turn_with_lifecycle(timeout, on_start, on_end).await
                    };
                    #[cfg(test)]
                    if let Some(hook) = self.completion_observed_hook.as_mut() { hook(); }
                    seg_turn += t0.elapsed().as_millis() as i64
                        - seg_cli
                        - seg_sess
                        - seg_model
                        - seg_open
                        - seg_accept;
                    let ended = follow.last_ended.clone();
                    // Deterministic race seam: hold bookkeeping after a genuine end,
                    // while the independent hard-cap handler can still run.
                    #[cfg(feature = "parity-clock")]
                    if self.context.is_isolated()
                        && ended.pointer("/reason/kind").and_then(|v| v.as_str()) == Some("completed")
                    {
                        let bookkeeping = async {
                            if self.context.read_optional_text(RuntimeArea::Data, "fault-hold-collector-bookkeeping")?.is_some() {
                                self.context.write_atomic(RuntimeArea::Data, "collector-bookkeeping-ready", b"completed")?;
                                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                                while self.context.read_optional_text(RuntimeArea::Data, "fault-hold-collector-bookkeeping")?.is_some() {
                                    if tokio::time::Instant::now() >= deadline {
                                        return Err("collector bookkeeping test gate timed out".to_string());
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                }
                            }
                            Ok::<(), String>(())
                        }.await;
                        // Releasing the fixture gate can race its optional-file read. A
                        // bookkeeping path failure cannot replace the observed DSH result;
                        // still classify backend refusals/errors through the normal path.
                        if bookkeeping.is_err() {
                            eprintln!("[collector] bookkeeping barrier unavailable; continuing completion classification");
                        }
                    }
                    // 失败回合也要落账：token 已经花了（JS 同样在事件到达时就记）。
                    self.record_turn_usage(
                        key,
                        &sid,
                        &follow,
                        Some(&chain[idx].model),
                        Some(&chain[idx].provider),
                    );
                    had_send = follow.tools.iter().any(|t| is_send_tool(&t.name));
                    let send_tool_ok = follow.tools.iter().any(|t| is_send_tool(&t.name) && t.done && t.ok);
                    if let Some(turn) = &self.cur_turn {
                        if let Some(dsh_turn) = follow.turn_id() {
                            let _ = turn.dsh_turn(&rpc_id, dsh_turn);
                        }
                        let _ = turn.tools(follow.tools.iter().map(|t| t.to_json()).collect());
                    }
                    follow.close().await;
                    let end_kind = ended.pointer("/reason/kind").and_then(|k| k.as_str());
                    let classified_failure = reply.as_ref().ok().and_then(|text| {
                        failover::classify_turn_end(&ended).or_else(|| {
                            if end_kind.is_some_and(|kind| kind != "completed") || text.trim().is_empty() { return None; }
                            crate::backend::plain_output_failure(text).map(|f| (failover::FailureKind::TurnError, f.code.clone(), f.summary()))
                        })
                    });
                    if reply.is_err() || classified_failure.is_some() {
                        if let Some(turn) = &self.cur_turn {
                            if let Err(error) = turn.release_completion(&rpc_id) {
                                return self.pause_unknown(key, "completion-release-failed", &error);
                            }
                        }
                    }
                    let ledger_cancel = self.cur_turn.as_ref().and_then(|turn| {
                        // Fold only this turn's records since its begin (offset captured in begin_ledger_turn).
                        // A tail without the turn's `begun` (a replaced ledger) falls back to the whole ledger.
                        let folded = self.turn_ledger_offset
                            .and_then(|offset| crate::turn_ledger::fold_turn_since(&self.context, offset, &turn.id).ok().flatten())
                            .or_else(|| crate::turn_ledger::read_records(&self.context).ok().and_then(|records| {
                                crate::turn_ledger::fold(&records).0.into_iter().find(|t| t.id == turn.id)
                            }));
                        folded.and_then(|t| {
                            if t.state == crate::turn_ledger::State::Cancelled { Some(t.outcome_reason) }
                            else { t.finalization_claim().filter(|(claim, _)| *claim == "cancel-requested")
                                .map(|(_, reason)| reason.to_string()) }
                        })
                    });
                    let observation_sleep = ledger_cancel.as_deref() == Some("observationSleep");
                    let cancel_reason = if observation_sleep { None } else if let Some(reason) = ledger_cancel.as_deref() {
                        Some(match reason {
                            "turnTimeout" => "turnTimeout",
                            "superseded" => "superseded",
                            _ => "turnCancelled",
                        })
                    } else if retired.is_some() { Some("turnInvalidated") }
                    else if matches!(end_kind, Some("aborted" | "cancelled")) { Some("dshAborted") }
                    else if reply.as_ref().err().is_some_and(|e| e.starts_with("等回复超时")) { Some("collectorTimeout") }
                    else if reply.is_err() { Some("collectorFailure") }
                    else { None };
                    Self::write_turn_timing(&self.context, serde_json::json!({
                        "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64).unwrap_or(0), "key":key,
                        "ok":reply.is_ok() && cancel_reason.is_none() && failover::classify_turn_end(&ended).is_none(),
                        "endKind":end_kind,"cancelled":cancel_reason.is_some(),"cancelReason":cancel_reason,
                        "turnId":self.cur_turn.as_ref().map(|turn| turn.id.as_str()),
                        "attemptRpcId":rpc_id,
                        "model":used_model,"attempts":attempts,"promptChars":prompt_chars,
                        "cliMs":seg_cli,"sessMs":seg_sess,"modelMs":seg_model,"openMs":seg_open,
                        "acceptMs":seg_accept,"turnMs":seg_turn,"totalMs":t0.elapsed().as_millis() as i64,
                    }));
                    if let Some(reason) = retired {
                        return Err(format!("回合已取消：{reason}"));
                    }
                    if let Some(reason) = ledger_cancel {
                        // The independent cap/preemption handler owns terminal settlement after
                        // revocation (Recorder rejects our competing completion/failure while its
                        // claim stands). DSH's aborted end is still a successful transport result.
                        self.turn_final = true;
                        if observation_sleep { return Ok(String::new()); }
                        return Err(format!("回合已取消：{reason}"));
                    }
                    match reply {
                        Ok(text) => match classified_failure {
                            Some((kind, code, detail)) => {
                                let message = ended
                                    .pointer("/reason/error/message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("未知错误")
                                    .to_string();
                                note_model_error(&self.context, key, &message);
                                // The model's words reach logs, the ledger and the console output through this text: redacted here.
                                let failure = format!(
                                    "回合失败（{} {} {}）",
                                    kind.as_str(),
                                    crate::activity::redact(&self.context, &code),
                                    crate::activity::redact(&self.context, &detail)
                                );
                                model_failure = Some((code, detail, message));
                                Err(failure)
                            }
                            None => {
                                self.last_report = Some(TurnReport {
                                    end_kind: ended
                                        .pointer("/reason/kind")
                                        .and_then(|k| k.as_str())
                                        .unwrap_or("completed")
                                        .to_string(),
                                    send_tool_ok,
                                    command: None,
                                });
                                self.finish_attempt("completed", "ok", Some(&rpc_id), true);
                                if ended.pointer("/reason/kind").and_then(|k| k.as_str()) == Some("completed") {
                                    self.failover_state().on_completed(key, &sid);
                                }
                                return Ok(text);
                            }
                        },
                        Err(e) => {
                            // 等回复超时 / 流断开：DSH 里这一轮可能还在跑。
                            may_still_run = true;
                            Err(e)
                        }
                    }
                }
            };

            let why = match failed {
                Err(e) => e,
                Ok(()) => unreachable!("成功分支已经提前 return"),
            };
            let (failure_code, failure_detail) = model_failure.as_ref()
                .map(|(code, detail, _)| (code.as_str(), detail.as_str())).unwrap_or(("", why.as_str()));
            self.report_backend_failure(cfg, key, "dsh", &chain[idx].provider, &chain[idx].model, failure_code, failure_detail);
            // 等回复超时 / 流断开：DSH 里这一轮可能还在跑，结果未知。绝不把整条 prompt 交给下一个候选再投一遍
            // （ticket 25 决定 Q3，ticket 10 的未知结果合同）：先清队列并取消 DSH 里的回合、再换令牌，让它之后迟到的
            // 工具调用和发送全部因令牌不符被拒；取消核实不了就暂停交管理员，否则按失败收尾。只有明确的
            // `turn/end` error 或 selectModel/prompt 被拒才换候选。
            if may_still_run {
                if self.cur_turn.is_some() {
                    if let Err(stop) = self.abandon_attempt(key, &sid, &why) {
                        recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "turn-unverified",
                            Outcome::Unknown,
                            &format!("{key}: {why}; stopping the DSH turn failed: {stop}"),
                        );
                        return self.pause_unknown(key, "turn-unverified", &format!("{why}；停止 DSH 回合失败：{stop}"));
                    }
                }
                recovery_log::record(
                    &self.context,
                    Trigger::Turn,
                    Component::ModelChain,
                    "turn-abandoned",
                    Outcome::Unknown,
                    &format!("{key}: {why}; the prompt is not delivered again"),
                );
                return Err(format!("{why}（回合状态未知，已取消该回合，不换候选重投）"));
            }
            // 重投之前核对这一轮是否仍然有效：会话被重置/替换、令牌被换过都意味着它已被取消，
            // 不能靠重投把它救回来。
            if let Err(invalid) = self.turn_validity(key, &sid) {
                if let Some((_, _, message)) = &model_failure {
                    self.report_turn_error(cfg, key, message);
                }
                self.finish_turn("cancelled", &invalid);
                return Err(format!("回合已取消：{invalid}（此前失败：{why}）"));
            }
            let kind = failover::classify_message(&why);
            // 这一轮里模型已经发过消息就不再换候选重投：重投会让它再发一遍。
            let already_sent = had_send || self.turn_has_send_batch(key);
            // Preserve local failover diagnostics. Private alerts contain metadata only.
            let (code, detail) = match &model_failure {
                Some((code, detail, _)) => (
                    crate::activity::redact(&self.context, code),
                    crate::activity::redacted_preview(&self.context, &detail, 200),
                ),
                None => ("prompt-refused".to_string(), crate::activity::redacted_preview(&self.context, &why, 200)),
            };
            let action = self.failover_state().plan(key, &sid, chain.len(), already_sent);
            let outcome = match action {
                failover::FailoverAction::Retry { idx: next } => {
                    println!(
                        "[failover] {key} 失败（{} {}），换 {} 重投",
                        kind.as_str(),
                        crate::activity::redacted_preview(&self.context, &why, 80),
                        chain[next].label()
                    );
                    recovery_log::record(
                        &self.context,
                        Trigger::Turn,
                        Component::ModelChain,
                        "failover-step",
                        Outcome::Ok,
                        &format!("{key}: {code} {detail}; next candidate {}", chain[next].label()),
                    );
                    self.failover_state().on_failover(key, &sid, next);
                    Ok(next)
                }
                failover::FailoverAction::GiveUp(g) => {
                    if g == failover::GiveUp::LastCandidate {
                            recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "candidates-exhausted",
                            Outcome::Failed,
                            &format!("{key}: {code} {detail}"),
                        );
                    }
                    Err(g)
                }
            };
            if let Some((_, _, message)) = &model_failure {
                self.report_turn_error(cfg, key, message);
            }
            match outcome {
                Ok(_) => match self.apply_model(cfg, key, &sid, &chain) {
                    Ok(next) => {
                        idx = next;
                        continue;
                    }
                    Err(select_error) => {
                        // Selection failed: retain the local diagnostic and allow the configured CLI fallback.
                        let safe = crate::activity::redacted_preview(&self.context, &select_error, 200);
                            recovery_log::record(
                            &self.context,
                            Trigger::Turn,
                            Component::ModelChain,
                            "candidates-exhausted",
                            Outcome::Failed,
                            &format!("{key}: the next candidate could not be selected: {safe}"),
                        );
                        return self.exhausted_chain(cfg, key, prompt, timeout_secs, format!("{why}；切换候选后设置模型失败：{select_error}"), "select-failed");
                    }
                },
                Err(failover::GiveUp::AlreadySent) => {
                    return Err(format!("{why}（模型已发送过消息，不重投、不回落）"));
                }
                Err(g) => {
                    // DSH 的候选链全挂了 —— 再试 `backends` 里配的 CLI 后端（R4，仅 `backends.failoverFallback` 开启时才会跑）。
                    // 只有整条 DSH 链都失败才走到这里，正常回合不会因此多花时间。
                    let why_full = format!("{}（{}）", why, g.as_str());
                    return self.exhausted_chain(cfg, key, prompt, timeout_secs, why_full, g.as_str());
                }
            }
        }
    }

    /// The DSH candidate chain failed: the opt-in CLI backend fallback gets its chance (`backends.failoverFallback`), else
    /// the turn fails with `why_full`.
    fn exhausted_chain(
        &mut self,
        cfg: &serde_json::Value,
        key: &str,
        prompt: &str,
        timeout_secs: u64,
        why_full: String,
        reason: &str,
    ) -> Result<String, String> {
        match self.cli_fallback(cfg, key, prompt, timeout_secs) {
            Ok((backend_id, text)) => {
                println!("[failover] DSH 全挂，改用 CLI 后端 {backend_id} 拿到回复");
                recovery_log::record(
                    &self.context,
                    Trigger::Turn,
                    Component::ModelChain,
                    "cli-fallback",
                    Outcome::Ok,
                    &format!("{key}: DSH chain failed ({reason}); answered by CLI backend {backend_id}"),
                );
                Ok(text)
            }
            Err(note) => Err(format!("{why_full}；CLI 后端也没成：{note}")),
        }
    }
    /// 投一句 prompt 并等到这一轮结束，返回回复正文。
    ///
    /// **先订阅再投递**：反过来的话回合可能在订阅建立前就结束，实时流不重放。
    pub async fn ask(&mut self, key: &str, prompt: &str, timeout_secs: u64) -> Result<String, String> {
        let sid = self.ensure_session(key)?;
        self.prepare_title();
        let mut follow = FollowStream::open(&self.rpc.base, &self.rpc.cookie(), &sid).await?;
        let accepted = self.rpc.call(
            "session/prompt",
            serde_json::json!({
                "sessionId": sid,
                "mode": "queue",
                "content": [{ "type": "text", "text": prompt }]
            }),
        )?;
        if accepted.get("accepted").and_then(|a| a.as_bool()) != Some(true) {
            return Err(format!("prompt 未被接受：{accepted}"));
        }
        self.flush_title();
        let reply = follow
            .next_turn(std::time::Duration::from_secs(timeout_secs))
            .await;
        // turn/end 之后 usage 已收集完：先落账，再关流。
        self.record_turn_usage(&key, &sid, &follow, None, None);
        follow.close().await;
        reply
    }

    /// 回合结束后把 usage 落账（Rust 接管写账本的唯一入口）。
    ///
    /// 写失败只打印一行，绝不影响回复 —— JS 那边也是 try/catch 吞掉，
    /// 账本坏了不能连带把 QQ 回复卡住。没有 usage 明细时写 `b:null`，不编造数字。
    fn record_turn_usage(
        &self,
        key: &str,
        sid: &str,
        follow: &FollowStream,
        fallback_model: Option<&str>,
        fallback_provider: Option<&str>,
    ) {
        // 事件里的 model/selection 不一定先于 usage 到达；用刚生效的候选兜底，
        // 避免账本出现 model:null 把单价算成兜底价。
        let model = follow
            .model
            .clone()
            .or_else(|| fallback_model.map(|s| s.to_string()));
        let provider = follow
            .provider
            .clone()
            .or_else(|| fallback_provider.map(|s| s.to_string()));
        let records = turn_records(
            key,
            sid,
            &follow.turn_usage,
            follow.last_seq,
            follow.turn_id(),
            &model,
            &provider,
            now_ms(),
        );
        // 开场快照里的历史采样与 tokenUsage 基线也一并核对：已在账本里的按序号跳过，缺的补记。
        let mut rows = records;
        rows.extend(crate::ledger::harvest_candidates(Some(key), sid, &follow.snapshot.harvest, now_ms()));
        if let Err(e) = crate::ledger::append_planned(&self.context, &rows) {
            eprintln!("[ledger] 写账本失败：{e}");
        }
    }

    /// 把回复发回 QQ（先看静默标记，再过哨兵）。
    ///
    /// `[SILENT]` 是模型"我选择不说"的合法出口（JS `isSilentMarker`）。
    /// 没有这一步的话，整段 `[SILENT]` 会被原样发到群里 —— 那是最尴尬的一种 bug：
    /// 它只会出现在"AI 决定闭嘴"的时刻，平时测不到。
    pub fn reply(&mut self, target: &Target, text: &str) -> Result<Vec<serde_json::Value>, String> {
        if is_silent_marker(text) {
            println!("[pipeline] {} 选择静默（[SILENT]），不发送", target.key());
            return Ok(Vec::new());
        }
        let cfg = self.context.load_config().unwrap_or_default();
        if let Some(why) = configured_outbound_violation(&cfg, text, &self.secrets) {
            return Err(format!("被出站哨兵拦下：{why}"));
        }
        self.ob.send_text(target, text, &self.tune, &mut self.limiter)
    }

    /// 完整一轮：投 prompt → 等回复 → 发回 QQ。
    ///
    /// `live=false` 时只走前半段并打印，不发送（影子模式）。
    // 保留「不带 failover 的简单路径」：排障时能一眼看到单候选下的行为，
    // 也方便以后写"只想跑一个候选"的测试。主流程一律走 handle_with_failover。
    #[allow(dead_code)]
    pub async fn handle(&mut self, target: &Target, prompt: &str) -> Result<Option<String>, String> {
        let key = target.key();
        let key: &str = &key;
        let reply = self.ask(&key, prompt, 180).await?;
        let trimmed = reply.trim().to_string();
        if trimmed.is_empty() {
            return Err("agent 返回了空回复".to_string());
        }
        if !self.live {
            println!("[pipeline] dry-run {key}：回复 {} 字，未发送", trimmed.chars().count());
            return Ok(Some(trimmed));
        }
        let sent = self.reply(target, &trimmed)?;
        println!("[pipeline] {key}：已发送 {} 条", sent.len());
        Ok(Some(trimmed))
    }

    /// 与 `handle` 相同，但走带 failover 的 ask（主路径用这个）。
    ///
    /// 单独开一个方法而不是改 `handle`：`handle` 是「单候选」的最简路径，
    /// 诊断/探针还会用到；主路径要的是候选链 + 失败重投。
    pub async fn handle_with_failover(
        &mut self,
        target: &Target,
        prompt: &str,
        cfg: &serde_json::Value,
    ) -> Result<Option<String>, String> {
        let key = target.key();
        let key: &str = &key;
        let reply = self.ask_with_failover(&key, prompt, 180, cfg).await?;
        let trimmed = reply.trim().to_string();
        if trimmed.is_empty() {
            return Err("agent 返回了空回复".to_string());
        }
        if !self.live {
            println!(
                "[pipeline] dry-run {key}：回复 {} 字，未发送",
                trimmed.chars().count()
            );
            return Ok(Some(trimmed));
        }
        let sent = self.reply(target, &trimmed)?;
        println!("[pipeline] {key}：已发送 {} 条", sent.len());
        Ok(Some(trimmed))
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
        let refs = if pinned_cli_backend(cfg, &key).is_some() { cli_refs } else { dsh_refs };
        if !self.context.is_isolated() {
            crate::console::begin_legacy_wait_turn(&self.context, &key)?;
            let deadline = self.dispatch_deadline.take().unwrap_or_else(||crate::turn_ledger::now_ms()
                .saturating_add(crate::wake::turn_budget(cfg).hard_cap_ms));
            crate::console::set_legacy_wait_deadline(&self.context, &key, deadline)?;
            self.carried_messages = refs.unwrap_or_default();
            return self.run_turn_inner(target, dsh_prompt, cli_prompt, cfg).await;
        }
        let key = target.key();
        let _update_turn = crate::release::turn_guard(&self.context).await?;
        let token = self.begin_ledger_turn(&key)?;
        let dispatched_at = self.dispatch_wake.as_ref().map(|(at, _)|*at).unwrap_or(self.turn_began);
        let cap_ms = crate::wake::turn_budget(cfg).hard_cap_ms;
        let deadline = self.dispatch_deadline.take().unwrap_or_else(||dispatched_at.saturating_add(cap_ms));
        // Engaged turns measure the cap from her last send (capMs), bounded by the dispatch ceiling.
        let ceiling = deadline.saturating_sub(cap_ms).saturating_add(crate::wake::turn_ceiling_ms(cfg)).max(deadline);
        if let Err(error) = crate::turn_ledger::append_record(&self.context,
            &serde_json::json!({"k":"budget","turn":self.cur_turn.as_ref().unwrap().id,"t":self.turn_began,"deadline":deadline,
                "capMs":cap_ms,"ceiling":ceiling})) {
            self.end_ledger_turn(Err(&error));
            return Err(error);
        }
        if let Some(refs) = refs { self.carried_messages = refs; }
        if let Some((at, previous)) = self.dispatch_wake.take() {
            let claim = serde_json::json!({"at":at,"previous":previous});
            let durable = self.cur_turn.as_ref().unwrap().wake_claim(&claim)
                .and_then(|_| crate::console::consume_reserved2_deadline(&self.context, &key, &token, at, &previous));
            if let Err(error) = durable {
                self.end_ledger_turn(Err(&error));
                return Err(error);
            }
        }
        let dsh_prompt = patch_token_line(dsh_prompt, &token);
        let result = self.run_turn_inner(target, &dsh_prompt, cli_prompt, cfg).await;
        self.end_ledger_turn(result.as_ref().map(|_| ()).map_err(String::as_str));
        result
    }

    /// Open the ledger entry of a turn (selected runtime). Refuses, recording the wake as `held`,
    /// while the conversation has a paused or unreconciled turn. Returns the agent token in force.
    fn begin_ledger_turn(&mut self, key: &str) -> Result<String, String> {
        self.begin_ledger_turn_for_engine(key, crate::harness::Engine::Dsh)
    }

    fn begin_ledger_turn_for_engine(&mut self, key: &str, engine: crate::harness::Engine) -> Result<String, String> {
        let inbound = std::mem::take(&mut self.next_inbound);
        self.refusals_recorded.clear();
        self.carried_messages = inbound.iter().filter_map(|key| key.rsplit_once("|message:")
            .map(|(_, id)| serde_json::json!({"messageId":id}))).collect();
        let reason = std::mem::take(&mut self.next_reason);
        self.current_reason = reason.clone();
        let records = crate::turn_ledger::read_records(&self.context)?;
        let (turns, _) = crate::turn_ledger::fold(&records);
        if let Some(blocking) = crate::turn_ledger::blocking_turn(&turns, key) {
            let _ = crate::turn_ledger::append_record(
                &self.context,
                &serde_json::json!({
                    "k": "held", "t": crate::turn_ledger::now_ms(), "key": key,
                    "inbound": inbound, "reason": reason, "blockedBy": blocking.id,
                }),
            );
            return Err(format!(
                "{HELD_ERROR_PREFIX}会话 {key} 有暂停或恢复中的回合 {}，本次唤醒已记录（管理端 /api/turns）",
                blocking.id
            ));
        }
        let token = crate::console::ensure_or_rotate_agent_token(&self.context, key, false)?;
        if self.dispatch_token.take().is_some_and(|expected| expected != token) {
            return Err("queued wake token invalidated".to_string());
        }
        let epoch = crate::send_outbox::token_epoch(&token);
        let ledger_offset = crate::turn_ledger::ledger_len(&self.context).ok();
        let recorder =
            crate::turn_ledger::Recorder::begin(&self.context, key, engine.id(), &reason, &epoch, &inbound, None)?;
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
            Err(error) => self.finish_turn("failed", error),
        }
        self.cur_turn = None;
        self.turn_ledger_offset = None;
    }

    // ── chat mode (ticket 12) ─────────────────────────────────────────────────────

    /// The checks JS `captureSendGuard` makes right before each part is sent: still chat mode,
    /// target still allowed, session and token of this turn still the current ones.
    fn chat_guard(&self, key: &str, sid: &str) -> Result<(), String> {
        let cfg = self.context.load_config().map_err(|e| e.to_string())?;
        if let Some(authority) = self.chat.as_ref().and_then(|c| c.reserved.as_ref()) {
            authority.check_generation(&self.context, key)?;
            if crate::console::chat_role_state(&self.context)?["mode"] == "silent"
                && !crate::chat::is_owner_key(&cfg, key)
            {
                return Err("静默模式已启用".into());
            }
        } else if self.chat.as_ref().is_none_or(|chat| crate::console::runtime_mode(&self.context) != chat.mode) {
            return Err("模式已变化".to_string());
        }
        let mode = crate::console::runtime_mode(&self.context);
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
                "{key} [reserved2] AI 内部输出：{}{}",
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
        let delay = self.chat.as_ref().map(|c| c.delay_ms).unwrap_or(0);
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
            crate::chat::send_recorded(
                &self.context,
                &self.ob,
                key,
                &self.turn_epoch,
                purpose,
                text,
                delay,
                &|| self.chat_guard(key, sid),
            )
        };
        if let Some(error) = &report.error {
            println!("[chat] {key} 发送未完成：{error}");
        }
        report
    }

    /// One chat turn: ledger entry, DSH turn on the chat preset session, then the automatic reply.
    /// Failures stay local (ledger, log); nothing internal is sent to the conversation.
    pub async fn run_chat_turn(
        &mut self,
        key: &str,
        prompt: &str,
        ack: Option<String>,
        cfg: &serde_json::Value,
        inbound: &[String],
        media: &[serde_json::Value],
        mode: &str,
        owner_started: bool,
    ) -> Result<(), String> {
        if !self.context.is_isolated() {
            return Err("chat 模式只在选定运行环境中运行".to_string());
        }
        let engine = crate::harness::Engine::selected(cfg, key)?;
        if engine == crate::harness::Engine::Harness && !matches!(mode, "chat" | "closed-agent") {
            return Err("harness supports chat and owner-private closed-agent modes only".into());
        }
        self.preset_chat = crate::chat::chat_preset(cfg);
        self.owner_qq = crate::chat::owner_qq(cfg);
        if mode == "closed-agent" && engine == crate::harness::Engine::Dsh { self.preset_closed = self.closed_preset()?; }
        self.note_turn("chat", inbound);
        let context = self.context.clone();
        let media_cfg = cfg.clone();
        let media = media.to_vec();
        let media_parts = tokio::task::spawn_blocking(move || {
            crate::media::model_parts(&context, &media_cfg, &media)
        })
        .await
        .map_err(|_| "图片获取失败".to_string())?;
        let _update_turn = crate::release::turn_guard(&self.context).await?;
        // Recheck after media and update admission: reset/revocation while waiting ends
        // the queued job before it can take the replacement session's current epoch.
        if let Some(reason) = self.before_turn.take().and_then(|check| check()) {
            return Err(reason);
        }
        self.begin_ledger_turn_for_engine(key, engine)?;
        self.chat = Some(ChatRun {
            engine,
            reserved: None,
            mode: mode.to_string(),
            silent: false,
            ack,
            delay_ms: crate::chat::send_delay_ms(cfg),
            media_parts,
            owner_started,
        });
        let result = self.run_chat_inner(key, prompt, cfg).await;
        self.chat = None;
        self.end_ledger_turn(result.as_ref().map(|_| ()).map_err(String::as_str));
        result
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
        if crate::harness::Engine::selected(cfg, key)? != crate::harness::Engine::Dsh {
            return Err("harness supports chat and owner-private closed-agent modes only".into());
        }
        authority.check(&self.context, key)?;
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
        let _update_turn = crate::release::turn_guard(&self.context).await?;
        authority.check(&self.context, key)?;
        self.begin_ledger_turn(key)?;
        let ack = if authority.social() || silent {
            None
        } else {
            crate::chat::ack_message(cfg)
        };
        self.chat = Some(ChatRun {
            engine: crate::harness::Engine::Dsh,
            reserved: Some(authority),
            mode: "reserved".to_string(),
            silent,
            ack,
            delay_ms: crate::chat::send_delay_ms(cfg),
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
    async fn ask_harness(&mut self, key: &str, prompt: &str, cfg: &serde_json::Value) -> Result<String, String> {
        use crate::harness::{Harness, ModelRoute, TurnRequest};
        self.chat_guard(key, "")?;
        let route = ModelRoute::from_runtime(&self.context, cfg)?;
        let model = route.model_name().to_string();
        let harness = Harness::open(&self.context)?;
        let recorder = self.cur_turn.as_ref().ok_or("harness requires a durable bridge turn")?;
        recorder.attempt(&format!("harness:{}", recorder.id), "")?;
        let mut turn = harness.admit(TurnRequest {
            id: recorder.id.clone(), conversation: key.to_string(), generation: self.turn_epoch.clone(),
            prompt: prompt.to_string(),
            profile: if self.chat.as_ref().is_some_and(|c| c.mode == "closed-agent") { "closed-agent".into() } else { self.preset_chat.clone() }, route,
            limits: crate::harness::Limits::from_config(cfg),
            tools: crate::harness::Tools::capture(&self.context, cfg, key, &self.turn_epoch)?,
            deadline: std::time::Duration::from_secs(turn_timeout_secs(cfg)),
        })?;
        self.dispatch_unsubmitted = false;
        recorder.accepted(&format!("harness:{}", recorder.id))?;
        if let Some(ack) = self.chat.as_mut().and_then(|c| c.ack.take()) {
            self.send_chat_text(key, "", "ack", &ack);
        }
        let result = loop {
            tokio::select! {
                result = turn.wait() => break result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    if self.chat_guard(key, "").is_err() {
                        turn.cancel();
                        break turn.wait().await;
                    }
                }
            }
        };
        for reference in turn.message_refs() {
            if !self.carried_messages.contains(&reference) {
                self.carried_messages.push(reference);
            }
        }
        match turn.status() {
            crate::harness::Status::Paused { .. } => self.turn_final = true,
            crate::harness::Status::Cancelled => self.finish_turn("cancelled", "harness turn cancelled"),
            _ => {}
        }
        let text = match result {
            Ok(text) => text,
            Err(error) => {
                if error.starts_with("model/backend ") || error == "harness model deadline exceeded" {
                    self.report_backend_failure(cfg, key, "harness", "openai", &model, "", &error);
                }
                return Err(error);
            }
        };
        self.chat_guard(key, "")?;
        self.last_report = Some(TurnReport { end_kind: "completed".into(), send_tool_ok: turn.sent_tool_reply(), ..Default::default() });
        Ok(text)
    }

    async fn run_chat_inner(&mut self, key: &str, prompt: &str, cfg: &serde_json::Value) -> Result<(), String> {
        let mut cfg_dsh = cfg.clone();
        if let Some(b) = cfg_dsh.get_mut("backends").and_then(|v| v.as_object_mut()) {
            b.remove("sessions");
        }
        let delivered =
            crate::prompt::wrap_for_delivery_with_runtime(&self.context, prompt, cfg, crate::parity_clock::now_ms());
        let text = if self.chat.as_ref().is_some_and(|c| c.engine == crate::harness::Engine::Harness) {
            self.ask_harness(key, &delivered, cfg).await?
        } else {
            self.ask_with_failover(key, &delivered, turn_timeout_secs(cfg), &cfg_dsh).await?
        };
        let report = self.last_report.take().unwrap_or_default();
        if self.chat.as_ref().is_some_and(|c|c.silent) { return Ok(()); }
        let sid = crate::session_map::lookup(&self.context, key)?.unwrap_or_default();
        let reply_cfg = self.context.load_config().unwrap_or_else(|_| cfg.clone());
        let audit = outbound_guard_enabled(&reply_cfg)
            && (!self.chat.as_ref().is_some_and(|c| c.mode == "closed-agent") || !crate::chat::is_owner_key(cfg, key));
        let mut secrets = self.secrets.clone();
        if let Ok(Some(token)) = crate::console::conversation_token(&self.context, key) {
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
                    &format!("{key} agent 回复被拦截（疑似敏感信息{}）", if has_token { "/会话令牌" } else { "" }),
                );
                self.alert_blocked_reply(cfg, key);
                println!("[chat] {key} 回复被安全策略拦截（{why}），未发出")
            }
            crate::chat::Reply::Stopped => {
                println!("[chat] {key} 回合被中止");
                self.send_chat_text(key, &sid, "reply", "⏹️ 已停止");
            }
            crate::chat::Reply::Send(plain) => {
                let silent_for_members = crate::console::chat_role_state(&self.context)
                    .ok()
                    .is_some_and(|state| state.get("mode").and_then(|m| m.as_str()) == Some("silent"))
                    && !crate::chat::is_owner_key(cfg, key);
                if silent_for_members {
                    crate::activity::append(&self.context, &format!("{key} 静默模式，拦截在途回复"));
                    println!("[chat] {key} 静默模式，拦截在途回复");
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
            else { crate::console::ensure_or_rotate_agent_token(&self.context, &key, false)? };
        let mut draft_prompt = cli_prompt.to_owned();
        // Match the shared hold's three revisions. The fourth draft uses its fixed deadline.
        for revision in 0..=3 {
            let delivered = crate::model_view::strip_text_with_runtime(&self.context, &draft_prompt, cfg);
            let started = std::time::Instant::now();
            let result = provider(&delivered);
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
                    self.report_backend_failure(cfg,&key,id,&provider,&model,&failure.code,&note);
                    return Err(failure.summary());
                }
            };
            let text = text.trim().to_owned();
            if text.is_empty() { return Ok(if revision == 0 { None } else { Some(0) }); }
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
            if !(200..300).contains(&status) || response["ok"] == false {
                return Err(format!("CLI reply send failed: HTTP {status}"));
            }
            if response["reviseRequired"] == true {
                if revision == 3 { return Err("CLI reply revision budget is exhausted".to_owned()); }
                crate::model_failure::merge_refs(&mut self.carried_messages,
                    crate::model_failure::message_refs(&response["newMessages"]));
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
    async fn run_turn_inner(
        &mut self,
        target: &Target,
        dsh_prompt: &str,
        cli_prompt: &str,
        cfg: &serde_json::Value,
    ) -> Result<usize, String> {
        let key = target.key();
        let key: &str = &key;
        if crate::harness::Engine::selected(cfg, key)? != crate::harness::Engine::Dsh {
            return Err("harness supports chat and owner-private closed-agent modes only".into());
        }

        // ── 1) 纯文本后端（opencode 免费档）──
        //
        // 关键设计：给它 `build_text_only` 那份**自包含**提示词（消息、状态都塞进去），
        // 要求它只输出一句话、不调任何工具；拿到文本后**由 Rust 代发**。
        //
        // 为什么不能让它自己调工具：opencode v2 的 MCP 工具藏在 `execute` 代码沙箱里，
        // flash 档模型想不明白，实测会转去 `shell`/`grep`/`glob` 翻仓库，
        // 15 步烧 95 秒还一条消息没读到。改成纯文本后它没有别的路可走。
        if let Some(id) = pinned_cli_backend(cfg, key) {
            let (context, backend_cfg, backend_id, backend_key) =
                (self.context.clone(), cfg.clone(), id.to_owned(), key.to_owned());
            if let Some(sent) = self.run_cli_turn(target, cli_prompt, cfg, id, move |prompt| {
                crate::backend::cli_fallback_by_id_with_runtime(
                    &context, &backend_cfg, &backend_id, &backend_key, prompt, 30,
                )
            }).await? { return Ok(sent); }
        }

        // ── 2) DSH：完整提示词，模型自己调工具发 ──
        //
        // 用一份**去掉 backends.sessions** 的配置副本，避免 ask_with_failover 里那段
        // CLI 分支又跑一遍（那样会用 dsh_prompt 再试一次纯文本后端，纯属浪费）。
        let mut cfg_dsh = cfg.clone();
        if let Some(b) = cfg_dsh.get_mut("backends").and_then(|v| v.as_object_mut()) {
            b.remove("sessions");
        }
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
        let reply = self.ask_with_failover_with_deadline(&key, &delivered, reserved2_turn_timeout(cfg), &cfg_dsh).await?;
        let after = Self::last_ai_reply_at(&self.context, &key);
        let trimmed = reply.trim();
        let n = trimmed.chars().count();
        // 记录模型到底输出了什么（截断）：既用于判断它有没有说话，也是排障的唯一线索。
        let preview = internal_output_preview(&self.context, trimmed);
        if crate::console::runtime_mode(&self.context) == "reserved2" {
            self.log_internal_output(key, trimmed);
        }
        println!(
            "[pipeline] {key}：DSH 回合结束（输出 {n} 字，{}）｜{preview}",
            if after > before {
                "模型自己发过"
            } else {
                "模型选择不说"
            }
        );
        Ok(n)
    }

}

/// The access state a new session was created under (see `Pipeline::ensure_session`).
struct AccessEpoch {
    mode_stamp: Result<String, String>,
    allowed: Result<bool, String>,
    resets: Result<usize, String>,
    token: Result<Option<String>, String>,
    policy: String,
}

impl AccessEpoch {
    /// Unchanged since `before`; anything unreadable counts as changed (fail closed). A token that
    /// appears where there was none is not a change (a first contact mints it); one that changes or
    /// disappears is.
    fn still(&self, before: &AccessEpoch) -> bool {
        fn same<T: PartialEq>(a: &Result<T, String>, b: &Result<T, String>) -> bool {
            matches!((a, b), (Ok(x), Ok(y)) if x == y)
        }
        let token = match (&before.token, &self.token) {
            (Ok(None), Ok(_)) => true,
            (Ok(Some(old)), Ok(Some(now))) => old == now,
            _ => false,
        };
        same(&before.mode_stamp, &self.mode_stamp)
            && matches!(self.allowed, Ok(true))
            && same(&before.allowed, &self.allowed)
            && same(&before.resets, &self.resets)
            && token
            && before.policy == self.policy
    }
}

#[cfg(test)]
mod tests {
    include!("pipeline_cli_relay_tests.rs");
    include!("pipeline_completion_tests.rs");
    use super::*;
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
        let before = crate::console::ensure_or_rotate_agent_token(&context, key, false).unwrap();
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
        let after = crate::console::conversation_token(&context, key).unwrap().unwrap();
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
        let before = crate::console::ensure_or_rotate_agent_token(&context,key,false).unwrap();
        let minted: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        pipe.share_minted_epoch(minted.clone());
        let result = pipe.pause_unknown(key,"turn-unverified","synthetic unknown collector outcome");
        let after = crate::console::conversation_token(&context,key).unwrap().unwrap();
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
            "暂缓唤醒：conversation has a paused turn",
            "模式已变化",
            "会话或白名单已变化",
            "会话令牌已变化（重置、取消或被替代）",
            "会话创建期间已重置或权限已变化，丢弃新会话",
            "模式已切换，丢弃排队的指令",
            "会话已重置或权限已撤销，丢弃排队的指令",
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
    #[test]
    fn ticket24_a_session_created_across_a_revocation_or_reset_is_archived_not_bound() {
        use std::net::TcpListener;
        let _env_lock = DSH_HOME_TEST_LOCK.lock().unwrap();
        for (tag, path, body) in [
            ("t24-create-revoke", "/api/whitelist", serde_json::json!({"allow": {"groups": []}})),
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
            let result = pipeline.ensure_session("group:42");
            let seen = server.join().unwrap();
            assert!(result.is_err(), "{tag}: a session created across the change was used: {result:?}");
            assert_eq!(crate::session_map::lookup(&context, "group:42").unwrap(), None, "{tag}: the new session was bound");
            assert!(seen.iter().any(|p| p == "/api/workspace/archiveSession"), "{tag}: the new session must be archived: {seen:?}");
            drop(pipeline);
            std::fs::remove_dir_all(root).unwrap();
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
                                let (status, reply) = crate::console::admin_request(&server_context, "POST", "/api/whitelist", Some(&serde_json::json!({"allow": {"groups": []}})));
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
            let error = result.expect_err(&format!("{tag}: the session was used"));
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

            let error = pipeline.ensure_session("group:42").unwrap_err();
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
        assert!(outbound_violation(r"C:\qqbot\config.json", &[]).is_some());
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
        assert!(outbound_violation("看 C:\\qqbot\\docs 里的文件", &secrets).is_some());
        assert!(outbound_violation("路径 C:/qqbot/x", &secrets).is_some());
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
}
