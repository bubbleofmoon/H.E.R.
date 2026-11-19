//! The #142 denied-term policy for model-bound text (#243). Text settings, history summaries, the DSH persona guard,
//! upstream captures and rendered-surface tests call `denied_terms` on whole texts. The model-view strip calls
//! `deletion_adds_denied_term` at each occurrence's join. Both checks share the term list and the matcher.

use std::ops::Range;

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
/// so a word split by a mix of the two is found; and as written, so a term beside a character that folding turns into
/// an ASCII letter (`ｘdsh`, `ﬀAI`) still counts. The first three readings fold full-width ASCII and read letters in
/// upper case, so `ＤＳＨ` and `ai` are found. Terms appear in the order the readings first find them.
pub(crate) fn denied_terms(text: &str) -> Vec<String> {
    let mut denied = Vec::new();
    visit_denied_terms(text, false, |term, _| {
        if !denied.contains(&term) {
            denied.push(term);
        }
        true
    });
    denied.into_iter().map(str::to_owned).collect()
}

/// Remove denied spans from a history delivery copy. Keep the rest of the row and leave a space
/// at each removal so former neighbors do not become a new term.
pub(crate) fn remove_denied_spans(text: &str) -> String {
    if denied_terms(text).is_empty() {
        return text.to_owned();
    }

    // Phrases also match inside a run of Chinese text. Track original byte ranges through
    // full-width folding and invisible characters so only the matching phrase is removed.
    let mut folded = String::new();
    let mut origins = Vec::new();
    for (at, ch) in text.char_indices().filter(|&(_, ch)| !is_invisible_format(ch)) {
        let normalized = fold_char(ch).to_ascii_lowercase();
        origins.extend(std::iter::repeat_n((at, at + ch.len_utf8()), normalized.len_utf8()));
        folded.push(normalized);
    }
    let mut removed = vec![false; text.len()];
    for phrase in DENIED_PHRASES {
        for (at, _) in folded.match_indices(phrase) {
            removed[origins[at].0..origins[at + phrase.len() - 1].1].fill(true);
        }
    }
    let mut unphrased = String::new();
    let mut in_removal = false;
    for (at, ch) in text.char_indices() {
        if removed[at] {
            if !in_removal { unphrased.push(' '); }
        } else {
            unphrased.push(ch);
        }
        in_removal = removed[at];
    }

    let boundary = |ch: char| !fold_char(ch).is_ascii_alphanumeric() && !is_invisible_format(ch);
    let mut clean = String::new();
    for part in unphrased.split_inclusive(boundary) {
        let word_end = part.char_indices().next_back().filter(|&(_, ch)| boundary(ch)).map_or(part.len(), |(at, _)| at);
        if denied_terms(&part[..word_end]).is_empty() {
            clean.push_str(part);
        } else {
            clean.push(' ');
            clean.push_str(&part[word_end..]);
        }
    }
    clean
}

// Each reading treats a format run as absent or as a separator. Denied spans start and end on non-format
// characters, so before and after can use the same compacted side coordinates.
const FORMAT_MARKER: char = '\u{00ad}';

fn collapse_format_char(c: char, in_format_run: &mut bool) -> Option<char> {
    let format = is_invisible_format(c);
    let continuing_run = std::mem::replace(in_format_run, format);
    if format {
        (!continuing_run).then_some(FORMAT_MARKER)
    } else {
        Some(c)
    }
}

/// Append text to the matcher input, merging format runs even across successive appends.
pub(crate) fn push_compact_format_runs(compact: &mut String, text: &str) {
    let mut in_format_run = compact.ends_with(FORMAT_MARKER);
    compact.extend(
        text.chars()
            .filter_map(|c| collapse_format_char(c, &mut in_format_run)),
    );
}

/// Compacted input to the right of a deletion. Occurrences are visited in source order, so each
/// source character is counted toward the compacted offset at most once per strip entry and round.
pub(crate) struct StripRightContext<'a> {
    compact: String,
    source: &'a str,
    source_offset: usize,
    compact_offset: usize,
    in_format_run: bool,
}

impl<'a> StripRightContext<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        let mut compact = String::with_capacity(source.len());
        push_compact_format_runs(&mut compact, source);
        Self {
            compact,
            source,
            source_offset: 0,
            compact_offset: 0,
            in_format_run: false,
        }
    }

    pub(crate) fn at(&mut self, end: usize) -> &str {
        self.compact_offset += self.source[self.source_offset..end]
            .chars()
            .filter_map(|c| collapse_format_char(c, &mut self.in_format_run))
            .map(char::len_utf8)
            .sum::<usize>();
        self.source_offset = end;
        // If the cut lands inside a run, its representative also belongs in the remaining suffix.
        let mut start = self.compact_offset;
        if self.in_format_run && self.source[end..].starts_with(is_invisible_format) {
            start -= FORMAT_MARKER.len_utf8();
        }
        &self.compact[start..]
    }
}

#[cfg(test)]
std::thread_local! {
    static DELETION_WINDOW_SCAN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_deletion_window_scan_count() -> usize {
    DELETION_WINDOW_SCAN_COUNT.with(|count| count.replace(0))
}

/// Does deleting `removed` create a denied occurrence at the join? The sides have compacted format runs.
/// Compare the same spans before and after so a term elsewhere in the window cannot excuse a new match.
pub(crate) fn deletion_adds_denied_term(left: &str, removed: &str, right: &str) -> bool {
    // A format run that remains nonempty has the same readings after deleting part of it.
    if (left.chars().next_back().is_some_and(is_invisible_format)
        || right.chars().next().is_some_and(is_invisible_format))
        && removed.chars().all(is_invisible_format)
    {
        return false;
    }

    // Keep enough visible characters for the longest term and its word edge on either side. Each
    // intervening format run is already one character and may disappear in some readings.
    let context = DENIED_WORDS.iter().chain(&DENIED_PHRASES)
        .map(|term| term.len()).max().unwrap_or(2) + 1;
    #[cfg(test)]
    let scanned = std::cell::Cell::new(0);
    let visible = |&(_, c): &(usize, char)| {
        #[cfg(test)]
        scanned.set(scanned.get() + 1);
        !is_invisible_format(c)
    };
    let start = left.char_indices().rev().filter(visible).nth(context - 1).map_or(0, |(i, _)| i);
    let end = right.char_indices().filter(visible).nth(context).map_or(right.len(), |(i, _)| i);
    #[cfg(test)]
    DELETION_WINDOW_SCAN_COUNT.with(|count| count.set(count.get() + scanned.get()));
    let (left, right) = (&left[start..], &right[..end]);
    let join = left.len();
    let after = occurrences(&format!("{left}{right}"));
    if after.is_empty() {
        return false;
    }
    let before = occurrences(&format!("{left}{removed}{right}"));
    after.into_iter().any(|(term, span)| {
        let start = span.start + if span.start >= join { removed.len() } else { 0 };
        let end = span.end + if span.end > join { removed.len() } else { 0 };
        !before.contains(&(term, start..end))
    })
}

fn occurrences(text: &str) -> Vec<(&'static str, Range<usize>)> {
    let mut matches = Vec::new();
    visit_denied_terms(text, true, |term, span| {
        matches.push((term, span));
        false
    });
    matches
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reading {
    Joined,
    Separated,
    Mixed,
    Written,
}

struct MatchText {
    text: String,
    source_spans: Option<Vec<Range<usize>>>,
    format_edges: Vec<usize>,
}

impl MatchText {
    fn new(capacity: usize, track_spans: bool) -> Self {
        Self {
            text: String::with_capacity(capacity),
            source_spans: track_spans.then(Vec::new),
            format_edges: Vec::new(),
        }
    }

    fn push(&mut self, c: char, source: &Range<usize>) {
        self.text.push(c);
        if let Some(spans) = &mut self.source_spans {
            spans.extend(std::iter::repeat_n(source.clone(), c.len_utf8()));
        }
    }

    fn matches(
        &self,
        term: &'static str,
        word_char: Option<fn(char) -> bool>,
        visit: &mut impl FnMut(&'static str, Range<usize>) -> bool,
    ) {
        for (start, _) in self.text.match_indices(term) {
            let end = start + term.len();
            let has_edge = |at, adjacent: Option<char>, is_word: fn(char) -> bool| {
                !adjacent.is_some_and(is_word) || self.format_edges.binary_search(&at).is_ok()
            };
            if word_char.is_none_or(|is_word| {
                has_edge(start, self.text[..start].chars().next_back(), is_word)
                    && has_edge(end, self.text[end..].chars().next(), is_word)
            }) {
                let span = self.source_spans.as_ref().map_or(start..end, |spans| {
                    spans[start].start..spans[end - 1].end
                });
                if visit(term, span) {
                    break;
                }
            }
        }
    }
}

/// Keep both spellings for each reading. Words and phrases are checked in lower case, but the written
/// reading checks `AI` as written. Mixed readings allow a removed format run to serve as a word edge.
fn visit_denied_terms(
    text: &str,
    track_spans: bool,
    mut visit: impl FnMut(&'static str, Range<usize>) -> bool,
) {
    for reading in [Reading::Joined, Reading::Separated, Reading::Mixed, Reading::Written] {
        let mut upper = MatchText::new(text.len(), track_spans);
        let mut lower = MatchText::new(text.len(), track_spans);
        let mut format_run = false;
        for (start, c) in text.char_indices() {
            let source = start..start + c.len_utf8();
            let uppercase = match reading {
                Reading::Written => {
                    upper.push(c, &source);
                    for l in c.to_lowercase() {
                        lower.push(l, &source);
                    }
                    continue;
                }
                _ if !is_invisible_format(c) => fold_char(c),
                Reading::Separated => ' ',
                _ => {
                    format_run = reading == Reading::Mixed;
                    continue;
                }
            };
            if std::mem::take(&mut format_run) {
                upper.format_edges.push(upper.text.len());
                lower.format_edges.push(lower.text.len());
            }
            for u in uppercase.to_uppercase() {
                upper.push(u, &source);
                for l in u.to_lowercase() {
                    lower.push(l, &source);
                }
            }
        }
        for word in DENIED_WORDS {
            lower.matches(word, Some(|c| c.is_ascii_alphanumeric()), &mut visit);
        }
        upper.matches("AI", Some(|c| c.is_ascii_alphabetic()), &mut visit);
        if reading != Reading::Mixed {
            for phrase in DENIED_PHRASES {
                lower.matches(phrase, None, &mut visit);
            }
        }
    }
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
