//! 人格卡（`roles/<名称>.md`）的解析与「按仿真模式取用」。
//!
//! 逐字移植自 `qq-bridge/src/role-card.js`，外加 `bridge.js` 里的
//! `roleWarnings` / `roleSummaries` / `firstMeaningfulLine`。
//!
//! 三条容易踩的判等细节：
//!   1. JS 的 `String.length` 是 **UTF-16 码元数**，Rust 的 `chars().count()` 是码点数。
//!      卡片里有 emoji 时两者会差，所以这里统一走 `utf16_len`。
//!   2. JS 的 `\s` 含 `\u{FEFF}` 不含 `\u{0085}`，与 `char::is_whitespace` 不同。
//!   3. 模式标记用正则写会踩两个坑（`reserved` 会吃掉 `reserved2` 的前缀、
//!      `2\s*代` 里的可选空格），所以这几个小模式手写匹配，语义对着 JS 逐条核。

use serde_json::{json, Value};

pub const MODE_ALL: &str = "all";
pub const MODE_V1: &str = "v1";
pub const MODE_V2: &str = "v2";

pub const ROLE_INJECT_MAX_CHARS_DEFAULT: usize = 6000;

/// JS 的 `\s`（ECMAScript WhiteSpace + LineTerminator）。
pub fn js_is_space(c: char) -> bool {
    matches!(c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' '
        | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}'
        | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

/// JS 的 `String.prototype.trim`。
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_is_space)
}

/// JS 的 `String.prototype.length`。
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JS 的 `s.slice(0, max)`（按 UTF-16 码元切；不会切出半个代理对）。
pub fn utf16_take(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.len_utf16();
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

// ── 模式标记 ────────────────────────────────────────────────────────────────
// JS: /[\[〔（(]\s*(?:二代|2\s*代|v2|reserved2)\s*[\]〕）)]/gi

fn is_open_bracket(c: char) -> bool {
    matches!(c, '[' | '〔' | '（' | '(')
}

fn is_close_bracket(c: char) -> bool {
    matches!(c, ']' | '〕' | '）' | ')')
}

fn lit_match(chars: &[char], at: usize, lit: &str) -> Option<usize> {
    let mut j = at;
    for want in lit.chars() {
        let got = *chars.get(j)?;
        let ok = if want.is_ascii() {
            got.to_ascii_lowercase() == want
        } else {
            got == want
        };
        if !ok {
            return None;
        }
        j += 1;
    }
    Some(j)
}

/// 在 `at`（开括号之后）尝试匹配模式词；成功返回闭合括号之后的下标。
/// 备选顺序与 JS 一致：中文 → 「2 代」→ 英文。
fn alt_match(chars: &[char], at: usize, two: bool) -> Option<usize> {
    let mut i = at;
    while i < chars.len() && js_is_space(chars[i]) {
        i += 1;
    }
    let (cn, digit, lits): (&str, char, [&str; 2]) = if two {
        ("二代", '2', ["v2", "reserved2"])
    } else {
        ("一代", '1', ["v1", "reserved"])
    };
    let mut end = lit_match(chars, i, cn);
    if end.is_none() && chars.get(i) == Some(&digit) {
        let mut j = i + 1;
        while j < chars.len() && js_is_space(chars[j]) {
            j += 1;
        }
        if chars.get(j) == Some(&'代') {
            end = Some(j + 1);
        }
    }
    if end.is_none() {
        for lit in lits {
            if let Some(e) = lit_match(chars, i, lit) {
                end = Some(e);
                break;
            }
        }
    }
    let mut e = end?;
    while e < chars.len() && js_is_space(chars[e]) {
        e += 1;
    }
    if chars.get(e).is_some_and(|c| is_close_bracket(*c)) {
        Some(e + 1)
    } else {
        None
    }
}

fn find_tag(chars: &[char], two: bool) -> Option<(usize, usize)> {
    for i in 0..chars.len() {
        if !is_open_bracket(chars[i]) {
            continue;
        }
        if let Some(end) = alt_match(chars, i + 1, two) {
            return Some((i, end));
        }
    }
    None
}

/// JS 的 `detectMode`：**先全串找 v2，再找 v1**（不是「先出现者胜」）。
fn detect_mode(heading: &str) -> &'static str {
    let chars: Vec<char> = heading.chars().collect();
    if find_tag(&chars, true).is_some() {
        MODE_V2
    } else if find_tag(&chars, false).is_some() {
        MODE_V1
    } else {
        MODE_ALL
    }
}

/// JS 的 `stripModeTags`：删掉所有标记，行尾空白去掉，连续空格压成一个。
fn strip_mode_tags(heading: &str) -> String {
    let chars: Vec<char> = heading.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if is_open_bracket(chars[i]) {
            let matched = alt_match(&chars, i + 1, true).or_else(|| alt_match(&chars, i + 1, false));
            if let Some(end) = matched {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    let s: String = out.into_iter().collect::<String>().trim_end_matches([' ', '\t']).to_string();
    let mut collapsed = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        if chars[k] == ' ' || chars[k] == '\t' {
            let mut j = k;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            collapsed.push(if j - k >= 2 { ' ' } else { chars[k] });
            k = j;
            continue;
        }
        collapsed.push(chars[k]);
        k += 1;
    }
    js_trim(&collapsed).to_string()
}

// ── 小节 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct RoleSection {
    pub mode: &'static str,
    pub heading: Option<String>,
    pub lines: Vec<String>,
}

/// JS 正则里的 LineTerminator。`.` 不匹配它们、不带 `/m` 的 `$` 也不在它们前面成立。
fn is_js_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// 匹配 `^(#{1,6})\s+(.*)$`；返回 (井号串, 标题原文)。
///
/// 关键：JS 的 `.` 不匹配 `\r`，行尾带 `\r`（CRLF 文件）时这条正则**整个不匹配**，
/// 那一行会被当成正文。Rust 的 `.` 匹配 `\r`，不显式挡掉就会解析出 JS 解析不出的标题。
fn parse_heading(line: &str) -> Option<(String, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i] == '#' {
        i += 1;
    }
    if i == 0 || i > 6 {
        return None;
    }
    let hashes = i;
    let mut j = i;
    while j < chars.len() && js_is_space(chars[j]) {
        j += 1;
    }
    if j == hashes {
        return None;
    }
    let rest: String = chars[j..].iter().collect();
    if rest.chars().any(is_js_line_terminator) {
        return None;
    }
    Some((chars[..hashes].iter().collect(), rest))
}

pub fn parse_role_sections(text: &str) -> Vec<RoleSection> {
    let mut sections: Vec<RoleSection> = Vec::new();
    let mut current: Option<usize> = None;
    for line in text.split('\n') {
        match parse_heading(line) {
            None => {
                let idx = match current {
                    Some(i) => i,
                    None => {
                        sections.push(RoleSection {
                            mode: MODE_ALL,
                            heading: None,
                            lines: Vec::new(),
                        });
                        sections.len() - 1
                    }
                };
                // 开头那一段（第一个标题之前）只建一次：漏了这句会给每一行
                // 都新建一个「开头部分」小节，字数统计和 sectionReport 全歪。
                current = Some(idx);
                sections[idx].lines.push(line.to_string());
            }
            Some((hashes, rest)) => {
                let mode = detect_mode(&rest);
                let title = strip_mode_tags(&rest);
                sections.push(RoleSection {
                    mode,
                    heading: Some(title.clone()),
                    lines: vec![format!("{hashes} {title}")],
                });
                current = Some(sections.len() - 1);
            }
        }
    }
    sections
}

fn collapse_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0usize;
    for c in s.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push('\n');
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

/// 取某次仿真真正要注入的人格文本（已去掉模式标记）。
pub fn select_role_text(text: &str, mode: &str) -> String {
    let want = if mode == MODE_V1 { MODE_V1 } else { MODE_V2 };
    let joined = parse_role_sections(text)
        .into_iter()
        .filter(|s| s.mode == MODE_ALL || s.mode == want)
        .map(|s| s.lines.join("\n"))
        .collect::<Vec<_>>()
        .join("\n");
    js_trim(&collapse_newlines(&joined)).to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleCharStats {
    pub total: usize,
    pub common: usize,
    pub v1_only: usize,
    pub v2_only: usize,
    pub v1: usize,
    pub v2: usize,
}

impl RoleCharStats {
    /// JS 的字段名是 camelCase，这里照着吐。
    pub fn to_json(&self) -> Value {
        json!({
            "total": self.total,
            "common": self.common,
            "v1Only": self.v1_only,
            "v2Only": self.v2_only,
            "v1": self.v1,
            "v2": self.v2,
        })
    }
}

fn section_chars(section: &RoleSection) -> usize {
    utf16_len(js_trim(&section.lines.join("\n")))
}

pub fn role_char_stats(text: &str) -> RoleCharStats {
    let sections = parse_role_sections(text);
    let sum = |mode: &str| -> usize {
        sections
            .iter()
            .filter(|s| s.mode == mode)
            .map(section_chars)
            .sum()
    };
    RoleCharStats {
        total: utf16_len(text),
        common: sum(MODE_ALL),
        v1_only: sum(MODE_V1),
        v2_only: sum(MODE_V2),
        v1: utf16_len(&select_role_text(text, MODE_V1)),
        v2: utf16_len(&select_role_text(text, MODE_V2)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleSectionReport {
    pub heading: String,
    pub mode: &'static str,
    pub injected: bool,
    pub chars: usize,
}

impl RoleSectionReport {
    pub fn to_json(&self) -> Value {
        json!({
            "heading": self.heading,
            "mode": self.mode,
            "injected": self.injected,
            "chars": self.chars,
        })
    }
}

pub fn role_section_report(text: &str, mode: &str) -> Vec<RoleSectionReport> {
    let want = if mode == MODE_V1 { MODE_V1 } else { MODE_V2 };
    parse_role_sections(text)
        .into_iter()
        .map(|section| RoleSectionReport {
            heading: section.heading.clone().unwrap_or_else(|| "(开头部分)".to_string()),
            mode: section.mode,
            injected: section.mode == MODE_ALL || section.mode == want,
            chars: section_chars(&section),
        })
        .collect()
}

// ── 摘要与告警 ──────────────────────────────────────────────────────────────

fn strip_hashes(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i] == '#' {
        i += 1;
    }
    if i == 0 {
        return line.to_string();
    }
    while i < chars.len() && js_is_space(chars[i]) {
        i += 1;
    }
    chars[i..].iter().collect()
}

fn strip_bullet(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.first().is_some_and(|c| *c == '-' || *c == '*') {
        let mut i = 1;
        while i < chars.len() && js_is_space(chars[i]) {
            i += 1;
        }
        return chars[i..].iter().collect();
    }
    line.to_string()
}

/// JS 的 `firstMeaningfulLine`：第一行非空内容（去掉 # 与列表符号），最多 80 字。
pub fn first_meaningful_line(content: &str) -> String {
    for line in content.split('\n') {
        let t = strip_bullet(&strip_hashes(line));
        let t = js_trim(&t);
        if !t.is_empty() {
            return utf16_take(t, 80);
        }
    }
    String::new()
}

/// JS 的 `GEN1_ROLE_LINE_RE`（`/\[SILENT\]|空格分隔|按空格|用空格|空格分句|空格代表|自动转发|回复会自动|输出\s*\[SILENT\]/i`）。
/// 注意最后那个 `输出\s*\[SILENT\]` 被前面的 `\[SILENT\]` 完全覆盖，是冗余分支。
pub fn gen1_role_line(text: &str) -> bool {
    let lower = text.to_lowercase();
    const NEEDLES: [&str; 7] = [
        "空格分隔", "按空格", "用空格", "空格分句", "空格代表", "自动转发", "回复会自动",
    ];
    lower.contains("[silent]") || NEEDLES.iter().any(|n| lower.contains(n))
}

fn is_cjk(c: char) -> bool {
    (0x4e00..=0x9fff).contains(&(c as u32))
}

/// `/[\u4e00-\u9fff]\s+[\u4e00-\u9fff]/`：中文之间夹空白。
fn has_cjk_space_cjk(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        if !is_cjk(chars[i]) {
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && js_is_space(chars[j]) {
            j += 1;
        }
        if j > i + 1 && chars.get(j).is_some_and(|c| is_cjk(*c)) {
            return true;
        }
    }
    false
}

/// `applyRoleInjectLimit`：非有限数回落 6000，否则取整并夹到 1000~20000。
pub fn apply_inject_limit(value: Option<&Value>) -> usize {
    let Some(n) = value.and_then(|v| v.as_f64()) else {
        return ROLE_INJECT_MAX_CHARS_DEFAULT;
    };
    if !n.is_finite() {
        return ROLE_INJECT_MAX_CHARS_DEFAULT;
    }
    (n.round() as i64).clamp(1000, 20000) as usize
}

pub fn role_warnings(content: &str, mode: &str, inject_max: usize) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    let stats = role_char_stats(content);
    let injected = if mode == MODE_V1 { stats.v1 } else { stats.v2 };
    if injected > inject_max {
        warnings.push(format!(
            "超出注入上限：当前模式实际注入 {injected} 字符，只有前 {inject_max} 字符会进入提示词（可在配置里调 role.maxInjectChars）"
        ));
    }
    let untagged = parse_role_sections(content)
        .into_iter()
        .filter(|s| s.mode == MODE_ALL)
        .map(|s| s.lines.join("\n"))
        .collect::<Vec<_>>()
        .join("\n");
    if gen1_role_line(&untagged) {
        warnings.push(
            "未标记的小节里含一代仿真专用指令（[SILENT] / 空格分句 / 自动转发…），二代下这些行会被兜底过滤——建议把它们移到带 〔一代〕 标记的小节里"
                .to_string(),
        );
    }
    if untagged.contains("回复示例") && has_cjk_space_cjk(&untagged) {
        warnings.push(
            "“回复示例”节里的中文空格会被自动改写成逗号（二代不用空格分条），建议直接写逗号，或把该节标记为 〔一代〕"
                .to_string(),
        );
    }
    warnings
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleSummary {
    pub name: String,
    pub bytes: u64,
    pub chars: usize,
    pub lines: usize,
    pub mtime: i64,
    pub excerpt: String,
    pub injected_v1: usize,
    pub injected_v2: usize,
    pub chars_v1: usize,
    pub chars_v2: usize,
    pub has_mode_tags: bool,
    pub truncated: bool,
    pub warnings: usize,
}

impl RoleSummary {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "bytes": self.bytes,
            "chars": self.chars,
            "lines": self.lines,
            "mtime": self.mtime,
            "excerpt": self.excerpt,
            "injectedV1": self.injected_v1,
            "injectedV2": self.injected_v2,
            "charsV1": self.chars_v1,
            "charsV2": self.chars_v2,
            "hasModeTags": self.has_mode_tags,
            "truncated": self.truncated,
            "warnings": self.warnings,
        })
    }
}

/// `listRoles()`：目录下所有 `*.md`（不含 README），去掉扩展名后按 JS `localeCompare(…, 'zh-CN')` 的顺序排序。
///
/// JS 的 zh-CN 排序把汉字排在拉丁字母之前，汉字之间按拼音。汉字顺序取自随源码提交的
/// `role_han_order.txt` stores the recorded ICU Han collation order.
/// 因此各平台一致，不依赖系统区域设置。汉字组内的其余字符按「标点 < 数字 < 汉字 < 字母」比较；
/// 拉丁组按「标点 < 数字 < 字母、不分大小写、同形时小写在前」比较。
/// ICU 对个别多字词的上下文规则（如「重庆」读作 chongqing）不在表里，按逐字排序处理。
fn is_han(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF)
}

const HAN_ORDER: &str = include_str!("role_han_order.txt");

fn han_rank(c: char) -> u32 {
    static RANKS: std::sync::OnceLock<std::collections::HashMap<char, u32>> = std::sync::OnceLock::new();
    let ranks = RANKS.get_or_init(|| HAN_ORDER.chars().enumerate().map(|(i, c)| (c, i as u32)).collect());
    ranks.get(&c).copied().unwrap_or(c as u32)
}

fn han_group_key(s: &str) -> Vec<(u8, u32)> {
    s.chars()
        .map(|c| {
            if is_han(c) {
                (2, han_rank(c))
            } else if c.is_alphabetic() {
                (3, c.to_lowercase().next().unwrap_or(c) as u32)
            } else if c.is_numeric() {
                (1, c as u32)
            } else {
                (0, c as u32)
            }
        })
        .collect()
}

fn latin_key(s: &str) -> Vec<(u8, char)> {
    s.chars()
        .map(|c| {
            let class = if c.is_alphabetic() { 2 } else if c.is_numeric() { 1 } else { 0 };
            (class, c.to_lowercase().next().unwrap_or(c))
        })
        .collect()
}

/// Order of role names as the JS list shows them (see `list_roles`).
pub fn compare_names(a: &str, b: &str) -> std::cmp::Ordering {
    let (ha, hb) = (a.chars().next().is_some_and(is_han), b.chars().next().is_some_and(is_han));
    if ha != hb {
        return if ha { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
    }
    if ha {
        return han_group_key(a)
            .cmp(&han_group_key(b))
            // same letters: lowercase first, as ICU's tertiary level does
            .then_with(|| b.cmp(a));
    }
    latin_key(a)
        .cmp(&latin_key(b))
        // same letters: lowercase first, as ICU's tertiary level does
        .then_with(|| b.cmp(a))
}

pub fn list_roles(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == "README" {
            continue;
        }
        names.push(stem.to_string());
    }
    names.sort_by(|a, b| compare_names(a, b));
    names
}

pub fn role_summaries(dir: &str, inject_max: usize) -> Vec<RoleSummary> {
    let files = list_roles(dir).into_iter().map(|name| {
        let path = format!("{dir}/{name}.md");
        let Ok(meta) = std::fs::metadata(&path) else {
            return (name, None, 0, 0);
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return (name, None, 0, 0);
        };
        let mtime = meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64).unwrap_or(0);
        (name, Some(content), meta.len(), mtime)
    });
    role_summaries_from_contents(files, inject_max)
}

/// Build role summaries from content read by a caller that owns path policy. This keeps isolated
/// console reads behind RuntimeContext rather than reopening paths in this module.
pub fn role_summaries_from_contents(
    files: impl IntoIterator<Item = (String, Option<String>, u64, i64)>,
    inject_max: usize,
) -> Vec<RoleSummary> {
    files.into_iter().map(|(name, content, bytes, mtime)| {
        let Some(content) = content else {
            return RoleSummary {
                name, bytes: 0, chars: 0, lines: 0, mtime: 0, excerpt: String::new(),
                injected_v1: 0, injected_v2: 0, chars_v1: 0, chars_v2: 0,
                has_mode_tags: false, truncated: false, warnings: 0,
            };
        };
        let stats = role_char_stats(&content);
        RoleSummary {
            bytes,
            chars: utf16_len(&content),
            lines: content.split('\n').count(),
            mtime,
            excerpt: first_meaningful_line(&content),
            injected_v1: stats.v1.min(inject_max),
            injected_v2: stats.v2.min(inject_max),
            chars_v1: stats.v1,
            chars_v2: stats.v2,
            has_mode_tags: stats.v1_only > 0 || stats.v2_only > 0,
            truncated: stats.v1 > inject_max || stats.v2 > inject_max,
            warnings: role_warnings(&content, MODE_V2, inject_max).len(),
            name,
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_names_sort_han_first_by_pinyin_then_latin_case_insensitively() {
        let mut names = vec!["crlf", "Beta", "alpha", "新角色", "测试角色", "10x"];
        names.sort_by(|a, b| compare_names(a, b));
        // 测(ce) < 新(xin) by pinyin, Han before Latin; digits before letters; alpha < Beta.
        assert_eq!(names, vec!["测试角色", "新角色", "10x", "alpha", "Beta", "crlf"]);
    }

    #[test]
    fn han_names_follow_the_node_icu_zh_cn_order_on_every_platform() {
        // Expected order: `names.sort(new Intl.Collator('zh-CN').compare)` in Node ICU 78 (frozen JS `localeCompare`).
        let expected = [
            "阿", "啊", "艾", "爱", "白", "测 x", "测-x", "测1", "测试", "测试10", "测试2", "测试角色", "测a", "测A",
            "龘", "二", "角色二", "角色一", "角色a", "角色B", "鲸", "李四", "龙", "哦", "三", "十", "王五", "小鲸", "小鲸鱼",
            "新角色", "行", "一", "乙", "银行", "张三", "长", "长城", "赵六", "中", "重", "㐀",
        ];
        let mut names: Vec<&str> = expected.iter().rev().copied().collect();
        names.sort_by(|a, b| compare_names(a, b));
        assert_eq!(names, expected);
    }

    #[test]
    fn utf16_len_counts_code_units_not_code_points() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("小鲸鱼"), 3);
        // 一个 emoji = 2 个 UTF-16 码元，但只有 1 个码点
        assert_eq!(utf16_len("🐟"), 2);
        assert_eq!("🐟".chars().count(), 1);
        assert_eq!(utf16_take("a🐟b", 3), "a🐟");
        assert_eq!(utf16_take("a🐟b", 2), "a");
    }

    #[test]
    fn mode_tags_are_detected_and_stripped() {
        assert_eq!(detect_mode("表情包体系 〔二代〕"), MODE_V2);
        assert_eq!(detect_mode("分条方式 [v1]"), MODE_V1);
        assert_eq!(detect_mode("你是谁"), MODE_ALL);
        assert_eq!(detect_mode("规则（reserved2）"), MODE_V2);
        assert_eq!(detect_mode("规则（reserved）"), MODE_V1);
        assert_eq!(strip_mode_tags("表情包体系 〔二代〕"), "表情包体系");
        assert_eq!(strip_mode_tags("  [v2]  尾巴  "), "尾巴");
        // 同时出现两种标记时，JS 先测 v2，所以判 v2
        assert_eq!(detect_mode("A 〔一代〕 B 〔二代〕"), MODE_V2);
        // 括号里的内容不是标记时原样保留
        assert_eq!(strip_mode_tags("颜色（红）"), "颜色（红）");
    }

    #[test]
    fn sections_split_on_headings_and_track_mode() {
        let text = "# 标题\n前言\n## 通用\n甲\n## 一代专用 〔一代〕\n乙\n## 二代专用 〔二代〕\n丙\n";
        let sections = parse_role_sections(text);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].heading.as_deref(), Some("标题"));
        assert_eq!(sections[1].lines, vec!["## 通用".to_string(), "甲".to_string()]);
        assert_eq!(sections[2].mode, MODE_V1);
        assert_eq!(sections[2].heading.as_deref(), Some("一代专用"));
        assert_eq!(sections[3].mode, MODE_V2);
    }

    #[test]
    fn select_role_text_filters_by_mode_and_collapses_newlines() {
        let text = "通用\n\n\n## 一代专用 〔一代〕\n乙\n## 二代专用 〔二代〕\n丙\n";
        let v2 = select_role_text(text, MODE_V2);
        assert!(v2.contains("通用"));
        assert!(v2.contains("丙"));
        assert!(!v2.contains("乙"));
        assert!(!v2.contains("\n\n\n"), "3 个以上换行要压成 2 个");
        let v1 = select_role_text(text, MODE_V1);
        assert!(v1.contains("乙"));
        assert!(!v1.contains("丙"));
    }

    #[test]
    fn char_stats_match_the_three_buckets() {
        let text = "共通 〔无标记〕\n一二三\n";
        let stats = role_char_stats(text);
        assert_eq!(stats.total, utf16_len(text));
        assert_eq!(stats.v1, stats.v2);
        let tagged = "共通\n甲\n## 专一 〔一代〕\n乙乙\n## 专二 〔二代〕\n丙丙丙\n";
        let s = role_char_stats(tagged);
        assert!(s.v1_only > 0 && s.v2_only > 0);
        // 两种模式的总字数都覆盖「共通部分」，且各自多出对方没有的那一段。
        // 注意不能断言 v1 == common + v1_only：select_role_text 是「拼接后整体 trim」，
        // 与小节逐个 trim 再相加不恒等（JS 也一样）。
        assert!(s.v1 > s.common);
        assert!(s.v2 > s.common);
        assert_ne!(s.v1, s.v2);
    }

    #[test]
    fn section_report_marks_injection() {
        let text = "## 通用\n甲\n## 只给一代 〔一代〕\n乙\n";
        let v2 = role_section_report(text, MODE_V2);
        assert_eq!(v2.len(), 2);
        assert!(v2[0].injected);
        assert!(!v2[1].injected);
        assert_eq!(v2[1].heading, "只给一代");
        let v1 = role_section_report(text, MODE_V1);
        assert!(v1[1].injected);
    }

    #[test]
    fn first_meaningful_line_skips_decoration() {
        assert_eq!(first_meaningful_line("\n# 小鲸鱼\n正文"), "小鲸鱼");
        assert_eq!(first_meaningful_line("### - 傲娇助手"), "傲娇助手");
        assert_eq!(first_meaningful_line(""), "");
        // 超长截到 80
        let long = "字".repeat(100);
        assert_eq!(utf16_len(&first_meaningful_line(&long)), 80);
    }

    #[test]
    fn gen1_lines_and_warnings() {
        assert!(gen1_role_line("输出 [SILENT] 表示不想说"));
        assert!(gen1_role_line("用空格分条"));
        assert!(!gen1_role_line("完全正常的二代卡片"));
        let text = "## 通用\n输出 [SILENT]\n";
        let w = role_warnings(text, MODE_V2, 6000);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("一代仿真专用指令"));
        // 标记过的一代小节不该报警
        let tagged = "## 通用\n\n## 分条 〔一代〕\n输出 [SILENT]\n";
        assert!(role_warnings(tagged, MODE_V2, 6000).is_empty());
    }

    #[test]
    fn inject_limit_clamps_like_js() {
        assert_eq!(apply_inject_limit(None), 6000);
        assert_eq!(apply_inject_limit(Some(&json!("abc"))), 6000);
        assert_eq!(apply_inject_limit(Some(&json!(10))), 1000);
        assert_eq!(apply_inject_limit(Some(&json!(99999))), 20000);
        assert_eq!(apply_inject_limit(Some(&json!(4321))), 4321);
        assert_eq!(apply_inject_limit(Some(&json!(4321.6))), 4322);
    }

    #[test]
    fn crlf_file_has_no_headings_like_js() {
        // JS: `.` 不匹配 `\r`，所以 CRLF 文件里的 `## 标题\r` 不算标题
        let text = "# 标题\r\n第一行\r\n\r\n## 通用\r\n第二行\r\n";
        let sections = parse_role_sections(text);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].heading.is_none());
        assert_eq!(sections[0].mode, MODE_ALL);
    }

    #[test]
    fn utf8_space_differs_from_js_space() {
        // U+FEFF 在 JS 里是空白，在 Rust 的 is_whitespace 里不是
        assert!(js_is_space('\u{FEFF}'));
        assert!(!'\u{FEFF}'.is_whitespace());
        // U+0085 相反
        assert!(!js_is_space('\u{0085}'));
        assert!('\u{0085}'.is_whitespace());
    }
}
