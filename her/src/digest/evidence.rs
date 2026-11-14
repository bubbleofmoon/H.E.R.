//! Stored conversation evidence per conversation type. Reserved2 reads social-state `recentMessages`
//! (space `msg`, both sides). Other modes read handled/backfilled inbound-journal messages (space
//! `inbound`) merged with HER's completed harness replies. HER's commands and her own frames never enter.
//! The whole input, header and newlines included, stays within `maxInputChars`.
use crate::memory::DigestCursor;
use crate::runtime::RuntimeContext;
use serde_json::Value;

/// Inbound rows one read consumes at most.
const ROWS: usize = 2000;

pub(super) struct Batch {
    /// The user message for the model.
    pub input: String,
    /// First and last consumed sequence (skipped commands included).
    pub first: i64,
    pub through: i64,
    /// Incoming messages that reached `input`.
    pub messages: usize,
    /// Incoming messages waiting after the cursor, counted past the input cap: the backlog.
    pub pending: usize,
    /// Creation time of the last HER reply consumed (journal space); the stored value otherwise.
    pub replies_ms: i64,
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

pub(super) fn read(context: &RuntimeContext, cfg: &Value, key: &str, space: &str, cursor: DigestCursor, max_chars: usize)
    -> Result<Option<Batch>, String> {
    let view = crate::model_view::with_blacklist(context, cfg);
    if space == "msg" { social(context, key, cursor, max_chars, &view) } else { journal(context, cfg, key, cursor, max_chars, &view) }
}

/// HER's commands, decided on the plain text without a quoted reply, as the bridge decides them:
/// any slash text, and the owner's exact recovery commands.
fn is_command(plain: &str, owner: bool) -> bool {
    let plain = plain.trim();
    plain.starts_with('/') || (owner && crate::turn_recovery::is_recovery_command(plain))
}

fn line(view: &Value, seq: Option<i64>, speaker: &str, text: &str) -> String {
    let (speaker, text) = (crate::model_view::strip_text(speaker, view), crate::model_view::strip_text(text, view));
    match seq { Some(seq) => format!("#{seq} {speaker}：{text}"), None => format!("{speaker}：{text}") }
}

/// The model input, capped as a whole: header, lines and the newlines between them.
struct Lines { text: String, chars: usize, max: usize, count: usize, closed: bool }

impl Lines {
    fn new(key: &str, max: usize) -> Self {
        let text = format!("会话：{}", if key.starts_with("private:") { "私聊" } else { "群聊" });
        Self { chars: text.chars().count(), text, max, count: 0, closed: false }
    }

    fn full(&self) -> bool { self.closed }

    /// Append `line` on a new line when it fits. A first line that alone exceeds the cap is cut to fit
    /// (by characters) and closes the input; the first later line that does not fit closes it too.
    fn push(&mut self, line: &str) -> bool {
        if self.closed { return false; }
        let cost = line.chars().count() + 1;
        if self.chars + cost <= self.max {
            self.text.push('\n');
            self.text.push_str(line);
            self.chars += cost;
            self.count += 1;
            return true;
        }
        self.closed = true;
        if self.count > 0 { return false; }
        let room = self.max.saturating_sub(self.chars + 1);
        self.text.push('\n');
        self.text.extend(line.chars().take(room));
        self.chars += 1 + room;
        self.count = 1;
        true
    }
}

fn social(context: &RuntimeContext, key: &str, cursor: DigestCursor, max_chars: usize, view: &Value) -> Result<Option<Batch>, String> {
    let state = context.read_social_state()?;
    let messages = state["conversations"][key]["recentMessages"].as_array().cloned().unwrap_or_default();
    let max_seq = messages.iter().filter_map(|m| m["seq"].as_i64()).max().unwrap_or(0);
    // A recreated conversation restarts its sequence below the cursor: read it from the start.
    let after = if max_seq < cursor.through { 0 } else { cursor.through };
    let start = messages.iter().rposition(|m| m["seq"].as_i64().is_some_and(|s| s <= after)).map_or(0, |i| i + 1);
    let mut out = Lines::new(key, max_chars);
    let (mut first, mut through, mut count, mut pending) = (0, 0, 0usize, 0usize);
    for m in &messages[start..] {
        let own = m["isSelf"] == true;
        let seq = m["seq"].as_i64();
        if !own && seq.is_none() { continue; }
        let owner = m["isOwner"] == true;
        let text = m["fullText"].as_str().or_else(|| m["text"].as_str()).unwrap_or("").trim().to_string();
        // The bridge decides commands on the plain text, which leaves out a quoted reply.
        let plain = m["plain"].as_str().unwrap_or(text.as_str());
        let speaker = if own { "我" } else if owner { "管理员" } else {
            m["sender"].as_str().filter(|s| !s.is_empty() && *s != "私聊").or_else(|| m["userId"].as_str()).unwrap_or("对方")
        };
        let rendered = (!text.is_empty() && (own || !is_command(plain, owner))).then(|| line(view, seq.filter(|_| !own), speaker, &text));
        let incoming = usize::from(rendered.is_some() && !own);
        pending += incoming;
        if out.full() { continue; }
        if let Some(rendered) = &rendered {
            if !out.push(rendered) { continue; }
        }
        count += incoming;
        if let Some(seq) = seq {
            if first == 0 { first = seq; }
            through = seq;
        }
    }
    if through == 0 { return Ok(None); }
    Ok(Some(Batch { input: out.text, first, through, messages: count, pending, replies_ms: cursor.replies_ms }))
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
    let rendered = crate::inbound_text::render_inbound(&event["message"], self_id, &mut crate::inbound_text::NoLookups);
    let (text, plain) = (rendered.text.trim(), rendered.plain.trim());
    let is_owner = !owner.is_empty() && user == owner;
    // Commands are decided on the plain rendering without the quoted reply, as the bridge decides them.
    if text.is_empty() || is_command(plain, is_owner) {
        return None;
    }
    let speaker = if is_owner { "管理员".to_string() } else {
        event["sender"]["card"].as_str().filter(|s| !s.is_empty())
            .or_else(|| event["sender"]["nickname"].as_str().filter(|s| !s.is_empty()))
            .map(str::to_string).unwrap_or(user)
    };
    Some(line(view, Some(seq), &speaker, text))
}

fn journal(context: &RuntimeContext, cfg: &Value, key: &str, cursor: DigestCursor, max_chars: usize, view: &Value)
    -> Result<Option<Batch>, String> {
    let journal = crate::inbound::Journal::open(context)?;
    // A recreated journal restarts its sequence below the cursor: read it from the start.
    let after = if journal.last_seq(key)? < cursor.through { 0 } else { cursor.through };
    let owner = cfg.get("ownerQQ").filter(|v| !v.is_null()).map(crate::aux_text::js_string).unwrap_or_default();
    // One row more than a read consumes shows whether the read reached the end.
    let rows = journal.conversation_events(key, after, ROWS + 1)?;
    let Some(first_at) = rows.first().map(|row| row.2) else { return Ok(None) };
    // HER's replies after their own progress; a conversation's first digest starts at its first row.
    let replies_after = if cursor.replies_ms > 0 { cursor.replies_ms } else { first_at - 1 };
    let mut replies = crate::harness::store::assistant_texts(context, key, replies_after + 1)?;
    replies.sort_by_key(|(at, _)| *at);
    let mut replies = replies.into_iter().peekable();
    let mut out = Lines::new(key, max_chars);
    let (mut first, mut through, mut messages, mut pending, mut replies_ms) = (0, 0, 0usize, 0usize, replies_after);
    let mut reached_end = true;
    for (i, (seq, stage, received_ms, raw)) in rows.iter().enumerate() {
        let (seq, received_ms, stage) = (*seq, *received_ms, stage.as_str());
        if i == ROWS || stage == "received" || stage == "processing" {
            reached_end = false; // beyond this read, or still in flight: the next run reads it
            break;
        }
        let rendered = if stage == "handled" || stage == "backfilled" { journal_line(raw, &owner, seq, view) } else { None };
        // HER's replies that came before this row.
        while !out.full() && replies.peek().is_some_and(|(at, _)| *at < received_ms) {
            let (at, text) = replies.next().expect("peeked reply");
            if out.push(&line(view, None, "我", &text)) { replies_ms = at; }
        }
        if out.full() {
            pending += usize::from(rendered.is_some());
            continue;
        }
        if let Some(rendered) = &rendered {
            pending += 1;
            if !out.push(rendered) { continue; }
            messages += 1;
        }
        if first == 0 { first = seq; }
        through = seq;
    }
    if reached_end {
        // HER's replies after the last row; only completed turns are stored as replies.
        for (at, text) in replies {
            if !out.push(&line(view, None, "我", &text)) { break; }
            replies_ms = at;
        }
    }
    if through == 0 {
        if replies_ms == replies_after { return Ok(None); }
        through = after; // only HER's replies fit: the messages wait for the next read
    }
    Ok(Some(Batch { input: out.text, first, through, messages, pending, replies_ms: replies_ms.max(cursor.replies_ms) }))
}
