//! P6：唤醒生命周期状态机。
//!
//! 这是 JS 桥接里那套定时器逻辑的 Rust 版，语义逐条对齐：
//!   scheduleWakeV2 / scheduleReplyCheckV2 / setupSleepTimerV2 /
//!   scheduleProactiveCheckV2 / replayPendingWakeReasonsV2 / ensureWakeableV2
//!
//! 设计取舍：**不用 tokio::time::sleep 直接挂在逻辑里**，而是把「截止时间」当数据，
//! 由调用方（session actor）统一算出下一个 deadline 再睡。这样：
//!   1. 单测可以注入 `now`，不用真等 30 秒；
//!   2. 离线重放可以按消息时间戳推进时钟，复现历史时序；
//!   3. 所有定时器只有一个 select 分支，不会出现「一个会话挂着十几个 sleep」。
//!
//! 注意：session actor（`session.rs`）接上之前，这些 API 只有单测在用，
//! 所以整体允许 dead_code；接上之后应该删掉这一行。

#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::VecDeque;

/// 唤醒原因优先级（对应 JS 的 WAKE_PRIORITY）。合并窗口内取优先级最高的。
pub fn wake_priority(reason: &str) -> i32 {
    match reason.split(':').next().unwrap_or("") {
        "private" => 100,
        "atMention" => 90,
        "question" => 80,
        "speaker" => 75,
        "nameMention" => 70,
        "keyword" => 60,
        "anyMessage" => 50,
        "replyCheck" => 40,
        "timeout" => 30,
        "proactiveCheck" => 20,
        _ => 0,
    }
}

/// 与「未读消息」无关的唤醒原因（定时器/引导）。
/// 这类原因在繁忙时被暂存后，不能靠 unread 里还有没有对应 seq 来判断要不要补发。
pub fn is_timer_reason(reason: &str) -> bool {
    matches!(
        reason.split(':').next().unwrap_or(""),
        "timeout" | "replyCheck" | "proactiveCheck" | "bootstrap" | "admin"
    )
}

/// 一条被暂存的唤醒原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWake {
    pub reason: String,
    pub seq: i64,
}

/// 调度动作的结果，调用方据此决定要不要发消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 立刻唤醒（例如睡眠已过期）。
    FireNow(String),
    /// 已排定定时器，到点由 `fire_due` 取。
    Armed,
    /// 会话忙，原因进队列，等回合结束补发。
    Buffered,
    /// 合并窗口内已有待发唤醒，本次按优先级升级了原因。
    Upgraded { from: String, to: String },
    /// 条件不满足，什么都没做。
    Skipped(&'static str),
    /// 已有定时器，本次忽略。
    AlreadyArmed,
    /// 打断当前回合：新来的确定性触发比「等它跑完」更值得。
    /// 只有「回合还没发出任何消息」时才允许，避免把已经说出去的话再改口。
    Preempt { reason: String },
    /// 唤醒频率超限：已排了重试定时器，到点再走 `fire_due`。
    RateLimited { wait_ms: i64 },
}

/// 补发队列排空的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// 队列本来就空。
    Empty,
    /// 定时器类原因被补排。
    Timers { count: usize },
    /// 消息类原因合并成一次唤醒。
    Coalesced { picked: String, merged: usize },
    /// 积压的消息都已被处理掉，不需要补发。
    AllHandled { count: usize },
}

/// 配置切片（从 config.json 的 socialV2 段读出来）。
#[derive(Debug, Clone)]
pub struct WakeCfg {
    pub enabled: bool,
    /// Live reply scheduling; None retains the offline legacy replay policy.
    pub reply_wake_quiet_ms: Option<i64>,
    /// 合批窗口下限（毫秒），低于 1000 按 1000 算。
    pub batch_window_ms: i64,
    /// 回复检查基准延迟。
    pub auto_reply_check_ms: i64,
    /// 回合判忙租约。
    pub turn_lease_ms: i64,
    pub proactive_enabled: bool,
    pub proactive_min_ms: i64,
    pub proactive_max_ms: i64,
    pub proactive_idle_ms: i64,
    pub proactive_probability: f64,
    /// 回合跑了多久之后，允许被「确定性触发」打断（毫秒）。**0 = 不打断（默认）**。
    ///
    /// 为什么默认关掉：JS 那边**根本没有打断机制**（`bridge.js` 里 preempt/打断 零命中），
    /// 新 @ 只会进 `pendingWakeReasons` 排队。而这条链的验收口径是「唤醒序列与 JS 偏差为 0」，
    /// 自己发明一种 JS 没有的打断，A/B 就永远解释不干净。
    ///
    /// 更要紧的是风险：原来默认 15 秒太激进。实测一个 DSH 回合 85 秒（`--ab-probe`），
    /// 15 秒就允许打断，意味着回合跑到一半来个 @ 就被取消重来 ——
    /// 连续几个 @ 就能让这个会话一条回复都发不出去（饿死），每次取消还白花 token。
    ///
    /// 想开就在配置里显式给 ms（比如 60000），本字段 > 0 才启用。
    pub preempt_after_ms: i64,
    /// Dispatch-clock hard cap, including the pre-sleep observation and closing headroom.
    /// While she is engaged it runs from her last successful send in the turn.
    pub turn_hard_cap_ms: i64,
    /// Absolute per-turn runaway guard measured from dispatch (`socialV2.wake.turnCeilingMs`).
    pub turn_ceiling_ms: i64,
    /// 唤醒频率上限（每分钟 / 每小时，0 = 不限）。
    /// 之前 Rust 完全没有这个限制，结果是「有 @ 就醒」—— 影子在 A/B 里
    /// 比 JS 多醒 78%，其中确定性触发多醒 6 次，全是这个缺口造成的。
    pub max_wake_per_minute: i64,
    pub max_wake_per_hour: i64,
}

impl Default for WakeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            reply_wake_quiet_ms: None,
            batch_window_ms: 1500,
            auto_reply_check_ms: 30_000,
            turn_lease_ms: 30 * 60 * 1000,
            proactive_enabled: true,
            proactive_min_ms: 30 * 60 * 1000,
            proactive_max_ms: 90 * 60 * 1000,
            proactive_idle_ms: 15 * 60 * 1000,
            proactive_probability: 0.4,
            preempt_after_ms: 0,
            turn_hard_cap_ms: DEFAULT_TURN_HARD_CAP_MS,
            turn_ceiling_ms: TURN_CEILING_DEFAULT_MS,
            max_wake_per_minute: 1,
            max_wake_per_hour: 12,
        }
    }
}

/// 唤醒配置（对应 JS 的 wakeConfig）。
#[derive(Debug, Clone, PartialEq)]
pub struct WakeConfig {
    pub mode_active: bool,
    pub infinite: bool,
    /// ISO 时间字符串，跟 JS 共享同一份状态文件。
    pub sleep_until: Option<String>,
    pub at_mention: bool,
    pub name_mention: bool,
    pub question_boolean: bool,
    pub poke: bool,
    pub any_message: bool,
    pub probability: f64,
    pub keywords: Vec<String>,
    pub speaker_ids: Vec<i64>,
    pub batch_window_ms: Option<i64>,
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            mode_active: false,
            infinite: true,
            sleep_until: None,
            at_mention: true,
            name_mention: true,
            question_boolean: true,
            poke: true,
            any_message: false,
            probability: 0.05,
            keywords: Vec::new(),
            speaker_ids: Vec::new(),
            batch_window_ms: None,
        }
    }
}

/// 最近一条自己发的消息（回复检查用它判断「是不是问了个问题」）。
#[derive(Debug, Clone)]
pub struct SelfMessage {
    pub at_ms: i64,
    pub text: String,
}

/// 每会话一个，持有所有定时器的截止时刻。
#[derive(Debug, Clone)]
pub struct WakeScheduler {
    pub cfg: WakeCfg,
    pub wc: WakeConfig,

    pub pending_wake_reason: Option<String>,
    pub pending_wake_at: Option<i64>,
    pub message_wake_first_at: Option<i64>,
    pub message_wake_urgent: bool,
    pub sleep_at: Option<i64>,
    pub reply_check_at: Option<i64>,
    pub proactive_at: Option<i64>,
    pub pending: VecDeque<PendingWake>,

    pub last_ai_reply_at: i64,
    pub last_incoming_at: i64,
    pub last_unread_seq: i64,
    pub turn_started_at: Option<i64>,
    /// Her last successful send (`engagedAt`); counts only when inside the running turn.
    pub engaged_at: i64,
    /// 当前回合有没有已经发出过消息（决定能不能被打断）。
    pub turn_has_sent: bool,
    /// 「唤醒已派发、回合还没开始」的时刻（对应 JS 的 pendingWakeKeys）。
    /// 缺了它就会：刚醒来的那 1~2 秒里又收到消息 → 再醒一次 → 重复回话。
    pub pending_wake_key_at: Option<i64>,
    pub paused: bool,
    /// 控制台/工具层是否还有一个"正在跑"的 prompt（JS 的 promptQueues.running）。
    pub prompt_running: bool,
    pub prompt_queued: bool,
    /// 最近一小时的真实唤醒时刻（JS 的 `st.wakeTimes`），限频就靠它。
    pub wake_times: Vec<i64>,
}

impl WakeScheduler {
    pub fn new(cfg: WakeCfg, wc: WakeConfig) -> Self {
        Self {
            cfg,
            wc,
            pending_wake_reason: None,
            pending_wake_at: None,
            message_wake_first_at: None,
            message_wake_urgent: false,
            sleep_at: None,
            reply_check_at: None,
            proactive_at: None,
            pending: VecDeque::new(),
            last_ai_reply_at: 0,
            last_incoming_at: 0,
            last_unread_seq: 0,
            turn_started_at: None,
            engaged_at: 0,
            turn_has_sent: false,
            pending_wake_key_at: None,
            paused: false,
            prompt_running: false,
            prompt_queued: false,
            wake_times: Vec::new(),
        }
    }

    fn batch_window(&self) -> i64 {
        if let Some(quiet) = self.cfg.reply_wake_quiet_ms { return quiet; }
        self.wc
            .batch_window_ms
            .unwrap_or(self.cfg.batch_window_ms)
            .max(1000)
    }

    /// Every admitted arrival participates, including messages that did not trigger a wake.
    /// Neutral media is collected without extending an existing quiet deadline.
    pub fn incoming(&mut self, reason: Option<&str>, now: i64, neutral: bool, urgent: bool) -> Outcome {
        let Some(quiet) = self.cfg.reply_wake_quiet_ms else {
            return reason.map(|r| self.schedule_wake(r, now)).unwrap_or(Outcome::Skipped("no trigger"));
        };
        let old_deadline = self.pending_wake_at;
        if !self.is_busy(now) && self.message_wake_first_at.is_none() {
            self.message_wake_first_at = Some(now);
        }
        let out = reason.map(|r| self.schedule_wake(r, now)).unwrap_or(Outcome::Skipped("no trigger"));
        if self.pending_wake_at.is_some() && reason.is_some_and(|r|urgent || !is_timer_reason(r)) {
            self.message_wake_first_at.get_or_insert(now);
        }
        if self.pending_wake_at.is_some() && self.message_wake_first_at.is_some()
            && !matches!(out, Outcome::Buffered | Outcome::Preempt { .. }) {
            self.message_wake_urgent |= urgent;
            let first = self.message_wake_first_at.unwrap();
            let at = if self.message_wake_urgent { now }
                else if neutral && old_deadline.is_some() { old_deadline.unwrap() }
                else { now.saturating_add(quiet).min(first.saturating_add(30_000)) };
            // Quiet/urgency never bypasses the existing wake quota.
            self.pending_wake_at = Some(self.rate_limit_wait(now).map_or(at, |wait| at.max(now + wait)));
        }
        out
    }

    /// 对应 JS 的 isConversationBusyV2。
    pub fn is_busy(&self, now: i64) -> bool {
        if self.pending_wake_at.is_some() || self.prompt_running || self.prompt_queued {
            return true;
        }
        // 唤醒刚派发、回合还没起来：这段时间也算忙，否则会重复醒。
        // 60 秒租约防止「醒了但回合永远没来」把会话锁死。
        if let Some(at) = self.pending_wake_key_at {
            if now - at < 60_000 {
                return true;
            }
        }
        // 回合只按「起始时间 + 租约」判忙：起始时间过期就解除忙碌，
        // 但不能因为 Map 里还有键就判忙（DSH 重启会把回合丢掉）。
        match self.turn_started_at {
            Some(t) if t > 0 => now - t < self.cfg.turn_lease_ms,
            _ => false,
        }
    }

    /// 对应 JS 的 scheduleWakeV2。
    pub fn schedule_wake(&mut self, reason: &str, now: i64) -> Outcome {
        if !self.cfg.enabled {
            return Outcome::Skipped("socialV2 已关闭");
        }
        if self.paused {
            return Outcome::Skipped("已暂停");
        }

        // 合并窗口内已有待发唤醒：按优先级升级最终原因，
        // 避免「先概率后 @」却仍按概率唤醒。
        if self.pending_wake_at.is_some() {
            let cur = self
                .pending_wake_reason
                .clone()
                .unwrap_or_else(|| reason.to_string());
            if wake_priority(reason) > wake_priority(&cur) {
                self.pending_wake_reason = Some(reason.to_string());
                return Outcome::Upgraded {
                    from: cur,
                    to: reason.to_string(),
                };
            }
            return Outcome::AlreadyArmed;
        }

        if self.is_busy(now) {
            // 打断判定：新来的是确定性触发 + 回合还在跑 + 还没发过话 + 已经跑够久。
            // 四个条件缺一不可 —— 少了「还没发过话」会把说出口的话再改口；
            // 少了「跑够久」会为了一句 @ 反复丢弃已经花掉的思考。
            // **另外 `preempt_after_ms == 0` 表示这个机制整个关掉（默认）**：
            // JS 没有打断，自己加一套会让 A/B 的"偏差为 0"永远做不到，还有点饿死风险。
            if self.cfg.preempt_after_ms > 0 && wake_priority(reason) >= 60 {
                if let Some(started) = self.turn_started_at {
                    if started > 0 && !self.turn_has_sent && now - started >= self.cfg.preempt_after_ms {
                        self.pending.clear();
                        return Outcome::Preempt {
                            reason: reason.to_string(),
                        };
                    }
                }
            }
            let seq = self.last_unread_seq;
            let dup = self
                .pending
                .iter()
                .any(|r| r.reason == reason && r.seq == seq);
            if !dup {
                self.pending.push_back(PendingWake {
                    reason: reason.to_string(),
                    seq,
                });
                // 有界队列：最多 20 条，防止消息洪峰下无限增长。
                while self.pending.len() > 20 {
                    self.pending.pop_front();
                }
            }
            return Outcome::Buffered;
        }

        // 真实唤醒已接管，取消回复检查，避免几十秒后再补一刀。
        self.reply_check_at = None;
        self.sleep_at = None;

        // 唤醒频率硬限制（JS 的 maxWakePerMinute / maxWakePerHour）。
        // 超限**不能直接丢弃**：丢掉的可能是唯一一次 @，而且 sleepTimer 已经清了，
        // 会话会停在「没有任何定时器」的静默态。这里按 JS 的做法排一个短重试。
        if let Some(wait_ms) = self.rate_limit_wait(now) {
            self.pending_wake_reason = Some(reason.to_string());
            self.pending_wake_at = Some(now + wait_ms);
            return Outcome::RateLimited { wait_ms };
        }

        // **不要在这里记 wake_time。** 限频统计的是「真的发出去了几次」，
        // 而这里只是「排上了」，1.5 秒后 fire_due 还会再查一次限频：
        // 如果排的时候就记一条，fire_due 会认为自己刚发过 → 再等 60 秒。
        // 实测就是这样：21:23:55 决定唤醒，日志里落地时间却是 21:24:55（整 60 秒），
        // 而 JS 同一时刻只花了不到 2 秒。wake_times 只由 fire_due 在真正发出去时记。
        self.pending_wake_reason = Some(reason.to_string());
        if self.cfg.reply_wake_quiet_ms.is_some() && !is_timer_reason(reason) {
            self.message_wake_first_at.get_or_insert(now);
            self.message_wake_urgent |= reason=="atMention";
        }
        self.pending_wake_at = Some(now + if self.cfg.reply_wake_quiet_ms.is_some() && reason=="atMention" {0} else {self.batch_window()});
        Outcome::Armed
    }

    /// 距下一次「窗口腾出位置」还要多久；没超限返回 None。
    fn rate_limit_wait(&mut self, now: i64) -> Option<i64> {
        self.trim_wake_times(now);
        let minute_cap = self.cfg.max_wake_per_minute;
        let hour_cap = self.cfg.max_wake_per_hour;
        let recent_minute: Vec<i64> = self
            .wake_times
            .iter()
            .copied()
            .filter(|t| now - *t < 60_000)
            .collect();
        let recent_hour = &self.wake_times;
        let over_minute = minute_cap > 0 && recent_minute.len() as i64 >= minute_cap;
        let over_hour = hour_cap > 0 && recent_hour.len() as i64 >= hour_cap;
        if !over_minute && !over_hour {
            return None;
        }
        let mut retry_after = 0i64;
        if over_minute {
            if let Some(oldest) = recent_minute.iter().min() {
                retry_after = retry_after.max(60_000 - (now - *oldest) + 500);
            }
        }
        if over_hour {
            if let Some(oldest) = self.wake_times.iter().min() {
                retry_after = retry_after.max(3_600_000 - (now - *oldest) + 500);
            }
        }
        // 每次只等一小段（最长 60s）：小时级限频不该一口气睡满一小时、错过中途的空档。
        Some(retry_after.clamp(1_000, 60_000))
    }

    fn trim_wake_times(&mut self, now: i64) {
        self.wake_times.retain(|t| now - *t < 3_600_000);
    }

    /// 对应 JS 的 scheduleReplyCheckV2。
    /// `recent_self` 是最近 15 秒内自己发的消息，用来判断要不要拉长延迟。
    pub fn schedule_reply_check(&mut self, now: i64, recent_self: &[SelfMessage]) -> Outcome {
        if !self.cfg.enabled || self.paused {
            return Outcome::Skipped("不满足条件");
        }
        // 已有真实唤醒 / 有限睡眠 / 回复检查在排队时不再重复安排。
        if self.pending_wake_at.is_some() || self.sleep_at.is_some() || self.reply_check_at.is_some()
        {
            return Outcome::AlreadyArmed;
        }
        let mut delay = self.cfg.auto_reply_check_ms.max(1000);
        let recent: Vec<&SelfMessage> = recent_self
            .iter()
            .filter(|m| now - m.at_ms < 15_000)
            .collect();
        let asked = recent.iter().any(|m| {
            // 与 JS 的 /[?？吗呢怎么有没有能不能]/ 逐字对齐：**不含 '不'**
            m.text
                .chars()
                .any(|c| matches!(c, '?' | '？' | '吗' | '呢' | '怎' | '么' | '有' | '没' | '能'))
        });
        if asked {
            delay = (delay as f64 * 1.5).round() as i64;
        }
        if recent.len() >= 3 {
            delay = (delay as f64 * 1.3).round() as i64;
        }
        self.reply_check_at = Some(now + delay);
        Outcome::Armed
    }

    /// 对应 JS 的 setupSleepTimerV2。
    pub fn setup_sleep_timer(&mut self, now: i64) -> Outcome {
        if !self.cfg.enabled {
            return Outcome::Skipped("socialV2 已关闭");
        }
        // 有限睡眠配置已接管，取消回复检查。
        self.reply_check_at = None;
        self.sleep_at = None;
        if self.paused {
            return Outcome::Skipped("已暂停");
        }
        if self.wc.infinite || self.wc.sleep_until.is_none() {
            return Outcome::Skipped("无限潜水");
        }
        let Some(until) = parse_iso_ms(self.wc.sleep_until.as_deref().unwrap_or("")) else {
            return Outcome::Skipped("sleepUntil 非法");
        };
        let delay = until - now;
        if delay <= 0 {
            return Outcome::FireNow("timeout".to_string());
        }
        // 远未来定时器先等一小段再续期，而不是提前唤醒。
        // 每次续期都用 now 重新算剩余，系统时钟被校正时能很快纠正。
        const MAX_TIMEOUT_MS: i64 = 100_009;
        if delay > MAX_TIMEOUT_MS {
            self.sleep_at = Some(now + MAX_TIMEOUT_MS);
            return Outcome::Armed;
        }
        self.sleep_at = Some(now + delay);
        Outcome::Armed
    }

    /// 对应 JS 的 scheduleProactiveCheckV2 的「安排下一次」部分。
    /// `rand01` 由调用方给，保证可测试。
    pub fn schedule_proactive(&mut self, now: i64, rand01: f64) -> Outcome {
        if !self.cfg.proactive_enabled || self.paused {
            return Outcome::Skipped("主动机会已关闭");
        }
        if self.proactive_at.is_some() {
            return Outcome::AlreadyArmed;
        }
        let min = self.cfg.proactive_min_ms.max(60_000);
        let max = self.cfg.proactive_max_ms.max(min);
        let span = (max - min) as f64;
        let delay = (min as f64 + rand01.clamp(0.0, 1.0) * span).floor() as i64;
        self.proactive_at = Some(now + delay);
        Outcome::Armed
    }

    /// 主动机会到点时的判定（对应 JS setTimeout 回调里的概率链）。
    /// 返回 Some(概率) 表示这次要唤醒，None 表示不唤醒。
    pub fn proactive_should_fire(
        &self,
        now: i64,
        rand01: f64,
        pending_thoughts: usize,
        self_msgs_last_hour: usize,
    ) -> bool {
        let idle = now - self.last_incoming_at;
        if idle < self.cfg.proactive_idle_ms {
            return false;
        }
        let mut prob = if self.cfg.proactive_probability.is_finite() {
            self.cfg.proactive_probability.clamp(0.0, 1.0)
        } else {
            0.4
        };
        if pending_thoughts > 0 {
            prob = (prob * 1.4).min(1.0);
        }
        if self.last_ai_reply_at > 0 && now - self.last_ai_reply_at < 30 * 60 * 1000 {
            prob *= 0.5;
        }
        // 深夜降权（23:00~08:00）：用本地时间的小时数。
        let hour = local_hour(now);
        if !(8..23).contains(&hour) {
            prob *= 0.3;
        }
        // 最近一小时自己已经说了 5 条以上 → 概率再降到 3 成。
        // 这条原本漏了：Rust 会比 JS 更爱说话，而"更爱说话"在群里是风控风险。
        if self_msgs_last_hour >= 5 {
            prob *= 0.3;
        }
        if prob <= 0.0 {
            return false;
        }
        rand01.clamp(0.0, 1.0) < prob && !self.is_busy(now)
    }

    /// 对应 JS 的 replayPendingWakeReasonsV2：一次排空。
    /// `unread_seqs` is retained unread state, not urgent-delivery evidence.
    /// The live hub removes positively delivered urgent wakes before replay.
    pub fn replay_pending(
        &mut self,
        now: i64,
        unread_seqs: &[i64],
        recent_self: &[SelfMessage],
    ) -> ReplayOutcome {
        if self.pending.is_empty() {
            return ReplayOutcome::Empty;
        }
        let drained: Vec<PendingWake> = self.pending.drain(..).collect();

        let (timers, messages): (Vec<_>, Vec<_>) = drained
            .into_iter()
            .partition(|item| is_timer_reason(&item.reason));

        // ① 定时器类：逐个处理，不发消息。
        let mut timer_count = 0usize;
        //
        // 注意 replyCheck：JS 的 scheduleReplyCheckV2 会**重新读状态**再算延迟
        // （近 15 秒自己说过话、说过问句都会拉长）。这里必须把 recent_self 传进去，
        // 早先传的是空切片 —— 延迟恒按基础值算，比 JS 更早唤醒。

        for item in &timers {
            match item.reason.as_str() {
                "timeout" => {
                    // 睡眠定时器可能在繁忙期间被清掉，这里重新按剩余时间续上。
                    self.setup_sleep_timer(now);
                    timer_count += 1;
                }
                "replyCheck" => {
                    self.schedule_reply_check(now, recent_self);
                    timer_count += 1;
                }
                "proactiveCheck" => {
                    // 繁忙期间的主动机会已过期，直接丢掉（不补发，避免翻旧账）。
                }
                _ => {
                    self.schedule_wake(&item.reason, now);
                    timer_count += 1;
                }
            }
        }

        if messages.is_empty() {
            return ReplayOutcome::Timers { count: timer_count };
        }

        // Coalesce once. An urgent wake survives retention eviction until the hub
        // proves delivery; ordinary wakes retain the unread-membership rule.
        let relevant: Vec<&PendingWake> = messages
            .iter()
            .filter(|item| item.reason == "atMention" || item.reason.starts_with("speaker:") || unread_seqs.contains(&item.seq))
            .collect();
        if let Some(pick) = relevant.last() {
            let merged = relevant.len();
            let reason = pick.reason.clone();
            self.schedule_wake(&reason, now);
            return ReplayOutcome::Coalesced { picked: reason, merged };
        }
        ReplayOutcome::AllHandled {
            count: messages.len(),
        }
    }

    /// 对应 JS 的「定时器到点」。返回该发的唤醒原因（如果有）。
    pub fn fire_due(&mut self, now: i64) -> Vec<String> {
        let mut out = Vec::new();

        if let Some(at) = self.pending_wake_at {
            if at <= now {
                self.pending_wake_at = None;
                self.message_wake_first_at = None;
                self.message_wake_urgent = false;
                if let Some(reason) = self.pending_wake_reason.take() {
                    out.push(reason);

                }
            }
        }

        if let Some(at) = self.sleep_at {
            if at <= now {
                self.sleep_at = None;
                // 远未来定时器续期而不是唤醒。
                if let Some(until) = self
                    .wc
                    .sleep_until
                    .as_deref()
                    .and_then(parse_iso_ms)
                {
                    if until > now {
                        self.setup_sleep_timer(now);
                    } else {
                        out.push("timeout".to_string());
                    }
                }
            }
        }

        if let Some(at) = self.reply_check_at {
            if at <= now {
                self.reply_check_at = None;
                // 已经回复过最新消息就别再唤醒。
                // 根因：群友喊一句 → AI 回复 → 回复检查又醒 → AI 不知道
                // 自己刚回过（出站消息不在上下文里），于是对同一条消息回第二遍。
                let already_replied =
                    self.last_ai_reply_at > 0 && self.last_ai_reply_at >= self.last_incoming_at;
                if !already_replied {
                    out.push("replyCheck".to_string());
                }
            }
        }

        out.into_iter().filter_map(|reason| {
            if self.is_busy(now) {
                // Frozen JS buffers busy timer callbacks; replay rearms timers using current
                // state rather than turning an expired reply-check into an unconditional wake.
                self.schedule_wake(&reason, now);
                None
            } else {
                self.admit_wake(reason, now)
            }
        }).collect()
    }

    pub fn admit_wake(&mut self, reason: String, now: i64) -> Option<String> {
        if let Some(wait_ms) = self.rate_limit_wait(now) {
            if self.pending_wake_reason.as_ref().is_none_or(|old| wake_priority(&reason) > wake_priority(old)) {
                self.pending_wake_reason = Some(reason);
            }
            self.pending_wake_at = Some(now + wait_ms);
            None
        } else {
            self.wake_times.push(now);
            self.trim_wake_times(now);
            Some(reason)
        }
    }

    /// 下一个最近的截止时刻，调用方用它决定睡多久。
    /// 回合硬上限的截止时刻（只在真的有回合时存在）。
    /// While she is engaged the cap runs from her last send in this turn, never past the ceiling.
    pub fn turn_cap_at(&self) -> Option<i64> {
        match self.turn_started_at {
            Some(t) if t > 0 => {
                let anchor = if self.engaged_at >= t { self.engaged_at } else { t };
                let ceiling = t.saturating_add(self.cfg.turn_ceiling_ms.max(self.cfg.turn_hard_cap_ms));
                Some(anchor.saturating_add(self.cfg.turn_hard_cap_ms).min(ceiling))
            }
            _ => None,
        }
    }

    pub fn next_deadline(&self) -> Option<i64> {
        [
            self.pending_wake_at,
            self.sleep_at,
            self.reply_check_at,
            self.proactive_at,
            self.turn_cap_at(),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// 防永眠：当前配置至少要有一个能触发唤醒的途径。
pub fn is_wakeable(wc: &WakeConfig, now: i64) -> bool {
    if wc.mode_active || wc.any_message {
        return true;
    }
    if wc.at_mention
        || wc.name_mention
        || wc.poke
        || wc.question_boolean
        || wc.probability > 0.0
        || !wc.keywords.is_empty()
        || !wc.speaker_ids.is_empty()
    {
        return true;
    }
    if !wc.infinite {
        if let Some(until) = wc.sleep_until.as_deref().and_then(parse_iso_ms) {
            if until > now {
                return true;
            }
        }
    }
    false
}

/// 简化版 ISO-8601 解析：支持 `2026-10-21T05:00:00.000Z` 与带时区的形式。
/// 只处理桥接产生的格式，不追求通用。
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 直接复用系统解析能力的简化实现：拆日期与时间。
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    let time_part = rest
        .trim_end_matches('Z')
        .split(['+', '-'])
        .next()
        .unwrap_or(rest);
    let mut t = time_part.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec_part = t.next().unwrap_or("0");
    let se: f64 = sec_part.parse().ok()?;
    Some(civil_to_ms(y, mo, da, h, mi, se))
}

fn civil_to_ms(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: f64) -> i64 {
    // Howard Hinnant 的 days_from_civil
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60;
    (secs as f64 * 1000.0 + (s * 1000.0)) as i64
}

fn local_hour(now_ms: i64) -> i64 {
    // 中国区固定 UTC+8，不做夏令时。
    ((now_ms / 1000 + 8 * 3600).div_euclid(3600)) % 24
}

// ---- wake-config 的纯函数部分（从 bridge.js 的 POST /api/socialV2/wake-config 抽出）----

/// triggers 的合法字段（JS 的 `WAKE_TRIGGER_KEYS`）：写入与加载都只认这几个。
pub const WAKE_TRIGGER_KEYS: [&str; 8] = [
    "atMention",
    "nameMention",
    "speakerIds",
    "keywords",
    "question",
    "poke",
    "anyMessage",
    "probability",
];

fn obj_of(v: &Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn flag_of(obj: &serde_json::Map<String, Value>, k: &str) -> bool {
    obj.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// JS 的 `normalizeSpeakerIdsV2`：只留正整数 QQ 串，去重，最多 20 个。
pub fn normalize_speaker_ids(value: Option<&Value>) -> Vec<String> {
    let Some(v) = value else { return Vec::new() };
    let raw: Vec<String> = match v {
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Value::String(s) => s
            .split([',', '，'])
            .flat_map(|p| p.split_whitespace().map(|x| x.to_string()))
            .collect(),
        Value::Null => Vec::new(),
        other => vec![other.to_string()],
    };
    let mut out: Vec<String> = Vec::new();
    for item in raw {
        let s = item.trim().to_string();
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) || s.starts_with('0') {
            continue;
        }
        if out.contains(&s) {
            continue;
        }
        out.push(s);
        if out.len() >= 20 {
            break;
        }
    }
    out
}

/// JS 的 `isSleepingConfigV2`：active 或 anyMessage 都不算「沉睡」。
pub fn is_sleeping_config(wc: &Value) -> bool {
    let tr = wc.get("triggers").map(obj_of).unwrap_or_default();
    if wc.get("mode").and_then(|v| v.as_str()) == Some("active") || flag_of(&tr, "anyMessage") {
        return false;
    }
    true
}

/// JS 的 `hasExplicitEndV2`：最后一条别人说的话里有没有明确的结束语。
fn has_explicit_end(st: &Value) -> bool {
    let Some(list) = st.get("recentMessages").and_then(|v| v.as_array()) else {
        return false;
    };
    let Some(last) = list
        .iter()
        .rev()
        .find(|m| !m.get("isSelf").and_then(|v| v.as_bool()).unwrap_or(false))
    else {
        return false;
    };
    let text = last
        .get("tail")
        .or_else(|| last.get("plain"))
        .or_else(|| last.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    const ENDS: [&str; 27] = [
        "不聊了", "不说了", "晚安", "睡了", "先睡了", "下了", "先下了", "拜拜", "再见", "走了",
        "先走", "撤了", "去忙", "忙了", "下次再聊", "下次聊", "散了吧", "结束", "就到这", "先这样",
        "就这样吧", "886", "88", "睡觉了", "下班了", "去洗澡", "去吃饭了",
    ];
    ENDS.iter().any(|e| text.contains(&e.to_lowercase()))
}

pub const PRE_SLEEP_WAIT_DEFAULT_MS: i64 = 300_000;
/// Default `socialV2.wake.turnCeilingMs`: 30 minutes from dispatch.
pub const TURN_CEILING_DEFAULT_MS: i64 = 1_800_000;
/// Default `max_quiet` of a messages wait: `min(120000, wait.maxMs default 600000)`.
const DEFAULT_WAIT_MAX_QUIET_MS: i64 = 120_000;
/// A messages wait may poll past its quiet bound by up to this much (`console::handle_wait_post`).
const WAIT_QUIET_SLACK_MS: i64 = 5_000;
const TURN_HEADROOM_MS: i64 = 90_000;
pub const COLLECTOR_HEADROOM_MS: i64 = 30_000;
const MIN_TURN_HARD_CAP_MS: i64 = 120_000;
/// Default dispatch-clock cap: 300000 + 120000 + 5000 + 90000.
pub const DEFAULT_TURN_HARD_CAP_MS: i64 =
    PRE_SLEEP_WAIT_DEFAULT_MS + DEFAULT_WAIT_MAX_QUIET_MS + WAIT_QUIET_SLACK_MS + TURN_HEADROOM_MS;

/// The pre-sleep gate's reading (`pre_sleep_remaining`, wait results).
pub fn pre_sleep_wait_ms(cfg: &Value) -> i64 {
    cfg.pointer("/socialV2/wake/preSleepWaitMs")
        .and_then(Value::as_i64).filter(|ms| *ms >= 0)
        .unwrap_or(PRE_SLEEP_WAIT_DEFAULT_MS)
}

/// The observation the agent is told to wait for (prompt, closing reminder, closing refusals).
/// JS `Math.max(0, Number(cfg.socialV2?.wake?.preSleepWaitMs) || 300000)`: 0 and non-numbers mean 300000.
pub fn prompt_pre_sleep_wait_ms(cfg: &Value) -> i64 {
    let n = cfg
        .pointer("/socialV2/wake/preSleepWaitMs")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n != 0.0)
        .unwrap_or(PRE_SLEEP_WAIT_DEFAULT_MS as f64);
    n.max(0.0) as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnBudget {
    pub hard_cap_ms: i64,
    pub collector_ms: u64,
}

/// The longest observation a turn may be asked for, plus the messages wait's quiet extension.
/// The prompt does not read `preSleepWaitEnabled`, so neither does the budget.
fn observation_with_quiet_ms(cfg: &Value) -> i64 {
    prompt_pre_sleep_wait_ms(cfg)
        .max(pre_sleep_wait_ms(cfg))
        .saturating_add(crate::console::wait_max_quiet_ms(cfg))
        .saturating_add(WAIT_QUIET_SLACK_MS)
}

fn derived_turn_budget(cfg: &Value) -> TurnBudget {
    let hard_cap_ms = observation_with_quiet_ms(cfg).saturating_add(TURN_HEADROOM_MS).max(MIN_TURN_HARD_CAP_MS);
    TurnBudget { hard_cap_ms, collector_ms: hard_cap_ms.saturating_add(COLLECTOR_HEADROOM_MS) as u64 }
}

/// Strict budget check for a configuration save: `socialV2.wake.turnHardCapMs`, when present,
/// must be an integer at least observation + quiet extension + 30000 ms.
pub fn checked_turn_budget(cfg: &Value) -> Result<TurnBudget, &'static str> {
    const INVALID: &str = "reserved2 turn budget is invalid: turnHardCapMs must be an integer at least the pre-sleep observation plus the wait's quiet extension plus 30000 ms";
    let Some(value) = cfg.pointer("/socialV2/wake/turnHardCapMs") else {
        return Ok(derived_turn_budget(cfg));
    };
    let minimum = observation_with_quiet_ms(cfg).checked_add(COLLECTOR_HEADROOM_MS).ok_or(INVALID)?;
    let hard_cap_ms = value.as_i64().filter(|ms| *ms >= minimum).ok_or(INVALID)?;
    let collector_ms = hard_cap_ms.checked_add(COLLECTOR_HEADROOM_MS).ok_or(INVALID)? as u64;
    Ok(TurnBudget { hard_cap_ms, collector_ms })
}

/// One reserved2 budget, shared by dispatch and collection. A running bot never fails on an
/// invalid override (saves refuse it): it uses the derived budget and logs each new bad value once.
pub fn turn_budget(cfg: &Value) -> TurnBudget {
    match checked_turn_budget(cfg) {
        Ok(budget) => budget,
        Err(error) => {
            static LOGGED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
            let shown = cfg.pointer("/socialV2/wake/turnHardCapMs").map(Value::to_string).unwrap_or_default();
            let mut logged = LOGGED.lock().unwrap_or_else(|p| p.into_inner());
            let budget = derived_turn_budget(cfg);
            if logged.as_deref() != Some(shown.as_str()) {
                eprintln!("[wake] ignoring socialV2.wake.turnHardCapMs={shown}: {error}; using {} ms", budget.hard_cap_ms);
                *logged = Some(shown);
            }
            budget
        }
    }
}

/// Upper bound of `socialV2.wake.turnCeilingMs`: two hours from dispatch.
pub const TURN_CEILING_MAX_MS: i64 = 7_200_000;

/// Strict check for a configuration save: `socialV2.wake.turnCeilingMs`, when present, must be
/// an integer from 1 to `TURN_CEILING_MAX_MS`.
pub fn checked_turn_ceiling(cfg: &Value) -> Result<(), &'static str> {
    const INVALID: &str = "reserved2 turn ceiling is invalid: turnCeilingMs must be an integer from 1 to 7200000 ms";
    match cfg.pointer("/socialV2/wake/turnCeilingMs") {
        None => Ok(()),
        Some(value) => value.as_i64().filter(|ms| (1..=TURN_CEILING_MAX_MS).contains(ms)).map(|_| ()).ok_or(INVALID),
    }
}

/// Absolute per-turn ceiling (`socialV2.wake.turnCeilingMs`, default 1800000); never below the
/// hard cap. A running bot never fails on an invalid override (saves refuse it): it uses the
/// default and logs each new bad value once.
pub fn turn_ceiling_ms(cfg: &Value) -> i64 {
    let ceiling = match checked_turn_ceiling(cfg) {
        Ok(()) => cfg.pointer("/socialV2/wake/turnCeilingMs").and_then(Value::as_i64).unwrap_or(TURN_CEILING_DEFAULT_MS),
        Err(error) => {
            static LOGGED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
            let shown = cfg.pointer("/socialV2/wake/turnCeilingMs").map(Value::to_string).unwrap_or_default();
            let mut logged = LOGGED.lock().unwrap_or_else(|p| p.into_inner());
            if logged.as_deref() != Some(shown.as_str()) {
                eprintln!("[wake] ignoring socialV2.wake.turnCeilingMs={shown}: {error}; using {TURN_CEILING_DEFAULT_MS} ms");
                *logged = Some(shown);
            }
            TURN_CEILING_DEFAULT_MS
        }
    };
    ceiling.max(turn_budget(cfg).hard_cap_ms)
}

/// The reserved2 collector covers an engaged turn up to its ceiling.
pub fn reserved2_collector_ms(cfg: &Value) -> u64 {
    turn_ceiling_ms(cfg).saturating_add(COLLECTOR_HEADROOM_MS) as u64
}

/// Length of her engagement window (`preSleepWaitMs`), or `None` when the window does not
/// exist (`preSleepWaitEnabled=false` or a non-positive gate reading).
pub fn engagement_window_ms(cfg: &Value) -> Option<i64> {
    if cfg.pointer("/socialV2/wake/preSleepWaitEnabled") == Some(&Value::Bool(false)) {
        return None;
    }
    Some(pre_sleep_wait_ms(cfg)).filter(|ms| *ms > 0)
}

/// End of her engagement window: `preSleepWaitMs` after her last successful send (`engagedAt`,
/// cleared at each wake claim). Only her own sends move it; incoming messages never do.
pub fn engagement_end(st: &Value, cfg: &Value) -> Option<i64> {
    let window = engagement_window_ms(cfg)?;
    let at = st.get("engagedAt").and_then(Value::as_i64).filter(|at| *at > 0)?;
    Some(at.saturating_add(window))
}

/// JS 的 `preSleepWaitBlockedV2`：返回还要等多久（None = 不拦）。
/// While she is engaged the requirement never outlasts her engagement window.
pub fn pre_sleep_remaining(st: &Value, cfg: &Value, now: i64) -> Option<i64> {
    let w = cfg
        .get("socialV2")
        .and_then(|s| s.get("wake"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if w.get("preSleepWaitEnabled").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    if has_explicit_end(st) {
        return None;
    }
    let window_end = engagement_end(st, cfg);
    if window_end.is_some_and(|end| now >= end) {
        return None;
    }
    let wait_ms = pre_sleep_wait_ms(cfg);
    let last_incoming = st.get("lastIncomingAt").and_then(|v| v.as_i64()).unwrap_or(0);
    if last_incoming > 0 && now - last_incoming >= wait_ms {
        return None;
    }
    let satisfied = st.get("preSleepWaitSatisfiedAt").and_then(|v| v.as_i64()).unwrap_or(0);
    if satisfied > 0 && (last_incoming == 0 || last_incoming <= satisfied) {
        return None;
    }
    let observed = st.get("preSleepWaitObservedAt").and_then(|v| v.as_i64()).unwrap_or(0);
    if observed > 0 && (last_incoming == 0 || last_incoming <= observed) {
        return None;
    }
    let remaining = if last_incoming > 0 {
        (wait_ms - (now - last_incoming)).max(0)
    } else {
        wait_ms
    };
    Some(window_end.map_or(remaining, |end| remaining.min(end - now)))
}

fn is_infinite(wc: &Value) -> bool {
    wc.get("infinite").and_then(|v| v.as_bool()).unwrap_or(true)
}

fn has_any_trigger(wc: &Value) -> bool {
    let t = wc.get("triggers").map(obj_of).unwrap_or_default();
    flag_of(&t, "atMention")
        || flag_of(&t, "nameMention")
        || flag_of(&t, "poke")
        || flag_of(&t, "question")
        || flag_of(&t, "anyMessage")
        || t.get("keywords").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false)
        || t.get("probability").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.0
        || t.get("speakerIds").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false)
}

/// `POST /api/socialV2/wake-config` 的配置归一化（纯函数，不含计时器副作用）。
///
/// `rand01` 是 [0,1) 的随机数，只在「有限睡眠但没给时间」时用来取推荐时长；
/// 传进来而不是内部随机，是为了离线重放/测试可复现。
pub fn normalize_wake_config(
    current: &Value,
    input: &Value,
    cfg: &Value,
    key: &str,
    now: i64,
    rand01: f64,
) -> Result<Value, (u16, String)> {
    let cur = current.as_object().cloned().unwrap_or_default();
    let input_obj = input.as_object().cloned().unwrap_or_default();
    let cur_tr = cur.get("triggers").map(obj_of).unwrap_or_default();
    let in_tr = input_obj.get("triggers").map(obj_of).unwrap_or_default();

    // pickWakeTriggersV2：按固定键序，input 有就用 input，否则用当前值
    let mut triggers = serde_json::Map::new();
    for name in WAKE_TRIGGER_KEYS {
        let v = if in_tr.contains_key(name) {
            in_tr.get(name).cloned()
        } else {
            cur_tr.get(name).cloned()
        };
        if let Some(v) = v {
            triggers.insert(name.to_string(), v);
        }
    }
    let trig_bool = |name: &str, fallback: bool| -> bool {
        if in_tr.contains_key(name) {
            in_tr.get(name).and_then(|v| v.as_bool()) == Some(true)
        } else {
            fallback
        }
    };
    let keywords: Vec<Value> = match in_tr.get("keywords") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|k| match k {
                Value::String(s) => s.trim().to_string(),
                Value::Null => String::new(),
                other => other.to_string().trim().to_string(),
            })
            .filter(|s| !s.is_empty())
            .take(50)
            .map(|s| Value::String(s.chars().take(100).collect()))
            .collect(),
        Some(_) => Vec::new(),
        None => cur_tr
            .get("keywords")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .take(50)
                    .map(|k| match k {
                        Value::String(s) => Value::String(s.chars().take(100).collect()),
                        other => other.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    triggers.insert("atMention".into(), Value::Bool(trig_bool("atMention", flag_of(&cur_tr, "atMention"))));
    triggers.insert("nameMention".into(), Value::Bool(trig_bool("nameMention", flag_of(&cur_tr, "nameMention"))));
    triggers.insert("question".into(), Value::Bool(trig_bool("question", flag_of(&cur_tr, "question"))));
    triggers.insert("anyMessage".into(), Value::Bool(trig_bool("anyMessage", flag_of(&cur_tr, "anyMessage"))));
    triggers.insert("poke".into(), Value::Bool(trig_bool("poke", flag_of(&cur_tr, "poke"))));
    triggers.insert("keywords".into(), Value::Array(keywords));

    let cur_mode = cur.get("mode").and_then(|v| v.as_str()).unwrap_or("diving");
    let mode = match input_obj.get("mode").and_then(|v| v.as_str()) {
        Some("active") => "active".to_string(),
        Some("diving") => "diving".to_string(),
        _ => cur_mode.to_string(),
    };
    let batch_window = match input_obj.get("batchWindowMs").and_then(|v| v.as_f64()) {
        Some(n) if n >= 1000.0 => Value::from((n as i64).min(3_600_000)),
        _ => cur.get("batchWindowMs").cloned().unwrap_or(Value::Null),
    };

    let mut next = cur.clone();
    next.insert("mode".into(), Value::String(mode.clone()));
    next.insert("infinite".into(), Value::Bool(is_infinite(current)));
    next.insert("sleepUntil".into(), cur.get("sleepUntil").cloned().unwrap_or(Value::Null));
    next.insert("triggers".into(), Value::Object(triggers));
    next.insert("batchWindowMs".into(), batch_window);

    let trig_map = |m: &serde_json::Map<String, Value>| -> serde_json::Map<String, Value> {
        m.get("triggers").map(obj_of).unwrap_or_default()
    };

    if mode == "diving" && !in_tr.contains_key("anyMessage") {
        let mut t = trig_map(&next);
        t.insert("anyMessage".into(), Value::Bool(false));
        next.insert("triggers".into(), Value::Object(t));
    }
    if mode == "active" {
        next.insert("infinite".into(), Value::Bool(true));
        next.insert("sleepUntil".into(), Value::Null);
        let mut t = trig_map(&next);
        t.insert("anyMessage".into(), Value::Bool(true));
        next.insert("triggers".into(), Value::Object(t));
    }
    if mode != "active" {
        if let Some(b) = input_obj.get("infinite").and_then(|v| v.as_bool()) {
            next.insert("infinite".into(), Value::Bool(b));
        }
    }

    let wake_cfg = cfg
        .get("socialV2")
        .and_then(|s| s.get("wake"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let num = |k: &str, d: i64| -> i64 {
        wake_cfg.get(k).and_then(|v| v.as_i64()).filter(|n| *n != 0).unwrap_or(d)
    };
    let min_ms = num("sleepMinMs", 0).max(0);
    let max_ms = num("sleepMaxMs", 0).max(0);

    if is_infinite(&Value::Object(next.clone())) {
        next.insert("sleepUntil".into(), Value::Null);
    } else if let Some(s) = input_obj.get("sleepUntil").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        if let Some(ms) = parse_iso_ms(s) {
            next.insert("sleepUntil".into(), Value::String(crate::sticker::iso_at(ms)));
        }
    } else if let Some(ms_in) = input_obj.get("sleepMs").and_then(|v| v.as_f64()) {
        let mut ms = ms_in.round().max(0.0) as i64;
        if ms < min_ms {
            ms = min_ms;
        }
        if max_ms > 0 && ms > max_ms {
            ms = max_ms;
        }
        next.insert("sleepUntil".into(), Value::String(crate::sticker::iso_at(now + ms)));
    } else if next.get("sleepUntil").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).is_none() {
        let rec_min = num("recommendedSleepMinMs", 300_000);
        let rec_max = num("recommendedSleepMaxMs", 7_200_000);
        let span = (rec_max - rec_min).max(0) as f64;
        let ms = rec_min + (rand01.clamp(0.0, 0.999_999) * span).round() as i64;
        next.insert("sleepUntil".into(), Value::String(crate::sticker::iso_at(now + ms)));
    }
    if !is_infinite(&Value::Object(next.clone())) {
        if let Some(s) = next.get("sleepUntil").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            if let Some(mut until) = parse_iso_ms(s) {
                if min_ms > 0 && until < now + min_ms {
                    until = now + min_ms;
                }
                if max_ms > 0 && until > now + max_ms {
                    until = now + max_ms;
                }
                next.insert("sleepUntil".into(), Value::String(crate::sticker::iso_at(until)));
            }
        }
    }

    let mut t = trig_map(&next);
    if let Some(p) = t.get("probability") {
        let pn = p.as_f64().unwrap_or(0.0);
        t.insert("probability".into(), json!(pn.clamp(0.0, 1.0)));
    }
    if in_tr.contains_key("poke") {
        t.insert("poke".into(), Value::Bool(in_tr.get("poke").and_then(|v| v.as_bool()) == Some(true)));
    }
    let speakers = normalize_speaker_ids(t.get("speakerIds"));
    t.insert("speakerIds".into(), Value::Array(speakers.into_iter().map(Value::String).collect()));
    if key.starts_with("private:") {
        t.insert("speakerIds".into(), Value::Array(Vec::new()));
    }
    next.insert("triggers".into(), Value::Object(t));

    let out = Value::Object(next);
    if is_infinite(&out) && !has_any_trigger(&out) {
        return Err((
            400,
            "无限期潜水必须至少保留一个唤醒条件（@/名字/拍一拍/关键词/提问/anyMessage/概率>0），否则 AI 可能永眠"
                .to_string(),
        ));
    }
    Ok(out)
}

// ---- 默认唤醒配置 + 防永眠（从 bridge.js 的 defaultWakeConfigV2 / ensureWakeableV2 抽出）----

/// JS 的 `defaultWakeConfigV2`：给「没有任何唤醒条件」的会话兜底。
///
/// `rand01` 同样由调用方传入 —— 有限睡眠的时长是推荐区间里的随机值，
/// 内部随机就没法离线重放/对拍。
pub fn default_wake_config(cfg: &Value, now: i64, rand01: f64) -> Value {
    let w = cfg
        .get("socialV2")
        .and_then(|s| s.get("wake"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mode_active = w.get("defaultMode").and_then(|v| v.as_str()) == Some("active");
    let infinite = w.get("recommendedDefaultInfinite").and_then(|v| v.as_bool()) != Some(false);
    let num = |k: &str, d: i64| -> i64 {
        w.get(k).and_then(|v| v.as_i64()).filter(|n| *n != 0).unwrap_or(d)
    };
    let rec_min = num("recommendedSleepMinMs", 300_000);
    let rec_max = num("recommendedSleepMaxMs", 7_200_000);
    let mut finite = rec_min as f64 + rand01.clamp(0.0, 0.999_999) * ((rec_max - rec_min).max(0)) as f64;
    let hard_min = num("sleepMinMs", 0).max(0);
    let hard_max = num("sleepMaxMs", 0);
    if hard_min > 0 && finite < hard_min as f64 {
        finite = hard_min as f64;
    }
    if hard_max > 0 && finite > hard_max as f64 {
        finite = hard_max as f64;
    }
    let flag = |k: &str| w.get(k).and_then(|v| v.as_bool()) != Some(false);
    let probability = w
        .get("recommendedProbability")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    json!({
        "mode": if mode_active { "active" } else { "diving" },
        "infinite": infinite,
        "sleepUntil": if infinite { Value::Null } else { Value::String(crate::sticker::iso_at(now + finite.round() as i64)) },
        "triggers": {
            "atMention": flag("recommendedAtMention"),
            "nameMention": flag("recommendedNameMention"),
            "speakerIds": [],
            "keywords": w.get("recommendedKeywords").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
            "question": flag("recommendedQuestion"),
            "poke": flag("recommendedPoke"),
            "anyMessage": mode_active,
            "probability": probability,
        },
        "batchWindowMs": w.get("batchWindowMs").and_then(|v| v.as_i64()).unwrap_or(8000).max(1000),
        "lastWakeAt": 0,
        "wakeCount": 0,
        "noActionCount": 0,
        "confirmedAt": 0,
        "confirmedBy": "default",
    })
}

/// JS 的 `ensureWakeableV2`：配置里一个触发条件都没有时重置为默认，避免永眠。
///
/// 返回 true 表示重置过。定时器重排不在这一层（见 handle 里的说明）。
pub fn ensure_wakeable(wc: &mut Value, cfg: &Value, now: i64, rand01: f64) -> bool {
    if !wc.is_object() {
        return false;
    }
    if !wc.get("triggers").is_some_and(|v| v.is_object()) {
        wc["triggers"] = json!({});
    }
    let speakers = normalize_speaker_ids(wc["triggers"].get("speakerIds"));
    wc["triggers"]["speakerIds"] = json!(speakers);

    let timed = wc.get("infinite").and_then(|v| v.as_bool()) == Some(false)
        && wc
            .get("sleepUntil")
            .and_then(|v| v.as_str())
            .and_then(parse_iso_ms)
            .map(|t| t > now)
            .unwrap_or(false);
    let wakeable = timed || has_any_trigger(wc);
    if wakeable {
        return false;
    }
    *wc = default_wake_config(cfg, now, rand01);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_and_reply_timers_obey_the_same_wake_rate_as_messages() {
        for reason in ["timeout","replyCheck"] {
            let mut s=sched();
            s.cfg.max_wake_per_minute=1;
            s.wake_times=vec![1_000_000];
            if reason=="timeout" {
                s.wc.infinite=false;
                s.wc.sleep_until=Some(crate::sticker::iso_at(1_000_000));
                s.sleep_at=Some(1_001_000);
            } else { s.reply_check_at=Some(1_001_000); s.last_incoming_at=1_001_000; }
            assert!(s.fire_due(1_001_000).is_empty(),"{reason} bypassed cooldown");
            assert_eq!(s.pending_wake_reason.as_deref(),Some(reason));
            assert_eq!(s.fire_due(1_061_000),vec![reason.to_string()]);
        }
    }

    fn sched() -> WakeScheduler {
        // 生产默认是限频开着的（1/分钟、12/小时）。测试默认关掉 ——
        // 否则「连发三条应该排上三条」这类用例会被限频挡住，测的就不是调度了。
        // 限频本身由 test_rate_limit_* 覆盖。
        let mut cfg = WakeCfg::default();
        cfg.max_wake_per_minute = 0;
        cfg.max_wake_per_hour = 0;
        WakeScheduler::new(cfg, WakeConfig::default())
    }

    #[test]
    fn priority_uses_base_name() {
        assert_eq!(wake_priority("keyword:小鲸鱼"), 60);
        assert_eq!(wake_priority("atMention"), 90);
        assert!(wake_priority("atMention") > wake_priority("keyword:x"));
    }

    #[test]
    fn timer_reasons_are_recognised() {
        assert!(is_timer_reason("timeout"));
        assert!(is_timer_reason("replyCheck"));
        assert!(!is_timer_reason("atMention"));
        assert!(!is_timer_reason("keyword:大肥鱼"));
    }

    #[test]
    fn schedule_wake_arms_batch_window() {
        let mut s = sched();
        let out = s.schedule_wake("atMention", 1_000_000);
        assert_eq!(out, Outcome::Armed);
        assert_eq!(s.pending_wake_at, Some(1_000_000 + 1500));
        assert_eq!(s.pending_wake_reason.as_deref(), Some("atMention"));
    }

    #[test]
    fn first_wake_is_not_self_throttled() {
        // 回归：排上唤醒时**不能**记 wake_time，否则 fire_due 会自己把自己限频，
        // 第一次唤醒凭空晚 60 秒（实测 21:23:55 决定、21:24:55 才落地）。
        let mut s = sched();
        s.cfg.max_wake_per_minute = 1;
        s.cfg.max_wake_per_hour = 12;
        assert_eq!(s.schedule_wake("question", 2_000_000), Outcome::Armed);
        assert_eq!(
            s.pending_wake_at,
            Some(2_001_500),
            "只是排上，不该被限频推迟"
        );
        assert_eq!(s.fire_due(2_001_500), vec!["question".to_string()], "第一次唤醒必须按时发出");
    }
    #[test]
    fn rate_limit_defers_instead_of_dropping() {
        let mut s = sched();
        s.cfg.max_wake_per_minute = 1;
        s.cfg.max_wake_per_hour = 12;
        // 第一次：armed → 到点由 fire_due 真正发出去，**此时才记 wakeTime**
        assert_eq!(s.schedule_wake("atMention", 1_000_000), Outcome::Armed);
        assert_eq!(s.fire_due(1_001_500), vec!["atMention".to_string()]);

        // 同一分钟内的第二次：被压住，但要排重试，**不能直接丢**
        let out = s.schedule_wake("atMention", 1_010_000);
        let Outcome::RateLimited { wait_ms } = out else {
            panic!("应该被限频，实际 {out:?}");
        };
        assert!((1_000..=60_000).contains(&wait_ms), "重试间隔被夹到 1~60s，实际 {wait_ms}");
        assert_eq!(
            s.pending_wake_reason.as_deref(),
            Some("atMention"),
            "原因必须留着，否则这次 @ 就真丢了"
        );
        assert_eq!(s.pending_wake_at, Some(1_010_000 + wait_ms));
    }

    #[test]
    fn reply_check_delay_matches_js_question_charset() {
        // JS 的 /[?？吗呢怎么有没有能不能]/ 里**没有 '不'**。
        // 含「不」的消息不该被当成「问过问题」而拉长 1.5 倍。
        let mut s = sched();
        s.cfg.auto_reply_check_ms = 30_000;
        let say = |text: &str| SelfMessage { text: text.to_string(), at_ms: 1_000_000 };
        assert_eq!(s.schedule_reply_check(1_000_000, &[say("不行")]), Outcome::Armed);
        assert_eq!(s.reply_check_at, Some(1_030_000), "含「不」不该拉长延迟");

        let mut s2 = sched();
        s2.cfg.auto_reply_check_ms = 30_000;
        assert_eq!(s2.schedule_reply_check(1_000_000, &[say("你来吗")]), Outcome::Armed);
        assert_eq!(s2.reply_check_at, Some(1_045_000), "含「吗」按 JS 拉长 1.5 倍");
    }
    #[test]
    fn rate_limit_retry_rechecks_until_window_frees() {
        let mut s = sched();
        s.cfg.max_wake_per_minute = 1;
        s.cfg.max_wake_per_hour = 0;
        s.wake_times = vec![1_000_000];
        // 立刻到点重试：还在 60s 窗口里，继续压住
        s.pending_wake_at = Some(1_000_100);
        s.pending_wake_reason = Some("atMention".to_string());
        assert!(s.fire_due(1_000_100).is_empty(), "窗口没腾出来不该放行");
        assert_eq!(s.pending_wake_reason.as_deref(), Some("atMention"));
        // 过了一分钟：放行
        let fired = s.fire_due(1_061_000);
        assert_eq!(fired, vec!["atMention".to_string()]);
    }

    #[test]
    fn zero_limits_mean_unlimited() {
        let mut s = sched();
        s.cfg.max_wake_per_minute = 0;
        s.cfg.max_wake_per_hour = 0;
        for i in 0..20 {
            s.pending_wake_at = None;
            s.pending_wake_reason = None;
            assert_eq!(s.schedule_wake("atMention", 1_000_000 + i * 10), Outcome::Armed);
        }
    }

    #[test]
    fn second_wake_in_window_upgrades_by_priority() {        let mut s = sched();
        s.schedule_wake("keyword:小鲸鱼", 0);
        let out = s.schedule_wake("atMention", 100);
        assert_eq!(
            out,
            Outcome::Upgraded {
                from: "keyword:小鲸鱼".into(),
                to: "atMention".into()
            }
        );
        assert_eq!(s.pending_wake_reason.as_deref(), Some("atMention"));
    }

    #[test]
    fn lower_priority_does_not_downgrade() {
        let mut s = sched();
        s.schedule_wake("atMention", 0);
        let out = s.schedule_wake("keyword:x", 100);
        assert_eq!(out, Outcome::AlreadyArmed);
        assert_eq!(s.pending_wake_reason.as_deref(), Some("atMention"));
    }

    #[test]
    fn busy_conversation_buffers_reason() {
        let mut s = sched();
        s.turn_started_at = Some(1);
        s.last_unread_seq = 42;
        let out = s.schedule_wake("atMention", 1000);
        assert_eq!(out, Outcome::Buffered);
        assert_eq!(s.pending.len(), 1);
        assert_eq!(s.pending[0].seq, 42);
    }

    #[test]
    fn buffer_dedupes_same_reason_and_seq() {
        let mut s = sched();
        s.turn_started_at = Some(1);
        s.last_unread_seq = 7;
        s.schedule_wake("atMention", 1000);
        s.schedule_wake("atMention", 1001);
        assert_eq!(s.pending.len(), 1);
    }

    #[test]
    fn buffer_is_bounded_to_20() {
        let mut s = sched();
        s.turn_started_at = Some(1);
        for i in 0..30 {
            s.last_unread_seq = i;
            s.schedule_wake(&format!("keyword:{i}"), 1000);
        }
        assert_eq!(s.pending.len(), 20);
    }

    #[test]
    fn expired_lease_is_no_longer_busy() {
        let mut s = sched();
        s.turn_started_at = Some(1);
        assert!(s.is_busy(1000));
        // 租约默认 30 分钟
        assert!(!s.is_busy(31 * 60 * 1000));
    }

    #[test]
    fn reply_check_is_skipped_when_already_replied() {
        let mut s = sched();
        s.last_ai_reply_at = 5000;
        s.last_incoming_at = 4000;
        s.reply_check_at = Some(1000);
        let fired = s.fire_due(2000);
        assert!(fired.is_empty(), "已回复过就不该再唤醒");
    }

    #[test]
    fn reply_check_fires_when_newer_message_arrived() {
        let mut s = sched();
        s.last_ai_reply_at = 5000;
        s.last_incoming_at = 6000;
        s.reply_check_at = Some(1000);
        let fired = s.fire_due(2000);
        assert_eq!(fired, vec!["replyCheck".to_string()]);
    }

    #[test]
    fn reply_check_delay_stretches_for_questions() {
        let mut s = sched();
        let now = 100_000;
        s.schedule_reply_check(now, &[SelfMessage { at_ms: now, text: "你在吗？".into() }]);
        let delay = s.reply_check_at.unwrap() - now;
        assert_eq!(delay, 45_000, "问句延迟 ×1.5");
    }

    #[test]
    fn reply_check_delay_stretches_for_many_messages() {
        let mut s = sched();
        let now = 100_000;
        let msgs: Vec<SelfMessage> = (0..3)
            .map(|i| SelfMessage {
                at_ms: now - i * 1000,
                text: "好的".into(),
            })
            .collect();
        s.schedule_reply_check(now, &msgs);
        let delay = s.reply_check_at.unwrap() - now;
        assert_eq!(delay, 39_000, "3 条以上延迟 ×1.3");
    }

    #[test]
    fn sleep_timer_not_armed_when_infinite() {
        let mut s = sched();
        let out = s.setup_sleep_timer(0);
        assert!(matches!(out, Outcome::Skipped(_)));
        assert_eq!(s.sleep_at, None);
    }

    #[test]
    fn sleep_timer_arms_for_finite() {
        let mut s = sched();
        s.wc.infinite = false;
        s.wc.sleep_until = Some("2026-10-21T05:00:00.000Z".into());
        let now = parse_iso_ms("2026-10-21T04:00:00.000Z").unwrap();
        let out = s.setup_sleep_timer(now);
        assert_eq!(out, Outcome::Armed);
        // 1 小时 > 100 秒续期上限，所以先排一小段再续期
        assert_eq!(s.sleep_at, Some(now + 100_009));
    }

    #[test]
    fn sleep_timer_arms_directly_for_short_sleep() {
        let mut s = sched();
        s.wc.infinite = false;
        s.wc.sleep_until = Some("2026-10-21T04:01:00.000Z".into());
        let now = parse_iso_ms("2026-10-21T04:00:00.000Z").unwrap();
        let out = s.setup_sleep_timer(now);
        assert_eq!(out, Outcome::Armed);
        assert_eq!(s.sleep_at, Some(now + 60_000), "60 秒内直接排");
    }

    #[test]
    fn sleep_timer_fires_immediately_when_expired() {
        let mut s = sched();
        s.wc.infinite = false;
        s.wc.sleep_until = Some("2026-10-21T04:00:00.000Z".into());
        let now = parse_iso_ms("2026-10-21T05:00:00.000Z").unwrap();
        let out = s.setup_sleep_timer(now);
        assert_eq!(out, Outcome::FireNow("timeout".into()));
    }

    #[test]
    fn far_future_sleep_rearms_in_small_steps() {
        let mut s = sched();
        s.wc.infinite = false;
        s.wc.sleep_until = Some("2026-11-21T04:00:00.000Z".into());
        let now = parse_iso_ms("2026-10-21T04:00:00.000Z").unwrap();
        let out = s.setup_sleep_timer(now);
        assert_eq!(out, Outcome::Armed);
        assert_eq!(s.sleep_at, Some(now + 100_009), "远未来只排 100 秒");
    }

    #[test]
    fn wakeable_requires_at_least_one_trigger() {
        let mut wc = WakeConfig::default();
        wc.at_mention = false;
        wc.name_mention = false;
        wc.poke = false;
        wc.question_boolean = false;
        wc.probability = 0.0;
        wc.infinite = true;
        assert!(!is_wakeable(&wc, 0), "全关 = 会永眠");
        wc.keywords = vec!["大肥鱼".into()];
        assert!(is_wakeable(&wc, 0), "有关键词就能唤醒");
    }

    #[test]
    fn wakeable_accepts_live_sleep_until() {
        let mut wc = WakeConfig::default();
        wc.at_mention = false;
        wc.name_mention = false;
        wc.poke = false;
        wc.question_boolean = false;
        wc.probability = 0.0;
        wc.infinite = false;
        wc.sleep_until = Some("2026-10-21T05:00:00.000Z".into());
        let now = parse_iso_ms("2026-10-21T04:00:00.000Z").unwrap();
        assert!(is_wakeable(&wc, now));
    }

    #[test]
    fn replay_coalesces_message_reasons() {
        let mut s = sched();
        // 只把会话置为忙（回合在跑），两次唤醒才会进暂存队列。
        // 若顺手把 pending_wake_at 也设上，schedule_wake 会走「升级」分支而不是「暂存」——
        // 那样队列一直是空的，replay 只会返回 Empty。
        s.last_unread_seq = 10;
        s.turn_started_at = Some(1);
        s.schedule_wake("atMention", 1000);
        s.last_unread_seq = 11;
        s.schedule_wake("keyword:小鲸鱼", 1001);
        s.turn_started_at = None;

        let out = s.replay_pending(2000, &[10, 11], &[]);
        match out {
            ReplayOutcome::Coalesced { picked, merged } => {
                assert_eq!(picked, "keyword:小鲸鱼");
                // Both triggering messages remain unread, so they coalesce into one wake.
                assert_eq!(merged, 2);
            }
            other => panic!("期望 Coalesced，实际 {other:?}"),
        }
        assert!(s.pending.is_empty(), "队列必须排空");
    }

    #[test]
    fn hf129_newer_unread_does_not_revive_an_older_ordinary_wake() {
        let mut s = sched();
        s.turn_started_at = Some(1);
        s.last_unread_seq = 10;
        s.schedule_wake("nameMention", 1000);
        s.turn_started_at = None;
        assert_eq!(s.replay_pending(2000, &[11], &[]), ReplayOutcome::AllHandled { count: 1 });
        assert!(s.pending.is_empty());
        assert!(s.pending_wake_at.is_none());
    }

    #[test]
    fn replay_skips_when_all_handled() {
        let mut s = sched();
        s.turn_started_at = Some(1);
        s.last_unread_seq = 5;
        s.schedule_wake("nameMention", 1000);
        s.turn_started_at = None;

        let out = s.replay_pending(2000, &[], &[]);
        assert_eq!(out, ReplayOutcome::AllHandled { count: 1 });
    }

    #[test]
    fn replay_rearms_timeout_instead_of_dropping() {
        let mut s = sched();
        s.turn_started_at = Some(1);
        s.pending.push_back(PendingWake {
            reason: "timeout".into(),
            seq: 0,
        });
        s.turn_started_at = None;
        s.wc.infinite = false;
        s.wc.sleep_until = Some("2026-10-21T05:00:00.000Z".into());
        let now = parse_iso_ms("2026-10-21T04:00:00.000Z").unwrap();

        let out = s.replay_pending(now, &[], &[]);
        assert_eq!(out, ReplayOutcome::Timers { count: 1 });
        assert!(s.sleep_at.is_some(), "timeout 必须重新挂上，不能静默丢失");
    }

    #[test]
    fn fire_due_returns_reason_when_window_elapsed() {
        let mut s = sched();
        s.schedule_wake("atMention", 1000);
        assert!(s.fire_due(2000).is_empty(), "还没到点");
        assert_eq!(s.fire_due(2500), vec!["atMention".to_string()]);
        assert_eq!(s.pending_wake_at, None);
    }

    #[test]
    fn next_deadline_is_min_of_all_timers() {
        let mut s = sched();
        s.pending_wake_at = Some(500);
        s.sleep_at = Some(300);
        s.reply_check_at = Some(900);
        assert_eq!(s.next_deadline(), Some(300));
    }

    #[test]
    fn parse_iso_handles_z_suffix() {
        let a = parse_iso_ms("2026-10-21T04:00:00.000Z").unwrap();
        let b = parse_iso_ms("2026-10-21T05:00:00.000Z").unwrap();
        assert_eq!(b - a, 3_600_000);
    }

    #[test]
    fn proactive_needs_idle_and_probability() {
        let mut s = sched();
        s.last_incoming_at = 0;
        s.cfg.proactive_idle_ms = 15 * 60 * 1000;
        assert!(!s.proactive_should_fire(1000, 0.0, 0, 0), "还没安静够");
        // 安静够了，rand 0 < prob
        let now = 20 * 60 * 1000;
        assert!(s.proactive_should_fire(now, 0.0, 0, 0));
        assert!(!s.proactive_should_fire(now, 0.99, 0, 0), "rand 超过概率不唤醒");
    }
}
