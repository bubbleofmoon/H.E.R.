//! JavaScript string semantics the frozen auxiliary servers rely on (`trim`, `\s`, `slice` by UTF-16 unit,
//! `Number(...)`, truthiness), so queries and results are cut and cleaned exactly where the JS cuts them.

use serde_json::Value;

/// JS `\s` / `String.prototype.trim` white space (WhiteSpace and LineTerminator), which differs from Unicode White_Space.
pub(crate) fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}'
            | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

pub(crate) fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

/// `text.replace(/\s+/g, ' ')`.
pub(crate) fn collapse_js_space(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `text.slice(0, units)` counted in UTF-16 code units. A surrogate pair cut in half cannot be held by a Rust
/// `String`; the whole pair is dropped (documented deviation, only the last character of an over-long text).
pub(crate) fn slice_utf16(text: &str, units: usize) -> String {
    let mut used = 0usize;
    let mut out = String::new();
    for c in text.chars() {
        used += c.len_utf16();
        if used > units {
            break;
        }
        out.push(c);
    }
    out
}

/// `[\u0000-\u001f\u007f]` replaced by a space, then `\s+` collapsed, trimmed and cut to 120 UTF-16 units; CQ codes
/// (`[CQ:...]`) are replaced by a space first (the MCP layer's `sanitizeQuery`).
pub(crate) fn sanitize_query(query: &str) -> String {
    use std::sync::OnceLock;
    static CQ: OnceLock<regex::Regex> = OnceLock::new();
    let cq = CQ.get_or_init(|| regex::Regex::new(r"(?i)\[CQ:[^\]]*\]").expect("CQ pattern"));
    let without_cq = cq.replace_all(query, " ");
    let without_control: String = without_cq
        .chars()
        .map(|c| if (c as u32) <= 0x1f || c == '\u{7f}' { ' ' } else { c })
        .collect();
    let collapsed = collapse_js_space(&without_control);
    slice_utf16(js_trim(&collapsed), 120)
}

/// JS `String(value)` for the scalar JSON values the response mappers see.
pub(crate) fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number_text(n.as_f64().unwrap_or(0.0), n),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|v| if v.is_null() { String::new() } else { js_string(v) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn js_number_text(f: f64, n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    if f.fract() == 0.0 && f.abs() < 1e21 {
        return format!("{}", f as i128);
    }
    n.to_string()
}

/// JS truthiness of a JSON value.
pub(crate) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `Number(value)`; `None` is NaN.
pub(crate) fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Null => Some(0.0),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => js_string_to_number(s),
        Value::Array(items) => match items.as_slice() {
            [] => Some(0.0),
            [single] if !single.is_object() && !single.is_array() => js_number(&Value::String(js_string(single))),
            _ => None,
        },
        Value::Object(_) => None,
    }
}

fn js_string_to_number(text: &str) -> Option<f64> {
    let t = js_trim(text);
    if t.is_empty() {
        return Some(0.0);
    }
    let radix = |digits: &str, base: u32| -> Option<f64> {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
            return None;
        }
        let mut acc = 0f64;
        for c in digits.chars() {
            acc = acc * f64::from(base) + f64::from(c.to_digit(base)?);
        }
        Some(acc)
    };
    let lower = t.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("0x") {
        return radix(rest, 16);
    }
    if let Some(rest) = lower.strip_prefix("0o") {
        return radix(rest, 8);
    }
    if let Some(rest) = lower.strip_prefix("0b") {
        return radix(rest, 2);
    }
    match t {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // Rust also accepts "inf"/"nan" spellings that JS reads as NaN.
    if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    t.parse::<f64>().ok()
}

/// `num(value, fallback, min, max)` of the JS search config: NaN/infinite -> fallback, else rounded and clamped.
pub(crate) fn clamped_int(value: Option<&Value>, fallback: i64, min: i64, max: i64) -> i64 {
    let Some(n) = value.and_then(js_number).filter(|n| n.is_finite()) else {
        return fallback;
    };
    let rounded = (n + 0.5).floor();
    (rounded.clamp(min as f64, max as f64)) as i64
}

/// JSON text with every unpaired `\uD800`-`\uDFFF` escape replaced by `\uFFFD`, or `None` when it has none.
/// A `String` cannot hold a lone UTF-16 unit, so serde_json refuses such a text; JS keeps the unit.
/// An escaped backslash (`\\uD800`) is plain text and is left alone.
pub(crate) fn replace_unpaired_surrogate_escapes(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let unit = |at: usize| -> Option<u16> {
        let digits = bytes.get(at..at + 6)?;
        if digits[0] != b'\\' || digits[1] != b'u' {
            return None;
        }
        u16::from_str_radix(std::str::from_utf8(&digits[2..]).ok()?, 16).ok()
    };
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        match unit(i) {
            Some(0xD800..=0xDBFF) if matches!(unit(i + 6), Some(0xDC00..=0xDFFF)) => i += 12,
            Some(0xD800..=0xDFFF) => {
                out.push_str(&text[copied..i]);
                out.push_str("\\uFFFD");
                changed = true;
                i += 6;
                copied = i;
            }
            Some(_) => i += 6,
            None => i += 2,
        }
    }
    out.push_str(&text[copied..]);
    changed.then_some(out)
}

/// `serde_json::from_str`, except that a text refused only because of unpaired surrogate escapes is parsed
/// again with those replaced by U+FFFD (accepted deviation 18). The second value is the repaired text when
/// that happened, so the caller can log it and keep the repaired text instead of the original.
pub(crate) fn parse_json_replacing_lone_surrogates<T: serde::de::DeserializeOwned>(
    text: &str,
) -> Result<(T, Option<String>), serde_json::Error> {
    match serde_json::from_str::<T>(text) {
        Ok(value) => Ok((value, None)),
        Err(error) => match replace_unpaired_surrogate_escapes(text) {
            Some(repaired) => serde_json::from_str::<T>(&repaired).map(|value| (value, Some(repaired))),
            None => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_follows_the_js_rules() {
        assert_eq!(sanitize_query("[CQ:at,qq=123]  "), "");
        assert_eq!(sanitize_query("  a\u{0}b\t\n[cq:face,id=1]c\u{3000}d\u{FEFF} "), "a b c d");
        assert_eq!(sanitize_query(&"x".repeat(200)).len(), 120);
        // 119 ASCII + an astral character: the pair would be cut in half at 120 UTF-16 units.
        assert_eq!(sanitize_query(&format!("{}\u{1F600}", "y".repeat(119))), "y".repeat(119));
        assert_eq!(sanitize_query(&format!("{}\u{1F600}", "y".repeat(118))), format!("{}\u{1F600}", "y".repeat(118)));
    }

    #[test]
    fn js_numbers_and_clamping() {
        assert_eq!(clamped_int(Some(&json!("12")), 8, 1, 20), 12);
        assert_eq!(clamped_int(Some(&json!(null)), 8, 1, 20), 1);
        assert_eq!(clamped_int(Some(&json!("abc")), 8, 1, 20), 8);
        assert_eq!(clamped_int(Some(&json!(99)), 8, 1, 20), 20);
        assert_eq!(clamped_int(Some(&json!(2.5)), 8, 1, 20), 3);
        assert_eq!(clamped_int(None, 8, 1, 20), 8);
        assert_eq!(clamped_int(Some(&json!("0x10")), 8, 1, 20), 16);
        assert_eq!(clamped_int(Some(&json!("inf")), 8, 1, 20), 8);
    }

    #[test]
    fn unpaired_surrogate_escapes_become_u_fffd_and_pairs_survive() {
        let r = replace_unpaired_surrogate_escapes;
        assert_eq!(r(r#"{"t":"a\ud83d b"}"#).as_deref(), Some(r#"{"t":"a\uFFFD b"}"#));
        assert_eq!(r(r#"{"t":"\ude00"}"#).as_deref(), Some(r#"{"t":"\uFFFD"}"#));
        assert_eq!(r(r#"{"t":"\ude00\ud83d"}"#).as_deref(), Some(r#"{"t":"\uFFFD\uFFFD"}"#), "low before high is two lone units");
        assert_eq!(r(r#"{"t":"\ud83d\ud83d\ude00"}"#).as_deref(), Some(r#"{"t":"\uFFFD\ud83d\ude00"}"#));
        assert_eq!(r(r#"{"t":"\ud83d\ude00"}"#), None, "a valid pair is untouched");
        assert_eq!(r(r#"{"t":"\\ud83d"}"#), None, "an escaped backslash makes it plain text");
        assert_eq!(r(r#"{"t":"\\\ud83d"}"#).as_deref(), Some(r#"{"t":"\\\uFFFD"}"#));
        assert_eq!(r(r#"{"t":"\ud83"#), None, "a truncated escape is not ours to repair");
        assert_eq!(r("{\"t\":\"\u{1F600}\"}"), None);
        let (value, repaired) = parse_json_replacing_lone_surrogates::<Value>(r#"{"t":"x\ud83dy"}"#).unwrap();
        assert_eq!(value["t"], json!("x\u{fffd}y"));
        assert!(repaired.is_some());
        let (value, repaired) = parse_json_replacing_lone_surrogates::<Value>(r#"{"t":"ok"}"#).unwrap();
        assert_eq!((value["t"].clone(), repaired), (json!("ok"), None));
        assert!(parse_json_replacing_lone_surrogates::<Value>(r#"{"t":"\ud83d"#).is_err());
    }
}
