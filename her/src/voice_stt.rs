//! Inbound voice transcription (#170): a member's QQ voice record becomes text the model reads, on this host only.
//!
//! A conversation opts in through `voiceTranscription.conversations` in `config.json` (default: none). For such a
//! conversation, a message whose segment array holds exactly one `record` segment with an http(s) `data.url` is
//! downloaded through `safe_fetch::fetch_voice` (public pinned addresses, at most two revalidated redirects, the
//! `maxBytes` cap, SILK v3 magic; the body and header files go to `<data>/voice/transfer` and are removed after each
//! hop; all hops together end within the download's share of the attempt, `HELPER_RESERVE` before the deadline),
//! checked as whole SILK v3 framing (every packet holds at least one 20 ms frame, so a packet count above 50 a second
//! proves a record longer than `maxSeconds` and refuses it) and posted as it is to the local transcription helper at
//! `voiceTranscription.helper`, which must be a loopback IP origin (`HttpTranscriber::new`). The helper's text is
//! cleaned (`clean`) and shown as `[语音转文字] <text>` in place of the message's `[语音]` label (`apply`), so it is
//! stored and filtered by `model_view` like typed text; the activity log keeps `[语音]`. Anything else (not opted in,
//! no helper, a failure, the deadline, the pause after a helper failure, another transcription running) leaves `[语音]`
//! and the message goes on.
//!
//! Each transcript is kept in `<data>/voice/transcripts.json` under its inbound key for a day, at most `KEEP_MAX`, so
//! handling the same event again (a replay after a restart) never transcribes it twice. Audio bytes are never logged,
//! stored or written outside the transfer folder; each failure records a fixed reason in the run record (component
//! `stt`). The helper's absence or failure is told to the owner once per outage (`admin_alert::notify_notice`, key
//! `voice-stt-down`; an outage notice the limiter refused is tried again on the next failure), and its recovery once
//! (key `voice-stt-up`). QQ's own transcription (`fetch_ptt_text`) is never used for inbound voice.

use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The label a transcript is shown with, in place of `VOICE_LABEL`.
pub(crate) const LABEL: &str = "[语音转文字]";
/// How an inbound voice record renders without a transcript (`chat::segments_to_text`).
const VOICE_LABEL: &str = "[语音]";
/// The helper's transcription route.
const PATH: &str = "/v1/transcribe";
/// Most characters of a transcript the model reads; a longer one is cut and ends with `…`.
pub(crate) const MAX_TRANSCRIPT_CHARS: usize = 600;
/// Most bytes of the helper's JSON answer.
const REPLY_MAX_BYTES: usize = 16 * 1024;
/// Most bytes of the helper's response head.
const HEAD_MAX_BYTES: usize = 8 * 1024;
/// After a helper failure or a missed deadline, no transcription starts for this long.
const COOL: Duration = Duration::from_secs(60);
/// How much longer than `timeoutMs` the caller waits for a transcription that overran its own deadline.
const GRACE: Duration = Duration::from_millis(500);
/// The kept transcripts (data area; not top level, so state backups never take it).
const STORE: &str = "voice/transcripts.json";
/// Serializes the writes of `STORE`. Never replaced or unlinked.
const STORE_LOCK: &str = "locks/voice-transcripts.lock";
const STORE_LOCK_WAIT: Duration = Duration::from_millis(250);
/// How long a transcript is kept, and how many at most.
pub(crate) const KEEP_MS: i64 = 24 * 60 * 60 * 1000;
pub(crate) const KEEP_MAX: usize = 2000;
/// Where the download's body and header files are written (`safe_fetch::fetch_voice`).
const TRANSFER_DIR: &str = "voice/transfer";
/// The owner notice keys (each one per minute, on the notice budget): an outage and its recovery have their own, so a
/// recovery never uses up the window of the next outage's notice.
const DOWN_KEY: &str = "voice-stt-down";
const UP_KEY: &str = "voice-stt-up";
/// The share of one attempt's time kept for the helper: the download ends this long before the deadline (at most half
/// of the attempt, so a short `timeoutMs` still leaves the download a share).
const HELPER_RESERVE: Duration = Duration::from_secs(3);
/// The helper is asked only when at least this much time is left; with less, a silent helper proves nothing and the
/// attempt ends as `Failure::Deadline`.
const HELPER_FLOOR: Duration = Duration::from_millis(250);
/// The most bytes one write to the helper carries; the write timeout is reset to the time left before each.
const WRITE_CHUNK: usize = 16 * 1024;

/// The size and length limits of one record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Limits {
    /// `maxBytes`: default 1 MiB, clamped to 64 KiB–4 MiB.
    pub(crate) max_bytes: usize,
    /// `maxSeconds`: default 60, clamped to 1–60.
    pub(crate) max_seconds: u64,
}

/// What `voiceTranscription` in `config.json` sets. Every member is optional; without the section nothing is
/// transcribed.
pub(crate) struct Settings {
    /// `conversations`: the conversation keys (`group:<id>`, `private:<id>`) whose voice is transcribed.
    conversations: Vec<String>,
    /// `helper`: the helper's origin as written; `HttpTranscriber::new` accepts only a loopback IP origin.
    helper: Option<String>,
    pub(crate) limits: Limits,
    /// `timeoutMs`: default 8000, clamped to 1000–20000. The whole attempt (download and helper) ends within it.
    pub(crate) timeout: Duration,
}

impl Settings {
    pub(crate) fn from_config(cfg: &Value) -> Self {
        let section = cfg.get("voiceTranscription");
        let number = |name: &str, default: u64, low: u64, high: u64| {
            section.and_then(|s| s.get(name)).and_then(Value::as_u64).map_or(default, |n| n.clamp(low, high))
        };
        let conversations = section
            .and_then(|s| s.get("conversations"))
            .and_then(Value::as_array)
            .map(|keys| keys.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let helper = section.and_then(|s| s.get("helper")).and_then(Value::as_str).map(str::to_string);
        Self {
            conversations,
            helper,
            limits: Limits {
                max_bytes: number("maxBytes", 1_048_576, 65_536, 4_194_304) as usize,
                max_seconds: number("maxSeconds", 60, 1, 60),
            },
            timeout: Duration::from_millis(number("timeoutMs", 8_000, 1_000, 20_000)),
        }
    }

    pub(crate) fn applies_to(&self, key: &str) -> bool {
        self.conversations.iter().any(|k| k == key)
    }
}

/// Why a record has no transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// `helper` is missing or not a loopback IP origin.
    NotConfigured,
    /// The helper did not take the connection or the request.
    Unreachable,
    /// The helper did not answer before the deadline.
    TimedOut,
    /// The helper answered with a status other than 200.
    Refused,
    /// The helper's answer was not a bounded `Content-Length` JSON object with a string `text`.
    InvalidReply,
    /// The record could not be downloaded (address, transfer, size cap or magic).
    Download,
    /// The record is not whole SILK v3 framing.
    Unsupported,
    /// The record's packets prove it longer than `maxSeconds`.
    TooLong,
    /// Nothing was left of the helper's text.
    Empty,
    /// Another transcription of this runtime was running.
    Busy,
    /// Transcription is paused after a helper failure or a missed deadline.
    Cooling,
    /// The attempt's time ran out (`timeoutMs` plus `GRACE`, or too little left to ask the helper): not the helper's
    /// fault.
    Deadline,
    /// The transcription task ended without a result.
    Internal,
}

impl Failure {
    /// The helper is missing or failed: the owner hears of it and transcription pauses (`COOL`).
    pub(crate) fn helper_down(self) -> bool {
        matches!(self, Self::NotConfigured | Self::Unreachable | Self::TimedOut | Self::Refused | Self::InvalidReply)
    }

    /// Nothing went wrong: the message was not one to transcribe now.
    fn skipped(self) -> bool {
        matches!(self, Self::Unsupported | Self::TooLong | Self::Empty | Self::Busy | Self::Cooling)
    }

    /// The run-record reason: fixed text, never an address, a path or audio.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NotConfigured => "voiceTranscription.helper is missing or not a loopback http origin",
            Self::Unreachable => "the transcription helper did not take the request",
            Self::TimedOut => "the transcription helper did not answer in time",
            Self::Refused => "the transcription helper refused the request",
            Self::InvalidReply => "the transcription helper's answer was not valid",
            Self::Download => "the voice record could not be downloaded",
            Self::Unsupported => "the voice record is not SILK v3",
            Self::TooLong => "the voice record is longer than voiceTranscription.maxSeconds",
            Self::Empty => "the transcript was empty",
            Self::Busy => "another transcription was running",
            Self::Cooling => "transcription is paused for a minute after a helper failure",
            Self::Deadline => "the transcription did not finish before its deadline",
            Self::Internal => "the transcription task ended without a result",
        }
    }

    /// What the owner notice says about a helper failure (`helper_down`).
    fn owner_reason(self) -> &'static str {
        match self {
            Self::NotConfigured => "尚未配置本地语音识别服务地址（请在配置中填入本机回环服务地址）",
            Self::Unreachable => "本机转写引擎连接不上",
            Self::TimedOut => "本机转写引擎没有按时回复",
            Self::Refused => "本机转写引擎拒绝了请求",
            Self::InvalidReply => "本机转写引擎的回复无效",
            _ => "转写失败",
        }
    }
}

/// The engine seam: the transcript of `audio` (SILK v3 bytes) before `deadline`.
pub(crate) trait Transcriber {
    fn transcribe(&self, audio: &[u8], deadline: Instant) -> Result<String, Failure>;
}

/// The local helper over HTTP/1.1: `POST /v1/transcribe` with the record as `application/octet-stream`; it answers
/// `200` with a `Content-Length` JSON object (at most `REPLY_MAX_BYTES`) whose string `text` is the transcript. Other
/// members are ignored.
pub(crate) struct HttpTranscriber {
    address: std::net::SocketAddr,
    authority: String,
}

impl HttpTranscriber {
    /// The helper at `origin`, which must be `http://<loopback IP literal>:<port>` with nothing after it
    /// (`runtime::is_loopback_http_endpoint`): member audio goes to this host only, and no name is resolved.
    pub(crate) fn new(origin: &str) -> Option<Self> {
        if !crate::runtime::is_loopback_http_endpoint(origin) {
            return None;
        }
        let authority = origin.strip_prefix("http://")?;
        let address: std::net::SocketAddr = authority.parse().ok()?;
        Some(Self { address, authority: authority.to_string() })
    }
}

impl Transcriber for HttpTranscriber {
    fn transcribe(&self, audio: &[u8], deadline: Instant) -> Result<String, Failure> {
        use std::io::Read;
        let left = || remaining(deadline).ok_or(Failure::TimedOut);
        let mut stream = std::net::TcpStream::connect_timeout(&self.address, left()?).map_err(io_failure)?;
        let head = format!(
            "POST {PATH} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.authority,
            audio.len()
        );
        write_bounded(&mut stream, head.as_bytes(), deadline)?;
        write_bounded(&mut stream, audio, deadline)?;
        // Every read gets only the time left, so a peer that dribbles bytes cannot extend the deadline.
        let mut response = Vec::new();
        let mut chunk = [0u8; 4096];
        while !reply_complete(&response) {
            stream.set_read_timeout(Some(left()?)).map_err(|_| Failure::Unreachable)?;
            let read = stream.read(&mut chunk).map_err(io_failure)?;
            if read == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..read]);
            if response.len() > HEAD_MAX_BYTES + REPLY_MAX_BYTES {
                return Err(Failure::InvalidReply);
            }
        }
        parse_reply(&response)
    }
}

/// Write all of `bytes` before `deadline`: in chunks of at most `WRITE_CHUNK`, each with the write timeout set to the
/// time left, so a helper that reads slowly cannot extend the deadline by taking a little at a time. Spent time is
/// `TimedOut`.
fn write_bounded(stream: &mut std::net::TcpStream, bytes: &[u8], deadline: Instant) -> Result<(), Failure> {
    use std::io::Write;
    let mut sent = 0;
    while sent < bytes.len() {
        let left = remaining(deadline).ok_or(Failure::TimedOut)?;
        stream.set_write_timeout(Some(left)).map_err(|_| Failure::Unreachable)?;
        let end = bytes.len().min(sent + WRITE_CHUNK);
        match stream.write(&bytes[sent..end]) {
            Ok(0) => return Err(Failure::Unreachable),
            Ok(written) => sent += written,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_failure(error)),
        }
    }
    Ok(())
}

/// A socket error as a failure: a timeout (Unix `WouldBlock`, Windows `TimedOut`) is `TimedOut`, anything else
/// `Unreachable`.
fn io_failure(error: std::io::Error) -> Failure {
    match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Failure::TimedOut,
        _ => Failure::Unreachable,
    }
}

/// The time left before `deadline`, `None` once it has passed.
fn remaining(deadline: Instant) -> Option<Duration> {
    deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())
}

fn head_end(response: &[u8]) -> Option<usize> {
    response.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Whether `response` holds a whole head and the `Content-Length` body it announces.
fn reply_complete(response: &[u8]) -> bool {
    let Some(end) = head_end(response) else { return false };
    let head = String::from_utf8_lossy(&response[..end]);
    let length = head.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
    });
    length.is_some_and(|length| response.len() >= end + 4 + length)
}

/// The helper's answer: `HTTP/1.x 200`, a `Content-Length` of at most `REPLY_MAX_BYTES` (no transfer encoding) and a
/// JSON object whose `text` is a string.
fn parse_reply(response: &[u8]) -> Result<String, Failure> {
    let end = head_end(response).ok_or(Failure::InvalidReply)?;
    let head = std::str::from_utf8(&response[..end]).map_err(|_| Failure::InvalidReply)?;
    let mut lines = head.split("\r\n");
    let mut status = lines.next().unwrap_or("").split_whitespace();
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(Failure::InvalidReply);
    }
    if status.next() != Some("200") {
        return Err(Failure::Refused);
    }
    let mut length = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(Failure::InvalidReply)?;
        if name.trim().eq_ignore_ascii_case("transfer-encoding") {
            return Err(Failure::InvalidReply);
        }
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse::<usize>().map_err(|_| Failure::InvalidReply)?);
        }
    }
    let length = length.filter(|length| *length <= REPLY_MAX_BYTES).ok_or(Failure::InvalidReply)?;
    let body = response.get(end + 4..end + 4 + length).ok_or(Failure::InvalidReply)?;
    let value: Value = serde_json::from_slice(body).map_err(|_| Failure::InvalidReply)?;
    value.get("text").and_then(Value::as_str).map(str::to_string).ok_or(Failure::InvalidReply)
}

/// The `data.url` of the message's voice record, when the message is a segment array holding exactly one `record`
/// segment and its URL is http(s).
pub(crate) fn record_url(message: &Value) -> Option<String> {
    let records: Vec<&Value> =
        message.as_array()?.iter().filter(|segment| segment.get("type").and_then(Value::as_str) == Some("record")).collect();
    let [record] = records.as_slice() else { return None };
    let url = record.pointer("/data/url").and_then(Value::as_str)?.trim();
    crate::safe_fetch::parse_url(url).map(|_| url.to_string())
}

/// The packet count of whole SILK v3 framing (an optional leading 0x02, `#!SILK_V3`, little-endian length-prefixed
/// packets, an optional final -1), or `None` when the bytes are not that or hold no packet. A zero-length (DTX) packet
/// counts.
pub(crate) fn silk_packets(bytes: &[u8]) -> Option<u64> {
    let body = if bytes.first() == Some(&0x02) { &bytes[1..] } else { bytes };
    let mut rest = body.strip_prefix(b"#!SILK_V3")?;
    let mut packets = 0u64;
    while !rest.is_empty() {
        if rest.len() < 2 {
            return None;
        }
        let length = i16::from_le_bytes([rest[0], rest[1]]);
        rest = &rest[2..];
        if length == -1 && rest.is_empty() {
            break;
        }
        if length < 0 || rest.len() < length as usize {
            return None;
        }
        rest = &rest[length as usize..];
        packets += 1;
    }
    (packets > 0).then_some(packets)
}

/// The transcript the model may read: the helper's tags (`<|zh|>`, `<|NEUTRAL|>`, ...) removed, control characters
/// (line breaks included) as spaces, square brackets full-width and lenticular brackets tortoise-shell (so it never
/// forms a `[CQ:...]`, `[语音]`-style or `【会话令牌】` atom of `model_view`), runs of spaces as one, trimmed, and cut to
/// `MAX_TRANSCRIPT_CHARS` characters followed by `…`. `None` when nothing is left.
pub(crate) fn clean(text: &str) -> Option<String> {
    static TAGS: OnceLock<regex::Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| regex::Regex::new(r"<\|[^|>]{0,64}\|>").unwrap());
    let mapped: String = tags
        .replace_all(text, " ")
        .chars()
        .map(|c| match c {
            '[' => '［',
            ']' => '］',
            '【' => '〔',
            '】' => '〕',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let joined = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        return None;
    }
    if joined.chars().count() <= MAX_TRANSCRIPT_CHARS {
        return Some(joined);
    }
    Some(format!("{}…", joined.chars().take(MAX_TRANSCRIPT_CHARS).collect::<String>()))
}

/// How a transcript is shown.
pub(crate) fn label(transcript: &str) -> String {
    format!("{LABEL} {transcript}")
}

/// `rendered` with its last `[语音]` (the message's own record; a quote renders before it) replaced by `label`.
pub(crate) fn apply(rendered: &str, label: &str) -> String {
    match rendered.rfind(VOICE_LABEL) {
        Some(at) => format!("{}{label}{}", &rendered[..at], &rendered[at + VOICE_LABEL.len()..]),
        None => rendered.to_string(),
    }
}

/// `<data>/voice/transfer`, created when missing through the runtime's path rules; leftovers of an earlier transfer
/// (`her-fetch-*`, from a crash mid-download) are removed first.
pub(crate) fn transfer_folder(context: &RuntimeContext) -> Result<PathBuf, Failure> {
    let folder =
        context.resolve_path(RuntimeArea::Data, TRANSFER_DIR, PathIntent::CreateDirectory).map_err(|_| Failure::Download)?;
    for entry in std::fs::read_dir(&folder).map_err(|_| Failure::Download)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("her-fetch-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(folder)
}

#[cfg(not(test))]
fn download(context: &RuntimeContext, url: &str, max_bytes: usize, budget: Duration) -> Result<Vec<u8>, Failure> {
    let _ = transfer_folder(context)?;
    crate::safe_fetch::fetch_voice(url, max_bytes, budget).map(|fetched| fetched.bytes).map_err(|_| Failure::Download)
}

/// Unit tests' records by URL; no test downloads anything.
#[cfg(test)]
pub(crate) static DOWNLOAD_FIXTURES: Mutex<std::collections::BTreeMap<String, Vec<u8>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Unit tests' slow downloads by URL, and the budget each download was given.
#[cfg(test)]
pub(crate) static DOWNLOAD_DELAYS: Mutex<std::collections::BTreeMap<String, DownloadDelay>> =
    Mutex::new(std::collections::BTreeMap::new());
#[cfg(test)]
pub(crate) static DOWNLOAD_BUDGETS: Mutex<std::collections::BTreeMap<String, Duration>> =
    Mutex::new(std::collections::BTreeMap::new());

/// How long a slow test download takes before it hands over its record.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum DownloadDelay {
    /// Exactly the budget it was given: a download that uses up its share.
    Share,
    /// This long, whatever the budget: a download that overshoots (a host lookup is not capped by the budget).
    Fixed(Duration),
}

#[cfg(test)]
fn download(context: &RuntimeContext, url: &str, max_bytes: usize, budget: Duration) -> Result<Vec<u8>, Failure> {
    DOWNLOAD_BUDGETS.lock().unwrap_or_else(|p| p.into_inner()).insert(url.to_string(), budget);
    let delay = DOWNLOAD_DELAYS.lock().unwrap_or_else(|p| p.into_inner()).get(url).copied();
    match delay {
        Some(DownloadDelay::Share) => std::thread::sleep(budget),
        Some(DownloadDelay::Fixed(fixed)) => std::thread::sleep(fixed),
        None => {}
    }
    transfer_folder(context)?;
    let bytes =
        DOWNLOAD_FIXTURES.lock().unwrap_or_else(|p| p.into_inner()).get(url).cloned().ok_or(Failure::Download)?;
    if bytes.len() > max_bytes {
        return Err(Failure::Download);
    }
    Ok(bytes)
}

/// The part of an attempt of `total` the helper keeps for itself (`HELPER_RESERVE`, at most half).
fn helper_share(total: Duration) -> Duration {
    HELPER_RESERVE.min(total / 2)
}

/// Whether the helper may still be asked at `now`: at least `HELPER_FLOOR` before `deadline`.
fn helper_has_time(deadline: Instant, now: Instant) -> bool {
    deadline.checked_duration_since(now).is_some_and(|left| left >= HELPER_FLOOR)
}

/// Download, check and transcribe one record before `deadline` (blocking). The download ends `helper_share` before
/// the deadline, so a slow host cannot leave the helper no time; when the download still overruns and leaves the helper
/// less than `HELPER_FLOOR`, the attempt ends as `Failure::Deadline`, which is no helper outage. The transcript is
/// `clean`ed.
pub(crate) fn transcribe_record(
    context: &RuntimeContext,
    url: &str,
    limits: Limits,
    engine: &dyn Transcriber,
    deadline: Instant,
) -> Result<String, Failure> {
    let total = remaining(deadline).ok_or(Failure::Deadline)?;
    let download_deadline = deadline.checked_sub(helper_share(total)).unwrap_or(deadline);
    let budget = remaining(download_deadline).ok_or(Failure::Deadline)?;
    let audio = download(context, url, limits.max_bytes, budget)?;
    let packets = silk_packets(&audio).ok_or(Failure::Unsupported)?;
    // Every packet holds at least one 20 ms frame: more than 50 a second prove the record too long.
    if packets > limits.max_seconds.saturating_mul(50) {
        return Err(Failure::TooLong);
    }
    if !helper_has_time(deadline, Instant::now()) {
        return Err(Failure::Deadline);
    }
    let text = engine.transcribe(&audio, deadline)?;
    clean(&text).ok_or(Failure::Empty)
}

/// The transcript kept for `inbound_key`, when one younger than `KEEP_MS` is stored.
pub(crate) fn cached(context: &RuntimeContext, inbound_key: &str, now: i64) -> Option<String> {
    let text = context.read_optional_text(RuntimeArea::Data, STORE).ok().flatten()?;
    let store: Value = serde_json::from_str(&text).ok()?;
    let entry = store.get("entries")?.get(inbound_key)?;
    let at = entry.get("at")?.as_i64()?;
    if now.saturating_sub(at) >= KEEP_MS {
        return None;
    }
    entry.get("text")?.as_str().map(str::to_string)
}

/// Keep `text` for `inbound_key`, dropping entries `KEEP_MS` old or older and, past `KEEP_MAX`, the oldest. An
/// unreadable or invalid store is replaced. `STORE_LOCK` is a leaf gate: taken with no other lock held, waited for at
/// most `STORE_LOCK_WAIT`, and nothing runs under it but this store's own read and atomic write.
pub(crate) fn remember(context: &RuntimeContext, inbound_key: &str, text: &str, now: i64) -> Result<(), String> {
    let deadline = Instant::now() + STORE_LOCK_WAIT;
    let _gate = loop {
        match context.open_locked_file(RuntimeArea::Data, STORE_LOCK) {
            Ok(gate) => break gate,
            Err(error) if error == "runtime file is already locked" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    };
    let old = context
        .read_optional_text(RuntimeArea::Data, STORE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|store| store.get("entries").and_then(Value::as_object).cloned())
        .unwrap_or_default();
    let mut entries: Vec<(String, Value)> = vec![(inbound_key.to_string(), json!({"at": now, "text": text}))];
    entries.extend(old.into_iter().filter(|(key, entry)| {
        key != inbound_key
            && entry.get("text").is_some_and(Value::is_string)
            && entry.get("at").and_then(Value::as_i64).is_some_and(|at| now.saturating_sub(at) < KEEP_MS)
    }));
    // Newest first; the new entry stays first among equal times, so the cap never drops it.
    entries.sort_by_key(|(_, entry)| std::cmp::Reverse(entry["at"].as_i64().unwrap_or(i64::MIN)));
    entries.truncate(KEEP_MAX);
    let store = json!({"v": 1, "entries": entries.into_iter().collect::<Map<String, Value>>()});
    context.write_atomic(RuntimeArea::Data, STORE, store.to_string().as_bytes())
}

/// The helper's state for one runtime.
#[derive(Default)]
struct Health {
    /// The helper failed last (`Failure::helper_down`); an answer clears it.
    down: bool,
    /// The owner heard of this outage: its notice was admitted (`tell_owner`). A refused notice is tried again on the
    /// next helper failure.
    told: bool,
    /// No transcription starts before this.
    cool_until: Option<Instant>,
}

/// Per runtime root. This map and `running` are leaf locks: each is held for one map update only, never across I/O,
/// an await or another lock.
fn health() -> &'static Mutex<HashMap<PathBuf, Health>> {
    static HEALTH: OnceLock<Mutex<HashMap<PathBuf, Health>>> = OnceLock::new();
    HEALTH.get_or_init(Default::default)
}

fn running() -> &'static Mutex<HashSet<PathBuf>> {
    static RUNNING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// Whether a pause that ends at `until` still holds at `now`.
pub(crate) fn cooling(until: Option<Instant>, now: Instant) -> bool {
    until.is_some_and(|until| now < until)
}

/// The one transcription a runtime runs at a time; released when dropped.
pub(crate) struct Running(PathBuf);

impl Running {
    pub(crate) fn claim(root: &Path) -> Option<Self> {
        running().lock().unwrap_or_else(|p| p.into_inner()).insert(root.to_path_buf()).then(|| Self(root.to_path_buf()))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        running().lock().unwrap_or_else(|p| p.into_inner()).remove(&self.0);
    }
}

/// Tell the owner; whether the notice was admitted (`admin_alert::notify_notice`'s answer).
#[cfg(not(test))]
fn tell_owner(context: &RuntimeContext, cfg: &Value, text: &str, key: &str) -> bool {
    crate::admin_alert::notify_notice(context, cfg, text, key)
}

/// Unit tests' record of the admitted owner notices: runtime root, key, text and the test clock's time.
#[cfg(test)]
pub(crate) static NOTICES: Mutex<Vec<(PathBuf, String, String, i64)>> = Mutex::new(Vec::new());

/// Unit tests' notice clock by runtime root (milliseconds, 0 unless a test sets it).
#[cfg(test)]
pub(crate) static NOTICE_CLOCK: Mutex<std::collections::BTreeMap<PathBuf, i64>> = Mutex::new(std::collections::BTreeMap::new());

/// The test stand-in for the notice limiter (`admin_alert`): a key is admitted once per 60 seconds of the root's test
/// clock.
#[cfg(test)]
fn tell_owner(context: &RuntimeContext, _cfg: &Value, text: &str, key: &str) -> bool {
    let root = context.runtime_root().to_path_buf();
    let now = NOTICE_CLOCK.lock().unwrap_or_else(|p| p.into_inner()).get(&root).copied().unwrap_or(0);
    let mut told = NOTICES.lock().unwrap_or_else(|p| p.into_inner());
    if told.iter().any(|(r, k, _, at)| *r == root && k == key && now - at < 60_000) {
        return false;
    }
    told.push((root, key.to_string(), text.to_string(), now));
    true
}

/// Record a transcription's outcome: a run-record line for each failure; the helper's state; and the owner notices.
/// The outage notice goes out on a helper failure until one is admitted (`Health::told`), so a refused one is tried
/// again on the next failure; the recovery notice goes out only for an outage the owner heard of.
fn settle(context: &RuntimeContext, cfg: &Value, outcome: Result<(), Failure>) {
    let root = context.runtime_root().to_path_buf();
    let (was_down, was_told, down, told) = {
        let mut all = health().lock().unwrap_or_else(|p| p.into_inner());
        let state = all.entry(root.clone()).or_default();
        let (was_down, was_told) = (state.down, state.told);
        match outcome {
            Ok(()) | Err(Failure::Empty) => *state = Health::default(),
            Err(failure) if failure.helper_down() => {
                state.down = true;
                state.cool_until = Some(Instant::now() + COOL);
            }
            Err(Failure::Deadline) => state.cool_until = Some(Instant::now() + COOL),
            Err(_) => {}
        }
        (was_down, was_told, state.down, state.told)
    };
    if let Err(failure) = outcome {
        let kind = if failure.skipped() { Outcome::Skipped } else { Outcome::Failed };
        crate::recovery_log::record(context, Trigger::Turn, Component::Stt, "voice-transcription", kind, failure.reason());
        if down && !was_down {
            crate::recovery_log::record(
                context,
                Trigger::Turn,
                Component::Stt,
                "voice-transcription-unavailable",
                Outcome::Failed,
                failure.reason(),
            );
        }
        if failure.helper_down() && !told {
            let admitted = tell_owner(
                context,
                cfg,
                &format!("语音转文字暂不可用：{}。语音消息先以「[语音]」交给对话，恢复后自动继续。", failure.owner_reason()),
                DOWN_KEY,
            );
            if admitted {
                // Only while the outage lasts: an answer that cleared it meanwhile leaves nothing to remember.
                if let Some(state) = health().lock().unwrap_or_else(|p| p.into_inner()).get_mut(&root).filter(|state| state.down) {
                    state.told = true;
                }
            }
        }
    }
    if was_down && !down {
        crate::recovery_log::record(
            context,
            Trigger::Turn,
            Component::Stt,
            "voice-transcription-recovered",
            Outcome::Ok,
            "the transcription helper answered again",
        );
        if was_told {
            tell_owner(context, cfg, "语音转文字已恢复。", UP_KEY);
        }
    }
}

/// `[语音转文字] <transcript>` for `message`'s voice record, or `None` to keep `[语音]`: the conversation `key` is not
/// opted in, the message is not one voice record, or the transcription failed, was skipped or missed its deadline.
/// A transcript kept for `inbound_key` is used without asking the helper. Waits at most `timeoutMs` plus `GRACE`; the
/// blocking work runs on the blocking pool and keeps the runtime's one transcription slot until it ends.
pub(crate) async fn for_message(
    context: &RuntimeContext,
    cfg: &Value,
    key: &str,
    inbound_key: &str,
    message: &Value,
) -> Option<String> {
    let root = context.runtime_root().to_path_buf();
    let settings = Settings::from_config(cfg);
    if !settings.applies_to(key) {
        return None;
    }
    let url = record_url(message)?;
    if let Some(text) = cached(context, inbound_key, crate::parity_clock::now_ms()) {
        return Some(label(&text));
    }
    let Some(engine) = settings.helper.as_deref().and_then(HttpTranscriber::new) else {
        settle(context, cfg, Err(Failure::NotConfigured));
        return None;
    };
    let cool_until = health().lock().unwrap_or_else(|p| p.into_inner()).get(&root).and_then(|state| state.cool_until);
    if cooling(cool_until, Instant::now()) {
        settle(context, cfg, Err(Failure::Cooling));
        return None;
    }
    let Some(permit) = Running::claim(&root) else {
        settle(context, cfg, Err(Failure::Busy));
        return None;
    };
    let deadline = Instant::now() + settings.timeout;
    let (task_context, task_key, limits) = (context.clone(), inbound_key.to_string(), settings.limits);
    let task = tokio::task::spawn_blocking(move || -> Result<(String, bool), Failure> {
        let _permit = permit;
        let text = transcribe_record(&task_context, &url, limits, &engine, deadline)?;
        let kept = remember(&task_context, &task_key, &text, crate::parity_clock::now_ms()).is_ok();
        Ok((text, kept))
    });
    let outcome = match tokio::time::timeout(settings.timeout + GRACE, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(Failure::Internal),
        Err(_) => Err(Failure::Deadline),
    };
    if matches!(outcome, Ok((_, false))) {
        crate::recovery_log::record(
            context,
            Trigger::Turn,
            Component::Stt,
            "voice-transcript-store",
            Outcome::Failed,
            "the transcript could not be kept; a replay of this message transcribes it again",
        );
    }
    settle(context, cfg, outcome.as_ref().map(|_| ()).map_err(|failure| *failure));
    outcome.ok().map(|(text, _)| label(&text))
}

#[cfg(test)]
#[path = "voice_stt_tests.rs"]
pub(crate) mod tests;
