//! Stored conversation evidence per conversation type. Reserved2 reads social-state `recentMessages`
//! (space `msg`, both sides). Other modes read handled/backfilled inbound-journal messages (space
//! `inbound`) merged with HER's completed harness replies. HER's commands and her own frames never enter.
use crate::runtime::RuntimeContext;
use serde_json::Value;

pub(super) struct Batch {
    /// The user message for the model.
    pub input: String,
    /// First and last consumed sequence (skipped commands included).
    pub first: i64,
    pub through: i64,
    /// Incoming messages that reached `input`.
    pub messages: usize,
}

pub(super) fn space(context: &RuntimeContext) -> &'static str {
    if crate::console::runtime_mode(context) == "reserved2" { "msg" } else { "inbound" }
}

pub(super) fn conversations(context: &RuntimeContext) -> Result<Vec<String>, String> {
    let state = context.read_social_state()?;
    let mut keys: Vec<String> = state["conversations"].as_object().map(|c| c.keys().cloned().collect()).unwrap_or_default();
    for key in crate::inbound::Journal::open(context)?.event_conversations()? {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.retain(|k| k.starts_with("group:") || k.starts_with("private:"));
    keys.sort();
    Ok(keys)
}

pub(super) fn read(context: &RuntimeContext, cfg: &Value, key: &str, space: &str, cursor: Option<(i64, i64)>, max_chars: usize)
    -> Result<Option<Batch>, String> {
    let (through, since) = cursor.unwrap_or((0, 0));
    let view = crate::model_view::with_blacklist(context, cfg);
    if space == "msg" { social(context, key, through, max_chars, &view) } else { journal(context, cfg, key, through, since, max_chars, &view) }
}

/// HER's commands: any slash text, and the owner's exact recovery commands.
fn is_command(text: &str, owner: bool) -> bool {
    let text = text.trim();
    text.starts_with('/') || (owner && crate::turn_recovery::is_recovery_command(text))
}

fn line(view: &Value, seq: Option<i64>, speaker: &str, text: &str) -> String {
    let (speaker, text) = (crate::model_view::strip_text(speaker, view), crate::model_view::strip_text(text, view));
    match seq { Some(seq) => format!("#{seq} {speaker}：{text}"), None => format!("{speaker}：{text}") }
}

fn input(key: &str, lines: &[String]) -> String {
    format!("会话：{}\n{}", if key.starts_with("private:") { "私聊" } else { "群聊" }, lines.join("\n"))
}

fn social(context: &RuntimeContext, key: &str, cursor: i64, max_chars: usize, view: &Value) -> Result<Option<Batch>, String> {
    let state = context.read_social_state()?;
    let messages = state["conversations"][key]["recentMessages"].as_array().cloned().unwrap_or_default();
    let max_seq = messages.iter().filter_map(|m| m["seq"].as_i64()).max().unwrap_or(0);
    // A recreated conversation restarts its sequence below the cursor: read it from the start.
    let cursor = if max_seq < cursor { 0 } else { cursor };
    let start = messages.iter().rposition(|m| m["seq"].as_i64().is_some_and(|s| s <= cursor)).map_or(0, |i| i + 1);
    let (mut lines, mut first, mut through, mut count, mut chars) = (Vec::new(), 0, 0, 0usize, 0usize);
    for m in &messages[start..] {
        let own = m["isSelf"] == true;
        let seq = m["seq"].as_i64();
        if !own && seq.is_none() {
            continue;
        }
        let owner = m["isOwner"] == true;
        let text = m["fullText"].as_str().or_else(|| m["text"].as_str()).unwrap_or("").trim().to_string();
        let speaker = if own { "我" } else if owner { "管理员" } else {
            m["sender"].as_str().filter(|s| !s.is_empty() && *s != "私聊").or_else(|| m["userId"].as_str()).unwrap_or("对方")
        };
        let rendered = (!text.is_empty() && (own || !is_command(&text, owner))).then(|| line(view, seq.filter(|_| !own), speaker, &text));
        if let Some(rendered) = &rendered {
            if !lines.is_empty() && chars + rendered.chars().count() > max_chars {
                break;
            }
        }
        if let Some(seq) = seq {
            if first == 0 { first = seq; }
            through = seq;
        }
        if let Some(rendered) = rendered {
            chars += rendered.chars().count();
            if !own { count += 1; }
            lines.push(rendered);
        }
    }
    if through == 0 {
        return Ok(None);
    }
    Ok(Some(Batch { input: input(key, &lines), first, through, messages: count }))
}

fn journal_line(raw: &str, owner: &str, seq: i64, view: &Value) -> Option<String> {
    let event: Value = serde_json::from_str(raw).ok()?;
    if event["post_type"] != "message" {
        return None; // includes `message_sent` echoes of HER's own notices
    }
    let user = crate::aux_text::js_string(&event["user_id"]);
    let self_id = event["self_id"].as_i64().unwrap_or(0);
    if user.is_empty() || user == self_id.to_string() {
        return None; // HER's own account
    }
    let text = crate::inbound_text::render_inbound(&event["message"], self_id, &mut crate::inbound_text::NoLookups).text;
    let text = text.trim();
    let is_owner = !owner.is_empty() && user == owner;
    if text.is_empty() || is_command(text, is_owner) {
        return None;
    }
    let speaker = if is_owner { "管理员".to_string() } else {
        event["sender"]["card"].as_str().filter(|s| !s.is_empty())
            .or_else(|| event["sender"]["nickname"].as_str().filter(|s| !s.is_empty()))
            .map(str::to_string).unwrap_or(user)
    };
    Some(line(view, Some(seq), &speaker, text))
}

fn journal(context: &RuntimeContext, cfg: &Value, key: &str, cursor: i64, since_ms: i64, max_chars: usize, view: &Value)
    -> Result<Option<Batch>, String> {
    let journal = crate::inbound::Journal::open(context)?;
    let cursor = if journal.last_seq(key)? < cursor { 0 } else { cursor };
    let owner = cfg.get("ownerQQ").filter(|v| !v.is_null()).map(crate::aux_text::js_string).unwrap_or_default();
    let mut timed: Vec<(i64, String)> = Vec::new();
    let (mut first, mut through, mut count, mut chars, mut truncated, mut first_at, mut last_at) = (0, 0, 0usize, 0usize, false, 0, 0);
    for (seq, stage, received_ms, raw) in journal.conversation_events(key, cursor, 2000)? {
        if stage == "received" || stage == "processing" {
            break; // still in flight: the next run reads it
        }
        let rendered = if stage == "handled" || stage == "backfilled" { journal_line(&raw, &owner, seq, view) } else { None };
        if let Some(rendered) = &rendered {
            if count > 0 && chars + rendered.chars().count() > max_chars {
                truncated = true;
                break;
            }
        }
        if first == 0 { first = seq; }
        through = seq;
        if let Some(rendered) = rendered {
            chars += rendered.chars().count();
            count += 1;
            if first_at == 0 { first_at = received_ms; }
            last_at = received_ms;
            timed.push((received_ms, rendered));
        }
    }
    if through == 0 {
        return Ok(None);
    }
    if count > 0 {
        for (at, text) in crate::harness::store::assistant_texts(context, key, since_ms.max(first_at))? {
            if truncated && at > last_at {
                continue;
            }
            timed.push((at, line(view, None, "我", &text)));
        }
    }
    timed.sort_by_key(|(at, _)| *at);
    let lines: Vec<String> = timed.into_iter().map(|(_, l)| l).collect();
    Ok(Some(Batch { input: input(key, &lines), first, through, messages: count }))
}
