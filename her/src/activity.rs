//! The QQ activity log (`qq-activity.log`): one line per send, receive or notable event, shown in
//! the administrator status (`GET /api/status` -> `activity`).
//!
//! Port of the JS `appendActivity` / `createCappedLogger` / `readActivityTail`:
//! - a line is `[HH:MM:SS] <text>` in local time, newlines collapsed to one space, sensitive text
//!   and known session tokens replaced by `***` before it is written;
//! - the file is rotated to `qq-activity.log.1` (overwriting the previous backup) once it holds more
//!   than 500 lines;
//! - the tail is the last `n` non-empty lines joined by `\n`, empty when the file is missing.
//!
//! Writing is best effort, as in JS: a failed append never fails the action it describes. The
//! file belongs to the selected runtime root (`data/`); the legacy context uses the old state
//! directory only when that directory already exists.

use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub const FILE: &str = "qq-activity.log";
const BACKUP: &str = "qq-activity.log.1";
const MAX_LINES: usize = 500;

/// Serializes append + rotation inside one process; the append itself is also OS-locked.
static APPEND_GATE: Mutex<()> = Mutex::new(());

// One selected runtime per live/console process. Protocol responses use their own
// writers; diagnostic macros consult this context, including after token rotation.
static LOG_CONTEXT: OnceLock<RuntimeContext> = OnceLock::new();

pub fn set_log_context(context: &RuntimeContext) {
    let _ = LOG_CONTEXT.set(context.clone());
}

/// Mask complete known tokens and the minted 48-hex shape in the original text.
/// Merge overlapping matches before replacement so one mask cannot hide another
/// token's match. Longer ordinary hashes (such as SHA-256) are not token shapes.
pub fn mask_session_tokens(text: &str, known: &[String]) -> String {
    mask_ranges(text, session_token_ranges(text, known), "***")
}

/// Match against the original text so another redactor cannot hide a token.
pub(crate) fn session_token_ranges(text: &str, known: &[String]) -> Vec<(usize, usize)> {
    static SHAPE: OnceLock<regex::Regex> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| regex::Regex::new(r"[0-9a-fA-F]{48}").unwrap());
    let mut ranges: Vec<(usize, usize)> = shape.find_iter(text)
        .filter(|m| {
            (m.start() == 0 || !text.as_bytes()[m.start() - 1].is_ascii_hexdigit())
                && (m.end() == text.len() || !text.as_bytes()[m.end()].is_ascii_hexdigit())
        })
        .map(|m| (m.start(), m.end())).collect();
    for token in known.iter().filter(|token| !token.is_empty()) {
        ranges.extend(text.match_indices(token.as_str()).map(|(start, token)| (start, start + token.len())));
    }
    // A known token can be wrapped between any two characters, including escaped
    // whitespace from Debug/JSON formatting. Match a compact view but replace the
    // entire original span. Shape recognition above stays contiguous-only.
    if !known.is_empty() && (text.chars().any(char::is_whitespace) || text.contains('\\')) {
        let mut compact = String::with_capacity(text.len());
        let mut positions = Vec::new();
        let mut chars = text.char_indices().peekable();
        while let Some((start, c)) = chars.next() {
            if c.is_whitespace() { continue; }
            if c == '\\' && chars.peek().is_some_and(|(_, next)| matches!(next, 'n' | 'r' | 't')) {
                chars.next();
                continue;
            }
            positions.push((compact.len(), start, start + c.len_utf8()));
            compact.push(c);
        }
        for token in known.iter().filter(|t| !t.is_empty() && !t.chars().any(char::is_whitespace)) {
            for (start, _) in compact.match_indices(token.as_str()) {
                let end = start + token.len();
                let first = positions.binary_search_by_key(&start, |p| p.0).unwrap();
                let last = positions.partition_point(|p| p.0 < end) - 1;
                ranges.push((positions[first].1, positions[last].2));
            }
        }
    }
    ranges
}

/// Combine overlapping token/sensitive spans while preserving adjacent masks.
pub(crate) fn mask_ranges(text: &str, mut ranges: Vec<(usize, usize)>, mask: &str) -> String {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut().filter(|last| start < last.1) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for (start, end) in merged {
        out.push_str(&text[from..start]);
        out.push_str(mask);
        from = end;
    }
    out.push_str(&text[from..]);
    out
}

/// Redact structured string values before JSON escaping can hide line-wrapped tokens.
/// Preserve the record's numeric/boolean/null types and nesting.
pub fn mask_json_session_tokens(value: &Value, known: &[String]) -> Value {
    map_log_strings(value, &|text| mask_session_tokens(text, known))
}

fn map_log_strings(value: &Value, redact: &dyn Fn(&str) -> String) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(|item| map_log_strings(item, redact)).collect()),
        Value::Object(fields) => Value::Object(fields.iter()
            .map(|(key, value)| (redact(key), map_log_strings(value, redact))).collect()),
        _ => value.clone(),
    }
}

/// Mask decoded JSON string literals and the complete raw source while preserving
/// other lexemes. Never call this with an already shortened source.
pub fn mask_log_text(text: &str, known: &[String]) -> String {
    // Decode only quoted strings: parsing the whole record could reinterpret a
    // 48-digit token as a number before masking, or change ordinary numeric lexemes.
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut from) = (0, 0);
    while i < bytes.len() {
        if bytes[i] != b'"' { i += 1; continue; }
        let start = i;
        i += 1;
        let mut escaped = false;
        while i < bytes.len() {
            let byte = bytes[i];
            i += 1;
            if escaped { escaped = false; continue; }
            if byte == b'\\' { escaped = true; continue; }
            if byte != b'"' { continue; }
            if let Ok(decoded) = serde_json::from_str::<String>(&text[start..i]) {
                let safe = mask_session_tokens(&decoded, known);
                if safe != decoded {
                    out.push_str(&text[from..start]);
                    out.push_str(&serde_json::to_string(&safe).expect("a JSON string can be serialized"));
                    from = i;
                }
            }
            break;
        }
    }
    out.push_str(&text[from..]);
    mask_session_tokens(&out, known)
}

/// Token-only redaction for diagnostics: independent of outbound message policy.
pub fn redact_log_line(text: &str) -> String {
    let known = LOG_CONTEXT.get().map(known_session_tokens).unwrap_or_default();
    mask_session_tokens(text, &known)
}

/// Token-safe diagnostic prefix, counted in Unicode characters (not UTF-16 units).
/// Always pass the complete source, including text beyond the visible boundary.
pub fn preview_chars(text: &str, n: usize) -> String {
    let known = LOG_CONTEXT.get().map(known_session_tokens).unwrap_or_default();
    mask_log_text(text, &known).chars().take(n).collect()
}

/// Context-aware diagnostic prefix, including the existing sensitive-text mask.
pub fn redacted_preview(context: &RuntimeContext, text: &str, n: usize) -> String {
    // Preserve original credential key/value spans before decoding quoted tokens.
    let safe = redact(context, text);
    mask_log_text(&safe, &known_session_tokens(context)).chars().take(n).collect()
}

/// Session tokens of the conversations in `social-v2.json` (JS `KNOWN_AGENT_TOKENS`), cached by file
/// identity so a busy log does not re-parse the state on every line.
pub fn known_session_tokens(context: &RuntimeContext) -> Vec<String> {
    known_tokens(context)
}

fn known_tokens(context: &RuntimeContext) -> Vec<String> {
    type Cache = HashMap<String, ((u64, std::time::SystemTime), Vec<String>)>;
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
    let scope = context
        .runtime_root()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "legacy".to_string());
    let identity = context
        .resolve_path(RuntimeArea::Data, "social-v2.json", PathIntent::ReadOptionalFile)
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|m| (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
    let Some(identity) = identity else { return Vec::new() };
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some((seen, tokens)) = cache.get(&scope) {
        if *seen == identity {
            return tokens.clone();
        }
    }
    let tokens: Vec<String> = context
        .read_text(RuntimeArea::Data, "social-v2.json")
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|state| {
            state.get("conversations").and_then(Value::as_object).map(|conversations| {
                conversations
                    .values()
                    .filter_map(|c| c.get("agentToken").and_then(Value::as_str))
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .collect()
            })
        })
        .unwrap_or_default();
    cache.insert(scope, (identity, tokens.clone()));
    tokens
}

/// JS `redactSensitiveText` with this runtime's known session tokens.
pub fn redact(context: &RuntimeContext, text: &str) -> String {
    crate::pipeline::redact_sensitive_text(text, &known_tokens(context))
}

/// One finished log line (without the clock prefix and newline) for `text`.
fn one_line(context: &RuntimeContext, text: &str) -> String {
    collapse_newlines(&redact(context, text))
}

/// JS `String(line).replace(/[\r\n]+/g, ' ')`.
fn collapse_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if c == '\r' || c == '\n' {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            in_run = false;
            out.push(c);
        }
    }
    out
}

/// JS `appendActivity(line)`.
pub fn append(context: &RuntimeContext, line: &str) {
    if !context.is_isolated() && !legacy_state_dir_exists(context) {
        return;
    }
    let text = format!(
        "[{}] {}\n",
        crate::prompt::local_clock(crate::parity_clock::now_ms()),
        one_line(context, line)
    );
    let _gate = APPEND_GATE.lock().unwrap_or_else(|p| p.into_inner());
    if context.append_bytes(RuntimeArea::Data, FILE, text.as_bytes()).is_err() {
        return;
    }
    let lines = context
        .read_text(RuntimeArea::Data, FILE)
        .map(|t| t.split('\n').filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    if lines > MAX_LINES {
        let _ = context.remove_file(RuntimeArea::Data, BACKUP);
        if context.rename_file(RuntimeArea::Data, FILE, BACKUP).is_err() {
            // Rename refused (busy): truncate so the file cannot grow without bound.
            let _ = context.write_atomic(RuntimeArea::Data, FILE, b"");
        }
    }
}

fn legacy_state_dir_exists(context: &RuntimeContext) -> bool {
    context.data_dir().is_dir()
}

/// JS `readActivityTail(n)`.
pub fn tail(context: &RuntimeContext, n: usize) -> String {
    let text = if context.is_isolated() {
        context.read_optional_text(RuntimeArea::Data, FILE).ok().flatten()
    } else {
        context.read_text(RuntimeArea::Data, FILE).ok()
    };
    let Some(text) = text else { return String::new() };
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Mask session tokens before taking `n` UTF-16 units for an activity/log preview, never splitting a pair.
pub fn slice(text: &str, n: usize) -> String {
    crate::social_mem::truncate_text(&redact_log_line(text), n as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(label: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = crate::install_root::development_root(std::env::temp_dir().join(format!("her-activity-{label}-{}-{stamp}", std::process::id())));
        std::fs::create_dir_all(path.join("data")).unwrap();
        std::fs::create_dir_all(path.join("config")).unwrap();
        std::fs::create_dir_all(path.join("resources")).unwrap();
        std::fs::write(path.join("config/config.json"), r#"{"consoleToken":"activity-test-token-0123"}"#).unwrap();
        std::fs::write(path.join("data/social-v2.json"), r#"{"conversations":{}}"#).unwrap();
        std::fs::write(
            path.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:43101","model":"http://127.0.0.1:43102","onebot":"http://127.0.0.1:43103"}"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn hotfix128_fix2_known_tokens_allow_debug_and_json_breaks() {
        let known = vec!["K128sessionVALUExyz".to_string()];
        for separator in ["\n", "\r", "\r\n", "\t", r"\n", r"\r", r"\r\n", r"\t", " \\n\t"] {
            let wrapped = format!("K128session{separator}VALUExyz");
            let rendered = if separator.contains('\\') { vec![wrapped.clone()] }
                else { vec![wrapped.clone(), format!("Err({wrapped:?})"), json_for_test(&wrapped)] };
            for text in rendered {
                assert_eq!(mask_session_tokens(&text, &known).matches("***").count(), 1, "{separator:?}: {text}");
                assert!(!mask_session_tokens(&text, &known).contains("K128session"), "{text}");
            }
        }
        // Ordinary escapes and unknown wrapped hex hashes are not session tokens.
        assert_eq!(mask_session_tokens(r"ordinary\ntext\tvalue", &known), r"ordinary\ntext\tvalue");
        assert_eq!(mask_session_tokens(r"0123456789abcdef01234567\n89abcdef0123456789abcdef", &known),
            r"0123456789abcdef01234567\n89abcdef0123456789abcdef");
        assert_eq!(mask_session_tokens("Unicode前雪\\n花後", &["雪花".into()]), "Unicode前***後");
    }

    fn json_for_test(text: &str) -> String { serde_json::json!({"error":text}).to_string() }

    #[test]
    fn hotfix128_fix1_json_previews_mask_strings_without_reinterpreting_other_text() {
        let known = vec!["known-session-token-128".to_string()];
        let raw = format!(r#" {{ "n":{}, "text":"known-session-\n token-128", "flag":true }} "#, "1".repeat(48));
        assert_eq!(mask_log_text(&raw, &known), r#" { "n":***, "text":"***", "flag":true } "#);
        let ordinary = r#"{"quote":"say \"hello\"","n":1e+09,"nested":[true,null]}"#;
        assert_eq!(mask_log_text(ordinary, &known), ordinary);
    }

    #[test]
    fn hotfix128_fix1_context_previews_preserve_credential_assignment_masking() {
        let dir = root("json-credential-preview");
        std::fs::write(dir.join("data/social-v2.json"),
            r#"{"conversations":{"group:1":{"agentToken":"token"}}}"#).unwrap();
        let context = RuntimeContext::from_root(&dir).unwrap();
        for label in ["token", "password"] {
            let source = format!(r#"{{"{label}":"visible-secret-value","ok":true}}"#);
            let preview = redacted_preview(&context, &source, 200);
            assert!(!preview.contains("visible-secret-value"), "credential assignment leaked: {preview}");
            assert!(preview.contains("***") && preview.contains("ok"), "{preview}");
        }
        let mixed = r#"{"password":"visible-secret-value","detail":"t\n oken","ok":true}"#;
        let preview = redacted_preview(&context, mixed, 200);
        assert!(!preview.contains("visible-secret-value") && !preview.contains("oken"), "{preview}");
        assert!(preview.contains("***") && preview.contains("ok"), "{preview}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hotfix128_fix1_all_diagnostic_prefix_limits_mask_complete_sources() {
        let dir = root("prefix-limits");
        std::fs::write(dir.join("data/social-v2.json"),
            r#"{"conversations":{"group:1":{"agentToken":"K128sessionVALUExyz"}}}"#).unwrap();
        let context = RuntimeContext::from_root(&dir).unwrap();
        // CLI/probe/auxiliary diagnostics, failover/health/errors, raw stream frames.
        for limit in [40, 80, 200, 300, 400] {
            let prefix = "x ".repeat((limit - 10) / 2);
            for token in ["K128sessionVALUExyz", "0123456789abcdef0123456789abcdef0123456789abcdef"] {
                let source = format!("{prefix}{token} trailing");
                assert_eq!(redacted_preview(&context, &source, limit), format!("{prefix}*** traili"));
                assert_eq!(source, format!("{prefix}{token} trailing"), "a preview must not alter its input");
                if token.len() == 48 {
                    assert_eq!(preview_chars(&source, limit), format!("{prefix}*** traili"));
                }
            }
        }
        assert_eq!(redacted_preview(&context, "ordinary 文本", 40), "ordinary 文本");
        // A token consisting only of digits is raw diagnostic text, even if it parses as JSON.
        assert_eq!(preview_chars(&"1".repeat(48), 40), "***");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hotfix128_wrapped_fix1_known_tokens_mask_before_activity_flattening() {
        let dir = root("wrapped");
        std::fs::write(dir.join("data/social-v2.json"),
            r#"{"conversations":{"group:1":{"agentToken":"known-session-token-128"}}}"#).unwrap();
        let context = RuntimeContext::from_root(&dir).unwrap();
        for newline in ["\n", "\r\n"] {
            append(&context, &format!("prefix known-session- {newline} token-128 suffix"));
            assert!(tail(&context, 1).ends_with("] prefix *** suffix"), "{}", tail(&context, 1));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hotfix128_wrapped_fix1_whitespace_between_known_characters_only() {
        let known = vec!["K128sessionVALUExyz".to_string(), "会话☃甲乙".to_string(), "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdef".to_string()];
        for token in &known {
            for whitespace in ["\n", "\r\n", " \r\n \t", " "] {
                let wrapped = token.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(whitespace);
                assert_eq!(mask_session_tokens(&format!("prefix {wrapped} suffix"), &known), "prefix *** suffix");
            }
        }
        let unknown = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let wrapped = format!("{}\n{}", &unknown[..24], &unknown[24..]);
        assert_eq!(mask_session_tokens(&wrapped, &known), wrapped, "unknown shapes stay contiguous-only");
        assert_eq!(mask_session_tokens(&"a".repeat(64), &known), "a".repeat(64));
    }

    #[test]
    fn lines_are_clocked_single_line_and_redacted() {
        let dir = root("line");
        std::fs::write(
            dir.join("data/social-v2.json"),
            r#"{"conversations":{"group:1":{"agentToken":"sess-token-0001"}}}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&dir).unwrap();
        append(&context, "group:1 a\r\nb\n\nc path /root/x/y with sess-token-0001 token=abc12345");
        let log = std::fs::read_to_string(dir.join("data/qq-activity.log")).unwrap();
        let (clock, rest) = log.split_once("] ").unwrap();
        assert!(clock.starts_with('[') && clock.len() == 9 && clock.as_bytes()[3] == b':', "{log}");
        assert_eq!(rest, "group:1 a b c path *** with *** ***\n");
        assert_eq!(tail(&context, 100), log.trim_end_matches('\n'));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_48_hex_session_tokens_are_masked_without_changing_hashes_or_ordinary_text() {
        let dir = root("token-shape");
        let context = RuntimeContext::from_root(&dir).unwrap();
        let token = "0123456789ABCDEF0123456789abcdef0123456789ABCDEF";
        let hash = "a".repeat(64);
        append(&context, &format!("ordinary {token} repeated {token}; hash {hash}"));
        let log = tail(&context, 1);
        assert!(log.ends_with(&format!("ordinary *** repeated ***; hash {hash}")), "{log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_known_token_cannot_hide_a_credential_assignment_from_redaction() {
        let dir = root("overlapping-token");
        std::fs::write(dir.join("data/social-v2.json"),
            r#"{"conversations":{"group:1":{"agentToken":"token"}}}"#).unwrap();
        let context = RuntimeContext::from_root(&dir).unwrap();
        append(&context, "token=another-secret-128");
        let log = tail(&context, 1);
        assert!(log.ends_with("] ***"), "the entire credential assignment must remain redacted: {log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rotation_keeps_one_backup_and_tail_takes_the_last_lines() {
        let dir = root("rotate");
        let context = RuntimeContext::from_root(&dir).unwrap();
        for i in 0..501 {
            append(&context, &format!("line {i}"));
        }
        // The 501st line pushed the count over 500: everything moved to the backup.
        assert!(!dir.join("data/qq-activity.log").exists() || std::fs::read_to_string(dir.join("data/qq-activity.log")).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(dir.join("data/qq-activity.log.1")).unwrap().lines().count(), 501);
        append(&context, "fresh");
        assert_eq!(tail(&context, 100).lines().count(), 1);
        for i in 0..150 {
            append(&context, &format!("more {i}"));
        }
        let tail100 = tail(&context, 100);
        assert_eq!(tail100.lines().count(), 100);
        assert!(tail100.lines().last().unwrap().ends_with("more 149"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_log_has_an_empty_tail() {
        let dir = root("missing");
        let context = RuntimeContext::from_root(&dir).unwrap();
        assert_eq!(tail(&context, 100), "");
        let _ = std::fs::remove_dir_all(dir);
    }
}
