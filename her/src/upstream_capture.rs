//! One-shot capture of the next model request HER sends upstream, and its sweep for the #142
//! denylist (#183). The operator arms a capture for one route with `her-cli upstream-capture arm`;
//! the next request on that route is snapshotted right before it leaves HER and written once, on a
//! background thread, to `data/upstream-capture/captured.json` in the selected runtime. `sweep`
//! prints only counts and JSON field paths of the denied terms, never content, and deletes the
//! capture. Nothing is captured unless armed, and an arming expires.
//!
//! Capture points (`offer`):
//! - `dsh`: every `session/prompt` envelope `dsh::Rpc` posts (wake, chat and learner prompts).
//! - `harness`: every model step's request in `harness::tool_loop`, as the provider-neutral
//!   `ChatRequest` with the model id and token limit that the model client turns into the relay call.
//!
//! The send path never waits on the capture and never panics for it: `offer` does one metadata
//! lookup and takes the snapshot; claiming, writing and every diagnostic happen on a detached
//! thread, and diagnostics ignore a failed stderr write.

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const ARMED_FILE: &str = "upstream-capture/armed.json";
const CLAIMED_FILE: &str = "upstream-capture/claimed.json";
const CAPTURED_FILE: &str = "upstream-capture/captured.json";
/// A request whose JSON is longer than this is recorded by its size only.
const CAPTURE_LIMIT_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MINUTES: i64 = 30;
const MAX_MINUTES: i64 = 1440;
/// A string of at least this many bytes made only of base64 characters, or a base64 data URL, is
/// binary content: counted, never checked.
const BINARY_MIN_BYTES: usize = 256;
/// The placeholder a field path shows for an object key outside `STRUCTURAL_KEYS`.
const KEY_PLACEHOLDER: &str = "<key>";
/// Object keys a field path shows: the structure of the DSH envelope, of the harness request
/// (`ChatRequest`, its messages, parts and tools) and of JSON Schema. Any other key (a tool
/// parameter, a model-written argument name, a stray identifier or secret) is shown as `<key>`.
const STRUCTURAL_KEYS: &[&str] = &[
    "$schema", "Base64", "Binary", "ReasoningContent", "Text", "ThoughtSignature", "ToolCall", "ToolResponse", "Url",
    "additionalProperties", "allOf", "anyOf", "args", "call_id", "config", "const", "content", "content_type", "data",
    "default", "description", "enum", "fn_arguments", "fn_name", "format", "function", "items", "maxItems",
    "maxLength", "maxTokens", "maximum", "messages", "method", "mimeType", "minItems", "minLength", "minimum", "mode",
    "model", "name", "oneOf", "options", "parameters", "parts", "pattern", "payload", "properties", "request",
    "requestId", "required", "role", "rpcId", "schema", "sessionId", "source", "system", "text", "thought_signatures",
    "title", "tools", "type", "url",
];

const USAGE: &str = "usage: her-cli upstream-capture arm --runtime-root <root> --route <dsh|harness> [--minutes <1-1440>]\n       her-cli upstream-capture status --runtime-root <root>\n       her-cli upstream-capture sweep --runtime-root <root>\n       her-cli upstream-capture disarm --runtime-root <root>";

/// Selected runtimes (by data folder) with a capture attempt in flight in this process. One attempt
/// per runtime at a time, so a burst of requests while armed starts no pile of threads; a request
/// that meets an attempt in flight is not captured.
static IN_FLIGHT: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Latin terms, matched case-insensitively as whole words (an ASCII letter or digit on either side
/// is not a match, so `dshReady` or `qq_send_message` pass while `user/dsh` and `deepseek娘` fail).
const DENIED_WORDS: [&str; 13] = [
    "deepseek", "dsh", "harness", "opencode", "codex", "claude", "anthropic", "astrbot", "cliproxy",
    "antigravity", "openai", "chatgpt", "gpt",
];

/// Phrases matched case-insensitively anywhere.
const DENIED_PHRASES: [&str; 9] = ["cli-proxy", "人工智能", "机器人", "语言模型", "大模型", "模型", "智能体", "助手", "深度求索"];

/// The #142 denied terms `text` contains, each once. "AI" is matched in upper case with no ASCII
/// letter on either side, so 反AI味 fails while `lastAiReplyAt`, `API` and `EMAIL` pass. The
/// model-visible text guard (`console::tests::model_text_guard_tests`) uses the same matcher.
pub(crate) fn denied_terms(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let lower = text.to_lowercase();
    let word_edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
    for word in DENIED_WORDS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(word) {
            let (start, end) = (from + at, from + at + word.len());
            if word_edge(lower[..start].chars().next_back()) && word_edge(lower[end..].chars().next()) {
                found.push(word.to_string());
                break;
            }
            from = end;
        }
    }
    let letter = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphabetic());
    let mut from = 0;
    while let Some(at) = text[from..].find("AI") {
        let (start, end) = (from + at, from + at + 2);
        if !letter(text[..start].chars().next_back()) && !letter(text[end..].chars().next()) {
            found.push("AI".to_string());
            break;
        }
        from = end;
    }
    for phrase in DENIED_PHRASES {
        if lower.contains(phrase) {
            found.push(phrase.to_string());
        }
    }
    found
}

/// One fixed diagnostic line on stderr. A failed write is ignored: nothing here panics.
fn note(text: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "[upstream-capture] {text}");
}

/// The upstream path a capture is armed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Dsh,
    Harness,
}

impl Route {
    fn id(self) -> &'static str {
        match self {
            Self::Dsh => "dsh",
            Self::Harness => "harness",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "dsh" => Some(Self::Dsh),
            "harness" => Some(Self::Harness),
            _ => None,
        }
    }
}

struct Arming {
    route: Route,
    expires_at: i64,
}

fn parse_arming(text: &str) -> Option<Arming> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value["v"].as_i64() != Some(1) {
        return None;
    }
    Some(Arming {
        route: Route::parse(value["route"].as_str()?)?,
        expires_at: value["expiresAt"].as_i64()?,
    })
}

/// The background half of a capture: claims, judges and writes.
type Persist = fn(&RuntimeContext, Route, i64, Value) -> bool;

/// Marks a runtime's capture attempt as in flight until dropped, even when the attempt panics.
struct Flight(PathBuf);

impl Drop for Flight {
    fn drop(&mut self) {
        IN_FLIGHT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&self.0);
    }
}

/// Called right before HER sends a model request upstream on `route`. It never waits on the
/// capture: without an arming file it does one metadata lookup and `request` is never built; with
/// one, it takes the snapshot `request` returns and hands it to a detached thread (`persist`).
pub(crate) fn offer(context: &RuntimeContext, route: Route, request: impl FnOnce() -> Value) {
    let _ = offer_at(context, route, crate::turn_ledger::now_ms(), request, persist);
}

/// The send-path half of `offer`. The handle is for tests; `offer` drops it, detaching the thread.
fn offer_at(
    context: &RuntimeContext,
    route: Route,
    now: i64,
    request: impl FnOnce() -> Value,
    persist: Persist,
) -> Option<std::thread::JoinHandle<bool>> {
    if !context.is_isolated() || !matches!(present(context, ARMED_FILE), Ok(true)) {
        return None;
    }
    let key = context.data_dir().to_path_buf();
    {
        let mut flight = match IN_FLIGHT.try_lock() {
            Ok(flight) => flight,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        if !flight.insert(key.clone()) {
            return None;
        }
    }
    let flight = Flight(key);
    let snapshot = request();
    let context = context.clone();
    let spawned = std::thread::Builder::new()
        .name("upstream-capture".to_string())
        .spawn(move || {
            let _flight = flight;
            persist(&context, route, now, snapshot)
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(_) => {
            note("the capture thread could not start; nothing was captured");
            None
        }
    }
}

/// Claims an arming for `route` by an atomic rename (one winner across threads and processes), judges
/// the claimed file and writes the capture. A slow or failing disk delays only this thread. An
/// expired arming of any route is removed without a capture.
fn persist(context: &RuntimeContext, route: Route, now: i64, request: Value) -> bool {
    let Ok(Some(text)) = context.read_optional_text(RuntimeArea::Data, ARMED_FILE) else {
        return false;
    };
    let Some(arming) = parse_arming(&text) else {
        return false;
    };
    if arming.route != route && now <= arming.expires_at {
        return false;
    }
    if context.rename_file(RuntimeArea::Data, ARMED_FILE, CLAIMED_FILE).is_err() {
        return false;
    }
    // Judge the claimed file itself: the arming read above may have been replaced before the rename.
    let claimed = context
        .read_optional_text(RuntimeArea::Data, CLAIMED_FILE)
        .ok()
        .flatten()
        .and_then(|text| parse_arming(&text));
    match claimed {
        Some(arming) if now > arming.expires_at => {
            note("the arming expired; nothing was captured");
            release_claim(context);
            false
        }
        Some(arming) if arming.route == route => write_claimed(context, route, now, &request),
        Some(_) => {
            // A newer arming for the other route replaced the one read above: give it back.
            if context.rename_file(RuntimeArea::Data, CLAIMED_FILE, ARMED_FILE).is_err() {
                note("an arming for another route could not be restored; run disarm");
            }
            false
        }
        None => {
            note("the claimed arming is unreadable; nothing was captured");
            release_claim(context);
            false
        }
    }
}

fn release_claim(context: &RuntimeContext) {
    if context.remove_file(RuntimeArea::Data, CLAIMED_FILE).is_err() {
        note("the claim could not be removed; run disarm");
    }
}

/// Writes the capture under a held claim, then gives the claim up. When the claim is gone by then
/// (`disarm` ran meanwhile), the capture just written is removed again, so a disarm always wins.
fn write_claimed(context: &RuntimeContext, route: Route, now: i64, request: &Value) -> bool {
    let record = capture_record(route, now, request, CAPTURE_LIMIT_BYTES);
    if context.create_new_file(RuntimeArea::Data, CAPTURED_FILE, &record).is_err() {
        note("the request could not be written; nothing was captured");
        release_claim(context);
        return false;
    }
    if !matches!(present(context, CLAIMED_FILE), Ok(true)) {
        if context.remove_file(RuntimeArea::Data, CAPTURED_FILE).is_err() {
            note("a capture written after disarm could not be removed; run disarm");
        }
        return false;
    }
    release_claim(context);
    true
}

/// The capture file's bytes: the request, or only its size when it is longer than `limit`.
fn capture_record(route: Route, now: i64, request: &Value, limit: usize) -> Vec<u8> {
    let bytes = request.to_string().len();
    let record = if bytes > limit {
        json!({"v": 1, "route": route.id(), "capturedAt": now, "tooLarge": true, "bytes": bytes})
    } else {
        json!({"v": 1, "route": route.id(), "capturedAt": now, "request": request})
    };
    record.to_string().into_bytes()
}

#[derive(Default)]
struct Findings {
    strings: usize,
    binary: usize,
    counts: BTreeMap<String, usize>,
    fields: Vec<Value>,
}

/// Counts and JSON field paths of the denied terms in every string value and object key of
/// `request`. The report holds the terms (fixed denylist entries), counts and paths only: a path
/// segment is an array index, a key from `STRUCTURAL_KEYS`, or `<key>` for any other key, so no
/// text from the request reaches the report.
pub(crate) fn sweep(request: &Value) -> Value {
    let mut findings = Findings::default();
    walk(request, &mut String::new(), &mut findings);
    let total: usize = findings.counts.values().sum();
    json!({
        "strings": findings.strings,
        "binaryStrings": findings.binary,
        "deniedTotal": total,
        "counts": findings.counts,
        "fields": findings.fields,
    })
}

fn walk(value: &Value, path: &mut String, findings: &mut Findings) {
    match value {
        Value::String(text) => check(text, path, "value", findings),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let length = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                walk(item, path, findings);
                path.truncate(length);
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                let length = path.len();
                path.push('/');
                path.push_str(path_segment(key));
                check(key, path, "key", findings);
                walk(item, path, findings);
                path.truncate(length);
            }
        }
        _ => {}
    }
}

fn path_segment(key: &str) -> &str {
    if STRUCTURAL_KEYS.contains(&key) {
        key
    } else {
        KEY_PLACEHOLDER
    }
}

fn is_binary(text: &str) -> bool {
    text.len() >= BINARY_MIN_BYTES
        && ((text.starts_with("data:") && text.contains(";base64,"))
            || text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\r' | b'\n')))
}

fn check(text: &str, path: &str, part: &str, findings: &mut Findings) {
    if is_binary(text) {
        findings.binary += 1;
        return;
    }
    findings.strings += 1;
    let terms = denied_terms(text);
    if terms.is_empty() {
        return;
    }
    for term in &terms {
        *findings.counts.entry(term.clone()).or_default() += 1;
    }
    findings.fields.push(json!({"path": path, "part": part, "terms": terms}));
}

/// Whether a capture file exists, without following a link.
fn present(context: &RuntimeContext, relative: &str) -> Result<bool, (i32, String)> {
    let unavailable = || (1, "the capture folder is unavailable".to_string());
    let path = context
        .resolve_path(RuntimeArea::Data, relative, PathIntent::ReadOptionalFile)
        .map_err(|_| unavailable())?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(unavailable()),
    }
}

fn arm(context: &RuntimeContext, route: Option<&str>, minutes: Option<&str>, now: i64) -> Result<Value, (i32, String)> {
    let route = route
        .and_then(Route::parse)
        .ok_or_else(|| (2, "--route must be dsh or harness".to_string()))?;
    let minutes = match minutes {
        None => DEFAULT_MINUTES,
        Some(text) => text
            .parse::<i64>()
            .ok()
            .filter(|minutes| (1..=MAX_MINUTES).contains(minutes))
            .ok_or_else(|| (2, "--minutes must be a whole number from 1 to 1440".to_string()))?,
    };
    for (file, refusal) in [
        (CAPTURED_FILE, "a capture waits for sweep or disarm"),
        (CLAIMED_FILE, "an interrupted capture is left; run disarm"),
        (ARMED_FILE, "a capture is already armed"),
    ] {
        if present(context, file)? {
            return Err((1, refusal.to_string()));
        }
    }
    let expires_at = now + minutes * 60_000;
    let arming = json!({"v": 1, "route": route.id(), "armedAt": now, "expiresAt": expires_at});
    context
        .create_new_file(RuntimeArea::Data, ARMED_FILE, arming.to_string().as_bytes())
        .map_err(|_| (1, "the arming could not be written".to_string()))?;
    Ok(json!({"armed": true, "route": route.id(), "expiresAt": expires_at}))
}

fn status(context: &RuntimeContext, now: i64) -> Result<Value, (i32, String)> {
    let armed = context
        .read_optional_text(RuntimeArea::Data, ARMED_FILE)
        .map_err(|_| (1, "the capture folder is unavailable".to_string()))?;
    let arming = armed.as_deref().and_then(parse_arming);
    Ok(json!({
        "armed": armed.is_some(),
        "unreadable": armed.is_some() && arming.is_none(),
        "route": arming.as_ref().map(|arming| arming.route.id()),
        "expiresAt": arming.as_ref().map(|arming| arming.expires_at),
        "expired": arming.as_ref().is_some_and(|arming| now > arming.expires_at),
        "interrupted": present(context, CLAIMED_FILE)?,
        "captured": present(context, CAPTURED_FILE)?,
    }))
}

fn disarm(context: &RuntimeContext) -> Result<Value, (i32, String)> {
    let mut removed = Vec::new();
    for (file, name) in [(ARMED_FILE, "armed"), (CLAIMED_FILE, "interrupted"), (CAPTURED_FILE, "captured")] {
        if present(context, file)? {
            context
                .remove_file(RuntimeArea::Data, file)
                .map_err(|_| (1, format!("the {name} file could not be removed")))?;
            removed.push(name);
        }
    }
    Ok(json!({"removed": removed}))
}

/// Sweeps the capture and deletes it. The report says `removed: false` when the deletion failed.
/// Every printed value is checked for its type first: a capture whose metadata or shape is not
/// exactly what `write_claimed` writes is refused as invalid, and none of its values is printed.
fn sweep_capture(context: &RuntimeContext) -> Result<Value, (i32, String)> {
    let invalid = || (1, "capture file invalid; run disarm".to_string());
    let text = context
        .read_optional_text(RuntimeArea::Data, CAPTURED_FILE)
        .map_err(|_| invalid())?
        .ok_or_else(|| (1, "nothing was captured; arm a capture first".to_string()))?;
    let record: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    if record["v"].as_i64() != Some(1) {
        return Err(invalid());
    }
    let route = record["route"].as_str().and_then(Route::parse).ok_or_else(invalid)?;
    let captured_at = record["capturedAt"].as_i64().ok_or_else(invalid)?;
    let mut report = json!({"route": route.id(), "capturedAt": captured_at});
    match (record.get("tooLarge"), record.get("request")) {
        (Some(Value::Bool(true)), None) => {
            let bytes = record["bytes"].as_u64().ok_or_else(invalid)?;
            report["tooLarge"] = json!(true);
            report["bytes"] = json!(bytes);
        }
        (None, Some(request)) => {
            if let Value::Object(fields) = sweep(request) {
                for (key, value) in fields {
                    report[key.as_str()] = value;
                }
            }
        }
        _ => return Err(invalid()),
    }
    report["removed"] = json!(context.remove_file(RuntimeArea::Data, CAPTURED_FILE).is_ok());
    Ok(report)
}

struct Args {
    command: String,
    root: Option<String>,
    route: Option<String>,
    minutes: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut items = args.iter();
    let command = items.next().ok_or("a command is required")?.clone();
    let mut parsed = Args { command, root: None, route: None, minutes: None };
    while let Some(flag) = items.next() {
        let slot = match flag.as_str() {
            "--runtime-root" => &mut parsed.root,
            "--route" => &mut parsed.route,
            "--minutes" => &mut parsed.minutes,
            _ => return Err("unknown argument".to_string()),
        };
        if slot.is_some() {
            return Err(format!("{flag} given twice"));
        }
        *slot = Some(items.next().ok_or_else(|| format!("{flag} needs a value"))?.clone());
    }
    Ok(parsed)
}

/// `her-cli upstream-capture`. Exit codes: 0 done; 1 refused, nothing to sweep, an invalid capture, a
/// capture too large to sweep or a capture that could not be deleted; 2 usage or runtime-root error.
pub(crate) fn run_cli(args: &[String]) -> i32 {
    use std::io::Write;
    if args.first().map(String::as_str) == Some("--help") {
        let _ = writeln!(std::io::stdout(), "{USAGE}");
        return 0;
    }
    let parsed = match parse(args) {
        Ok(parsed) if matches!(parsed.command.as_str(), "arm" | "status" | "sweep" | "disarm") => parsed,
        Ok(_) => {
            note(&format!("unknown command\n{USAGE}"));
            return 2;
        }
        Err(error) => {
            note(&format!("{error}\n{USAGE}"));
            return 2;
        }
    };
    if parsed.command != "arm" && (parsed.route.is_some() || parsed.minutes.is_some()) {
        note(&format!("--route and --minutes belong to arm\n{USAGE}"));
        return 2;
    }
    let Some(root) = parsed.root.as_deref() else {
        note(&format!("--runtime-root is required\n{USAGE}"));
        return 2;
    };
    let context = match RuntimeContext::from_root(Path::new(root)) {
        Ok(context) => context,
        Err(error) => {
            note(error);
            return 2;
        }
    };
    let now = crate::turn_ledger::now_ms();
    let result = match parsed.command.as_str() {
        "arm" => arm(&context, parsed.route.as_deref(), parsed.minutes.as_deref(), now),
        "status" => status(&context, now),
        "sweep" => sweep_capture(&context),
        _ => disarm(&context),
    };
    match result {
        Ok(report) => {
            let _ = writeln!(std::io::stdout(), "{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            if report["removed"] == false || report["tooLarge"] == true {
                1
            } else {
                0
            }
        }
        Err((code, error)) => {
            note(&error);
            code
        }
    }
}

/// Fixtures the capture-point tests in `dsh` and here share.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::runtime::{RuntimeArea, RuntimeContext};
    use serde_json::{json, Value};

    /// A disposable selected runtime, removed on drop.
    pub(crate) struct FixtureRoot(pub(crate) std::path::PathBuf);

    impl Drop for FixtureRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A selected closed-agent runtime with a harness model whose dependencies all use `endpoint`.
    /// Its model view strips the literal `STRIPPED183`.
    pub(crate) fn runtime(label: &str, endpoint: &str) -> (FixtureRoot, RuntimeContext) {
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(format!("her-183-{label}-{}", crate::turn_ledger::mint_id(""))),
        );
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), config().to_string()).unwrap();
        std::fs::write(root.join("config/harness-api-key"), "synthetic-model-key-183").unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(root.join("data/mode.json"), r#"{"mode":"closed-agent"}"#).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            json!({"dsh": endpoint, "model": endpoint, "onebot": endpoint}).to_string(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (FixtureRoot(root), context)
    }

    /// The fixture runtime's configuration.
    pub(crate) fn config() -> Value {
        json!({"ownerQQ": 8808, "consoleToken": "synthetic-console-token-183", "harness": {"model": "fake-chat"},
            "socialV2": {"context": {"stripFromModelText": ["STRIPPED183"]}}})
    }

    /// Arms a capture for `route` now, for the default 30 minutes.
    pub(crate) fn arm(context: &RuntimeContext, route: &str) {
        super::arm(context, Some(route), None, crate::turn_ledger::now_ms()).unwrap();
    }

    /// The capture record, when one was written.
    pub(crate) fn captured(context: &RuntimeContext) -> Option<Value> {
        context
            .read_optional_text(RuntimeArea::Data, super::CAPTURED_FILE)
            .unwrap()
            .map(|text| serde_json::from_str(&text).unwrap())
    }

    /// Waits (at most 5 s) until no capture attempt for this runtime is in flight, then returns the
    /// capture record, when one was written.
    pub(crate) fn settled(context: &RuntimeContext) -> Option<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while super::IN_FLIGHT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(context.data_dir()) {
            assert!(std::time::Instant::now() < deadline, "a capture attempt did not finish");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        captured(context)
    }

    /// Removes the arming, a claim and a capture.
    pub(crate) fn disarm(context: &RuntimeContext) {
        super::disarm(context).unwrap();
    }

    /// Whether the arming or the claim file exists.
    pub(crate) fn armed_or_claimed(context: &RuntimeContext) -> (bool, bool) {
        (super::present(context, super::ARMED_FILE).unwrap(), super::present(context, super::CLAIMED_FILE).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{armed_or_claimed, captured, runtime, settled};
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    /// One `offer` at `now` with the real writer, waited for: true when it wrote the capture.
    fn attempt(context: &RuntimeContext, route: Route, now: i64, request: impl FnOnce() -> Value) -> bool {
        offer_at(context, route, now, request, persist).is_some_and(|handle| handle.join().unwrap())
    }

    fn slow_persist(context: &RuntimeContext, route: Route, now: i64, request: Value) -> bool {
        std::thread::sleep(Duration::from_secs(1));
        persist(context, route, now, request)
    }

    fn panicking_persist(_: &RuntimeContext, _: Route, _: i64, _: Value) -> bool {
        panic!("fixture writer failure 183")
    }

    #[test]
    fn nothing_is_captured_unless_armed_for_the_route_and_then_only_once() {
        let (_root, context) = runtime("once", "http://127.0.0.1:1");
        let built = std::cell::Cell::new(0);
        assert!(!attempt(&context, Route::Dsh, 1_000, || {
            built.set(built.get() + 1);
            json!({"n": 0})
        }));
        assert_eq!(built.get(), 0, "an unarmed offer builds no request");
        assert!(captured(&context).is_none());
        // An arming for the other route is left alone.
        arm(&context, Some("harness"), None, 1_000).unwrap();
        assert!(!attempt(&context, Route::Dsh, 2_000, || json!({"n": 1})));
        assert_eq!(armed_or_claimed(&context), (true, false));
        assert_eq!(disarm(&context).unwrap(), json!({"removed": ["armed"]}));
        // Armed for this route: the next request is captured, and the next one is not.
        arm(&context, Some("dsh"), None, 1_000).unwrap();
        assert!(attempt(&context, Route::Dsh, 2_000, || json!({"n": 2})));
        assert!(!attempt(&context, Route::Dsh, 3_000, || json!({"n": 3})));
        assert_eq!(captured(&context).unwrap(), json!({"v": 1, "route": "dsh", "capturedAt": 2_000, "request": {"n": 2}}));
        assert_eq!(armed_or_claimed(&context), (false, false));
    }

    #[test]
    fn a_slow_or_failing_writer_never_holds_up_the_request() {
        let (_root, context) = runtime("slow-writer", "http://127.0.0.1:1");
        arm(&context, Some("dsh"), None, 1_000).unwrap();
        // The writer takes a second; the send path returns at once, and a request meanwhile is not
        // held either (the attempt in flight makes it skip the capture).
        let started = Instant::now();
        let slow = offer_at(&context, Route::Dsh, 2_000, || json!({"n": 1}), slow_persist).expect("an armed offer starts a writer");
        let second = offer_at(&context, Route::Dsh, 2_000, || json!({"n": 2}), persist);
        assert!(started.elapsed() < Duration::from_millis(500), "the send path waited {:?}", started.elapsed());
        assert!(second.is_none(), "a request during an attempt in flight starts no second writer");
        assert!(slow.join().unwrap());
        assert_eq!(captured(&context).unwrap()["request"], json!({"n": 1}));
        disarm(&context).unwrap();
        // A writer that fails outright ends only its own thread and frees the runtime for the next one.
        arm(&context, Some("dsh"), None, 1_000).unwrap();
        let started = Instant::now();
        let failed = offer_at(&context, Route::Dsh, 2_000, || json!({"n": 3}), panicking_persist).expect("an armed offer starts a writer");
        assert!(started.elapsed() < Duration::from_millis(500), "the send path waited {:?}", started.elapsed());
        assert!(failed.join().is_err());
        assert!(attempt(&context, Route::Dsh, 2_000, || json!({"n": 4})));
        assert_eq!(captured(&context).unwrap()["request"], json!({"n": 4}));
    }

    #[test]
    fn an_arming_expires_at_its_minute_boundary_without_a_capture() {
        let (_root, context) = runtime("expiry", "http://127.0.0.1:1");
        // Armed at 1000 for one minute: still valid at 61000, expired at 61001.
        arm(&context, Some("dsh"), Some("1"), 1_000).unwrap();
        assert!(attempt(&context, Route::Dsh, 61_000, || json!({"at": "boundary"})));
        disarm(&context).unwrap();
        arm(&context, Some("dsh"), Some("1"), 1_000).unwrap();
        assert!(!attempt(&context, Route::Dsh, 61_001, || json!({"at": "late"})));
        assert!(captured(&context).is_none());
        assert_eq!(armed_or_claimed(&context), (false, false));
        // An expired arming for the other route is removed by any route's request.
        arm(&context, Some("harness"), Some("1"), 1_000).unwrap();
        assert!(!attempt(&context, Route::Dsh, 61_001, || json!({"at": "other"})));
        assert_eq!(armed_or_claimed(&context), (false, false));
    }

    #[test]
    fn concurrent_writers_capture_once() {
        // Writers in different processes meet only at the claim; eight threads stand in for them.
        let (_root, context) = runtime("concurrent", "http://127.0.0.1:1");
        arm(&context, Some("dsh"), None, 1_000).unwrap();
        let start = Arc::new(Barrier::new(8));
        let callers: Vec<_> = (0..8)
            .map(|caller| {
                let (context, start) = (context.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    persist(&context, Route::Dsh, 2_000, json!({"caller": caller}))
                })
            })
            .collect();
        let written = callers.into_iter().map(|caller| caller.join().unwrap()).filter(|wrote| *wrote).count();
        assert_eq!(written, 1);
        assert!(captured(&context).unwrap()["request"]["caller"].is_u64());
        assert_eq!(armed_or_claimed(&context), (false, false));
    }

    #[test]
    fn a_claim_left_by_an_interrupted_capture_blocks_capture_and_arming_until_disarm() {
        let (_root, context) = runtime("interrupted", "http://127.0.0.1:1");
        arm(&context, Some("dsh"), None, 1_000).unwrap();
        // A process stopped between its claim and its capture left the claim behind.
        context
            .write_atomic(RuntimeArea::Data, CLAIMED_FILE, br#"{"v":1,"route":"dsh","expiresAt":9000}"#)
            .unwrap();
        assert!(!attempt(&context, Route::Dsh, 2_000, || json!({"n": 1})));
        assert!(captured(&context).is_none());
        assert_eq!(status(&context, 2_000).unwrap()["interrupted"], true);
        assert_eq!(arm(&context, Some("dsh"), None, 2_000).unwrap_err().1, "an interrupted capture is left; run disarm");
        assert_eq!(disarm(&context).unwrap(), json!({"removed": ["armed", "interrupted"]}));
        assert_eq!(arm(&context, Some("dsh"), None, 2_000).unwrap()["armed"], true);
    }

    #[test]
    fn a_disarm_during_the_write_removes_the_capture() {
        let (_root, context) = runtime("disarm-race", "http://127.0.0.1:1");
        // The writer holds no claim any more: `disarm` removed it between the claim and the write.
        assert!(!write_claimed(&context, Route::Dsh, 2_000, &json!({"n": 1})));
        assert!(captured(&context).is_none());
        assert_eq!(armed_or_claimed(&context), (false, false));
    }

    #[test]
    fn a_failed_capture_write_uses_up_the_arming_and_keeps_the_file_it_found() {
        let (_root, context) = runtime("write-failure", "http://127.0.0.1:1");
        arm(&context, Some("dsh"), None, 1_000).unwrap();
        // The capture file appeared after the arming (the exclusive create then fails).
        context.create_new_file(RuntimeArea::Data, CAPTURED_FILE, br#"{"v":1,"route":"dsh","capturedAt":1,"request":{"n":0}}"#).unwrap();
        assert!(!attempt(&context, Route::Dsh, 2_000, || json!({"n": 1})));
        assert_eq!(captured(&context).unwrap()["request"], json!({"n": 0}));
        assert_eq!(armed_or_claimed(&context), (false, false));
    }

    #[test]
    fn an_unreadable_arming_captures_nothing_and_status_names_it() {
        let (_root, context) = runtime("unreadable", "http://127.0.0.1:1");
        context.create_new_file(RuntimeArea::Data, ARMED_FILE, br#"{"v":1,"route":"relay","expiresAt":9000}"#).unwrap();
        assert!(!attempt(&context, Route::Dsh, 2_000, || json!({"n": 1})));
        assert!(captured(&context).is_none());
        let state = status(&context, 2_000).unwrap();
        assert_eq!((state["armed"].clone(), state["unreadable"].clone()), (json!(true), json!(true)));
        assert_eq!(disarm(&context).unwrap(), json!({"removed": ["armed"]}));
    }

    #[test]
    fn arm_checks_its_route_minutes_and_existing_files() {
        let (_root, context) = runtime("arm", "http://127.0.0.1:1");
        for (route, minutes) in [(None, None), (Some("other"), None), (Some("dsh"), Some("0")), (Some("dsh"), Some("1441")), (Some("dsh"), Some("x"))] {
            assert_eq!(arm(&context, route, minutes, 1_000).unwrap_err().0, 2, "{route:?} {minutes:?}");
        }
        assert_eq!(arm(&context, Some("dsh"), Some("1440"), 1_000).unwrap()["expiresAt"], 1_000 + 1440 * 60_000);
        assert_eq!(arm(&context, Some("dsh"), None, 1_000).unwrap_err(), (1, "a capture is already armed".to_string()));
        assert!(attempt(&context, Route::Dsh, 2_000, || json!({"n": 1})));
        assert_eq!(arm(&context, Some("harness"), None, 3_000).unwrap_err(), (1, "a capture waits for sweep or disarm".to_string()));
        assert_eq!(status(&context, 3_000).unwrap()["captured"], true);
        assert_eq!(disarm(&context).unwrap(), json!({"removed": ["captured"]}));
        assert_eq!(arm(&context, Some("harness"), Some("1"), 3_000).unwrap()["expiresAt"], 63_000);
    }

    #[test]
    fn the_sweep_reports_counts_and_field_paths_and_never_text() {
        let request = json!({
            "payload": {"args": {"request": {"content": [
                {"type": "text", "text": "今天天气不错，dsh 你好呀"},
                {"type": "text", "text": "Ask the AI helper"},
            ]}}},
            "tools": [{"name": "qq_send_message", "description": "talks to deepseek and DeepSeek",
                "schema": {"properties": {"dshReady": {"type": "boolean"}, "雨天的消息": {"type": "string", "description": "模型"}}}}],
            "image": "A".repeat(300),
            "dataUrl": format!("data:image/png;base64,{}", "A".repeat(300)),
            "edge": format!("{}dsh", "dsh/".repeat(63)),
            "edgeBinary": "dsh/".repeat(64),
            "harness": "plain",
        });
        let report = sweep(&request);
        assert_eq!(report, json!({
            "strings": 34,
            "binaryStrings": 3,
            "deniedTotal": 6,
            "counts": {"AI": 1, "deepseek": 1, "dsh": 2, "harness": 1, "模型": 1},
            "fields": [
                {"path": "/payload/args/request/content/0/text", "part": "value", "terms": ["dsh"]},
                {"path": "/payload/args/request/content/1/text", "part": "value", "terms": ["AI"]},
                {"path": "/tools/0/description", "part": "value", "terms": ["deepseek"]},
                {"path": "/tools/0/schema/properties/<key>/description", "part": "value", "terms": ["模型"]},
                {"path": "/<key>", "part": "value", "terms": ["dsh"]},
                {"path": "/<key>", "part": "key", "terms": ["harness"]},
            ],
        }));
        let text = report.to_string();
        for message in ["今天天气不错", "你好", "Ask the", "talks to", "雨天", "plain", "dshReady", "edge"] {
            assert!(!text.contains(message), "{message}: {text}");
        }
    }

    #[test]
    fn a_key_outside_the_request_structure_is_never_printed() {
        let keys = sweep(&json!({"messages": [{"content": "dsh", "private-token-183": "dsh"}]}));
        let paths: Vec<&str> = keys["fields"].as_array().unwrap().iter().map(|field| field["path"].as_str().unwrap()).collect();
        assert_eq!(paths, vec!["/messages/0/content", "/messages/0/<key>"]);
        assert!(!keys.to_string().contains("private-token-183"), "{keys}");
    }

    #[test]
    fn a_request_over_the_limit_is_recorded_by_size_only() {
        let request = json!({"text": "fixture request 183"});
        let size = request.to_string().len();
        let kept: Value = serde_json::from_slice(&capture_record(Route::Dsh, 5, &request, size)).unwrap();
        assert_eq!(kept["request"], request);
        let dropped: Value = serde_json::from_slice(&capture_record(Route::Dsh, 5, &request, size - 1)).unwrap();
        assert_eq!(dropped, json!({"v": 1, "route": "dsh", "capturedAt": 5, "tooLarge": true, "bytes": size}));
    }

    #[test]
    fn sweeping_deletes_the_capture_and_a_second_sweep_finds_nothing() {
        let (_root, context) = runtime("sweep", "http://127.0.0.1:1");
        arm(&context, Some("dsh"), None, 1_000).unwrap();
        assert!(attempt(&context, Route::Dsh, 2_000, || json!({"text": "fixture wake 183 dsh"})));
        let report = sweep_capture(&context).unwrap();
        assert_eq!(report["route"], "dsh");
        assert_eq!(report["counts"], json!({"dsh": 1}));
        assert_eq!(report["fields"], json!([{"path": "/text", "part": "value", "terms": ["dsh"]}]));
        assert_eq!(report["removed"], true);
        assert!(!report.to_string().contains("fixture wake"));
        assert!(captured(&context).is_none());
        assert_eq!(sweep_capture(&context).unwrap_err(), (1, "nothing was captured; arm a capture first".to_string()));
        // A capture recorded by size only is reported and deleted, never swept.
        context
            .create_new_file(RuntimeArea::Data, CAPTURED_FILE, br#"{"v":1,"route":"harness","capturedAt":3,"tooLarge":true,"bytes":99}"#)
            .unwrap();
        assert_eq!(sweep_capture(&context).unwrap(), json!({"route": "harness", "capturedAt": 3, "tooLarge": true, "bytes": 99, "removed": true}));
        assert!(captured(&context).is_none());
    }

    #[test]
    fn an_invalid_capture_prints_none_of_its_values() {
        let (_root, context) = runtime("invalid", "http://127.0.0.1:1");
        // Metadata of the wrong type, and a capture of another shape, are refused without a value.
        for record in [
            r#"{"v":1,"route":"dsh","capturedAt":"PRIVATE_CHAT","request":{}}"#,
            r#"{"v":1,"route":"dsh","capturedAt":0,"tooLarge":true,"bytes":"PRIVATE_TOKEN"}"#,
            r#"{"v":2}"#,
        ] {
            context.write_atomic(RuntimeArea::Data, CAPTURED_FILE, record.as_bytes()).unwrap();
            assert_eq!(sweep_capture(&context).unwrap_err(), (1, "capture file invalid; run disarm".to_string()), "{record}");
        }
        assert_eq!(disarm(&context).unwrap(), json!({"removed": ["captured"]}));
    }

    #[test]
    fn the_capture_module_never_uses_panicking_print_macros() {
        // `println!` and `eprintln!` panic when their stream fails; this module writes through `note`.
        let print = ["print", "ln!("].concat();
        assert!(!include_str!("upstream_capture.rs").contains(&print), "the capture module uses a panicking print macro");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_armed_harness_capture_holds_the_filtered_request_with_its_replayed_tool_call() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", model.local_addr().unwrap());
        let (_root, context) = runtime("harness", &endpoint);
        let cfg = super::test_support::config();
        let token = crate::conversation_authority::ensure_or_rotate(&context, "private:8808", false).unwrap();
        let generation = crate::send_outbox::token_epoch(&token);
        // Two model steps: a call to a tool that does not exist (its error result is replayed), then
        // the reply. The relay arms the capture while it holds the first step, so the capture takes
        // the second: the filtered prompt, the replayed tool call and its result.
        let call = json!([{"index": 0, "id": "call-183", "type": "function", "function": {"name": "fixture_tool_183", "arguments": "{}"}}]);
        let steps = [
            (json!({"role": "assistant", "tool_calls": call}), "tool_calls"),
            (json!({"role": "assistant", "content": "fixture reply 183"}), "stop"),
        ];
        let responses: Vec<String> = steps
            .iter()
            .map(|(delta, finish)| {
                let chunk = json!({"id": "fake-183", "object": "chat.completion.chunk", "model": "fake-chat",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": null}]});
                let end = json!({"id": "fake-183", "object": "chat.completion.chunk", "model": "fake-chat",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]});
                let body = format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n");
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            })
            .collect();
        let arming = context.clone();
        let relay = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (step, response) in responses.into_iter().enumerate() {
                let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), model.accept())
                    .await
                    .expect("the relay received no request")
                    .unwrap();
                let mut bytes = Vec::new();
                let (start, length) = loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0, "truncated relay request");
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let length = String::from_utf8_lossy(&bytes[..split])
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                            .unwrap();
                        if bytes.len() >= split + 4 + length {
                            break (split + 4, length);
                        }
                    }
                };
                if step == 0 {
                    super::test_support::arm(&arming, "harness");
                }
                socket.write_all(response.as_bytes()).await.unwrap();
                requests.push(serde_json::from_slice::<Value>(&bytes[start..start + length]).unwrap());
            }
            requests
        });
        let harness = crate::harness::Harness::open(&context).unwrap();
        let mut turn = harness
            .admit(crate::harness::TurnRequest {
                id: "capture-183".into(),
                conversation: "private:8808".into(),
                generation: generation.clone(),
                prompt: "fixture prompt 183 STRIPPED183".into(),
                profile: "closed-agent".into(),
                route: crate::harness::ModelRoute::from_runtime(&context, &cfg).unwrap(),
                deadline: Duration::from_secs(10),
                tools: crate::harness::Tools::capture(&context, &cfg, "private:8808", &generation).unwrap(),
                limits: crate::harness::Limits::from_config(&cfg),
            })
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), turn.wait()).await.expect("the turn did not end");
        let wire = relay.await.unwrap();
        assert_eq!(result, Ok("fixture reply 183".to_string()));
        let record = settled(&context).expect("the armed harness request was captured");
        assert_eq!(record["route"], "harness");
        assert_eq!(record["request"]["model"], "fake-chat");
        assert_eq!(wire[1]["model"], "fake-chat");
        assert_eq!(armed_or_claimed(&context), (false, false));
        // Every text, tool call id and tool name the relay received in the second step is in the
        // capture, and the stripped word reached neither.
        fn strings(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::String(text) => out.push(text.clone()),
                Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
                Value::Object(fields) => fields.values().for_each(|item| strings(item, out)),
                _ => {}
            }
        }
        let mut held = Vec::new();
        strings(&record["request"]["request"], &mut held);
        // The wire spells content as a string or as parts with a "text" field.
        let mut sent = Vec::new();
        for message in wire[1]["messages"].as_array().unwrap() {
            match &message["content"] {
                Value::String(text) => sent.push(text.clone()),
                Value::Array(parts) => sent.extend(parts.iter().filter_map(|part| part["text"].as_str().map(str::to_string))),
                _ => {}
            }
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                sent.push(call["id"].as_str().unwrap().to_string());
                sent.push(call["function"]["name"].as_str().unwrap().to_string());
            }
            if let Some(id) = message["tool_call_id"].as_str() {
                sent.push(id.to_string());
            }
        }
        for tool in wire[1]["tools"].as_array().into_iter().flatten() {
            sent.push(tool["function"]["name"].as_str().unwrap().to_string());
        }
        sent.retain(|text| !text.is_empty());
        assert!(sent.iter().any(|text| text == "call-183"), "the second step replays the tool call: {sent:?}");
        assert!(sent.iter().any(|text| text.starts_with("fixture prompt 183")), "{sent:?}");
        assert!(!wire[1].to_string().contains("STRIPPED183"), "the relay received text the model view strips");
        for text in &sent {
            assert!(held.contains(text), "the relay received text the capture lacks: {text}");
        }
        assert!(!record.to_string().contains("STRIPPED183"), "the capture holds text the model view strips");
        // The capture holds no credential.
        assert!(!record.to_string().contains("synthetic-model-key-183"));
    }
}
