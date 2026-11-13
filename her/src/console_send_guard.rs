//! Per-turn text-send admission. Call only while holding the shared send chain, before any hold.
use super::*;

const MAX_SAFE: u64 = 9_007_199_254_740_991;

struct Sent {
    order: usize,
    scope: String,
    text: Option<String>,
}

// Token rotation invalidates tools and revision evidence; it does not mint another actual turn.
fn same_turn(left: &str, right: &str) -> bool {
    let identity = |scope: &str| scope.find(':').map(|idx| idx + 1).unwrap_or(0);
    left[identity(left)..] == right[identity(right)..]
}

pub(super) fn admit(
    context: &RuntimeContext,
    key: &str,
    messages: &[String],
) -> Result<String, (u16, String)> {
    use crate::send_outbox::{fold_batch, read_records, ItemState};
    // Re-read after waiting for the send chain; operator edits apply without restarting a turn.
    let fresh = context
        .load_config()
        .map_err(|_| (503, "send guard config is unavailable".into()))?;
    let settings = &fresh["socialV2"]["send"];
    let positive = |name: &str, default| {
        settings[name]
            .as_u64()
            .filter(|n| (1..=MAX_SAFE).contains(n))
            .unwrap_or(default)
    };
    let repeat = positive("repeatLimit", 2);
    let cap = turn_send_cap(&fresh);
    let window = settings["repeatWindowSends"]
        .as_u64()
        .filter(|n| *n <= MAX_SAFE)
        .unwrap_or(0);
    let state = load_social_state_for(context)?;
    let (scope, wait_used) = wait_usage(context, key, &state["conversations"][key])?;
    let records =
        read_records(context).map_err(|_| (503, "send guard outbox is unavailable".into()))?;
    let turns = if context.is_isolated() {
        crate::turn_ledger::fold(
            &crate::turn_ledger::read_records(context)
                .map_err(|_| (503, "send guard turn ledger is unavailable".into()))?,
        )
        .0
    } else {
        Vec::new()
    };
    let current = turns
        .iter()
        .rposition(|t| scope.ends_with(&format!(":{}", t.id)) && t.key == key);
    let previous_turn = current.and_then(|idx| turns[..idx].iter().rev().find(|t| t.key == key));
    // Group once: folding each batch against the whole append-only file would be quadratic.
    let mut grouped: HashMap<&str, Vec<Value>> = HashMap::new();
    let mut attempts: HashMap<(&str, u64), (usize, Option<&str>)> = HashMap::new();
    for (order, record) in records.iter().enumerate() {
        if let Some(id) = record["batch"].as_str() {
            grouped.entry(id).or_default().push(record.clone());
            if record["k"] == "attempt" {
                if let Some(idx) = record["idx"].as_u64() {
                    attempts.insert((id, idx), (order, record["turnScope"].as_str()));
                }
            }
        }
    }
    let mut by_epoch: HashMap<&str, Vec<&crate::turn_ledger::Turn>> = HashMap::new();
    for turn in turns.iter().filter(|t| t.key == key) {
        for epoch in &turn.epochs {
            by_epoch.entry(epoch).or_default().push(turn);
        }
    }
    for entries in by_epoch.values_mut() {
        entries.sort_by_key(|t| t.begun_at);
    }
    let mut history: Vec<Sent> = Vec::new();
    for (intent_order, intent) in records
        .iter()
        .enumerate()
        .filter(|(_, r)| r["k"] == "intent" && r["key"] == key)
    {
        let Some(batch) = intent["batch"]
            .as_str()
            .and_then(|id| grouped.get(id).and_then(|rows| fold_batch(rows, id)))
        else {
            continue;
        };
        // Older candidates did not stamp the turn. Infer it from the existing durable ledger.
        let turn_scope = intent["turnScope"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let turn = by_epoch.get(batch.epoch.as_str()).and_then(|entries| {
                    let end = entries.partition_point(|t| t.begun_at <= batch.created_at);
                    end.checked_sub(1).map(|idx| entries[idx])
                });
                turn.map(|t| format!("{}:{}", batch.epoch, t.id))
                    .unwrap_or_else(|| format!("{}:legacy", batch.epoch))
            });
        for (idx, item) in batch.items.iter().enumerate() {
            if matches!(item, ItemState::Confirmed { .. }) || item.needs_verification() {
                // A resumed item is charged to the turn that actually submitted it, not its intent.
                let attempt = attempts.get(&(batch.id.as_str(), idx as u64));
                history.push(Sent {
                    order: attempt.map(|(order, _)| *order).unwrap_or(intent_order),
                    scope: attempt
                        .and_then(|(_, scope)| *scope)
                        .unwrap_or(&turn_scope)
                        .to_owned(),
                    // Typed sends shift the window and spend cap, but never compare as literal text.
                    text: (batch.kind == "text")
                        .then(|| crate::outbound_message::normalize(&batch.messages[idx])),
                });
            }
        }
    }
    history.sort_by_key(|sent| sent.order);
    let previous_scope = history
        .iter()
        .rev()
        .find(|sent| !same_turn(&sent.scope, &scope))
        .map(|sent| sent.scope.clone());
    let current_count = history
        .iter()
        .filter(|sent| same_turn(&sent.scope, &scope))
        .count() as u64;
    let mut counts: HashMap<&str, u64> = HashMap::new();
    let window_start = history
        .len()
        .saturating_sub(usize::try_from(window).unwrap_or(usize::MAX));
    for (idx, sent) in history.iter().enumerate() {
        let s = &sent.scope;
        let cross = if window > 0 {
            idx >= window_start
        } else if let Some(previous) = previous_turn {
            s.ends_with(&format!(":{}", previous.id))
        } else if current.is_none() {
            previous_scope
                .as_ref()
                .is_some_and(|previous| same_turn(previous, s))
        } else {
            false
        };
        if same_turn(s, &scope) || cross {
            if let Some(text) = &sent.text {
                *counts.entry(text).or_default() += 1;
            }
        }
    }
    let mut violation = (current_count.saturating_add(messages.len() as u64) > cap)
        .then_some(("cap", current_count.saturating_add(messages.len() as u64)));
    if violation.is_none() {
        for text in messages {
            let count = counts.entry(text.as_str()).or_default();
            *count += 1;
            if *count > repeat {
                violation = Some(("repeat", *count));
                break;
            }
        }
    }
    let Some((reason, count)) = violation else {
        return Ok(scope);
    };
    crate::safe_println!("[send-guard] key={key} turn={scope} reason={reason} count={count}");
    if !records.iter().any(|r| {
        r["k"] == "send_guard_notice"
            && r["key"] == key
            && r["turnScope"]
                .as_str()
                .is_some_and(|prior| same_turn(prior, &scope))
    }) {
        // Claim durably before enqueueing; restart cannot send another notice for this turn.
        crate::send_outbox::append_record(
            context,
            &json!({"k":"send_guard_notice","key":key,
            "turnScope":scope,"reason":reason,"count":count,"t":now_ms()}),
        )
        .map_err(|_| (503, "send guard notice claim is unavailable".into()))?;
        crate::admin_alert::notify_notice(
            context,
            &fresh,
            &format!("[发送保护] {key} 本轮已触发发送限制；reason={reason}, count={count}。"),
            &format!("send-guard:{key}:{scope}"),
        );
    }
    Err((
        429,
        if reason == "cap" || wait_used >= wait_policy(&fresh).max_per_turn {
            END_TURN.to_owned()
        } else {
            "你正在重复自己，已拒绝整个发送批次。停止发送或说新的内容；不要重试相同消息。".into()
        },
    ))
}
