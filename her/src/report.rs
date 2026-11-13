//! Local, metadata-only production diagnostics. Never constructs a RuntimeContext or acquires a gate.
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Seek},
    path::{Path, PathBuf},
};

const USAGE: &str = "usage: her-cli report --runtime-root <root> [--since <duration>] [--key <conversation>] [--json] [--relay-log <path>]";

struct Args {
    root: PathBuf,
    json: bool,
    since: Option<i64>,
    key: Option<String>,
    relay: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Args, &'static str> {
    let mut since = None;
    let mut selected_key = None;
    let mut relay = None;
    let mut root = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" if !json => json = true,
            "--relay-log" if relay.is_none() => {
                i += 1;
                relay = Some(PathBuf::from(
                    args.get(i)
                        .filter(|s| !s.is_empty() && !s.starts_with("--"))
                        .ok_or(USAGE)?,
                ));
            }
            "--since" if since.is_none() => {
                i += 1;
                since =
                    Some(duration_ms(args.get(i).ok_or(USAGE)?).ok_or("invalid --since duration")?);
            }
            "--key" if selected_key.is_none() => {
                i += 1;
                let value = args
                    .get(i)
                    .filter(|s| key(s))
                    .ok_or("invalid conversation key")?;
                selected_key = Some(value.clone());
            }
            "--runtime-root" if root.is_none() => {
                i += 1;
                root = Some(PathBuf::from(
                    args.get(i).filter(|s| !s.starts_with("--")).ok_or(USAGE)?,
                ));
            }
            _ => return Err(USAGE),
        }
        i += 1;
    }
    let root = root.filter(|r| r.is_absolute()).ok_or(USAGE)?;
    crate::install_root::check_runtime(&root, false)
        .map_err(|_| "report runtime selection refused")?;
    let meta = fs::symlink_metadata(&root).map_err(|_| "report runtime root is unavailable")?;
    if !meta.is_dir() || crate::guarded_file::is_link(&meta) {
        return Err("report runtime root is unavailable");
    }
    let root = fs::canonicalize(root).map_err(|_| "report runtime root is unavailable")?;
    crate::install_root::check_runtime(&root, false)
        .map_err(|_| "report runtime selection refused")?;
    Ok(Args {
        root,
        json,
        since,
        key: selected_key,
        relay,
    })
}

// Integer Go-style components: 30m, 1h30m, 2d. Checked arithmetic rejects overflow.
fn duration_ms(mut text: &str) -> Option<i64> {
    let mut total = 0u64;
    while !text.is_empty() {
        let n = text.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        let value = text[..n].parse::<u64>().ok()?;
        text = &text[n..];
        let (unit, scale) = [
            ("ms", 1),
            ("s", 1000),
            ("m", 60_000),
            ("h", 3_600_000),
            ("d", 86_400_000),
        ]
        .into_iter()
        .find(|(u, _)| text.starts_with(u))?;
        total = total.checked_add(value.checked_mul(scale)?)?;
        text = &text[unit.len()..];
    }
    i64::try_from(total).ok().filter(|n| *n > 0)
}

/// Shared READ/WRITE/DELETE permits append and atomic replacement by a running production writer.
/// Reading only the length at open avoids chasing an active append stream. No create or lock.
fn snapshot(path: &Path) -> std::io::Result<Snapshot> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(1 | 2 | 4).custom_flags(0x0020_0000); // OPEN_REPARSE_POINT
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || crate::guarded_file::is_link(&meta) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    Ok(Snapshot {
        file,
        len: meta.len(),
    })
}

struct Snapshot {
    file: fs::File,
    len: u64,
}
impl Snapshot {
    // Replays use the same handle and captured length, even across append or atomic rotation.
    fn reader(&mut self) -> std::io::Result<BufReader<std::io::Take<&mut fs::File>>> {
        self.file.rewind()?;
        let mut reader = BufReader::new((&mut self.file).take(self.len));
        if reader.fill_buf()?.starts_with(&[0xef, 0xbb, 0xbf]) {
            reader.consume(3);
        }
        Ok(reader)
    }
    fn lines(&mut self, mut visit: impl FnMut(&[u8])) -> std::io::Result<()> {
        let mut reader = self.reader()?;
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                return Ok(());
            }
            if !line.iter().all(u8::is_ascii_whitespace) {
                visit(&line);
            }
        }
    }
}

struct Source {
    status: &'static str,
    skipped: usize,
}
impl Source {
    fn open(root: &Path, relative: &str) -> (Self, Option<Snapshot>) {
        let mut out = Self {
            status: "present",
            skipped: 0,
        };
        let path = crate::runtime::resolve_contained_path(
            root,
            "",
            relative,
            crate::runtime::PathIntent::ReadOptionalFile,
        );
        let file = match path
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .and_then(|p| snapshot(&p))
        {
            Ok(file) => Some(file),
            Err(e) => {
                out.status = if e.kind() == std::io::ErrorKind::NotFound {
                    "absent"
                } else {
                    "unreadable"
                };
                None
            }
        };
        (out, file)
    }
    fn scan(&mut self, file: &mut Snapshot, mut visit: impl FnMut(Value)) {
        self.skipped = 0;
        let result = file.lines(|line| {
            if !line.ends_with(b"\n") {
                self.skipped += 1;
                return;
            }
            match serde_json::from_slice::<Value>(line) {
                Ok(v) if v.is_object() => visit(v),
                _ => self.skipped += 1,
            }
        });
        if result.is_err() {
            self.status = "unreadable";
        }
    }
    fn read(root: &Path, relative: &str, visit: impl FnMut(Value)) -> Self {
        let (mut source, file) = Self::open(root, relative);
        if let Some(mut file) = file {
            source.scan(&mut file, visit);
        }
        source
    }
    fn info(&self) -> Value {
        json!({"status":self.status,"skippedLines":self.skipped})
    }
}

// Move only the fields needed for correlation/folding; never retain a source payload.
fn project(mut row: Value, fields: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for field in fields {
        if let Some(value) = row.as_object_mut().and_then(|m| m.remove(*field)) {
            out.insert((*field).into(), value);
        }
    }
    Value::Object(out)
}

fn key(value: &str) -> bool {
    value.split_once(':').is_some_and(|(kind, id)| {
        matches!(kind, "group" | "private")
            && id
                .parse::<u64>()
                .is_ok_and(|n| n > 0 && n <= 9_007_199_254_740_991 && n.to_string() == id)
    })
}
fn str_field<'a>(v: &'a Value, field: &str) -> &'a str {
    v.get(field).and_then(Value::as_str).unwrap_or("")
}
fn timestamp(v: &Value, field: &str) -> i64 {
    v.get(field).and_then(Value::as_i64).unwrap_or(0)
}
fn choice<'a>(value: &str, allowed: &[&'a str]) -> &'a str {
    allowed
        .iter()
        .copied()
        .find(|s| *s == value)
        .unwrap_or("other")
}
/// Only symbolic model metadata is printable; free text, credentials and account-sized numbers aren't.
fn model(value: &str) -> String {
    if value.is_empty() {
        return "unknown".into();
    }
    if value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
    {
        return "redacted".into();
    }
    static PATTERNS: std::sync::OnceLock<(regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    let (credential, identifiers) = PATTERNS.get_or_init(|| (
        regex::Regex::new(r"(?i)(?:^|/)(?:(?:authorization|bearer|token|api[_-]?key|password|secret)[:=_-]|dsh-auth-)").unwrap(),
        regex::Regex::new(r"\d{5,}|[A-Za-z0-9_-]{32,}|(?i:sk-[A-Za-z0-9_-]+)").unwrap(),
    ));
    if credential.is_match(value) {
        return "redacted".into();
    }
    identifiers.replace_all(value, "redacted").into_owned()
}
fn next_model(row: &Value) -> Option<String> {
    // pipeline::ask_with_failover_inner always appends Candidate::label() to this
    // event. Exhaustion reasons end in provider-controlled detail and have no such suffix.
    if str_field(row, "event") != "failover-step" || str_field(row, "outcome") != "ok" {
        return None;
    }
    let (_, candidate) = str_field(row, "reason").rsplit_once("; next candidate ")?;
    let (provider, name) = candidate.split_once('/')?;
    if provider.is_empty() || name.is_empty() || model(candidate) != candidate {
        return None;
    }
    if candidate
        .split('/')
        .any(|part| !part.bytes().any(|b| b.is_ascii_alphanumeric()))
    {
        return None;
    }
    Some(candidate.into())
}

fn failure_code(value: &str) -> String {
    if value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    {
        return "redacted".into();
    }
    model(value)
}

fn final_attempt(row: &Value, turn: &crate::turn_ledger::Turn) -> bool {
    let Some(last) = turn.attempts.last() else {
        return false;
    };
    let count = row.get("attempts").and_then(Value::as_u64);
    if let Some(rpc) = row.get("attemptRpcId").and_then(Value::as_str) {
        !rpc.is_empty()
            && rpc == last.rpc_id
            && count.is_none_or(|n| n == turn.attempts.len() as u64)
    } else {
        count == Some(turn.attempts.len() as u64)
    }
}

fn pause_reason(value: &str) -> &'static str {
    choice(
        value,
        &[
            "send-unresolved",
            "inconsistent-state",
            "dsh-history-incomplete",
            "dsh-unreachable",
            "dsh-turn-unresolved",
            "cli-send-unverifiable",
            "harness-interrupted",
            "prompt-unverified",
            "turn-unverified",
        ],
    )
}

fn turn_id(value: &str) -> &str {
    if value
        .strip_prefix("turn-")
        .is_some_and(|s| s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        value
    } else {
        "redacted"
    }
}

#[derive(Default)]
struct Conversation {
    turns: Vec<Value>,
    timing: Vec<Value>,
    paused: Vec<Value>,
    refusals: Vec<Value>,
    failover: Vec<Value>,
    events: Vec<Value>,
    model_turns: BTreeMap<(String, i64), Vec<Value>>,
}
fn stats(mut times: Vec<f64>) -> Value {
    times.sort_by(f64::total_cmp);
    let median = if times.is_empty() {
        None
    } else {
        Some((times[(times.len() - 1) / 2] + times[times.len() / 2]) / 2.0)
    };
    json!({"medianMs":median,"maxMs":times.last()})
}
impl Conversation {
    fn into_json(self, key: String) -> Value {
        let mut outcomes = BTreeMap::<String, usize>::new();
        for t in &self.turns {
            *outcomes.entry(str_field(t, "outcome").into()).or_default() += 1;
        }
        let mut duration = stats(
            self.turns
                .iter()
                .filter_map(|t| t["turnMs"].as_f64())
                .collect(),
        );
        duration["sampledCount"] = json!(self
            .turns
            .iter()
            .filter(|t| t["turnMs"].is_number())
            .count());
        let mut timing = stats(
            self.timing
                .iter()
                .filter_map(|t| t["turnMs"].as_f64())
                .collect(),
        );
        timing["count"] = json!(self.timing.len());
        timing["samples"] = json!(self.timing);
        let model_turns: Vec<_> = self
            .model_turns
            .into_iter()
            .map(|((_, turn), mut calls)| {
                calls.sort_by_key(|c| (timestamp(c, "step"), timestamp(c, "retry")));
                json!({"turn":turn,"callCount":calls.len(),"calls":calls})
            })
            .collect();
        json!({"key":key,"turns":{"count":self.turns.len(),"outcomes":outcomes,"entries":self.turns,"duration":duration},"timing":timing,"modelTurns":model_turns,"pausedTurns":self.paused,"refusals":self.refusals,"failoverAttempts":self.failover,"failoverEvents":self.events})
    }
}

fn collect(args: &Args) -> (Value, i32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let since = args
        .since
        .map(|duration| now.saturating_sub(duration).max(0));
    let in_window = |at: i64| at <= now && since.is_none_or(|lower| at >= lower);
    let selected = |k: &str| key(k) && args.key.as_deref().is_none_or(|wanted| wanted == k);
    let mut sources = BTreeMap::new();
    let mut conversations = BTreeMap::<String, Conversation>::new();
    let mut groups = BTreeMap::<String, Vec<Value>>::new();
    let source = Source::read(&args.root, "data/turn-ledger.jsonl", |row| {
        let id = str_field(&row, "turn");
        let kind = str_field(&row, "k");
        if kind == "begun" {
            if selected(str_field(&row, "key")) && timestamp(&row, "t") <= now {
                groups.insert(id.into(), vec![project(row, &["k", "t", "turn", "key"])]);
            }
            return;
        }
        let Some(records) = groups.get_mut(id) else {
            return;
        };
        if matches!(kind, "ended" | "resolved")
            && since.is_some_and(|lower| timestamp(&row, "t") < lower)
        {
            groups.remove(id);
        } else if matches!(
            kind,
            "attempt" | "accepted" | "dsh_turn" | "ended" | "unknown" | "resolved"
        ) {
            let mut row = project(
                row,
                &[
                    "k", "t", "turn", "outcome", "reason", "rpcId", "dshTurn", "action",
                ],
            );
            // Only pause classifications are used; terminal/provider reasons are never retained.
            if row["k"] != "unknown" {
                row.as_object_mut().unwrap().remove("reason");
            } else {
                row["reason"] = json!(pause_reason(str_field(&row, "reason")));
            }
            records.push(row);
        }
    });
    sources.insert("turnLedger", source);
    let mut turns: Vec<_> = groups
        .into_values()
        .flat_map(|r| crate::turn_ledger::fold(&r).0)
        .collect();
    turns.sort_by_key(|t| t.begun_at);
    let mut intervals = BTreeMap::<&str, Vec<&crate::turn_ledger::Turn>>::new();
    for t in &turns {
        if in_window(t.ended_at.unwrap_or(t.begun_at)) {
            intervals.entry(&t.key).or_default().push(t);
        }
    }
    let turns_by_id: BTreeMap<_, _> = turns
        .iter()
        .filter(|t| in_window(t.ended_at.unwrap_or(t.begun_at)))
        .map(|t| (t.id.as_str(), t))
        .collect();
    let mut last_timings = BTreeMap::<String, Value>::new();
    let source = Source::read(&args.root, "data/logs/turn-timing.jsonl", |row| {
        let k = str_field(&row, "key");
        let at = timestamp(&row, "ts");
        if !selected(k) || at > now {
            return;
        }
        let id = str_field(&row, "turnId");
        let matched = if !id.is_empty() {
            // Collector bookkeeping can run after terminal cancellation and after the
            // next turn begins. Identity and the final submitted attempt own the row.
            turns_by_id
                .get(id)
                .copied()
                .filter(|t| t.key == k && at >= t.begun_at && final_attempt(&row, t))
        } else {
            // Older writers have no identity. Keep only unambiguous interval matches
            // consistent with the final attempt; never label an earlier retry as final.
            intervals.get(k).and_then(|turns| {
                let end = turns.partition_point(|t| t.begun_at <= at);
                end.checked_sub(1)
                    .map(|index| turns[index])
                    .filter(|t| at <= t.ended_at.unwrap_or(now))
                    .filter(|t| t.attempts.is_empty() || final_attempt(&row, t))
                    .filter(|t| {
                        t.state != crate::turn_ledger::State::Cancelled
                            || matches!(str_field(&row, "endKind"), "aborted" | "cancelled")
                    })
            })
        };
        if let Some(t) = matched {
            let rank = (!id.is_empty(), at);
            if last_timings.get(&t.id).is_none_or(|prev| {
                (!str_field(prev, "turnId").is_empty(), timestamp(prev, "ts")) <= rank
            }) {
                last_timings.insert(t.id.clone(), json!({"ts":at,"turnId":id,
                    "model":model(str_field(&row,"model")),
                    "endKind":row["endKind"].as_str().map(|s| choice(s,&["completed","error","aborted","cancelled","command"])),
                    "turnMs":row["turnMs"].as_f64().filter(|n| n.is_finite() && *n >= 0.0)}));
            }
        }
        if in_window(at) {
            conversations.entry(k.into()).or_default().timing.push(json!({"at":row.get("ts").and_then(Value::as_i64),"ok":row.get("ok").and_then(Value::as_bool),"endKind":row.get("endKind").and_then(Value::as_str).map(|s| choice(s,&["completed","error","aborted","cancelled","command"])),"model":model(str_field(&row,"model")),"attempts":row.get("attempts").and_then(Value::as_u64),"turnMs":row.get("turnMs").and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0)}));
        }
    });
    sources.insert("timing", source);
    drop(intervals);
    drop(turns_by_id);
    for t in turns {
        if !selected(&t.key) || t.begun_at > now {
            continue;
        }
        let c = conversations.entry(t.key.clone()).or_default();
        if t.state == crate::turn_ledger::State::Paused {
            c.paused.push(json!({"id":turn_id(&t.id),"reason":pause_reason(&t.pause_reason),"ageMs":now.saturating_sub(t.begun_at),"beganAt":t.begun_at}));
        }
        for (index, attempt) in t
            .attempts
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, a)| in_window(a.at))
        {
            c.failover.push(json!({"turnId":turn_id(&t.id),"attempt":index+1,"at":attempt.at,"accepted":attempt.accepted,"dshTurn":attempt.dsh_turn}));
        }
        if in_window(t.ended_at.unwrap_or(t.begun_at)) {
            let last = last_timings.get(&t.id);
            c.turns.push(json!({"id":turn_id(&t.id),"outcome":t.state.label(),"beganAt":t.begun_at,"endedAt":t.ended_at,"attemptCount":t.attempts.len(),"model":last.map(|r| model(str_field(r,"model"))),"endKind":last.and_then(|r| r["endKind"].as_str()).map(|s| choice(s,&["completed","error","aborted","cancelled","command"])),"turnMs":last.and_then(|r| r["turnMs"].as_f64()).filter(|n| n.is_finite() && *n >= 0.0)}));
        }
    }
    // Resolve session ownership before filtering token calls. Historical keys can appear on
    // baselines or later records; the current session map takes precedence over journal keys.
    let (mut session_source, session_file) = Source::open(&args.root, "data/sessions.json");
    let mut session_keys = BTreeMap::<String, String>::new();
    if let Some(mut file) = session_file {
        match file
            .reader()
            .and_then(|r| serde_json::from_reader::<_, Value>(r).map_err(std::io::Error::other))
        {
            Ok(v) if v.is_object() => {
                if let Some(map) = v.get("sessions").and_then(Value::as_object) {
                    for (k, v) in map {
                        if let Some(sid) = v
                            .as_str()
                            .or_else(|| v.get("sessionId").and_then(Value::as_str))
                            .filter(|s| !s.is_empty())
                        {
                            session_keys.insert(sid.into(), k.clone());
                        }
                    }
                }
            }
            _ => session_source.status = "invalid",
        }
    }
    sources.insert("sessions", session_source);
    let mut token_files = [
        Source::open(&args.root, "data/token-usage.jsonl"),
        Source::open(&args.root, "data/token-usage-rust.jsonl"),
    ];
    let call_id = |row: &Value| -> Option<(String, i64, i64, i64)> {
        let sid = str_field(row, "sid");
        if sid.is_empty() || row["k"] != "turn" || row.get("b").is_some_and(Value::is_null) {
            return None;
        }
        Some((
            sid.into(),
            row["turn"].as_i64()?,
            row["step"].as_i64()?,
            row["retry"].as_i64().unwrap_or(0),
        ))
    };
    // Old sessions without a call in this window need no ownership metadata.
    let mut needed_sessions = std::collections::BTreeSet::new();
    for (source, file) in &mut token_files {
        if let Some(file) = file {
            source.scan(file, |row| {
                if in_window(timestamp(&row, "t")) {
                    if let Some((sid, _, _, _)) = call_id(&row) {
                        needed_sessions.insert(sid);
                    }
                }
            });
        }
    }
    let mut historical_keys = BTreeMap::<String, ((i64, usize, usize), String)>::new();
    let mut file_order_keys = BTreeMap::<String, String>::new();
    let mut nonempty = [false; 2];
    for (file_index, (source, file)) in token_files.iter_mut().enumerate() {
        let mut line = 0;
        if let Some(file) = file {
            source.scan(file, |row| {
                line += 1;
                nonempty[file_index] = true;
                let sid = str_field(&row, "sid");
                let k = str_field(&row, "key");
                if !needed_sessions.contains(sid) || k.is_empty() || session_keys.contains_key(sid)
                {
                    return;
                }
                file_order_keys.insert(sid.into(), k.into());
                let order = (timestamp(&row, "t"), file_index, line);
                if historical_keys
                    .get(sid)
                    .is_none_or(|(prev, _)| *prev <= order)
                {
                    historical_keys.insert(sid.into(), (order, k.into()));
                }
            });
        }
    }
    let merged = nonempty.into_iter().filter(|present| *present).count() > 1;
    if merged {
        session_keys.extend(historical_keys.into_iter().map(|(sid, (_, k))| (sid, k)));
    } else {
        session_keys.extend(file_order_keys);
    }
    // The eligibility pass retains only call identities in the requested window/key.
    // A final replay includes duplicates outside the window for those identities so sequence
    // deduplication still precedes the window filter, exactly like the shared ledger fold.
    let mut eligible = std::collections::BTreeSet::new();
    for (source, file) in &mut token_files {
        if let Some(file) = file {
            source.scan(file, |row| {
                if in_window(timestamp(&row, "t"))
                    && session_keys
                        .get(str_field(&row, "sid"))
                        .is_some_and(|k| selected(k))
                {
                    if let Some(id) = call_id(&row) {
                        eligible.insert(id);
                    }
                }
            });
        }
    }
    let mut calls = Vec::new();
    for (file_index, (source, file)) in token_files.iter_mut().enumerate() {
        let mut line = 0;
        if let Some(file) = file {
            source.scan(file, |row| {
                line += 1;
                if !call_id(&row).is_some_and(|id| eligible.contains(&id)) { return; }
                let usage = crate::pricing::normalize_usage(row.get("b"));
                let mut row = project(row, &["k", "t", "sid", "seq", "turn", "step", "retry", "model"]);
                row["b"] = json!({"cacheMiss":usage.cache_miss,"cacheHit":usage.cache_hit,"output":usage.output});
                calls.push(((timestamp(&row, "t"), file_index, line), row));
            });
        }
    }
    // A single ledger preserves file order; two present ledgers merge by timestamp/file/line.
    if merged {
        calls.sort_by_key(|(order, _)| *order);
    }
    let mut ledger = crate::ledger::Ledger::default();
    for (_, row) in calls {
        ledger.apply(&row);
    }
    for (name, (source, _)) in ["legacyTokens", "tokens"].into_iter().zip(token_files) {
        sources.insert(name, source);
    }
    let mut samples: Vec<_> = ledger.samples.values().collect();
    samples.sort_by_key(|s| (s.t, &s.session_id, s.turn, s.step, s.retry));
    for s in samples {
        let Some(k) = session_keys.get(&s.session_id).filter(|k| selected(k)) else {
            continue;
        };
        if !in_window(s.t) {
            continue;
        }
        conversations.entry(k.clone()).or_default().model_turns.entry((s.session_id.clone(),s.turn)).or_default().push(json!({"at":s.t,"step":s.step,"retry":s.retry,"model":model(&s.model),"inputTokens":s.buckets.cache_miss+s.buckets.cache_hit,"outputTokens":s.buckets.output}));
    }
    let source = Source::read(&args.root, "data/logs/model-refusals.jsonl", |r| {
        let k = str_field(&r, "key");
        if !selected(k) || !in_window(timestamp(&r, "time")) {
            return;
        }
        conversations.entry(k.into()).or_default().refusals.push(json!({"at":timestamp(&r,"time"),"backend":model(str_field(&r,"backend")),"provider":model(str_field(&r,"provider")),"model":model(str_field(&r,"model")),"errorClass":"refusal","errorCode":failure_code(str_field(&r,"errorCode"))}));
    });
    sources.insert("refusals", source);
    let source = Source::read(&args.root, "data/run-record.jsonl", |row| {
        let event = str_field(&row, "event");
        let at = row
            .get("lastTs")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| timestamp(&row, "ts"));
        if str_field(&row, "component") != "model-chain"
            || str_field(&row, "trigger") != "turn"
            || !matches!(event, "failover-step" | "candidates-exhausted")
            || !in_window(at)
        {
            return;
        }
        // These writer-owned reasons start with a canonical key and code. Never copy the free-form detail.
        let Some((k, rest)) = str_field(&row, "reason").split_once(": ") else {
            return;
        };
        if !selected(k) {
            return;
        }
        let mut entry = json!({"at":at,"event":event,"outcome":choice(str_field(&row,"outcome"),&["ok","failed","unknown","skipped"]),"errorCode":failure_code(rest.split_whitespace().next().unwrap_or("")),"repeat":row.get("repeat").and_then(Value::as_u64),"firstAt":row.get("firstTs").and_then(Value::as_i64),"lastAt":row.get("lastTs").and_then(Value::as_i64)});
        if let Some(candidate) = next_model(&row) {
            entry["nextModel"] = json!(candidate);
        }
        conversations
            .entry(k.into())
            .or_default()
            .events
            .push(entry);
    });
    sources.insert("runRecord", source);
    conversations.retain(|_, c| {
        !c.turns.is_empty()
            || !c.timing.is_empty()
            || !c.model_turns.is_empty()
            || !c.paused.is_empty()
            || !c.refusals.is_empty()
            || !c.failover.is_empty()
            || !c.events.is_empty()
    });
    let mut partial = sources
        .values()
        .any(|s| matches!(s.status, "unreadable" | "invalid"));
    let mut info: BTreeMap<_, _> = sources.iter().map(|(k, s)| (*k, s.info())).collect();
    let relay = args.relay.as_ref().map(|path| {
        let (summary, source) = relay_report(path, since, now);
        partial |= source["status"] == "unreadable";
        info.insert("relay", source);
        summary
    });
    (
        json!({"window":{"since":since,"until":now},"sources":info,"partial":partial,"relay":relay,"conversations":conversations.into_iter().map(|(k,c)| c.into_json(k)).collect::<Vec<_>>()}),
        if partial { 1 } else { 0 },
    )
}

fn render(report: &Value) -> String {
    let mut out = format!(
        "HER production report (until {} ms)\n",
        report["window"]["until"]
    );
    for (name, source) in report["sources"].as_object().unwrap() {
        out.push_str(&format!(
            "{name}: {} (skipped lines: {})\n",
            str_field(source, "status"),
            source["skippedLines"]
        ));
    }
    if let Some(relay) = report.get("relay").filter(|r| !r.is_null()) {
        out.push_str(&format!("relay: {} requests; status codes {}; median {} ms, max {} ms; durations {} ms; unparsed lines {}\n",relay["requestCount"],relay["statusCodes"],relay["medianMs"],relay["maxMs"],relay["durationsMs"],relay["unparsedLines"]));
    }
    for c in report["conversations"].as_array().unwrap() {
        out.push_str(&format!("\n{}: {} turns; outcomes {}\n  turn time: median {} ms, max {} ms ({} matched turns)\n", str_field(c,"key"),c["turns"]["count"],c["turns"]["outcomes"],c["turns"]["duration"]["medianMs"],c["turns"]["duration"]["maxMs"],c["turns"]["duration"]["sampledCount"]));
        for (field, label) in [
            ("pausedTurns", "paused turn"),
            ("refusals", "refusal"),
            ("failoverAttempts", "failover attempt"),
            ("failoverEvents", "failover event"),
        ] {
            for row in c[field].as_array().unwrap() {
                out.push_str(&format!("  {label}: {row}\n"));
            }
        }
        for turn in c["turns"]["entries"].as_array().unwrap() {
            out.push_str(&format!("  turn: {turn}\n"));
        }
        out.push_str(&format!(
            "  collector samples: {} (median {} ms, max {} ms)\n",
            c["timing"]["count"], c["timing"]["medianMs"], c["timing"]["maxMs"]
        ));
        for t in c["timing"]["samples"].as_array().unwrap() {
            out.push_str(&format!(
                "  timing: model {} endKind {} ok {} attempts {} turnMs {}\n",
                t["model"], t["endKind"], t["ok"], t["attempts"], t["turnMs"]
            ));
        }
        for t in c["modelTurns"].as_array().unwrap() {
            out.push_str(&format!(
                "  model turn {}: {} calls\n",
                t["turn"], t["callCount"]
            ));
            for call in t["calls"].as_array().unwrap() {
                out.push_str(&format!(
                    "    step {} retry {} model {}: input {} output {} tokens\n",
                    call["step"],
                    call["retry"],
                    call["model"],
                    call["inputTokens"],
                    call["outputTokens"]
                ));
            }
        }
    }
    if report["conversations"].as_array().unwrap().is_empty() {
        out.push_str("No conversations in window.\n");
    }
    out
}

// CLIProxyAPI GinLogrusLogger's local wall time; use HER's documented production zone.
fn relay_timestamp(caps: &regex::Captures<'_>) -> Option<i64> {
    let n = |i| caps.get(i)?.as_str().parse::<i64>().ok();
    let (year, month, day, hour, minute, second) = (n(1)?, n(2)?, n(3)?, n(4)?, n(5)?, n(6)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let days_in_month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][month as usize - 1];
    if !(1..=days_in_month).contains(&day) {
        return None;
    }
    // Gregorian civil date to days since 1970-01-01.
    let y = year - i64::from(month <= 2);
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some(
        ((days * 24 + hour) * 60 * 60 + minute * 60 + second) * 1000
            - crate::tasks::local_offset_ms(),
    )
}

fn relay_duration(mut text: &str) -> Option<f64> {
    let mut total = 0.0;
    if text.is_empty() {
        return None;
    }
    while !text.is_empty() {
        let n = text
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .count();
        let value = text.get(..n)?.parse::<f64>().ok()?;
        text = &text[n..];
        let (unit, scale) = [
            ("ns", 0.000001),
            ("us", 0.001),
            ("µs", 0.001),
            ("μs", 0.001),
            ("ms", 1.0),
            ("s", 1000.0),
            ("m", 60_000.0),
            ("h", 3_600_000.0),
        ]
        .into_iter()
        .find(|(u, _)| text.starts_with(u))?;
        total += value * scale;
        text = &text[unit.len()..];
    }
    total.is_finite().then_some(total)
}

fn relay_report(path: &Path, since: Option<i64>, until: i64) -> (Value, Value) {
    let mut status = "present";
    let mut unparsed = 0;
    let mut durations = Vec::new();
    let mut codes = BTreeMap::<u16, usize>::new();
    // The documented modern format and retained [GIN] format. No captured free text is emitted.
    static PATTERNS: std::sync::OnceLock<(regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    let (date, request) = PATTERNS.get_or_init(|| (
        regex::Regex::new(r"(?:^\[|\[GIN\]\s+)(\d{4})[-/](\d{2})[-/](\d{2})(?:\s+-\s+|\s+)(\d{2}):(\d{2}):(\d{2})").unwrap(),
        regex::Regex::new(r"\|\s*([1-5][0-9]{2})\s*\|\s*([^|]+?)\s*\|").unwrap(),
    ));
    match snapshot(path) {
        Err(e) => {
            status = if e.kind() == std::io::ErrorKind::NotFound {
                "absent"
            } else {
                "unreadable"
            }
        }
        Ok(mut file) => {
            let result = file.lines(|line| {
                let parsed = || {
                    if !line.ends_with(b"\n") {
                        return None;
                    }
                    let line = std::str::from_utf8(line).ok()?;
                    let at = relay_timestamp(&date.captures(line)?)?;
                    let caps = request.captures(line)?;
                    let code = caps.get(1)?.as_str().parse::<u16>().ok()?;
                    let duration = relay_duration(caps.get(2)?.as_str().trim())?;
                    Some((at, code, duration))
                };
                match parsed() {
                    Some((at, code, duration))
                        if at <= until && since.is_none_or(|lower| at >= lower) =>
                    {
                        *codes.entry(code).or_default() += 1;
                        durations.push(duration);
                    }
                    Some(_) => {}
                    None => unparsed += 1,
                }
            });
            if result.is_err() {
                status = "unreadable";
            }
        }
    }
    let mut summary = stats(durations.clone());
    summary["requestCount"] = json!(durations.len());
    summary["statusCodes"] = json!(codes);
    summary["durationsMs"] = json!(durations);
    summary["unparsedLines"] = json!(unparsed);
    (summary, json!({"status":status,"skippedLines":unparsed}))
}

pub fn run_cli(args: &[String]) -> i32 {
    if args == ["--help"] {
        println!("{USAGE}");
        return 0;
    }
    let args = match parse(args) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let (report, code) = collect(&args);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        print!("{}", render(&report));
    }
    code
}
