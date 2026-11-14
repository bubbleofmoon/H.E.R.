//! Dependency and recovery run record (ticket 25, AC1; rendered and indexed by ticket 27).
//!
//! An append-only, bounded JSON-lines log of what the bridge did about its dependencies and why: which
//! component, which trigger (startup, timer, recovery, manual, a turn), what happened and with what outcome.
//! Every reason is redacted and cut before it is written, identical repeats are collapsed into one line plus a
//! counter (a six-hour outage is one line and a few `repeat` summaries, not six hours of lines), the file is
//! compacted when it exceeds its byte cap, and a torn last line after a crash never poisons the reader.
//!
//! Record fields (format version 1), one JSON object per line:
//! `v`, `seq` (increasing; the pagination cursor), `ts` (ms), `runId`, `trigger`, `component`, `event`
//! (short slug), `outcome`, `reason` (redacted, at most 300 chars) and, on collapse summaries only,
//! `repeat`, `firstTs`, `lastTs`.
//!
//! This module owns the format, the writer and the reader. Where the file lives and the HTTP route that serves
//! [`Page::to_json`] are wired in phase B.
#![allow(dead_code)]

use crate::dependency::Clock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;

pub const FORMAT_VERSION: u8 = 1;
pub const FILE_NAME: &str = "run-record.jsonl";
pub const MAX_REASON_CHARS: usize = 300;
pub const MAX_EVENT_CHARS: usize = 64;
/// Most records one page returns.
pub const MAX_PAGE: usize = 200;
pub const DEFAULT_PAGE: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    Startup,
    Timer,
    Recovery,
    Manual,
    Turn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Component {
    Dsh,
    Onebot,
    ModelChain,
    Queue,
    Alert,
    Snowluma,
    Process,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Ok,
    Failed,
    /// The effect may or may not have happened (a send or a turn whose result was not seen).
    Unknown,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub v: u8,
    pub seq: u64,
    pub ts: i64,
    pub run_id: String,
    pub trigger: Trigger,
    pub component: Component,
    pub event: String,
    pub outcome: Outcome,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<i64>,
}

/// What a caller reports. `reason` must describe the failure, never carry chat text, tokens or paths (it is
/// scrubbed regardless).
#[derive(Debug, Clone)]
pub struct RecordInput<'a> {
    /// The run this belongs to; `None` makes the writer's process run id (one per bridge start).
    pub run_id: Option<&'a str>,
    pub trigger: Trigger,
    pub component: Component,
    pub event: &'a str,
    pub outcome: Outcome,
    pub reason: &'a str,
}

// ---- redaction ------------------------------------------------------------------------------

fn scrub_patterns() -> &'static [(regex::Regex, &'static str)] {
    use std::sync::OnceLock;
    static RE: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    RE.get_or_init(|| {
        let p = |pattern: &str, to: &'static str| (regex::Regex::new(pattern).unwrap(), to);
        vec![
            p(r"(?i)dsh-auth-[A-Za-z0-9_-]+(?:=[^\s;,]+)?", "<cookie>"),
            p(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{6,}", "<token>"),
            p(r"(?i)\b(token|secret|password|passwd|api[_-]?key|authorization)(\s*[:=]\s*)[^\s,;]+", "$1$2<redacted>"),
            p(r"[A-Za-z]:[\\/][^\s'\x22<>|]*", "<path>"),
            p(r"\\\\[^\\\s'\x22<>|]+\\[^\s'\x22<>|]*", "<path>"),
            p(r"(^|[\s'\x22(=])/(?:[A-Za-z0-9._~-]+/)+[A-Za-z0-9._~-]*", "$1<path>"),
            p(r"[A-Za-z0-9_-]{32,}", "<redacted>"),
        ]
    })
}

/// `reason` after the caller's redactor (for example `activity::redact`), then this module's own scrub:
/// credential-looking values, absolute paths and long opaque strings are replaced, control characters and
/// newlines become single spaces, and the result is cut to [`MAX_REASON_CHARS`].
pub fn sanitize_reason(reason: &str, redact: &dyn Fn(&str) -> String) -> String {
    let mut text = redact(reason);
    for (re, to) in scrub_patterns() {
        text = re.replace_all(&text, *to).into_owned();
    }
    let mut out = String::with_capacity(text.len().min(MAX_REASON_CHARS * 4));
    let mut gap = false;
    for c in text.chars() {
        if c.is_control() || c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    cut_chars(&out, MAX_REASON_CHARS)
}

fn cut_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn event_slug(event: &str) -> String {
    let slug: String = event
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(MAX_EVENT_CHARS)
        .collect();
    if slug.is_empty() {
        "event".to_string()
    } else {
        slug
    }
}

/// The reason with numbers folded, so "retry 3 in 4000 ms" and "retry 4 in 8000 ms" collapse together.
fn collapse_form(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut in_digits = false;
    for c in reason.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

// ---- storage ---------------------------------------------------------------------------------

/// The bytes behind the record. [`FileIo`] is the plain-file implementation; the wiring may supply one that goes
/// through the selected runtime's file gate.
pub trait RecordIo: Send + Sync {
    /// Append one complete line (ending in `\n`). A previous torn line is terminated first.
    fn append(&self, line: &[u8]) -> io::Result<()>;
    /// The last `max_bytes` of the file (all of it when smaller), starting at a line boundary. Missing file: empty.
    fn read_tail(&self, max_bytes: usize) -> io::Result<Vec<u8>>;
    fn len(&self) -> io::Result<u64>;
    /// Replace the whole content atomically.
    fn replace(&self, content: &[u8]) -> io::Result<()>;
}

pub struct FileIo {
    path: PathBuf,
}

impl FileIo {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl RecordIo for FileIo {
    fn append(&self, line: &[u8]) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.path)?;
        let len = file.metadata()?.len();
        let mut buf = Vec::with_capacity(line.len() + 1);
        if len > 0 {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                buf.push(b'\n'); // a crash left half a line: terminate it so this record is not glued to it
            }
        }
        buf.extend_from_slice(line);
        file.write_all(&buf) // one write call: O_APPEND keeps concurrent writers' lines whole
    }

    fn read_tail(&self, max_bytes: usize) -> io::Result<Vec<u8>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        let start = len.saturating_sub(max_bytes as u64);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity((len - start) as usize);
        file.read_to_end(&mut bytes)?;
        if start > 0 {
            match bytes.iter().position(|b| *b == b'\n') {
                Some(i) => bytes.drain(..=i),
                None => bytes.drain(..),
            };
        }
        Ok(bytes)
    }

    fn len(&self) -> io::Result<u64> {
        match std::fs::metadata(&self.path) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn replace(&self, content: &[u8]) -> io::Result<()> {
        Ok(crate::guarded_file::write_atomic(&self.path, content)?)
    }
}

// ---- writer ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct RecordLimits {
    /// Compaction starts when the file exceeds this.
    pub max_bytes: u64,
    /// Compaction keeps the newest lines up to this many bytes.
    pub keep_bytes: u64,
    /// Identical records inside this window are counted, not written.
    pub collapse_window_ms: i64,
    /// Distinct collapse keys remembered (all are flushed and dropped beyond this).
    pub max_pending: usize,
}

impl Default for RecordLimits {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024,
            keep_bytes: 384 * 1024,
            collapse_window_ms: 3_600_000,
            max_pending: 128,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordResult {
    Written(u64),
    /// Identical to a record written within the collapse window; counted for the next summary.
    Collapsed,
}

type CollapseKey = (Option<String>, Component, String, Outcome, String);

struct Pending {
    template: RunRecord,
    window_start: i64,
    suppressed: u32,
    first_ts: i64,
    last_ts: i64,
}

struct State {
    next_seq: u64,
    pending: HashMap<CollapseKey, Pending>,
}

pub struct RunRecordWriter {
    io: Box<dyn RecordIo>,
    clock: Box<dyn Clock>,
    redact: Box<dyn Fn(&str) -> String + Send + Sync>,
    limits: RecordLimits,
    process_run_id: String,
    state: Mutex<State>,
}

impl RunRecordWriter {
    pub fn new(
        io: Box<dyn RecordIo>,
        clock: Box<dyn Clock>,
        redact: Box<dyn Fn(&str) -> String + Send + Sync>,
        limits: RecordLimits,
    ) -> Self {
        let start = clock.now_ms();
        Self {
            io,
            clock,
            redact,
            limits,
            process_run_id: format!("run-{start:x}"),
            state: Mutex::new(State {
                next_seq: 1,
                pending: HashMap::new(),
            }),
        }
    }

    /// The run id given to records that do not name one (one per bridge start).
    pub fn process_run_id(&self) -> &str {
        &self.process_run_id
    }

    /// Record one event. An I/O failure is returned, never swallowed; the caller still reports it elsewhere
    /// (the activity log, the console) instead of losing the failure with the record.
    pub fn record(&self, input: &RecordInput) -> io::Result<RecordResult> {
        let now = self.clock.now_ms();
        let reason = sanitize_reason(input.reason, &*self.redact);
        let event = event_slug(input.event);
        let key: CollapseKey = (
            input.run_id.map(event_slug_id),
            input.component,
            event.clone(),
            input.outcome,
            collapse_form(&reason),
        );
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());

        if let Some(p) = state.pending.get_mut(&key) {
            if now - p.window_start < self.limits.collapse_window_ms {
                if p.suppressed == 0 {
                    p.first_ts = now;
                }
                p.suppressed = p.suppressed.saturating_add(1);
                p.last_ts = now;
                return Ok(RecordResult::Collapsed);
            }
        }
        // A genuinely new record (or an expired window): publish the counters of what was being collapsed first, so
        // the file reads in order (outage repeats, then the recovery).
        self.flush_locked(&mut state, now)?;
        if state.pending.len() >= self.limits.max_pending {
            state.pending.clear();
        }
        let seq = self.next_seq(&mut state)?;
        let record = RunRecord {
            v: FORMAT_VERSION,
            seq,
            ts: now,
            run_id: input
                .run_id
                .map(|r| event_slug_id(r))
                .unwrap_or_else(|| self.process_run_id.clone()),
            trigger: input.trigger,
            component: input.component,
            event,
            outcome: input.outcome,
            reason,
            repeat: None,
            first_ts: None,
            last_ts: None,
        };
        self.write(&record)?;
        state.next_seq = seq + 1;
        state.pending.insert(
            key,
            Pending {
                template: record,
                window_start: now,
                suppressed: 0,
                first_ts: now,
                last_ts: now,
            },
        );
        self.compact_if_needed()?;
        Ok(RecordResult::Written(seq))
    }

    /// Write a `repeat` summary for everything collapsed so far (periodic tick, recovery, shutdown). Returns lines written.
    pub fn flush(&self) -> io::Result<usize> {
        let now = self.clock.now_ms();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let n = self.flush_locked(&mut state, now)?;
        if n > 0 {
            self.compact_if_needed()?;
        }
        Ok(n)
    }

    fn flush_locked(&self, state: &mut State, now: i64) -> io::Result<usize> {
        let mut due: Vec<CollapseKey> = state
            .pending
            .iter()
            .filter(|(_, p)| p.suppressed > 0)
            .map(|(k, _)| k.clone())
            .collect();
        due.sort_by_key(|k| state.pending[k].first_ts);
        let mut written = 0;
        for key in due {
            let seq = self.next_seq(state)?;
            let p = state.pending.get_mut(&key).expect("key came from the map");
            let mut summary = p.template.clone();
            summary.seq = seq;
            summary.ts = now;
            summary.repeat = Some(p.suppressed);
            summary.first_ts = Some(p.first_ts);
            summary.last_ts = Some(p.last_ts);
            self.write(&summary)?;
            p.suppressed = 0;
            state.next_seq = seq + 1;
            written += 1;
        }
        Ok(written)
    }

    /// Next sequence number: ahead of this writer's own and of whatever another process appended to the file.
    fn next_seq(&self, state: &mut State) -> io::Result<u64> {
        let tail = self.io.read_tail(16 * 1024)?;
        let in_file = parse_lines(&tail)
            .0
            .iter()
            .map(|r| r.seq)
            .max()
            .unwrap_or(0);
        Ok(state.next_seq.max(in_file + 1))
    }

    fn write(&self, record: &RunRecord) -> io::Result<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        self.io.append(line.as_bytes())
    }

    fn compact_if_needed(&self) -> io::Result<()> {
        if self.io.len()? <= self.limits.max_bytes {
            return Ok(());
        }
        let tail = self.io.read_tail(self.limits.keep_bytes as usize)?;
        // keep only whole, parseable lines
        let mut kept = Vec::with_capacity(tail.len());
        for line in tail.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            if serde_json::from_slice::<RunRecord>(line).is_ok() {
                kept.extend_from_slice(line);
                kept.push(b'\n');
            }
        }
        self.io.replace(&kept)
    }
}

/// A caller-supplied run id, reduced to a safe slug.
fn event_slug_id(id: &str) -> String {
    let slug: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':') {
                c
            } else {
                '-'
            }
        })
        .take(96)
        .collect();
    if slug.is_empty() {
        "run".to_string()
    } else {
        slug
    }
}

// ---- reader ----------------------------------------------------------------------------------

/// Whole parseable records of `bytes` in file order, and the number of non-empty lines that were not records
/// (a torn line after a crash, a future format).
fn parse_lines(bytes: &[u8]) -> (Vec<RunRecord>, usize) {
    let mut records = Vec::new();
    let mut skipped = 0;
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
    {
        match serde_json::from_slice::<RunRecord>(line) {
            Ok(r) if r.v == FORMAT_VERSION => records.push(r),
            _ => skipped += 1,
        }
    }
    (records, skipped)
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Page size; clamped to `1..=MAX_PAGE`, default [`DEFAULT_PAGE`].
    pub limit: Option<usize>,
    /// Only records older than this `seq` (the previous page's `next_before`).
    pub before_seq: Option<u64>,
    pub component: Option<Component>,
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Newest first.
    pub records: Vec<RunRecord>,
    /// Pass as `before_seq` for the next (older) page; `None` at the end.
    pub next_before: Option<u64>,
    pub skipped: usize,
}

impl Page {
    pub fn to_json(&self) -> Value {
        json!({
            "records": self.records,
            "nextBefore": self.next_before,
            "skipped": self.skipped,
        })
    }
}

/// Read one page, newest first. Reads at most the last `max_read_bytes` of the file, so the cost is bounded
/// however large the file is.
pub fn read_page(io: &dyn RecordIo, query: &Query, max_read_bytes: usize) -> io::Result<Page> {
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let (mut records, skipped) = parse_lines(&io.read_tail(max_read_bytes)?);
    records.sort_by(|a, b| b.seq.cmp(&a.seq).then(b.ts.cmp(&a.ts)));
    let mut matching = records.into_iter().filter(|r| {
        query.before_seq.is_none_or(|b| r.seq < b)
            && query.component.is_none_or(|c| r.component == c)
            && query.outcome.is_none_or(|o| r.outcome == o)
    });
    let mut page: Vec<RunRecord> = matching.by_ref().take(limit).collect();
    let more = matching.next().is_some();
    let next_before = if more {
        page.last().map(|r| r.seq)
    } else {
        None
    };
    page.shrink_to_fit();
    Ok(Page {
        records: page,
        next_before,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency::ManualClock;
    use std::sync::Arc;

    /// In-memory storage; clones share the bytes so a test can inspect what the writer wrote.
    #[derive(Clone, Default)]
    struct MemIo(Arc<Mutex<Vec<u8>>>);

    impl MemIo {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
        fn lines(&self) -> Vec<Value> {
            self.text()
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    impl RecordIo for MemIo {
        fn append(&self, line: &[u8]) -> io::Result<()> {
            let mut b = self.0.lock().unwrap();
            if b.last().is_some_and(|c| *c != b'\n') {
                b.push(b'\n');
            }
            b.extend_from_slice(line);
            Ok(())
        }
        fn read_tail(&self, max_bytes: usize) -> io::Result<Vec<u8>> {
            let b = self.0.lock().unwrap();
            let start = b.len().saturating_sub(max_bytes);
            let mut out = b[start..].to_vec();
            if start > 0 {
                match out.iter().position(|c| *c == b'\n') {
                    Some(i) => {
                        out.drain(..=i);
                    }
                    None => out.clear(),
                }
            }
            Ok(out)
        }
        fn len(&self) -> io::Result<u64> {
            Ok(self.0.lock().unwrap().len() as u64)
        }
        fn replace(&self, content: &[u8]) -> io::Result<()> {
            *self.0.lock().unwrap() = content.to_vec();
            Ok(())
        }
    }

    struct SharedClock(Arc<ManualClock>);
    impl Clock for SharedClock {
        fn now_ms(&self) -> i64 {
            self.0.now_ms()
        }
    }

    fn writer(io: &MemIo, clock: &Arc<ManualClock>, limits: RecordLimits) -> RunRecordWriter {
        RunRecordWriter::new(
            Box::new(io.clone()),
            Box::new(SharedClock(clock.clone())),
            Box::new(|t: &str| t.to_string()),
            limits,
        )
    }

    fn input<'a>(
        component: Component,
        event: &'a str,
        outcome: Outcome,
        reason: &'a str,
    ) -> RecordInput<'a> {
        RecordInput {
            run_id: None,
            trigger: Trigger::Timer,
            component,
            event,
            outcome,
            reason,
        }
    }

    #[test]
    fn a_record_has_the_documented_fields_and_nothing_secret() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(1_760_000_000_000));
        let w = writer(&io, &clock, RecordLimits::default());
        let r = w
            .record(&RecordInput {
                run_id: Some("startup-1"),
                trigger: Trigger::Startup,
                component: Component::Dsh,
                event: "probe",
                outcome: Outcome::Failed,
                reason: "connection refused",
            })
            .unwrap();
        assert_eq!(r, RecordResult::Written(1));
        let lines = io.lines();
        assert_eq!(lines.len(), 1);
        let obj = lines[0].as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "component",
                "event",
                "outcome",
                "reason",
                "runId",
                "seq",
                "trigger",
                "ts",
                "v"
            ]
        );
        assert_eq!(obj["runId"], "startup-1");
        assert_eq!(obj["trigger"], "startup");
        assert_eq!(obj["component"], "dsh");
        assert_eq!(obj["outcome"], "failed");
        assert_eq!(obj["ts"], 1_760_000_000_000i64);
        assert_eq!(obj["v"], 1);
    }

    #[test]
    fn default_run_id_is_the_process_run_id() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(255));
        let w = writer(&io, &clock, RecordLimits::default());
        assert_eq!(w.process_run_id(), "run-ff");
        w.record(&input(Component::Onebot, "connect", Outcome::Ok, ""))
            .unwrap();
        assert_eq!(io.lines()[0]["runId"], "run-ff");
    }

    #[test]
    fn reasons_are_redacted_scrubbed_and_cut() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(1));
        let w = RunRecordWriter::new(
            Box::new(io.clone()),
            Box::new(SharedClock(clock.clone())),
            Box::new(|t: &str| t.replace("hunter2", "***")),
            RecordLimits::default(),
        );
        let nasty = "failed: password=hunter2 cookie dsh-auth-AbC=v1.eyJ.sig Authorization: Bearer aaaaaaaaaaaa at /root/her/her/config.json and C:\\qqbot\\config.json\nnext\tline token=zzzzzzzz sid=abcdefabcdefabcdefabcdefabcdefab0123";
        w.record(&input(Component::Dsh, "x", Outcome::Failed, nasty))
            .unwrap();
        let text = io.text();
        for secret in [
            "hunter2",
            "AbC",
            "aaaaaaaaaaaa",
            "/root/her",
            "C:\\\\qqbot",
            "zzzzzzzz",
            "abcdefabcdefabcdefabcdefabcdefab0123",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(
            io.text().matches('\n').count(),
            1,
            "one line per record, newlines in the reason cannot split it"
        );
        let long = "x ".repeat(1000);
        w.record(&input(Component::Dsh, "y", Outcome::Failed, &long))
            .unwrap();
        assert!(io.lines()[1]["reason"].as_str().unwrap().chars().count() <= MAX_REASON_CHARS);
    }

    #[test]
    fn event_and_run_id_are_slugged() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(1));
        let w = writer(&io, &clock, RecordLimits::default());
        w.record(&RecordInput {
            run_id: Some("a b/../c"),
            trigger: Trigger::Manual,
            component: Component::Queue,
            event: "Flush Queue/ \"x\"",
            outcome: Outcome::Skipped,
            reason: "",
        })
        .unwrap();
        let l = &io.lines()[0];
        assert_eq!(l["event"], "flush-queue---x-");
        assert_eq!(l["runId"], "a-b-..-c");
    }

    #[test]
    fn identical_repeats_collapse_into_one_line_and_a_counter() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(1_000_000));
        let w = writer(&io, &clock, RecordLimits::default());
        let down = input(
            Component::Dsh,
            "probe",
            Outcome::Failed,
            "connection refused (attempt 1)",
        );
        assert_eq!(w.record(&down).unwrap(), RecordResult::Written(1));
        for i in 1..=100 {
            clock.advance(5_000);
            let mut again = down.clone();
            let reason = format!("connection refused (attempt {})", i + 1);
            again.reason = &reason;
            assert_eq!(
                w.record(&again).unwrap(),
                RecordResult::Collapsed,
                "number-only differences collapse"
            );
        }
        assert_eq!(
            io.lines().len(),
            1,
            "a long outage is one line while it lasts"
        );
        // recovery: a different record publishes the counter first, then itself
        clock.advance(5_000);
        w.record(&input(Component::Dsh, "probe", Outcome::Ok, ""))
            .unwrap();
        let lines = io.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1]["repeat"], 100);
        assert_eq!(lines[1]["firstTs"], 1_005_000);
        assert_eq!(lines[1]["lastTs"], 1_500_000);
        assert_eq!(lines[1]["outcome"], "failed");
        assert_eq!(lines[2]["outcome"], "ok");
        let seqs: Vec<u64> = lines.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn identical_failures_in_distinct_explicit_runs_remain_individually_visible() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(1_000_000));
        let w = writer(&io, &clock, RecordLimits::default());
        let mut record = input(Component::Process, "task-failed", Outcome::Failed, "exit code 7");
        record.run_id = Some("run-one");
        assert!(matches!(w.record(&record).unwrap(), RecordResult::Written(_)));
        record.run_id = Some("run-two");
        assert!(matches!(w.record(&record).unwrap(), RecordResult::Written(_)), "distinct runs need their own record");
        let lines = io.lines();
        assert_eq!(lines.iter().map(|line| line["runId"].as_str().unwrap()).collect::<Vec<_>>(), ["run-one", "run-two"]);
    }

    #[test]
    fn collapse_window_ends_with_a_summary_and_a_fresh_line() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let w = writer(
            &io,
            &clock,
            RecordLimits {
                collapse_window_ms: 60_000,
                ..RecordLimits::default()
            },
        );
        let a = input(Component::Onebot, "connect", Outcome::Failed, "refused");
        w.record(&a).unwrap();
        clock.advance(10_000);
        assert_eq!(w.record(&a).unwrap(), RecordResult::Collapsed);
        clock.advance(60_000);
        assert!(matches!(w.record(&a).unwrap(), RecordResult::Written(_)));
        let lines = io.lines();
        assert_eq!(
            lines.len(),
            3,
            "first, summary of the one repeat, fresh line: {lines:?}"
        );
        assert_eq!(lines[1]["repeat"], 1);
        assert!(lines[2].get("repeat").is_none());
    }

    #[test]
    fn flush_publishes_counters_and_is_idempotent() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let w = writer(&io, &clock, RecordLimits::default());
        let a = input(Component::Alert, "send", Outcome::Skipped, "key-window");
        w.record(&a).unwrap();
        w.record(&a).unwrap();
        w.record(&a).unwrap();
        assert_eq!(w.flush().unwrap(), 1);
        assert_eq!(w.flush().unwrap(), 0, "nothing new to publish");
        assert_eq!(io.lines()[1]["repeat"], 2);
    }

    #[test]
    fn different_components_do_not_collapse() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let w = writer(&io, &clock, RecordLimits::default());
        assert!(matches!(
            w.record(&input(Component::Dsh, "probe", Outcome::Failed, "refused"))
                .unwrap(),
            RecordResult::Written(_)
        ));
        assert!(matches!(
            w.record(&input(
                Component::Onebot,
                "probe",
                Outcome::Failed,
                "refused"
            ))
            .unwrap(),
            RecordResult::Written(_)
        ));
    }

    #[test]
    fn the_file_is_compacted_to_its_cap_and_keeps_the_newest_records() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let limits = RecordLimits {
            max_bytes: 4000,
            keep_bytes: 2500,
            collapse_window_ms: 0,
            max_pending: 8,
        };
        let w = writer(&io, &clock, limits);
        for i in 0..200 {
            clock.advance(1);
            w.record(&input(
                Component::Queue,
                "replay",
                Outcome::Failed,
                &format!(
                    "distinct reason {}",
                    "abcdefghij".repeat(i % 5 + 1) + &i.to_string()
                ),
            ))
            .unwrap();
        }
        assert!(
            io.0.lock().unwrap().len() as u64 <= 4000 + 400,
            "bounded: {}",
            io.0.lock().unwrap().len()
        );
        let lines = io.lines();
        assert_eq!(lines.last().unwrap()["seq"], 200, "newest record survives");
        assert!(
            lines.first().unwrap()["seq"].as_u64().unwrap() > 100,
            "oldest were dropped"
        );
        let seqs: Vec<u64> = lines.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "order kept");
    }

    #[test]
    fn a_torn_last_line_is_skipped_by_the_reader_and_not_glued_to_the_next_record() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let w = writer(&io, &clock, RecordLimits::default());
        w.record(&input(Component::Dsh, "a", Outcome::Failed, "one"))
            .unwrap();
        io.0.lock()
            .unwrap()
            .extend_from_slice(br#"{"v":1,"seq":2,"ts":5,"runId":"r","trig"#);
        w.record(&input(Component::Dsh, "b", Outcome::Failed, "two"))
            .unwrap();
        let page = read_page(&io, &Query::default(), 1 << 20).unwrap();
        assert_eq!(
            page.records
                .iter()
                .map(|r| r.event.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
        assert_eq!(page.skipped, 1, "the torn line is counted, not fatal");
        assert!(page.records[0].seq > page.records[1].seq);
    }

    #[test]
    fn sequence_numbers_continue_after_a_restart_and_after_another_writer() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let first = writer(&io, &clock, RecordLimits::default());
        first
            .record(&input(Component::Dsh, "a", Outcome::Failed, "x"))
            .unwrap();
        first
            .record(&input(Component::Dsh, "b", Outcome::Failed, "y"))
            .unwrap();
        let second = writer(&io, &clock, RecordLimits::default()); // a restarted (or second) process
        assert_eq!(
            second
                .record(&input(Component::Dsh, "c", Outcome::Failed, "z"))
                .unwrap(),
            RecordResult::Written(3)
        );
        assert_eq!(
            first
                .record(&input(Component::Dsh, "d", Outcome::Failed, "w"))
                .unwrap(),
            RecordResult::Written(4)
        );
    }

    #[test]
    fn pages_are_newest_first_filtered_and_bounded() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let w = writer(
            &io,
            &clock,
            RecordLimits {
                collapse_window_ms: 0,
                ..RecordLimits::default()
            },
        );
        for i in 0..30 {
            clock.advance(1);
            let comp = if i % 3 == 0 {
                Component::Onebot
            } else {
                Component::Dsh
            };
            w.record(&input(
                comp,
                "e",
                if i % 2 == 0 {
                    Outcome::Ok
                } else {
                    Outcome::Failed
                },
                &format!("r{i}"),
            ))
            .unwrap();
        }
        let p1 = read_page(
            &io,
            &Query {
                limit: Some(10),
                ..Query::default()
            },
            1 << 20,
        )
        .unwrap();
        assert_eq!(p1.records.len(), 10);
        assert_eq!(p1.records[0].seq, 30);
        let p2 = read_page(
            &io,
            &Query {
                limit: Some(10),
                before_seq: p1.next_before,
                ..Query::default()
            },
            1 << 20,
        )
        .unwrap();
        assert_eq!(p2.records[0].seq, 20);
        let p3 = read_page(
            &io,
            &Query {
                limit: Some(10),
                before_seq: p2.next_before,
                ..Query::default()
            },
            1 << 20,
        )
        .unwrap();
        assert_eq!(
            (p3.records.len(), p3.next_before),
            (10, None),
            "last page has no cursor"
        );
        let only_dsh_failed = read_page(
            &io,
            &Query {
                component: Some(Component::Dsh),
                outcome: Some(Outcome::Failed),
                limit: Some(500),
                ..Query::default()
            },
            1 << 20,
        )
        .unwrap();
        assert!(only_dsh_failed
            .records
            .iter()
            .all(|r| r.component == Component::Dsh && r.outcome == Outcome::Failed));
        assert!(only_dsh_failed.records.len() <= MAX_PAGE);
        let zero = read_page(
            &io,
            &Query {
                limit: Some(0),
                ..Query::default()
            },
            1 << 20,
        )
        .unwrap();
        assert_eq!(zero.records.len(), 1, "limit is clamped to at least 1");
        let json = p1.to_json();
        assert_eq!(json["records"].as_array().unwrap().len(), 10);
        assert_eq!(json["nextBefore"], p1.next_before.unwrap());
    }

    #[test]
    fn reading_is_bounded_by_the_read_cap() {
        let io = MemIo::default();
        let clock = Arc::new(ManualClock::new(0));
        let w = writer(
            &io,
            &clock,
            RecordLimits {
                collapse_window_ms: 0,
                max_bytes: u64::MAX,
                ..RecordLimits::default()
            },
        );
        for i in 0..100 {
            w.record(&input(
                Component::Dsh,
                "e",
                Outcome::Failed,
                &format!("reason number {i} {}", "z".repeat(10)),
            ))
            .unwrap();
        }
        let small = read_page(
            &io,
            &Query {
                limit: Some(200),
                ..Query::default()
            },
            2000,
        )
        .unwrap();
        assert!(
            small.records.len() < 100 && !small.records.is_empty(),
            "only the tail within the cap is read: {}",
            small.records.len()
        );
        assert_eq!(small.records[0].seq, 100);
    }

    #[test]
    fn unknown_future_versions_are_skipped_not_misread() {
        let io = MemIo::default();
        io.append(br#"{"v":2,"seq":1,"ts":1,"runId":"r","trigger":"timer","component":"dsh","event":"e","outcome":"ok","reason":""}"#).unwrap();
        io.append(b"\n").unwrap();
        let page = read_page(&io, &Query::default(), 1 << 20).unwrap();
        assert_eq!((page.records.len(), page.skipped), (0, 1));
    }

    #[test]
    fn file_io_round_trip_with_newline_guard_and_replace() {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "run-record-test-{}-{}",
            std::process::id(),
            crate::parity_clock::now_ms()
        )));
        let path = dir.join("nested").join(FILE_NAME);
        let io = FileIo::new(&path);
        assert_eq!(io.len().unwrap(), 0);
        assert!(
            io.read_tail(100).unwrap().is_empty(),
            "missing file reads as empty"
        );
        io.append(b"{\"a\":1}\n").unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"torn\"")
            .unwrap();
        io.append(b"{\"b\":2}\n").unwrap();
        let text = String::from_utf8(io.read_tail(1000).unwrap()).unwrap();
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            vec!["{\"a\":1}", "{\"torn\"", "{\"b\":2}"]
        );
        let tail = String::from_utf8(io.read_tail(12).unwrap()).unwrap();
        assert_eq!(tail, "{\"b\":2}\n", "a tail read starts at a line boundary");
        io.replace(b"{\"c\":3}\n").unwrap();
        assert_eq!(
            String::from_utf8(io.read_tail(1000).unwrap()).unwrap(),
            "{\"c\":3}\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writer_works_on_a_real_file_end_to_end() {
        let dir = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "run-record-e2e-{}-{}",
            std::process::id(),
            crate::parity_clock::now_ms()
        )));
        let io = FileIo::new(dir.join(FILE_NAME));
        let clock = Arc::new(ManualClock::new(42));
        let w = RunRecordWriter::new(
            Box::new(FileIo::new(dir.join(FILE_NAME))),
            Box::new(SharedClock(clock.clone())),
            Box::new(|t: &str| t.to_string()),
            RecordLimits::default(),
        );
        w.record(&input(
            Component::Process,
            "startup",
            Outcome::Failed,
            "config missing",
        ))
        .unwrap();
        let page = read_page(&io, &Query::default(), 1 << 20).unwrap();
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].reason, "config missing");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
