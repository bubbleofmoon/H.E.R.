//! Stored conversation evidence per conversation type. Reserved2 reads social-state `recentMessages`
//! (space `msg`, both sides). Other modes read handled/backfilled inbound-journal messages (space
//! `inbound`) merged with HER's completed harness replies. HER's commands and her own frames never enter.
//! The whole input, header and newlines included, stays within `maxInputChars`, and HER's own lines
//! never take the room of the next message that moves the cursor.
use crate::memory::DigestCursor;
use crate::runtime::RuntimeContext;
use serde_json::Value;

/// Inbound rows one read consumes, and one backlog page counts, at most.
const ROWS: usize = 2000;

pub(super) struct Batch {
    /// The user message for the model.
    pub input: String,
    /// First and last consumed sequence (skipped commands included).
    pub first: i64,
    pub through: i64,
    /// Incoming messages that reached `input`.
    pub messages: usize,
    /// Incoming messages waiting after the cursor, counted past the input cap and past one journal page
    /// until the eligibility threshold or the end of the evidence: the backlog.
    pub pending: usize,
    /// Position (creation ms, event sequence) of the last HER reply consumed (journal space); the stored value otherwise.
    pub replies: (i64, i64),
    /// The reply position this read started from; a skip that consumes no model call keeps it.
    pub from: (i64, i64),
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

pub(super) fn read(context: &RuntimeContext, cfg: &Value, key: &str, space: &str, cursor: DigestCursor, max_chars: usize, need: usize)
    -> Result<Option<Batch>, String> {
    let view = crate::model_view::with_blacklist(context, cfg);
    if space == "msg" { social(context, key, cursor, max_chars, &view) } else { journal(context, cfg, key, cursor, max_chars, need, &view) }
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

fn in_flight(stage: &str) -> bool {
    stage == "received" || stage == "processing"
}

/// The model input, capped as a whole: header, lines and the newlines between them. Until the first item
/// that moves the cursor is taken, room for its line is kept: HER's own lines may use only the rest and
/// never close the input.
struct Lines { text: String, chars: usize, max: usize, count: usize, closed: bool, lead: Option<usize>, lead_closed: bool }

impl Lines {
    fn new(key: &str, max: usize) -> Self {
        let text = format!("会话：{}", if key.starts_with("private:") { "私聊" } else { "群聊" });
        Self { chars: text.chars().count(), text, max, count: 0, closed: false, lead: None, lead_closed: false }
    }

    fn full(&self) -> bool { self.closed }

    fn append(&mut self, line: &str, chars: usize) {
        self.text.push('\n');
        self.text.push_str(line);
        self.chars += chars + 1;
        self.count += 1;
    }

    /// Keep room for the first cursor-moving item's `line` (`None`: it renders no line) until `release`.
    fn hold_for(&mut self, line: Option<&str>) {
        self.lead = Some(line.map_or(0, |l| l.chars().count() + 1));
    }

    /// The first cursor-moving item is taken: later lines share the cap normally.
    fn release(&mut self) {
        self.lead = None;
        self.lead_closed = false;
    }

    /// Append `line` when it fits. A first line that alone exceeds the cap is cut to fit (by characters)
    /// and closes the input; the first later line that does not fit closes it too.
    fn push(&mut self, line: &str) -> bool {
        if self.closed { return false; }
        let chars = line.chars().count();
        if self.chars + chars + 1 <= self.max {
            self.append(line, chars);
            return true;
        }
        self.closed = true;
        if self.count > 0 { return false; }
        let room = self.max.saturating_sub(self.chars + 1);
        let cut: String = line.chars().take(room).collect();
        self.append(&cut, room);
        true
    }

    /// Append one of HER's own lines. While room is kept for the first cursor-moving item it may use only
    /// the rest: the first one that does not fit ends HER's leading lines for this input without closing it
    /// (a first such line is cut to the room left).
    fn push_context(&mut self, line: &str) -> bool {
        let Some(kept) = self.lead else { return self.push(line) };
        if self.lead_closed { return false; }
        let room = self.max.saturating_sub(self.chars + kept);
        let chars = line.chars().count();
        if chars + 1 <= room {
            self.append(line, chars);
            return true;
        }
        self.lead_closed = true;
        if self.count > 0 || room < 2 { return false; }
        let cut: String = line.chars().take(room - 1).collect();
        self.append(&cut, room - 1);
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
    // (own, seq, line) after the cursor; an incoming message without a sequence is not evidence.
    let entries: Vec<(bool, Option<i64>, Option<String>)> = messages[start..].iter().filter_map(|m| {
        let own = m["isSelf"] == true;
        let seq = m["seq"].as_i64();
        if !own && seq.is_none() { return None; }
        let owner = m["isOwner"] == true;
        let text = m["fullText"].as_str().or_else(|| m["text"].as_str()).unwrap_or("").trim().to_string();
        // The bridge decides commands on the plain text, which leaves out a quoted reply.
        let plain = m["plain"].as_str().unwrap_or(text.as_str());
        let speaker = if own { "我" } else if owner { "管理员" } else {
            m["sender"].as_str().filter(|s| !s.is_empty() && *s != "私聊").or_else(|| m["userId"].as_str()).unwrap_or("对方")
        };
        let rendered = (!text.is_empty() && (own || !is_command(plain, owner))).then(|| line(view, seq.filter(|_| !own), speaker, &text));
        Some((own, seq, rendered))
    }).collect();
    // Only a sequenced message moves the cursor; HER's own lines before it never take its room.
    let Some((_, _, lead)) = entries.iter().find(|(_, seq, _)| seq.is_some()) else { return Ok(None) };
    let mut out = Lines::new(key, max_chars);
    out.hold_for(lead.as_deref());
    let (mut first, mut through, mut count, mut pending) = (0, 0, 0usize, 0usize);
    for (own, seq, rendered) in &entries {
        let incoming = usize::from(rendered.is_some() && !*own);
        pending += incoming;
        let Some(seq) = *seq else {
            if let Some(rendered) = rendered { out.push_context(rendered); }
            continue;
        };
        if out.full() { continue; }
        if let Some(rendered) = rendered {
            if !out.push(rendered) { continue; }
        }
        count += incoming;
        if first == 0 { first = seq; }
        through = seq;
        out.release();
    }
    if through == 0 { return Ok(None); }
    Ok(Some(Batch { input: out.text, first, through, messages: count, pending, replies: (cursor.replies_ms, cursor.replies_seq), from: (cursor.replies_ms, cursor.replies_seq) }))
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

fn journal(context: &RuntimeContext, cfg: &Value, key: &str, cursor: DigestCursor, max_chars: usize, need: usize, view: &Value)
    -> Result<Option<Batch>, String> {
    let journal = crate::inbound::Journal::open(context)?;
    // A recreated journal restarts its sequence below the cursor: read it from the start.
    let after = if journal.last_seq(key)? < cursor.through { 0 } else { cursor.through };
    let owner = cfg.get("ownerQQ").filter(|v| !v.is_null()).map(crate::aux_text::js_string).unwrap_or_default();
    let line_of = |seq: i64, stage: &str, raw: &str| {
        if stage == "handled" || stage == "backfilled" { journal_line(raw, &owner, seq, view) } else { None }
    };
    // One row more than a read consumes shows whether the read reached the end.
    let rows = journal.conversation_events(key, after, ROWS + 1)?;
    // Nothing new, or the oldest new row is still in flight: the next run reads it, and HER's replies wait with it.
    let Some((lead_seq, lead_stage, first_at, lead_raw)) = rows.first() else { return Ok(None) };
    if in_flight(lead_stage.as_str()) { return Ok(None); }
    // HER's replies after their own position (creation ms, then event sequence); a conversation's first
    // digest starts at its first row.
    let start = if cursor.replies_ms > 0 { (cursor.replies_ms, cursor.replies_seq) } else { (*first_at - 1, i64::MAX) };
    let mut replies = crate::harness::store::assistant_texts(context, key, start)?.into_iter().peekable();
    let mut lead = line_of(*lead_seq, lead_stage.as_str(), lead_raw.as_str());
    let mut out = Lines::new(key, max_chars);
    out.hold_for(lead.as_deref());
    let (mut first, mut through, mut messages, mut pending, mut progress) = (0, 0, 0usize, 0usize, start);
    // A reply that does not fit ends HER's replies for this input, so their progress has no gap.
    let (mut replies_open, mut stopped, mut paged) = (true, false, false);
    for (i, (seq, stage, received_ms, raw)) in rows.iter().enumerate() {
        let (seq, received_ms, stage) = (*seq, *received_ms, stage.as_str());
        if i == ROWS || in_flight(stage) {
            stopped = true; // beyond this read, or still in flight: the next run reads it
            paged = i == ROWS;
            break;
        }
        let rendered = if i == 0 { lead.take() } else { line_of(seq, stage, raw.as_str()) };
        // HER's replies that came before this row.
        while replies_open && !out.full() && replies.peek().is_some_and(|(at, _, _)| *at < received_ms) {
            let (at, id, text) = replies.next().expect("peeked reply");
            if out.push_context(&line(view, None, "我", &text)) { progress = (at, id); } else { replies_open = false; }
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
        out.release();
    }
    if !stopped && replies_open {
        // HER's replies after the last row; only completed turns are stored as replies.
        for (at, id, text) in replies {
            if !out.push(&line(view, None, "我", &text)) { break; }
            progress = (at, id);
        }
    }
    // The backlog counts past this page until it reaches `need` or the evidence ends.
    if paged {
        let mut last = rows[ROWS - 1].0;
        'pages: while pending < need {
            let page = journal.conversation_events(key, last, ROWS)?;
            for (seq, stage, _, raw) in &page {
                if in_flight(stage.as_str()) { break 'pages; }
                pending += usize::from(line_of(*seq, stage.as_str(), raw.as_str()).is_some());
                if pending >= need { break 'pages; }
                last = *seq;
            }
            if page.len() < ROWS { break; }
        }
    }
    if through == 0 { return Ok(None); } // defensive: the first row always has its room
    Ok(Some(Batch { input: out.text, first, through, messages, pending, replies: progress, from: start }))
}
