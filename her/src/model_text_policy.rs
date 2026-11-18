//! The #142 denied-term policy for model-bound text (#243): the one term list and the one check (`denied_terms`) that
//! every model-text check calls. Callers: text settings on save and load (`settings::kinds::text`), the model-view strip
//! (`model_view::strip_model_text`), the history summary (`harness::handover::accept`), the DSH persona guard
//! (`dsh_profile`), the upstream capture sweep (`upstream_capture::sweep`) and the rendered-surface guard
//! (`console::tests::model_text_guard_tests`). No other code matches denied terms, so no caller can pick a weaker check.

/// Latin terms, matched case-insensitively as whole words (an ASCII letter or digit on either side is not a match, so
/// `dshReady` or `qq_send_message` pass while `user/dsh` and `deepseek娘` fail).
const DENIED_WORDS: [&str; 13] = [
    "deepseek", "dsh", "harness", "opencode", "codex", "claude", "anthropic", "astrbot", "cliproxy", "antigravity",
    "openai", "chatgpt", "gpt",
];

/// Phrases matched case-insensitively anywhere.
const DENIED_PHRASES: [&str; 9] = ["cli-proxy", "人工智能", "机器人", "语言模型", "大模型", "模型", "智能体", "助手", "深度求索"];

/// The #142 denied terms in model-bound text, each once. The text is read four ways, and a term any of them finds
/// counts: with invisible format characters removed, so a split spelling (`D` and `SH` around a zero-width space) is
/// found; with them read as word boundaries, as the text is shown; with each of them both a boundary and a joiner
/// (`denied_words_across_format`), so a word split by a mix of the two is found; and as written, so a term beside a
/// character that folding turns into an ASCII letter (`ｘdsh`, `ﬀAI`) still counts. The first three fold full-width
/// ASCII and read letters in upper case, so `ＤＳＨ` and `ai` are found. The terms come in that order, the first
/// reading's first.
pub(crate) fn denied_terms(text: &str) -> Vec<String> {
    let readings = [
        plain_terms(&fold_for_match(text, "")),
        plain_terms(&fold_for_match(text, " ")),
        denied_words_across_format(text),
        plain_terms(text),
    ];
    let mut denied: Vec<String> = Vec::new();
    for term in readings.into_iter().flatten() {
        if !denied.contains(&term) {
            denied.push(term);
        }
    }
    denied
}

/// The denied terms `text` holds as written, each once: `DENIED_WORDS` as whole words in any case, `AI` in upper case
/// with no ASCII letter on either side (so 反AI味 fails while `lastAiReplyAt`, `API` and `EMAIL` pass), and
/// `DENIED_PHRASES` anywhere in any case. Only `denied_terms` calls it, on the text and on its folded forms.
fn plain_terms(text: &str) -> Vec<String> {
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

/// Whether `c` is an invisible format character (general category Cf), which the check reads through.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}' | '\u{0890}'..='\u{0891}'
            | '\u{08E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}' | '\u{110CD}' | '\u{13430}'..='\u{13438}' | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}' | '\u{E0001}' | '\u{E0020}'..='\u{E007F}'
    )
}

/// One character folded for matching: full-width ASCII becomes ASCII and the ideographic space a space.
fn fold_char(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        other => other,
    }
}

/// The text folded for matching: each invisible format character becomes `invisible`, full-width ASCII is folded and
/// letters are upper case.
fn fold_for_match(text: &str, invisible: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for c in text.chars() {
        if is_invisible_format(c) {
            folded.push_str(invisible);
        } else {
            folded.push(fold_char(c));
        }
    }
    folded.to_uppercase()
}

/// The denied words of `text` (`DENIED_WORDS`, and `AI`) when each invisible format character is both a word boundary
/// and a joiner. The text is the folded upper-case characters, each marked when a removed format run precedes it. A
/// word matches with its inner characters contiguous, and each side of it may be a removed run or a character that is
/// not a word character (`word_char` names the word characters).
fn denied_words_across_format(text: &str) -> Vec<String> {
    let mut upper: Vec<(char, bool)> = Vec::new();
    let mut lower: Vec<(char, bool)> = Vec::new();
    let mut run = false;
    for c in text.chars() {
        if is_invisible_format(c) {
            run = true;
            continue;
        }
        let mut boundary = std::mem::take(&mut run);
        for u in fold_char(c).to_uppercase() {
            upper.push((u, boundary));
            for (i, l) in u.to_lowercase().enumerate() {
                lower.push((l, boundary && i == 0));
            }
            boundary = false;
        }
    }
    let mut found = Vec::new();
    for word in DENIED_WORDS {
        if spans_with_edges(&lower, word, |c| c.is_ascii_alphanumeric()) {
            found.push(word.to_string());
        }
    }
    if spans_with_edges(&upper, "AI", |c| c.is_ascii_alphabetic()) {
        found.push("AI".to_string());
    }
    found
}

/// Whether `term` occurs in `units` (character, removed-run-before flag) with both edges boundaries: the character
/// before it is a removed run or not a word character, and so is the character after it.
fn spans_with_edges(units: &[(char, bool)], term: &str, word_char: fn(char) -> bool) -> bool {
    let term: Vec<char> = term.chars().collect();
    if term.is_empty() || units.len() < term.len() {
        return false;
    }
    (0..=units.len() - term.len()).any(|start| {
        let end = start + term.len();
        units[start..end].iter().map(|unit| unit.0).eq(term.iter().copied())
            && (start == 0 || units[start].1 || !word_char(units[start - 1].0))
            && (end == units.len() || units[end].1 || !word_char(units[end].0))
    })
}

#[cfg(test)]
mod tests {
    use super::denied_terms;

    #[test]
    fn denied_term_policy_finds_whole_words_and_ai_in_any_case() {
        assert_eq!(denied_terms("【反AI味】"), vec!["AI".to_string()]);
        assert_eq!(denied_terms("AI 已暂停"), vec!["AI".to_string()]);
        assert_eq!(
            denied_terms("@deepseek-ai/dsh-mcp-client"),
            vec!["deepseek".to_string(), "dsh".to_string(), "AI".to_string()],
            "a lower-case ai is AI too"
        );
        assert_eq!(denied_terms("我是ai"), vec!["AI".to_string()]);
        assert_eq!(denied_terms("C:/runtime/user/dsh/.agent-presets"), vec!["dsh".to_string()]);
        assert_eq!(denied_terms("DeepSeek娘.png"), vec!["deepseek".to_string()]);
        assert_eq!(denied_terms("harness tool authority changed"), vec!["harness".to_string()]);
        assert_eq!(denied_terms("Claude-Haiku"), vec!["claude".to_string()]);
        assert_eq!(denied_terms("@机器人"), vec!["机器人".to_string()]);
        assert_eq!(denied_terms("返回给视觉模型"), vec!["模型".to_string()]);
        assert_eq!(denied_terms("ＤＳＨ 和 ａｉ"), vec!["dsh".to_string(), "AI".to_string()], "full-width");
        assert_eq!(denied_terms("D\u{200b}SH 和 A\u{200b}I"), vec!["dsh".to_string(), "AI".to_string()], "zero-width split");
        assert_eq!(denied_terms("x\u{200b}D\u{200b}SH"), vec!["dsh".to_string()], "a zero-width boundary and joiner at once");
        for clean in [
            "lastAiReplyAt",
            "API 令牌",
            "EMAIL",
            "dshReady",
            "qq_send_message",
            "mcp__web-search-safe__web_search",
            "snowluma_status",
            "end this turn now: produce your final answer and make no further tool calls",
            "XDSHX",
            "x\u{200b}DSHy",
        ] {
            assert!(denied_terms(clean).is_empty(), "{clean:?}: {:?}", denied_terms(clean));
        }
    }

    #[test]
    fn denied_term_policy_also_reads_the_text_as_written() {
        // Folding makes `xdsh` and `FFAI` of these, one word; as written, the term stands beside a non-ASCII character.
        assert_eq!(denied_terms("ｘdsh"), vec!["dsh".to_string()], "as written: a full-width letter beside a term");
        assert_eq!(denied_terms("ﬀAI"), vec!["AI".to_string()], "as written: a ligature beside AI");
        assert_eq!(denied_terms("ＤＳＨ ｘdsh"), vec!["dsh".to_string()], "each term once");
    }
}
