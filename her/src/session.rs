//! P6：每会话 actor 的驱动器。
//!
//! `wake.rs` 只管「什么时候该醒」，这里负责「把事件喂进去、把该醒的时刻吐出来」。
//! 关键设计：**时间由调用方推进**，不用真 sleep —— 所以既能在线跑（tokio 定时器），
//! 也能离线重放（按 JSONL 的时间戳推进），两条路走同一份逻辑。
//!
//! 在线模式下，`SessionActor` 在 tokio 里跑：
//! ```text
//! loop {
//!     let next = sched.next_deadline();
//!     tokio::select! {
//!         _ = sleep_until(next) => { for ev in driver.pump(now) { emit(ev) } }
//!         input = inbox.recv()   => { for ev in driver.apply(input) { emit(ev) } }
//!     }
//! }
//! ```

#![allow(dead_code)]

use crate::wake::{self, ReplayOutcome, WakeConfig, WakeScheduler, SelfMessage};

#[cfg(test)]
mod hf119_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hf119_message_wake_debounces_three_seconds_and_caps_at_thirty() {
        let cfg = crate::wake_cfg_of(&json!({"socialV2":{"proactive":{"enabled":false},
            "wake":{"maxWakePerMinute":0,"maxWakePerHour":0}}}));
        let mut d = Driver::new(WakeScheduler::new(cfg, WakeConfig::default()), 119);
        assert!(d.apply(Input::Message {reason:"keyword:hi".into(),seq:1,at_ms:1000}).is_empty());
        assert_eq!(d.sched.pending_wake_at, Some(4000));
        for n in 1..15 {
            assert!(d.apply(Input::Message {reason:"keyword:hi".into(),seq:n+1,at_ms:1000+n*2000}).is_empty());
        }
        assert!(d.pump(30999).is_empty());
        assert_eq!(d.pump(31000), vec![WakeEvent {reason:"keyword:hi".into(),at_ms:31000}]);
    }

    #[test]
    fn hf119_nontrigger_messages_reset_wake_but_media_does_not_and_urgent_wakes_now() {
        for urgent in [false,true] {
            let cfg=crate::wake_cfg_of(&json!({"socialV2":{"proactive":{"enabled":false},"wake":{"maxWakePerMinute":0,"maxWakePerHour":0}}}));
            let mut d=Driver::new(WakeScheduler::new(cfg,WakeConfig::default()),119);
            d.apply(Input::Incoming {reason:Some("keyword:hi".into()),seq:1,at_ms:1000,neutral:false,urgent:false});
            d.apply(Input::Incoming {reason:None,seq:2,at_ms:2000,neutral:false,urgent:false});
            assert_eq!(d.sched.pending_wake_at,Some(5000));
            d.apply(Input::Incoming {reason:None,seq:3,at_ms:3000,neutral:true,urgent:false});
            assert_eq!(d.sched.pending_wake_at,Some(5000));
            let events=d.apply(Input::Incoming {reason:Some(if urgent {"speaker:owner"} else {"atMention"}.into()),seq:4,at_ms:3500,neutral:false,urgent:true});
            assert_eq!(events.len(),1);
            assert_eq!(events[0].at_ms,3500);
            assert_eq!(d.unread_len(),4,"neutral media must stay in the batch");
        }
    }
}

/// 一次唤醒（输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeEvent {
    pub reason: String,
    pub at_ms: i64,
}

/// 喂给驱动器的事件（输入）。
/// 注意：WakeConfig 里有 f64，所以只能 PartialEq，不能 Eq。
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// 收到一条会产生唤醒的消息（reason 已由触发器判定）。
    Message { reason: String, seq: i64, at_ms: i64 },
    Incoming { reason: Option<String>, seq: i64, at_ms: i64, neutral: bool, urgent: bool },
    /// 回合开始（此后判忙，直到 TurnEnd 或租约过期）。
    TurnStart { at_ms: i64 },
    /// 回合结束：解除忙碌 + 补发积压。
    TurnEnd { at_ms: i64 },
    /// 自己发了一条消息（触发回复检查 + 更新 last_ai_reply_at）。
    ReplySent { at_ms: i64, text: String },
    /// 唤醒配置变了（例如工具调用改了 sleepUntil）。
    ConfigChanged { at_ms: i64, wc: WakeConfig },
    /// 未读水位变化（mark_read 之后）。
    UnreadChanged { at_ms: i64, seqs: Vec<i64> },
    Paused { at_ms: i64, paused: bool },
    /// 纯粹的时钟推进（离线重放时用来让定时器到点）。
    Tick { at_ms: i64 },
}

/// 驱动器：包一个 WakeScheduler，提供「推进时间 + 应用事件」的统一入口。
pub struct Driver {
    pub sched: WakeScheduler,
    /// 最近自己发的消息（回复检查用它判断要不要拉长延迟）。
    recent_self: Vec<SelfMessage>,
    /// 仍未被读掉的消息序号。
    unread_seqs: Vec<i64>,
    /// 可复现的伪随机源（离线重放必须确定，所以不用线程随机数）。
    rng_state: u64,
    /// 待说未说的条数（JS 的 pendingThoughts）；主动发言概率要用它。
    pending_thoughts: usize,
    /// 上一次决策是否触发了「打断回合」。主循环读到它就先取消 DSH 回合。
    pub preempted: Option<String>,
    /// 最近一次被唤醒限频压住的记录 (时刻, 重试间隔)，用于日志/对拍取证。
    pub last_rate_limited: Option<(i64, i64)>,
}

impl Driver {
    /// Refresh actual ingress/read/send outcomes, never infer a send from model text.
    pub fn refresh_observed_state(&mut self, conv: &serde_json::Value, now: i64) {
        let previous_reply = self.sched.last_ai_reply_at;
        self.unread_seqs = conv["unread"].as_array().map(|a| a.iter().filter_map(|m| m["seq"].as_i64()).collect()).unwrap_or_default();
        self.sched.last_unread_seq = conv["lastUnreadSeq"].as_i64().unwrap_or(0);
        self.sched.wake_times=conv["wakeTimes"].as_array().map(|a|a.iter().filter_map(|v|v.as_i64()).collect()).unwrap_or_default();
        self.sched.last_incoming_at = conv["lastIncomingAt"].as_i64().unwrap_or(0);
        self.sched.last_ai_reply_at = conv["lastAiReplyAt"].as_i64().unwrap_or(0);
        self.sched.engaged_at = conv["engagedAt"].as_i64().unwrap_or(0);
        self.recent_self = conv["recentMessages"].as_array().map(|a| a.iter().filter(|m| m["isSelf"] == true).map(|m| SelfMessage {
            at_ms:m["time"].as_i64().unwrap_or(0), text:m["text"].as_str().unwrap_or("").to_string()
        }).filter(|m| now - m.at_ms < 3_600_000).collect()).unwrap_or_default();
        self.sched.turn_has_sent = self.sched.turn_started_at.is_some_and(|start| self.sched.last_ai_reply_at >= start);
        self.pending_thoughts = conv["pendingThoughts"].as_array().map(|a| a.iter().filter(|t| t["expiresAt"].as_i64().is_none_or(|at| at == 0 || at > now)).count()).unwrap_or(0);
        if self.sched.last_ai_reply_at > previous_reply {
            self.sched.schedule_reply_check(now, &self.recent_self);
        }
    }
    pub fn new(sched: WakeScheduler, seed: u64) -> Self {
        Self {
            sched,
            recent_self: Vec::new(),
            unread_seqs: Vec::new(),
            rng_state: seed | 1,
            pending_thoughts: 0,
            preempted: None,
            last_rate_limited: None,
        }
    }

    /// xorshift64*：确定性、够用，离线重放两次跑出来一样。
    fn next_rand01(&mut self) -> f64 {
        let mut x = self.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Adopt a wake configuration that a tool or the console wrote to the state file after this
    /// driver was built (JS `qq_set_wake_config` rebuilds the timers in the same request): pending
    /// wake and reply-check timers are dropped, the sleep timer is set up again from the new config.
    pub fn adopt_wake_config(&mut self, wc: WakeConfig, at_ms: i64) {
        self.sched.wc = wc;
        self.sched.pending_wake_at = None;
        self.sched.pending_wake_reason = None;
        self.sched.message_wake_first_at = None;
        self.sched.message_wake_urgent = false;
        self.sched.reply_check_at = None;
        self.arm_sleep_timer(at_ms);
    }

    fn arm_sleep_timer(&mut self, at_ms: i64) {
        if self.sched.wc.infinite || self.sched.wc.sleep_until.is_none() { return; }
        if let wake::Outcome::FireNow(_) = self.sched.setup_sleep_timer(at_ms) {
            // Let the normal pump emit the immediate wake exactly once.
            self.sched.sleep_at = Some(at_ms);
        }
    }

    /// 记录模型已经看过哪些消息（对应 JS 的 noteModelMessagesV2）。
    ///
    /// 为什么必须有：唤醒时把消息展示给模型看，但如果不记水位，
    /// 之后 mark_read 传的 throughSeq 会被桥接判为「没看过」→ 400，
    /// 未读数一直涨，模型反复被同一个 backlog 唤醒。
    pub fn note_seen(&mut self, seqs: &[i64]) {
        for s in seqs {
            if !self.unread_seqs.contains(s) {
                self.unread_seqs.push(*s);
            }
        }
        // 水位单调推进，不回退
        if let Some(max) = self.unread_seqs.iter().max() {
            self.sched.last_unread_seq = self.sched.last_unread_seq.max(*max);
        }
    }

    /// 未读条数（测试与诊断用）。
    pub fn unread_len(&self) -> usize {
        self.unread_seqs.len()
    }

    /// 已读水位（mark_read 的 throughSeq）。
    pub fn read_watermark(&self) -> i64 {
        self.sched.last_unread_seq
    }

    /// 挂上「主动机会」定时器。离线重放必须在起点调一次，
    /// 否则 proactiveCheck 永远不会到点（JS 是在会话初始化时就 schedule 的）。
    /// 待说未说的条数（由调用方从会话状态注入；主动发言概率要用它）。
    pub fn set_pending_thoughts(&mut self, n: usize) {
        self.pending_thoughts = n;
    }

    /// 立刻把主动检查排到当前时刻。
    pub fn arm_proactive_now(&mut self, now: i64) {
        if self.sched.proactive_at.is_some() {
            return;
        }
        let r = self.next_rand01();
        self.sched.schedule_proactive(now, r);
    }

    /// Ticket 33: the owner cancelled the reach-out chance at `at` on the Schedule page. That chance is skipped and
    /// the next one is armed from it, as if it had passed without firing (never a fresh, possibly sooner, roll from now).
    pub fn skip_proactive(&mut self, at: i64) {
        if self.sched.proactive_at != Some(at) {
            return;
        }
        self.sched.proactive_at = None;
        let roll = self.next_rand01();
        let _ = self.sched.schedule_proactive(at, roll);
    }
    /// 把所有已到期的定时器跑一遍，返回该发的唤醒。
    /// 反复调用直到没有更早的截止时刻，保证同一个时间点上的多个定时器都处理。
    pub fn pump(&mut self, now: i64) -> Vec<WakeEvent> {
        let mut out = Vec::new();
        loop {
            // 先处理主动机会（它要掷骰子决定要不要醒，并且一定会续期）。
            // 注意：先把 deadline 拷出来，避免「借用 sched 的同时又可变借用 self」。
            let proactive_at = self.sched.proactive_at;
            if let Some(at) = proactive_at {
                if at <= now {
                    self.sched.proactive_at = None;
                    // 待说未说的条数会抬高主动概率（JS: prob *= 1.4）；最近一小时自己说太多则压低。
                    // 这两个值以前一个写死 0、一个根本没传 —— 概率就永远比 JS 高。
                    let thoughts = self.pending_thoughts;
                    let self_msgs_last_hour = self
                        .recent_self
                        .iter()
                        .filter(|m| now.saturating_sub(m.at_ms) < 3_600_000)
                        .count();
                    let roll_fire = self.next_rand01();
                    if self.sched.proactive_should_fire(at, roll_fire, thoughts, self_msgs_last_hour)
                        && self.sched.admit_wake("proactiveCheck".into(), at).is_some() {
                        self.sched.pending_wake_key_at = Some(at);
                        out.push(WakeEvent {
                            reason: "proactiveCheck".into(),
                            at_ms: at,
                        });
                    }
                    let roll_next = self.next_rand01();
                    let _ = self.sched.schedule_proactive(at, roll_next);
                    continue;
                }
            }

            // 积压补发：不忙了就把暂存的唤醒放出来。
            //
            // 为什么放在 pump 里而不是只在 TurnEnd 做：**影子模式没有回合**，
            // 永远等不到 TurnEnd。不补发的话，一阵密集消息之后被暂存的那几条
            // 会一直躺着，直到 60 秒租约过期后又被下一次唤醒覆盖掉 ——
            // 实测 11:35:13 的 poke 就是这样丢的（前一次唤醒把它判成了「忙」）。
            if !self.sched.pending.is_empty() && !self.sched.is_busy(now) {
                let recent = self.recent_self.clone();
                let unread = self.unread_seqs.clone();
                if let ReplayOutcome::Coalesced { .. } | ReplayOutcome::Timers { .. } =
                    self.sched.replay_pending(now, &unread, &recent)
                {
                    continue;
                }
            }

            // 回合硬上限：跑太久就取消，把控制权交回调度器。
            if let Some(cap) = self.sched.turn_cap_at() {
                if cap <= now {
                    self.sched.turn_started_at = None;
                    self.sched.turn_has_sent = false;
                    self.sched.pending_wake_key_at = None;
                    out.push(WakeEvent {
                        reason: "turnTimeout".into(),
                        at_ms: cap,
                    });
                    continue;
                }
            }

            let before = self.sched.next_deadline();
            if before.map(|d| d > now).unwrap_or(true) {
                break;
            }
            // fire_due 处理 pending/sleep/replyCheck 三类；主动机会上面已经吃掉，
            // 顺手把它清掉避免死循环。
            for reason in self.sched.fire_due(now) {
                let at = before.unwrap_or(now);
                // 唤醒一吐出就进入「已派发、回合未起」状态，
                // 对应 JS 的 pendingWakeKeys —— 否则紧接着来的消息会再唤醒一次。
                self.sched.pending_wake_key_at = Some(at);
                out.push(WakeEvent { reason, at_ms: at });
            }
            if self.sched.next_deadline().map(|d| d > now).unwrap_or(true) {
                break;
            }
        }
        out
    }

    /// 应用一个事件，返回期间产生的唤醒。
    pub fn apply(&mut self, input: Input) -> Vec<WakeEvent> {
        let mut out = Vec::new();
        match input {
            Input::Tick { at_ms } => {
                out.extend(self.pump(at_ms));
            }
            Input::Message { reason, seq, at_ms } => {
                let urgent = reason == "atMention";
                return self.apply(Input::Incoming {reason:Some(reason),seq,at_ms,neutral:false,urgent});
            }
            Input::Incoming { reason, seq, at_ms, neutral, urgent } => {
                out.extend(self.pump(at_ms));
                self.sched.last_incoming_at = at_ms;
                self.sched.last_unread_seq = seq;
                if !self.unread_seqs.contains(&seq) {
                    self.unread_seqs.push(seq);
                }
                match self.sched.incoming(reason.as_deref(), at_ms, neutral, urgent) {
                    wake::Outcome::Preempt { reason: r } => {
                        // 打断：回合立刻作废，用新原因马上开一轮。
                        self.sched.turn_started_at = None;
                        self.sched.turn_has_sent = false;
                        self.sched.pending_wake_key_at = Some(at_ms);
                        self.preempted = Some(r.clone());
                        out.push(WakeEvent { reason: r, at_ms });
                    }
                    wake::Outcome::RateLimited { wait_ms } => {
                        // 只留痕，不抛事件：重试定时器已经排好，到点由 pump 取。
                        self.last_rate_limited = Some((at_ms, wait_ms));
                    }
                    _ => {}
                }
                out.extend(self.pump(at_ms));
            }
            Input::TurnStart { at_ms } => {
                out.extend(self.pump(at_ms));
                self.sched.turn_started_at = Some(at_ms);
                self.sched.turn_has_sent = false;
                // 回合起来了，「已派发但未开始」这个临时状态就该让位给回合租约。
                self.sched.pending_wake_key_at = None;
            }
            Input::TurnEnd { at_ms } => {
                out.extend(self.pump(at_ms));
                self.sched.turn_started_at = None;
                self.sched.turn_has_sent = false;
                self.sched.pending_wake_key_at = None;
                // 回合结束必须进入 schedule 态：先补发积压，再把回复检查/睡眠定时器挂回来。
                match self.sched.replay_pending(at_ms, &self.unread_seqs.clone(), &self.recent_self.clone()) {
                    ReplayOutcome::Coalesced { .. } | ReplayOutcome::Timers { .. } => {}
                    ReplayOutcome::Empty | ReplayOutcome::AllHandled { .. } => {
                        // Actual sends arm reply checks; completion only rearms a finite sleep.
                        self.arm_sleep_timer(at_ms);
                    }
                }
                out.extend(self.pump(at_ms));
            }
            Input::ReplySent { at_ms, text } => {
                self.sched.last_ai_reply_at = at_ms;
                // 说出口了就不再允许被打断（改口比晚回更难解释）。
                self.sched.turn_has_sent = true;
                self.recent_self.push(SelfMessage {
                    at_ms,
                    text,
                });
                // 只留最近 5 分钟，避免无限增长
                self.recent_self.retain(|m| at_ms - m.at_ms < 300_000);
                // 发完消息就安排回复检查（JS 里是发送成功后调 scheduleReplyCheckV2）
                let recent = self.recent_self.clone();
                self.sched.schedule_reply_check(at_ms, &recent);
            }
            Input::ConfigChanged { at_ms, wc } => {
                out.extend(self.pump(at_ms));
                self.adopt_wake_config(wc, at_ms);
                out.extend(self.pump(at_ms));
            }
            Input::UnreadChanged { at_ms, seqs } => {
                out.extend(self.pump(at_ms));
                self.unread_seqs = seqs;
                if self.unread_seqs.is_empty() && self.sched.pending_wake_at.is_none() {
                    self.sched.message_wake_first_at = None;
                    self.sched.message_wake_urgent = false;
                }
            }
            Input::Paused { at_ms, paused } => {
                out.extend(self.pump(at_ms));
                self.sched.paused = paused;
                if paused {
                    // 暂停时清掉所有定时器（JS 的 clearSocialV2Timers）
                    self.sched.pending_wake_at = None;
                    self.sched.pending_wake_reason = None;
                    self.sched.message_wake_first_at = None;
                    self.sched.message_wake_urgent = false;
                    self.sched.sleep_at = None;
                    self.sched.reply_check_at = None;
                    self.sched.proactive_at = None;
                    self.sched.pending.clear();
                } else {
                    self.arm_sleep_timer(at_ms);
                }
            }
        }
        out
    }
}

/// 推进所有会话的定时器，返回「该醒的」列表（主循环的 tick 分支就调它）。
///
/// 抽成独立函数的原因：主循环里的 `tokio::select!` 没法单测，
/// 但「到点该不该醒」这个纯逻辑可以——不抽出来就只能靠等真实流量。
pub fn pump_all(drivers: &mut std::collections::HashMap<String, Driver>, now: i64) -> Vec<(String, WakeEvent)> {
    let mut due = Vec::new();
    for (k, d) in drivers.iter_mut() {
        for e in d.pump(now) {
            due.push((k.clone(), e));
        }
    }
    due
}
/// 从 social-v2.json 的一条会话记录构造调度器。
pub fn scheduler_from_state(
    cfg: wake::WakeCfg,
    session: &serde_json::Value,
) -> WakeScheduler {
    let wc = wake_config_from_json(session.get("wakeConfig"));
    let mut s = WakeScheduler::new(cfg, wc);
    s.last_ai_reply_at = session
        .get("lastAiReplyAt")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    s.last_incoming_at = session
        .get("lastIncomingAt")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    s.last_unread_seq = session
        .get("lastUnreadSeq")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    s.wake_times = session["wakeTimes"].as_array().map(|a| a.iter().filter_map(|v|v.as_i64()).collect()).unwrap_or_default();
    s
}

/// 对应 JS 的 `ensureWakeableV2(st)`：**加载时**兜一次「配置里一个触发条件都没有」，
/// 重置成默认配置，避免永眠。
///
/// JS 在四处调用它：加载状态、主动检查定时器、回合收尾（设过唤醒配置/读过消息时）、
/// mark_read 端点。Rust 原来只有 mark_read 那一处 —— 也就是说「加载时兜底」这条没有。
/// 现在的影子是从 JS 那边加载的、配置早就被 JS 兜过了，所以线上看不出差别；
/// 但等 Rust 独立跑，这条不能少。
pub fn ensure_wakeable_at_load(sched: &mut WakeScheduler, cfg_json: &serde_json::Value, now: i64, rand01: f64) -> bool {
    let wc = &sched.wc;
    let timed = !wc.infinite
        && wc
            .sleep_until
            .as_deref()
            .and_then(wake::parse_iso_ms)
            .map(|t| t > now)
            .unwrap_or(false);
    let wakeable = timed
        || wc.mode_active
        || wc.any_message
        || wc.at_mention
        || wc.name_mention
        || wc.poke
        || wc.question_boolean
        || wc.probability > 0.0
        || !wc.keywords.is_empty()
        || !wc.speaker_ids.is_empty();
    if wakeable {
        return false;
    }
    // 一个触发条件都没有：用默认配置兜底（与 JS ensureWakeableV2 一致）
    let def = wake::default_wake_config(cfg_json, now, rand01);
    sched.wc = wake_config_from_json(Some(&def));
    true
}
/// Marker of a conversation's persisted wake settings that the live hub compares with the state
/// file: what a running driver consumes (`WakeConfig`) plus `confirmedAt`, and the last non-null
/// `sleepUntil`. A refreshed default (JS `refreshDefaultWakeConfigV2`) changes the settings and
/// keeps `confirmedAt`, so the timestamp alone cannot be the signal; counters the bridge bumps
/// itself (`wakeCount`, `lastWakeAt`, `noActionCount`) are not part of it. The bridge also writes
/// `sleepUntil` itself: a claimed finite wake consumes it (null) and a refused dispatch restores
/// the previous value. Neither is a settings change; the driver already cleared its own deadline
/// at dispatch and must not re-adopt (and so re-fire) a restored deadline ahead of the retry backoff.
#[derive(Clone, Debug, PartialEq)]
pub struct WakeSync {
    settings: u64,
    deadline: Option<String>,
}

impl WakeSync {
    pub fn of(wake_config: &serde_json::Value) -> Self {
        use std::hash::{Hash, Hasher};
        let mut config = wake_config_from_json(Some(wake_config));
        let deadline = config.sleep_until.take();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format!("{config:?}").hash(&mut hasher);
        wake_config.get("confirmedAt").and_then(|v| v.as_i64()).unwrap_or(0).hash(&mut hasher);
        WakeSync { settings: hasher.finish(), deadline }
    }

    /// `Some(next)` when the running driver must adopt `wake_config`, which also holds when
    /// nothing was seen yet; `next` replaces the marker.
    pub fn changed(seen: Option<&WakeSync>, wake_config: &serde_json::Value) -> Option<WakeSync> {
        let next = WakeSync::of(wake_config);
        let Some(seen) = seen else { return Some(next) };
        let moved = next.settings != seen.settings
            || next.deadline.as_ref().is_some_and(|deadline| seen.deadline.as_ref() != Some(deadline));
        moved.then_some(next)
    }
}

pub fn wake_config_from_json(v: Option<&serde_json::Value>) -> WakeConfig {
    let Some(v) = v else {
        return WakeConfig::default();
    };
    let tr = v.get("triggers").cloned().unwrap_or_else(|| serde_json::json!({}));
    let b = |name: &str| tr.get(name).and_then(|x| x.as_bool()).unwrap_or(false);
    WakeConfig {
        mode_active: v.get("mode").and_then(|m| m.as_str()) == Some("active"),
        infinite: v.get("infinite").and_then(|x| x.as_bool()).unwrap_or(true),
        sleep_until: v
            .get("sleepUntil")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        at_mention: b("atMention"),
        name_mention: b("nameMention"),
        question_boolean: b("question"),
        poke: b("poke"),
        any_message: b("anyMessage"),
        probability: tr.get("probability").and_then(|x| x.as_f64()).unwrap_or(0.0),
        keywords: tr
            .get("keywords")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        speaker_ids: tr
            .get("speakerIds")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|k| k.as_i64()).collect())
            .unwrap_or_default(),
        batch_window_ms: v.get("batchWindowMs").and_then(|x| x.as_i64()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::WakeCfg;

    #[test]
    fn wake_sync_ignores_the_bridges_own_deadline_consume_and_restore_but_not_a_new_deadline() {
        let wc = |sleep: serde_json::Value, count: i64| serde_json::json!({"mode":"diving","infinite":false,"sleepUntil":sleep,"triggers":{"atMention":true},"confirmedAt":5,"wakeCount":count});
        let d1 = serde_json::json!("2020-01-01T00:00:00.000Z");
        let seen = WakeSync::of(&wc(d1.clone(), 0));
        assert!(WakeSync::changed(Some(&seen), &wc(d1.clone(), 3)).is_none(), "counters are not settings");
        assert!(WakeSync::changed(Some(&seen), &wc(serde_json::Value::Null, 3)).is_none(), "claim consumed the deadline");
        assert!(WakeSync::changed(Some(&seen), &wc(d1.clone(), 0)).is_none(), "refused dispatch restored the deadline");
        let refreshed = WakeSync::changed(Some(&seen), &wc(serde_json::json!("2030-01-01T00:00:00.000Z"), 0));
        assert!(refreshed.is_some(), "a refreshed default carries a new deadline");
        assert!(WakeSync::changed(Some(&seen), &serde_json::json!({"mode":"active","infinite":false,"sleepUntil":d1,"triggers":{"atMention":true},"confirmedAt":5})).is_some());
        assert!(WakeSync::changed(None, &wc(serde_json::Value::Null, 0)).is_some(), "nothing seen yet");
    }

    #[test]
    fn proactive_deadline_survives_ordinary_batch_dispatch() {
        let mut d=drv();
        d.arm_proactive_now(100_000);
        let deadline=d.sched.proactive_at;
        d.apply(Input::Message{reason:"atMention".into(),seq:1,at_ms:100_000});
        assert_eq!(reasons(&d.pump(101_500)),vec!["atMention"]);
        assert_eq!(d.sched.proactive_at,deadline,"unrelated batch consumed the proactive timer");
    }

    fn base_of(reason: &str) -> &str {
        reason.split(':').next().unwrap_or(reason)
    }

    fn drv() -> Driver {
        let mut cfg = WakeCfg::default();
        cfg.batch_window_ms = 1500;
        // 限频关掉：这些用例测的是「排队/合并/补发」，不是频率上限。
        cfg.max_wake_per_minute = 0;
        cfg.max_wake_per_hour = 0;
        Driver::new(WakeScheduler::new(cfg, WakeConfig::default()), 42)
    }

    fn reasons(evs: &[WakeEvent]) -> Vec<&str> {
        evs.iter().map(|e| e.reason.as_str()).collect()
    }

    #[test]
    fn adopted_elapsed_sleep_deadline_emits_one_timeout() {
        let mut d = drv();
        let now = wake::parse_iso_ms("2026-10-26T06:00:00.000Z").unwrap();
        let mut wc = WakeConfig::default();
        wc.infinite = false;
        wc.sleep_until = Some("2026-10-26T05:59:59.000Z".into());
        d.adopt_wake_config(wc, now);
        assert_eq!(d.pump(now), vec![WakeEvent { reason: "timeout".into(), at_ms: now }]);
        assert!(d.pump(now + 500).is_empty(), "synchronization must not duplicate the wake");
    }

    #[test]
    fn config_changed_elapsed_sleep_deadline_emits_timeout() {
        let mut d = drv();
        let now = wake::parse_iso_ms("2026-10-26T06:00:00.000Z").unwrap();
        let mut wc = WakeConfig::default();
        wc.infinite = false;
        wc.sleep_until = Some("2026-10-26T05:59:59.000Z".into());
        assert_eq!(d.apply(Input::ConfigChanged { at_ms: now, wc }), vec![WakeEvent { reason: "timeout".into(), at_ms: now }]);
    }

    #[test]
    fn resumed_elapsed_sleep_deadline_emits_timeout() {
        let mut d = drv();
        let now = wake::parse_iso_ms("2026-10-26T06:00:00.000Z").unwrap();
        d.sched.paused = true;
        d.sched.wc.infinite = false;
        d.sched.wc.sleep_until = Some("2026-10-26T05:59:59.000Z".into());
        let mut events = d.apply(Input::Paused { at_ms: now, paused: false });
        events.extend(d.pump(now));
        assert_eq!(reasons(&events), vec!["timeout"]);
    }

    #[test]
    fn turn_end_elapsed_sleep_deadline_emits_timeout() {
        let mut d = drv();
        let now = wake::parse_iso_ms("2026-10-26T06:00:00.000Z").unwrap();
        d.sched.wc.infinite = false;
        d.sched.wc.sleep_until = Some("2026-10-26T05:59:59.000Z".into());
        assert_eq!(reasons(&d.apply(Input::TurnEnd { at_ms: now })), vec!["timeout"]);
    }

    #[test]
    fn message_arms_batch_then_fires() {
        let mut d = drv();
        // t=0 收到 @，合批窗口 1.5 秒
        assert!(d.apply(Input::Message { reason: "atMention".into(), seq: 1, at_ms: 0 }).is_empty());
        // t=1000 还没到点
        assert!(d.apply(Input::Tick { at_ms: 1000 }).is_empty());
        // t=1600 到点
        let evs = d.apply(Input::Tick { at_ms: 1600 });
        assert_eq!(reasons(&evs), vec!["atMention"]);
        assert_eq!(evs[0].at_ms, 1500, "唤醒时刻应是截止时刻，不是 tick 时刻");
    }

    #[test]
    fn burst_within_window_coalesces_into_one_wake() {
        let mut d = drv();
        d.apply(Input::Message { reason: "keyword:小鲸鱼".into(), seq: 1, at_ms: 0 });
        d.apply(Input::Message { reason: "atMention".into(), seq: 2, at_ms: 300 });
        d.apply(Input::Message { reason: "question".into(), seq: 3, at_ms: 600 });
        let evs = d.apply(Input::Tick { at_ms: 2000 });
        assert_eq!(evs.len(), 1, "三条消息只醒一次");
        assert_eq!(evs[0].reason, "atMention", "取优先级最高的原因");
    }

    #[test]
    fn busy_conversation_buffers_then_replays_on_turn_end() {
        // 用真实量级的时间戳：JS 判忙的条件是 `startedAt > 0`，
        // 用 0 当回合起点会被当成「没有回合」，测试就测不到忙的分支。
        const T0: i64 = 1_790_000_000_000;
        let mut d = drv();
        d.apply(Input::TurnStart { at_ms: T0 });
        d.apply(Input::Message { reason: "atMention".into(), seq: 10, at_ms: T0 + 100 });
        d.apply(Input::Message { reason: "keyword:大肥鱼".into(), seq: 11, at_ms: T0 + 200 });
        // 忙期间不醒
        assert!(d.apply(Input::Tick { at_ms: T0 + 5000 }).is_empty());
        // 回合结束时是「排定」补发（还要再走一次合批窗口），不是立刻发
        let at_end = d.apply(Input::TurnEnd { at_ms: T0 + 6000 });
        assert!(at_end.is_empty(), "回合结束只排定，不立刻唤醒");
        assert_eq!(d.sched.pending.len(), 0, "队列必须排空");
        // 再推进一个合批窗口，应该正好醒一次
        let evs = d.apply(Input::Tick { at_ms: T0 + 8000 });
        assert_eq!(evs.len(), 1, "积压合并成一次，实际 {evs:?}");
    }

    #[test]
    fn reply_check_skipped_when_already_replied() {
        let mut d = drv();
        d.sched.cfg.auto_reply_check_ms = 1000;
        d.apply(Input::Message { reason: "atMention".into(), seq: 1, at_ms: 0 });
        d.apply(Input::Tick { at_ms: 1600 }); // 醒来
        d.apply(Input::TurnStart { at_ms: 1700 });
        d.apply(Input::ReplySent { at_ms: 3000, text: "在的".into() });
        d.apply(Input::TurnEnd { at_ms: 3100 });
        // 回复检查会在 4000 到点；期间没有新消息 -> 不该醒
        let evs = d.apply(Input::Tick { at_ms: 5000 });
        assert!(
            !reasons(&evs).contains(&"replyCheck"),
            "自己刚回过就不该再醒，实际 {evs:?}"
        );
    }

    #[test]
    fn reply_check_fires_when_someone_replied() {
        let mut d = drv();
        d.sched.cfg.auto_reply_check_ms = 1000;
        d.apply(Input::ReplySent { at_ms: 0, text: "在的".into() });
        // 别人回了
        d.apply(Input::Message { reason: "anyMessage".into(), seq: 2, at_ms: 500 });
        let evs = d.apply(Input::Tick { at_ms: 3000 });
        assert!(reasons(&evs).contains(&"replyCheck") || reasons(&evs).contains(&"anyMessage"));
    }

    #[test]
    fn pause_clears_all_timers() {
        let mut d = drv();
        d.apply(Input::Message { reason: "atMention".into(), seq: 1, at_ms: 0 });
        d.apply(Input::Paused { at_ms: 100, paused: true });
        assert_eq!(d.sched.next_deadline(), None);
        assert!(d.apply(Input::Tick { at_ms: 99999 }).is_empty());
    }

    #[test]
    fn finite_sleep_fires_timeout() {
        let mut d = drv();
        let mut wc = WakeConfig::default();
        wc.infinite = false;
        wc.sleep_until = Some("2026-10-21T04:01:00.000Z".into());
        let t0 = wake::parse_iso_ms("2026-10-21T04:00:00.000Z").unwrap();
        d.apply(Input::ConfigChanged { at_ms: t0, wc });
        let evs = d.apply(Input::Tick { at_ms: t0 + 61_000 });
        assert_eq!(reasons(&evs), vec!["timeout"]);
    }

    #[test]
    fn buffered_wake_is_replayed_once_not_busy() {
        // 实盘场景：11:34:42 一次唤醒把会话标成「忙」，
        // 11:35:13 的 poke 被暂存；影子模式没有回合，永远等不到 TurnEnd →
        // 那条 poke 就这样丢了，JS 却醒了（A/B 记成「漏判」）。
        // 修法：pump 里只要不忙了就补发。
        const T0: i64 = 1_790_000_000_000;
        let mut d = drv();
        d.apply(Input::TurnStart { at_ms: T0 });
        d.apply(Input::Message {
            reason: "poke:other".into(),
            seq: 1,
            at_ms: T0 + 1000,
        });
        assert_eq!(d.sched.pending.len(), 1, "忙时先暂存");
        // 回合一直没结束，但租约（30 分钟）过不去；这里手工解除回合模拟租约过期
        d.sched.turn_started_at = None;
        // 不忙了 → 补发；补发走的是 schedule_wake，所以还要再过一个合批窗口才真的醒
        let _ = d.apply(Input::Tick { at_ms: T0 + 2000 });
        assert!(d.sched.pending.is_empty(), "补发后队列必须清空");
        let evs = d.apply(Input::Tick { at_ms: T0 + 4000 });
        assert!(!evs.is_empty(), "补发后过了合批窗口必须真的醒");
        assert_eq!(base_of(&evs[0].reason), "poke");
    }
    #[test]
    fn deterministic_trigger_preempts_a_stalled_turn() {
        // 实盘场景：香草群 10:57 起一个回合跑了 3 分半，期间的 @ 只能排队。
        // 打断是**可选机制**：`preempt_after_ms > 0` 才生效，默认 0（关）——
        // 因为 JS 根本没有打断，而 A/B 要求唤醒序列与 JS 偏差为 0。
        // 这个用例验证"打开时行为正确"，默认关闭由 preempt_is_off_by_default 守。
        const T0: i64 = 1_790_000_000_000;
        let mut d = drv();
        d.sched.cfg.preempt_after_ms = 15_000;
        d.apply(Input::TurnStart { at_ms: T0 });

        // 才跑了 5 秒：不打断，排队
        let early = d.apply(Input::Message {
            reason: "atMention".into(),
            seq: 1,
            at_ms: T0 + 5_000,
        });
        assert!(early.is_empty());
        assert_eq!(d.sched.pending.len(), 1, "太早的话先排队");

        // 跑满 20 秒：允许打断
        let late = d.apply(Input::Message {
            reason: "atMention".into(),
            seq: 2,
            at_ms: T0 + 20_000,
        });
        assert_eq!(late.len(), 1, "应该立刻醒");
        assert_eq!(late[0].reason, "atMention");
        assert_eq!(late[0].at_ms, T0 + 20_000, "不再等合批窗口");
    }

    #[test]
    fn preempt_is_off_by_default() {
        // 默认配置下打断机制整个关掉 —— 新 @ 一律排队，和 JS 一样（JS 没有打断）。
        // 这条是护栏：谁把默认值改回正数，这里立刻变红。
        let d = drv();
        assert_eq!(d.sched.cfg.preempt_after_ms, 0, "打断必须默认关闭");
    }

    #[test]
    fn turn_that_already_spoke_is_never_preempted() {
        // 说出口了就不改口：宁可让它收尾，也不打断。
        const T0: i64 = 1_790_000_000_000;
        let mut d = drv();
        d.apply(Input::TurnStart { at_ms: T0 });
        d.apply(Input::ReplySent { at_ms: T0 + 3_000, text: "刚说了一句".into() });
        let evs = d.apply(Input::Message {
            reason: "atMention".into(),
            seq: 1,
            at_ms: T0 + 30_000,
        });
        assert!(evs.is_empty(), "已发过话的回合不打断");
        assert_eq!(d.sched.pending.len(), 1, "而是排队等它收尾");
    }

    #[test]
    fn low_priority_reason_never_preempts() {
        // 概率/任意消息这类弱触发，不值得打断一个正在跑的回合。
        const T0: i64 = 1_790_000_000_000;
        let mut d = drv();
        d.apply(Input::TurnStart { at_ms: T0 });
        let evs = d.apply(Input::Message {
            reason: "probability".into(),
            seq: 1,
            at_ms: T0 + 30_000,
        });
        assert!(evs.is_empty());
        assert_eq!(d.sched.pending.len(), 1);
    }

    #[test]
    fn turn_hard_cap_fires_turn_timeout() {
        const T0: i64 = 1_790_000_000_000;
        let mut d = drv();
        d.sched.cfg.turn_hard_cap_ms = 120_000;
        d.apply(Input::TurnStart { at_ms: T0 });
        // 到上限前不吐
        assert!(d.apply(Input::Tick { at_ms: T0 + 119_000 }).is_empty());
        // 到点吐 turnTimeout，并解除回合
        let evs = d.apply(Input::Tick { at_ms: T0 + 121_000 });
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].reason, "turnTimeout");
        assert_eq!(d.sched.turn_started_at, None, "硬上限之后回合必须结束");

        let mut d = drv();
        d.sched.cfg.turn_hard_cap_ms = 120_000;
        d.apply(Input::TurnStart { at_ms: T0 });
        d.sched.prompt_running = true;
        let events = d.apply(Input::Message { reason: "atMention".into(), seq: 1, at_ms: T0 + 120_014 });
        assert_eq!(reasons(&events), vec!["turnTimeout"], "message at cap + 14 ms emits one control event");
        assert!(d.pump(T0 + 120_015).is_empty(), "the control event must not fire twice");
        assert_eq!(d.sched.pending.len(), 1, "the triggering message remains buffered");
    }

    #[test]
    fn turn_budget_allows_full_default_observation_and_normal_close() {
        const T0: i64 = 1_790_000_000_000;
        let cfg = serde_json::json!({});
        let conv = serde_json::json!({"lastIncomingAt":T0});
        let mut d = Driver::new(scheduler_from_state(crate::wake_cfg_of(&cfg), &conv), 1);
        d.sched.cfg.proactive_enabled = false;
        d.apply(Input::TurnStart { at_ms: T0 });
        assert_eq!(crate::wake::pre_sleep_remaining(&conv, &cfg, T0), Some(300_000));
        assert!(d.apply(Input::Tick { at_ms: T0 + 300_000 }).is_empty(),
            "default observation must finish before the turn cap");
        assert!(crate::wake::pre_sleep_remaining(&conv, &cfg, T0 + 300_000).is_none());
        assert!(d.apply(Input::TurnEnd { at_ms: T0 + 301_000 }).is_empty());
        assert_eq!(d.sched.cfg.turn_hard_cap_ms, 515_000);
        assert!(d.apply(Input::Tick { at_ms: T0 + 515_001 }).is_empty(), "closed turn must not be cancelled later");
    }
    #[test]
    fn wake_marks_pending_key_so_next_message_buffers() {
        // 实盘抓到的 bug：10:58:06 JS 判忙并暂存 atMention，Rust 却在 07 秒又醒了一次。
        // 原因是缺少「唤醒已派发、回合还没起」这个状态。
        let mut d = drv();
        d.apply(Input::Message { reason: "atMention".into(), seq: 1, at_ms: 0 });
        let evs = d.apply(Input::Tick { at_ms: 2000 });
        assert_eq!(evs.len(), 1, "第一次该醒");

        // 醒来后 1 秒内又来一条：应该暂存，不能再醒
        let more = d.apply(Input::Message {
            reason: "keyword:小鲸鱼".into(),
            seq: 2,
            at_ms: 2500,
        });
        assert!(more.is_empty(), "刚醒来的会话不该立刻再醒");
        assert_eq!(d.sched.pending.len(), 1, "该消息应进暂存队列");
    }

    #[test]
    fn pending_key_expires_after_lease() {
        // 醒了但回合一直没来（DSH 挂了）：60 秒后必须能重新被唤醒，不能锁死。
        let mut d = drv();
        d.sched.pending_wake_key_at = Some(0);
        assert!(d.sched.is_busy(1000));
        assert!(!d.sched.is_busy(61_000));
    }

    #[test]
    fn turn_start_clears_pending_key() {
        let mut d = drv();
        d.sched.pending_wake_key_at = Some(1000);
        d.apply(Input::TurnStart { at_ms: 1200 });
        assert_eq!(d.sched.pending_wake_key_at, None, "回合接管忙碌语义");
        assert!(d.sched.is_busy(1300), "回合租约仍然算忙");
    }
    #[test]
    fn pump_all_fires_only_due_sessions() {
        use std::collections::HashMap;
        let mut m: HashMap<String, Driver> = HashMap::new();
        m.insert("group:1".into(), drv());
        m.insert("group:2".into(), drv());

        // 只有 1 号群收到消息
        m.get_mut("group:1").unwrap().apply(Input::Message {
            reason: "atMention".into(),
            seq: 1,
            at_ms: 0,
        });

        // 还没到合批窗口
        assert!(pump_all(&mut m, 1000).is_empty());

        // 到点后只应该醒 1 号群
        let due = pump_all(&mut m, 2000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "group:1");
        assert_eq!(due[0].1.reason, "atMention");

        // 再推一次不应该重复唤醒
        assert!(pump_all(&mut m, 3000).is_empty());
    }
    #[test]
    fn note_seen_advances_watermark_monotonically() {
        let mut d = drv();
        d.note_seen(&[3, 5, 4]);
        assert_eq!(d.read_watermark(), 5);
        // 再来一批更早的，水位不能回退
        d.note_seen(&[2]);
        assert_eq!(d.read_watermark(), 5);
        // 更新的消息推进水位
        d.note_seen(&[9]);
        assert_eq!(d.read_watermark(), 9);
    }

    #[test]
    fn note_seen_does_not_duplicate() {
        let mut d = drv();
        d.note_seen(&[1, 1, 1]);
        d.note_seen(&[1]);
        assert_eq!(d.unread_len(), 1);
    }
    #[test]
    fn deterministic_rng_is_reproducible() {
        let mut a = drv();
        let mut b = drv();
        let ra: Vec<f64> = (0..5).map(|_| a.next_rand01()).collect();
        let rb: Vec<f64> = (0..5).map(|_| b.next_rand01()).collect();
        assert_eq!(ra, rb, "同 seed 必须同序列");
        assert!(ra.iter().all(|x| (0.0..1.0).contains(x)));
    }

    #[test]
    fn ensure_wakeable_at_load_resets_dead_config() {
        // 一个触发条件都没有的配置（全 false + 概率 0 + 无关键词）必须被兜底重置，
        // 否则这条会话永远不会被唤醒（JS 在加载时就会兜）。
        let cfg = serde_json::json!({"socialV2": {"wake": {"recommendedSleepMinMs": 300000}}});
        let dead = serde_json::json!({
            "infinite": true,
            "triggers": {"atMention": false, "nameMention": false, "question": false,
                         "poke": false, "anyMessage": false, "keywords": [], "probability": 0,
                         "speakerIds": []}
        });
        let session = serde_json::json!({"wakeConfig": dead});
        let mut sched = scheduler_from_state(wake::WakeCfg::default(), &session);
        assert!(ensure_wakeable_at_load(&mut sched, &cfg, 1_000_000, 0.5), "死配置应被重置");
        assert!(sched.wc.at_mention || sched.wc.name_mention || sched.wc.probability > 0.0, "重置后必须可唤醒");

        // 正常配置不该被动
        let alive = serde_json::json!({"wakeConfig": {"infinite": true, "triggers": {"atMention": true}}});
        let mut s2 = scheduler_from_state(wake::WakeCfg::default(), &alive);
        assert!(!ensure_wakeable_at_load(&mut s2, &cfg, 1_000_000, 0.5), "正常配置不该被重置");
    }
    #[test]
    fn wake_config_parses_from_json() {
        let v = serde_json::json!({
            "mode": "diving",
            "infinite": true,
            "batchWindowMs": 1500,
            "triggers": {
                "atMention": true, "nameMention": true, "question": true, "poke": true,
                "anyMessage": false, "probability": 0.05,
                "keywords": ["小鲸鱼", "大肥鱼"],
                "speakerIds": [123]
            }
        });
        let wc = wake_config_from_json(Some(&v));
        assert!(wc.at_mention && wc.name_mention && wc.question_boolean && wc.poke);
        assert_eq!(wc.keywords.len(), 2);
        assert_eq!(wc.speaker_ids, vec![123]);
        assert_eq!(wc.batch_window_ms, Some(1500));
        assert!(!wc.mode_active);
    }
}
