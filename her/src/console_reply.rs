//! Reserved2 send timing. Uses the console's clock/state seam, never a model-side wait.
use super::*;

const MAX_REVISIONS: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct QuietWindow {
    pub median: i64,
    pub min: i64,
    pub max: i64,
}

impl QuietWindow {
    pub(super) fn configured(cfg: &Value, urgent: bool) -> Self {
        if urgent {
            return Self {
                median: 3000,
                min: 1000,
                max: 6000,
            };
        }
        let field = |name: &str, default| {
            cfg.pointer(&format!("/socialV2/reply/{name}"))
                .and_then(Value::as_i64)
                .filter(|n| (1..=120_000).contains(n))
                .unwrap_or(default)
        };
        let window = Self {
            median: field("quietMedianMs", 10000),
            min: field("quietMinMs", 4000),
            max: field("quietMaxMs", 25000),
        };
        if window.min <= window.median && window.median <= window.max {
            window
        } else {
            Self {
                median: 10000,
                min: 4000,
                max: 25000,
            }
        }
    }
}

pub(super) fn urgent(cfg: &Value, key: &str, conv: &Value) -> bool {
    let latest = conv["recentMessages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|m| m["isSelf"] != true && m["quietNeutral"] != true);
    conv["lastReplyUrgent"] == true
        || latest.is_some_and(|m| m["isOwner"] == true || m["atSelf"] == true)
        || key
            == format!(
                "private:{}",
                cfg.get("ownerQQ")
                    .map(crate::aux_text::js_string)
                    .unwrap_or_default()
            )
}

/// xorshift64*, seeded with OS entropy in production and explicitly in seam tests.
pub(super) struct TimingRandom(u64);
impl TimingRandom {
    pub(super) fn seeded(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn uniform(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        // Open interval, so Box-Muller never takes ln(0).
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    pub(super) fn quiet(&mut self, window: QuietWindow) -> i64 {
        let normal =
            (-2.0 * self.uniform().ln()).sqrt() * (std::f64::consts::TAU * self.uniform()).cos();
        (window.median as f64 * (0.5 * normal).exp())
            .round()
            .clamp(window.min as f64, window.max as f64) as i64
    }
    fn typing(&mut self, budget: i64) -> i64 {
        (budget as f64 * (0.75 + self.uniform() * 0.5)).round() as i64
    }
}

#[cfg(test)]
thread_local! { pub(super) static TEST_SEED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) }; }

fn random() -> Result<TimingRandom, (u16, String)> {
    #[cfg(test)]
    if let Some(seed) = TEST_SEED.with(|slot| {
        let seed = slot.get();
        slot.set(seed.map(|s| s.wrapping_add(1)));
        seed
    }) {
        return Ok(TimingRandom::seeded(seed));
    }
    let mut bytes = [0; 8];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| (503, "reply timing randomness is unavailable".into()))?;
    Ok(TimingRandom::seeded(u64::from_ne_bytes(bytes)))
}

/// The observed model start, or the last delivered reply when thinking about a later bubble.
fn thinking_start(context: &RuntimeContext, key: &str, epoch: &str, conv: &Value) -> i64 {
    let started = crate::turn_ledger::read_records(context)
        .ok()
        .and_then(|records| {
            let (turns, _) = crate::turn_ledger::fold(&records);
            turns
                .iter()
                .rev()
                .find(|t| t.key == key && t.state.is_open() && t.epochs.iter().any(|e| e == epoch))
                .and_then(|t| {
                    records
                        .iter()
                        .rev()
                        .find(|r| r["k"] == "started" && r["turn"] == t.id)
                        .and_then(|r| r["t"].as_i64())
                })
        })
        .unwrap_or(0);
    let last_bubble = conv["recentMessages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["isSelf"] == true)
        .filter_map(|m| m["time"].as_i64())
        .max()
        .unwrap_or(0);
    started
        .max(conv["lastAiReplyAt"].as_i64().unwrap_or(0))
        .max(last_bubble)
}

struct Typing<'a> {
    ob: &'a crate::onebot::OneBot,
    user: Option<i64>,
}
impl<'a> Typing<'a> {
    fn new(ob: &'a crate::onebot::OneBot, user: Option<i64>) -> Self {
        let status = Self { ob, user };
        status.set(1);
        status
    }
    fn set(&self, event: u8) {
        if let Some(user) = self.user {
            // A missing/unsupported action is cosmetic, including timeout/refusal.
            self.ob.set_input_status(user, event);
        }
    }
}
impl Drop for Typing<'_> {
    fn drop(&mut self) {
        self.set(0);
    }
}

fn revision(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    epoch: &str,
    after: i64,
) -> Result<Option<Value>, (u16, String)> {
    mutate_social_state_for(context, |state| {
        let conv = &mut state["conversations"][key];
        if !conversation_matches_epoch(conv, epoch) {
            return Err((409, SEND_CANCELLED_MESSAGE.into()));
        }
        let (scope, _) = wait_usage(context, key, conv)?;
        let used = if conv["replyRevisionBudget"]["scope"] == scope {
            conv["replyRevisionBudget"]["used"].as_u64().unwrap_or(0)
        } else {
            0
        };
        if used >= MAX_REVISIONS {
            return Ok(None);
        }
        conv["replyRevisionBudget"] = json!({"scope":scope,"used":used+1});
        let mut fresh = conv.clone();
        fresh["unread"] = json!(conv["unread"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["seq"].as_i64().is_some_and(|s| s > after))
            .cloned()
            .collect::<Vec<_>>());
        fresh["recentMessages"] = json!([]);
        let packet = agent::build_wake_snapshot(context, cfg, key, &fresh)
            .unwrap_or_else(|| json!({"messages":[],"partial":true}));
        let messages = packet["messages"].as_array().cloned().unwrap_or_default();
        let next = messages
            .last()
            .and_then(|m| m["seq"].as_i64())
            .unwrap_or(after);
        Ok(Some(
            json!({"ok":true,"held":true,"reviseRequired":true,"newMessages":messages,
            "partial":packet["partial"],"nextAfterSeq":next,"readThroughSeq":packet["readThroughSeq"],
            "replyRevisionsUsed":used+1,"replyRevisionLimitReached":used+1>=MAX_REVISIONS,
            "reason":"对方又说了… 本次待发内容已取消。先看 newMessages，再修改或重新发送；partial=true 时继续读取未读。最多退回修改三次，之后发送按本次请求的静默截止时间放行。"}),
        ))
    })
}

/// Only the successful response boundary offers a revision to the model. Budget,
/// discard and send settlement must all have succeeded before publishing receipts.
pub(super) fn publish_revision(
    context: &RuntimeContext,
    key: &str,
    epoch: &str,
    packet: &mut Value,
) -> Result<(), (u16, String)> {
    let _gate = context.is_isolated().then(|| acquire_social_state_gate(context)).transpose()?;
    let state = load_social_state_for(context)?;
    let conv = &state["conversations"][key];
    if !conversation_matches_epoch(conv, epoch) {
        return Err((409, SEND_CANCELLED_MESSAGE.into()));
    }
    let unread = conv["unread"].as_array().cloned().unwrap_or_default();
    let messages = packet["newMessages"].as_array().cloned().unwrap_or_default();
    agent::note_receipts(context, key, conv, &unread, &messages);
    packet["readThroughSeq"] = json!(agent::through_of(context, key, conv));
    Ok(())
}

/// Returns a revision packet without a QQ submission, or waits until this bubble is ready.
#[allow(clippy::too_many_arguments)]
pub(super) fn hold(
    context: &RuntimeContext,
    cfg: &Value,
    ob: &crate::onebot::OneBot,
    key: &str,
    kind: &str,
    id: &str,
    epoch: &str,
    policy: GuardPolicy,
    tool: &str,
    text: &str,
    baseline: &Value,
    previous_sent: Option<i64>,
) -> Result<Option<Value>, (u16, String)> {
    if current_mode_for(context)? != "reserved2" {
        return Ok(None);
    }
    send_guard_for(context, cfg, key, epoch, policy, tool)?;
    let after = baseline["lastUnreadSeq"].as_i64().unwrap_or(0);
    let last_incoming = baseline["lastReplyIncomingAt"]
        .as_i64()
        .or_else(|| baseline["lastIncomingAt"].as_i64())
        .or_else(|| {
            baseline["recentMessages"]
                .as_array()
                .into_iter()
                .flatten()
                .rev()
                .filter(|m| m["isSelf"] != true && m["quietNeutral"] != true)
                .find_map(|m| m["time"].as_i64())
        })
        .filter(|t| *t > 0);
    let urgent = urgent(cfg, key, baseline);
    let mut rng = random()?;
    let quiet = rng.quiet(QuietWindow::configured(cfg, urgent));
    let send_cfg = cfg.pointer("/socialV2/send").unwrap_or(&Value::Null);
    let typing_budget = if send_cfg["firstGapEnabled"] == false {
        0
    } else {
        rng.typing(agent::first_typing_delay(send_cfg, text, 0))
    };
    let start = if send_cfg["fillTypingBudget"] == false {
        now_ms()
    } else {
        previous_sent.unwrap_or_else(|| thinking_start(context, key, epoch, baseline))
    };
    let typing_at = if start > 0 {
        start.saturating_add(typing_budget)
    } else {
        now_ms().saturating_add(typing_budget)
    };
    let deadline = last_incoming
        .map(|at| at.saturating_add(quiet))
        .unwrap_or_else(now_ms)
        .max(typing_at);
    let typing = Typing::new(
        ob,
        (kind == "private" && now_ms() < deadline)
            .then(|| id.parse::<i64>().ok())
            .flatten(),
    );
    let mut revisions_exhausted = false;
    loop {
        send_guard_for(context, cfg, key, epoch, policy, tool)?;
        let state = load_social_state_for(context)?;
        if !revisions_exhausted
            && state["conversations"][key]["lastUnreadSeq"]
                .as_i64()
                .unwrap_or(0)
                > after
        {
            if let Some(packet) = revision(context, cfg, key, epoch, after)? {
                return Ok(Some(packet));
            }
            // After three revision returns, this request's fixed deadline wins over later arrivals.
            revisions_exhausted = true;
        }
        let remaining = deadline.saturating_sub(now_ms());
        if remaining <= 0 {
            break;
        }
        wait_sleep(remaining.min(80) as u64);
    }
    // Clearing private typing can wait on the gateway. Only authorize submission after it ends.
    drop(typing);
    send_guard_for(context, cfg, key, epoch, policy, tool)?;
    if !revisions_exhausted {
        let state = load_social_state_for(context)?;
        if state["conversations"][key]["lastUnreadSeq"]
            .as_i64()
            .unwrap_or(0)
            > after
        {
            return revision(context, cfg, key, epoch, after);
        }
    }
    Ok(None)
}
