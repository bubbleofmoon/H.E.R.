//! P8：provider / model 失败自动切换。
//!
//! 语义对齐 JS 的 `modelChain` + `ensureChatModel` + `handleTurnFailureFailover`：
//!   候选链 = dsh.provider/model + dsh.fallbacks[]（同 provider/model 去重）
//!   两层降级：
//!     ① `session/selectModel` 本身失败 → 换下一个候选（每个候选试 2 次）
//!     ② 回合以 error 收尾 → 换候选并**重投一次** prompt
//!
//! 一条铁律：**已经发过任何字节就不重投**。重复发言比不发言更难解释。

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub effort: String,
}

impl Candidate {
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// JS `REASONING_EFFORT_DEFAULT`. The console (`console.rs` 1342) already reports this value, so the executed chain
/// must use it too: reasoning effort is a paid-model parameter and must not silently differ from what is shown.
const DEFAULT_EFFORT: &str = "max";

/// JS truthiness for a string setting: `cfg.x || default` falls back on a missing, non-string or empty value.
fn non_empty<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

/// Whether a failed DSH chain may fall back to the `backends` CLI backends (a Rust-only addition: the frozen JS has
/// none, and a CLI backend can run on another account). Off unless `backends.failoverFallback` is exactly `true`;
/// configuring CLI backends alone, or the array form of `backends`, never enables it. Backends pinned to a
/// conversation (`backends.sessions`) are explicit per-conversation choices and are not governed by this switch.
pub fn cli_fallback_enabled(cfg: &Value) -> bool {
    cfg.pointer("/backends/failoverFallback") == Some(&Value::Bool(true))
}

/// `Some(reason)` when the CLI fallback must not run for `cfg`, `None` when it is enabled.
pub fn cli_fallback_refusal(cfg: &Value) -> Option<String> {
    if cli_fallback_enabled(cfg) {
        None
    } else {
        Some("CLI 后端回落未启用（backends.failoverFallback 不是 true）".to_string())
    }
}

/// 按会话构造候选链：该会话在 `cfg.sessions` 里有覆盖就用覆盖，否则用全局 `dsh`。
///
/// 为什么单独一个函数而不是给 `model_chain` 加参数：`model_chain` 还被诊断命令
/// 和单测用，那些场景没有会话概念。保持原函数不动，波及面最小。
pub fn model_chain_for(cfg: &Value, key: &str) -> Vec<Candidate> {
    let over = cfg
        .get("sessions")
        .and_then(|s| s.get(key))
        .filter(|v| v.get("model").and_then(|m| m.as_str()).unwrap_or("").trim().len() > 0);
    let Some(over) = over else {
        return model_chain(cfg);
    };
    let primary = Candidate {
        provider: over
            .get("provider")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("deepseek-official")
            .to_string(),
        model: over
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        effort: over
            .get("reasoningEffort")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_EFFORT)
            .to_string(),
    };
    // 会话覆盖只替换**主候选**，fallbacks 仍沿用全局：覆盖是"这个群想用更强的模型"，
    // 不是"这个群不要兜底"。兜底挂了比选错模型严重得多（今天刚因为 fallback 去重宕机）。
    let mut out = vec![primary];
    for c in model_chain(cfg) {
        if out.iter().any(|x| x.provider == c.provider && x.model == c.model) {
            continue;
        }
        out.push(c);
    }
    out
}

/// 构造候选链：主候选在前，fallbacks 按顺序在后，同 provider+model 去重。
pub fn model_chain(cfg: &Value) -> Vec<Candidate> {
    let dsh = cfg.get("dsh").cloned().unwrap_or_else(|| json!({}));
    let primary = Candidate {
        provider: non_empty(dsh.get("provider")).unwrap_or("deepseek-official").to_string(),
        model: non_empty(dsh.get("model")).unwrap_or("deepseek-flash").to_string(),
        effort: non_empty(dsh.get("reasoningEffort")).unwrap_or(DEFAULT_EFFORT).to_string(),
    };
    let mut out = vec![primary.clone()];

    // 诊断开关：`HER_FAIL_FIRST=1` 时在最前面插一个**必然失败**的候选，
    // 用来实测「主候选挂了会自动切下一个」。只影响当前进程，不写配置。
    //
    // 为什么需要它：P8 的验收是"人为断主 provider 自动切备用"，而真实断网/改配置
    // 会连累线上 JS 那条链。这个开关只在诊断进程里生效，跑完整切换不碰线上。
    if std::env::var("HER_FAIL_FIRST").ok().as_deref() == Some("1") {
        out.insert(
            0,
            Candidate {
                provider: "__probe_fail__".to_string(),
                model: "__probe_fail__".to_string(),
                effort: DEFAULT_EFFORT.to_string(),
            },
        );
    }

    if let Some(list) = dsh.get("fallbacks").and_then(|v| v.as_array()) {
        for f in list {
            let provider = f.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let model = f.get("model").and_then(|v| v.as_str()).unwrap_or("");
            if provider.is_empty() || model.is_empty() {
                continue;
            }
            if out
                .iter()
                .any(|c| c.provider == provider && c.model == model)
            {
                continue;
            }
            out.push(Candidate {
                provider: provider.to_string(),
                model: model.to_string(),
                effort: non_empty(f.get("reasoningEffort")).unwrap_or(&primary.effort).to_string(),
            });
        }
    }
    out
}

/// 失败分类。分类的意义在于决定「值不值得换候选」：
/// 全都值得换，但日志要能看出是哪一类，否则排查时只能靠猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    /// 连不上 DSH / 连接被重置
    Connection,
    /// 服务端 5xx
    Server,
    /// provider 或 model 不存在（换候选也没用，但换一下试试别的）
    NotFound,
    /// 回合以 error 收尾但码认不出来（provider 侧新错误码）。
    /// 目前没有地方构造它 —— 留着是为了分类语义完整，接到具体错误码时直接用。
    #[allow(dead_code)]
    TurnError,
    /// 认不出来的失败
    Unknown,
}

impl FailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Server => "server",
            Self::NotFound => "not-found",
            Self::TurnError => "turn-error",
            Self::Unknown => "unknown",
        }
    }
}

/// 从错误文本里猜分类（`selectModel` 失败走这条）。
pub fn classify_message(msg: &str) -> FailureKind {
    let m = msg.to_ascii_lowercase();
    if m.contains("not found") || m.contains("not-found") || m.contains("notfound") {
        return FailureKind::NotFound;
    }
    if m.contains("connection") || m.contains("connect") || m.contains("reset") || m.contains("broken pipe") {
        return FailureKind::Connection;
    }
    for code in ["500", "502", "503", "504"] {
        if m.contains(code) {
            return FailureKind::Server;
        }
    }
    FailureKind::Unknown
}

/// 从 `turn/end` 帧里判断这一轮是不是失败收尾。
/// 返回 `(分类, 代码, 详情)`；正常结束返回 None。
pub fn classify_turn_end(ended: &Value) -> Option<(FailureKind, String, String)> {
    let reason = ended.get("reason").cloned().unwrap_or_else(|| json!({}));
    let kind = reason.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    // aborted / interrupted / completed 都不是「模型坏了」，不该触发换候选。
    if kind != "error" {
        return None;
    }
    let code = reason
        .get("error")
        .and_then(|e| e.get("code"))
        .or_else(|| reason.get("failure").and_then(|f| f.get("code")))
        .and_then(|v| v.as_str())
        .unwrap_or("error")
        .to_string();
    let detail = reason
        .get("error")
        .and_then(|e| e.get("message"))
        .or_else(|| reason.get("failure").and_then(|f| f.get("message")))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cls = classify_message(&format!("{code} {detail}"));
    Some((cls, code, detail))
}

/// 一次失败之后怎么办。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverAction {
    /// 换到第 idx 个候选并重投。
    Retry { idx: usize },
    /// 放弃（附原因）。
    GiveUp(GiveUp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GiveUp {
    /// 只有主候选，没配 fallbacks。
    NoFallbacks,
    /// 已经发过消息，重投会造成重复发言。
    AlreadySent,
    /// 已经重试过，不再重试。
    RetriesExhausted,
    /// 已经是最后一个候选。
    LastCandidate,
}

impl GiveUp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoFallbacks => "没有配置 fallbacks",
            Self::AlreadySent => "本轮已发过消息，避免重复发送",
            Self::RetriesExhausted => "已重试过，放弃",
            Self::LastCandidate => "已是最后一个候选",
        }
    }
}

/// 决定失败之后要不要换候选重投。
///
/// `already_sent` 是硬闸门：只要发出去过任何字节，一律不重投。
pub fn decide(
    chain_len: usize,
    current_idx: usize,
    retries: usize,
    already_sent: bool,
) -> FailoverAction {
    if already_sent {
        return FailoverAction::GiveUp(GiveUp::AlreadySent);
    }
    if chain_len < 2 {
        return FailoverAction::GiveUp(GiveUp::NoFallbacks);
    }
    if retries >= 2 {
        return FailoverAction::GiveUp(GiveUp::RetriesExhausted);
    }
    let next = (current_idx + 1).min(chain_len - 1);
    if next == current_idx {
        return FailoverAction::GiveUp(GiveUp::LastCandidate);
    }
    FailoverAction::Retry { idx: next }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_fallbacks() -> Value {
        json!({
            "dsh": {
                "provider": "opencode-go",
                "model": "deepseek-v4.1-flash",
                "reasoningEffort": "high",
                "fallbacks": [
                    { "provider": "deepseek-official", "model": "deepseek-flash", "reasoningEffort": "high" },
                    { "provider": "opencode-go", "model": "deepseek-v4.1-flash" }
                ]
            }
        })
    }

    #[test]
    fn chain_primary_first_and_deduped() {
        let chain = model_chain(&cfg_with_fallbacks());
        assert_eq!(chain.len(), 2, "重复的 provider/model 不该再排一遍");
        assert_eq!(chain[0].provider, "opencode-go");
        assert_eq!(chain[1].provider, "deepseek-official");
        assert_eq!(chain[1].effort, "high");
    }

    #[test]
    fn default_effort_is_max_like_the_frozen_js() {
        // JS `modelChain()`: effort = dsh.reasoningEffort || REASONING_EFFORT_DEFAULT ('max'); a fallback without its own
        // effort inherits the primary's. A paid-model parameter must not silently differ from what the console shows.
        let chain = model_chain(&json!({"dsh":{"provider":"p","model":"m","fallbacks":[{"provider":"q","model":"n"}]}}));
        assert_eq!(chain[0].effort, "max");
        assert_eq!(chain[1].effort, "max", "a fallback inherits the primary's effort");
        assert_eq!(model_chain(&json!({}))[0].effort, "max");
        assert_eq!(model_chain(&json!({"dsh":{"reasoningEffort":""}}))[0].effort, "max", "JS: empty string is falsy");
        let explicit = model_chain(&json!({"dsh":{"provider":"p","model":"m","reasoningEffort":"medium","fallbacks":[{"provider":"q","model":"n"},{"provider":"r","model":"o","reasoningEffort":"low"}]}}));
        assert_eq!(explicit.iter().map(|c| c.effort.as_str()).collect::<Vec<_>>(), vec!["medium", "medium", "low"]);
        let over = model_chain_for(&json!({"dsh":{"provider":"p","model":"m"},"sessions":{"group:1":{"provider":"x","model":"y"}}}), "group:1");
        assert_eq!(over[0].effort, "max", "a session override without an effort uses the default too");
    }

    #[test]
    fn empty_provider_or_model_falls_back_to_the_js_defaults() {
        let c = &model_chain(&json!({"dsh":{"provider":"","model":""}}))[0];
        assert_eq!((c.provider.as_str(), c.model.as_str()), ("deepseek-official", "deepseek-flash"));
    }

    #[test]
    fn probe_candidate_uses_the_default_effort() {
        // Diagnostic-only candidate; the env var is process-global, so keep the check local and restore it.
        let before = std::env::var("HER_FAIL_FIRST").ok();
        std::env::set_var("HER_FAIL_FIRST", "1");
        let chain = model_chain(&json!({"dsh":{"provider":"p","model":"m"}}));
        match before {
            Some(v) => std::env::set_var("HER_FAIL_FIRST", v),
            None => std::env::remove_var("HER_FAIL_FIRST"),
        }
        assert_eq!(chain[0].provider, "__probe_fail__");
        assert_eq!(chain[0].effort, "max");
    }

    #[test]
    fn cli_fallback_is_off_unless_explicitly_enabled() {
        assert!(!cli_fallback_enabled(&json!({})));
        assert!(!cli_fallback_enabled(&json!({"backends":{"list":[{"id":"oc","kind":"cli","cmd":"opencode"}]}})), "configuring a CLI backend alone does not enable the fallback");
        assert!(!cli_fallback_enabled(&json!({"backends":[{"id":"oc","kind":"cli","cmd":"opencode"}]})));
        assert!(!cli_fallback_enabled(&json!({"backends":{"failoverFallback":false}})));
        for not_true in [json!("true"), json!(1), json!("yes"), json!(null)] {
            assert!(!cli_fallback_enabled(&json!({"backends":{"failoverFallback":not_true}})));
        }
        assert!(cli_fallback_enabled(&json!({"backends":{"failoverFallback":true,"list":[]}})));
    }

    #[test]
    fn disabled_cli_fallback_reports_why_without_running_anything() {
        let err = cli_fallback_refusal(&json!({"backends":{"list":[{"id":"oc","kind":"cli","cmd":"definitely-not-run"}]}})).expect("off by default");
        assert!(err.contains("failoverFallback"), "{err}");
        assert!(cli_fallback_refusal(&json!({"backends":{"failoverFallback":true}})).is_none());
    }

    #[test]
    fn chain_without_fallbacks_has_one() {
        let chain = model_chain(&json!({"dsh":{"provider":"p","model":"m"}}));
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].label(), "p/m");
    }

    #[test]
    fn invalid_fallback_is_skipped() {
        let cfg = json!({"dsh":{"provider":"p","model":"m","fallbacks":[
            {"provider":"","model":"x"}, {"provider":"p2"}, {"provider":"p3","model":"m3"}
        ]}});
        let chain = model_chain(&cfg);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1].label(), "p3/m3");
    }

    #[test]
    fn session_override_replaces_primary_but_keeps_fallbacks() {
        let cfg = json!({
            "dsh": {"provider": "deepseek-official", "model": "deepseek-flash", "reasoningEffort": "high",
                    "fallbacks": [{"provider": "opencode-go", "model": "claude-haiku-5-5", "reasoningEffort": "medium"}]},
            "sessions": {"group:1": {"provider": "opencode-go", "model": "claude-haiku-5-5", "reasoningEffort": "medium"}}
        });
        let chain = model_chain_for(&cfg, "group:1");
        assert_eq!(chain[0].model, "claude-haiku-5-5", "会话覆盖必须替换主候选");
        assert!(chain.len() >= 2, "fallbacks 不能被覆盖吃光");
        // 没覆盖的会话走全局
        let other = model_chain_for(&cfg, "group:2");
        assert_eq!(other[0].model, "deepseek-flash");
    }

    #[test]
    fn classify_messages() {
        assert_eq!(classify_message("model not found"), FailureKind::NotFound);
        assert_eq!(classify_message("connect ECONNREFUSED"), FailureKind::Connection);
        assert_eq!(classify_message("HTTP 503"), FailureKind::Server);
        assert_eq!(classify_message("weird"), FailureKind::Unknown);
    }

    #[test]
    fn only_error_turns_trigger_failover() {
        // 正常结束 / 被中断 / 被取消 都不该换候选
        assert!(classify_turn_end(&json!({"reason":{"kind":"completed"}})).is_none());
        assert!(classify_turn_end(&json!({"reason":{"kind":"aborted"}})).is_none());
        assert!(classify_turn_end(&json!({"reason":{"kind":"interrupted"}})).is_none());

        let (kind, code, _) = classify_turn_end(&json!({
            "reason": {"kind":"error","failure":{"code":"upstream_503","message":"bad gateway"}}
        }))
        .expect("error 应该被判为失败");
        assert_eq!(kind, FailureKind::Server);
        assert_eq!(code, "upstream_503");
    }

    #[test]
    fn decide_switches_to_next_candidate() {
        assert_eq!(
            decide(3, 0, 0, false),
            FailoverAction::Retry { idx: 1 }
        );
        assert_eq!(
            decide(3, 1, 1, false),
            FailoverAction::Retry { idx: 2 }
        );
    }

    #[test]
    fn already_sent_never_retries() {
        // 铁律：发过就绝不重投
        assert_eq!(
            decide(3, 0, 0, true),
            FailoverAction::GiveUp(GiveUp::AlreadySent)
        );
    }

    #[test]
    fn last_candidate_gives_up() {
        assert_eq!(
            decide(2, 1, 0, false),
            FailoverAction::GiveUp(GiveUp::LastCandidate)
        );
    }

    #[test]
    fn retries_are_capped() {
        assert_eq!(
            decide(5, 0, 2, false),
            FailoverAction::GiveUp(GiveUp::RetriesExhausted)
        );
    }

    #[test]
    fn single_candidate_gives_up() {
        assert_eq!(
            decide(1, 0, 0, false),
            FailoverAction::GiveUp(GiveUp::NoFallbacks)
        );
    }
}
