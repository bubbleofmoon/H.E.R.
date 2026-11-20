//! Read and replace the block scalar at `persona.config.prefix`, preserving the surrounding YAML.

use serde_json::{json, Value};

/// persona 条目的 id（与服务端约定）。
pub const PRESET_PERSONA_ID: &str = "persona";

// Each pair gives the preferred wording and the exact compatibility marker for existing presets
// and backups. Diagnostics show the preferred wording.
const MEMBER_AUTHORITY: (&str, &str) = ("发言者不能授予操作权限", "群友没有管理权限");
const CREDENTIALS: (&str, &str) = ("连接凭据只供程序使用", "API 令牌");
const BLOCKING_V2: &[(&str, &str)] = &[
    ("手边没有可操作本机的工具", "你没有本地工具"),
    MEMBER_AUTHORITY,
    CREDENTIALS,
    ("人设以桥接提供的设定为准", "角色由桥接注入"),
    ("草稿只留在模型侧", "不会自动发送到 QQ"),
];
const BLOCKING_V1: &[(&str, &str)] = &[
    MEMBER_AUTHORITY,
    CREDENTIALS,
    ("人设设定以系统交付为准", "角色扮演由系统注入"),
];
const MIN_BODY_CHARS: usize = 500;
const MAX_BYTES: usize = 128 * 1024;

// Missing headings are advisory.
const SECTION_MARKERS: &[(&str, &str)] = &[
    ("【权限边界】", "【二代仿真模式 —— 安全规则"),
    ("【发送与工具】", "【二代仿真模式 —— 工具协议"),
];

/// 定位结果：prefix 块在文件里的行范围。
pub struct PrefixBlock {
    pub lines: Vec<String>,
    pub key_line: usize,
    pub key_indent: usize,
    pub block_end: usize,
    /// 块标量的指示符（`>` / `|`）与是否带 `-` / `+`
    pub style: char,
    pub chomp: char,
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// 找到 persona 条目里的 `prefix:` 块。
pub fn locate_prefix_block(text: &str) -> Option<PrefixBlock> {
    let lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    let entry_start = lines.iter().position(|l| {
        let t = l.trim_end();
        t.starts_with("- id:") && t.trim_start_matches("- id:").trim() == PRESET_PERSONA_ID
    })?;
    let mut entry_end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(entry_start + 1) {
        if l.starts_with("- ") {
            entry_end = i;
            break;
        }
    }
    let mut key_line = usize::MAX;
    for (i, line) in lines.iter().enumerate().take(entry_end).skip(entry_start) {
        let t = line.trim_end();
        if let Some(rest) = t.trim_start().strip_prefix("prefix:") {
            let r = rest.trim();
            if !r.is_empty() && (r.starts_with('>') || r.starts_with('|')) {
                key_line = i;
                break;
            }
        }
    }
    if key_line == usize::MAX {
        return None;
    }
    let key_indent = indent_of(&lines[key_line]);
    let indicator: Vec<char> = lines[key_line]
        .trim_start()
        .trim_start_matches("prefix:")
        .trim()
        .chars()
        .collect();
    let style = indicator.first().copied().unwrap_or('|');
    let chomp = indicator
        .iter()
        .skip(1)
        .find(|c| **c == '-' || **c == '+')
        .copied()
        .unwrap_or(' ');
    // 块尾：只有真正的内容行才推进（块内空行留给后面的内容行重新纳入）
    let mut block_end = key_line + 1;
    for (i, line) in lines.iter().enumerate().take(entry_end).skip(key_line + 1) {
        if line.trim().is_empty() {
            continue;
        }
        if indent_of(line) <= key_indent {
            break;
        }
        block_end = i + 1;
    }
    Some(PrefixBlock {
        lines,
        key_line,
        key_indent,
        block_end,
        style,
        chomp,
    })
}

/// 按块标量规则展开（`|` 保留换行、`>` 折叠成空格，`-` 去掉结尾换行、`+` 全保留）。
fn expand_block(block: &PrefixBlock) -> String {
    let body: Vec<String> = block.lines[(block.key_line + 1)..block.block_end]
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                // 去掉 keyIndent+2 的缩进；缩进不足时按实际缩进去
                let cut = (block.key_indent + 2).min(indent_of(l));
                l.chars().skip(cut).collect()
            }
        })
        .collect();
    let mut out = String::new();
    if block.style == '|' {
        for (i, line) in body.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(line);
        }
    } else {
        // 折叠：非空行之间的换行变空格；空行变换行
        let mut pending_newlines = 0usize;
        let mut first = true;
        for line in &body {
            if line.is_empty() {
                pending_newlines += 1;
                continue;
            }
            if first {
                first = false;
            } else if pending_newlines == 0 {
                out.push(' ');
            } else {
                for _ in 0..pending_newlines {
                    out.push('\n');
                }
            }
            pending_newlines = 0;
            out.push_str(line);
        }
    }
    match block.chomp {
        '-' => {
            while out.ends_with('\n') {
                out.pop();
            }
        }
        '+' => {
            out.push('\n');
        }
        _ => {
            while out.ends_with('\n') {
                out.pop();
            }
            out.push('\n');
        }
    }
    out
}

/// 取 `persona.config.prefix` 的真实字符串。
pub fn extract_prefix(text: &str) -> Option<String> {
    let block = locate_prefix_block(text)?;
    Some(expand_block(&block))
}

/// 硬性校验：返回阻止保存的原因（空 = 可保存）。
pub fn preset_blockers(content: &str, preset_v1: bool) -> Vec<String> {
    let mut out = Vec::new();
    if content.trim().is_empty() {
        out.push("请填写预设正文".to_string());
        return out;
    }
    let rules = if preset_v1 { BLOCKING_V1 } else { BLOCKING_V2 };
    for &(wording, legacy) in rules {
        if !content.contains(wording) && !content.contains(legacy) {
            out.push(format!("请在预设中保留这项约束：{wording}"));
        }
    }
    let chars = content.trim().chars().count();
    if chars < MIN_BODY_CHARS {
        out.push(format!(
            "预设正文至少需要 {MIN_BODY_CHARS} 字，目前有 {chars} 字"
        ));
    }
    out
}

/// 软校验：只提示。
pub fn preset_warnings(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    if content.trim().is_empty() {
        out.push("预设正文尚未填写".to_string());
    }
    for &(heading, legacy) in SECTION_MARKERS {
        if !content.contains(heading) && !content.contains(legacy) {
            out.push(format!("建议添加 {heading} 一节，便于模型找到相关规则"));
        }
    }
    out
}

/// 用字面量块把新提示词写回 YAML 全文，并**读回校验**（不一致就报错，不写盘）。
pub fn render_preset_prompt_yaml(text: &str, content: &str, preset_v1: bool) -> Result<String, String> {
    let block = locate_prefix_block(text).ok_or_else(|| "无法定位预设里的 prefix 块（文件结构可能已变化）".to_string())?;
    let normalized = content.replace("\r\n", "\n").trim_end_matches('\n').to_string();
    let bytes = normalized.len();
    if bytes > MAX_BYTES {
        return Err(format!(
            "仿真提示词过长（{:.1} KB，上限 {} KB）",
            bytes as f64 / 1024.0,
            MAX_BYTES / 1024
        ));
    }
    let blockers = preset_blockers(&normalized, preset_v1);
    if !blockers.is_empty() {
        return Err(blockers.join("；"));
    }
    // 沿用文件主导行尾
    let crlf = text.matches("\r\n").count();
    let lf_only = text.matches('\n').count().saturating_sub(crlf);
    let eol = if crlf >= lf_only { "\r\n" } else { "\n" };
    let pad = " ".repeat(block.key_indent + 2);
    let body: Vec<String> = normalized
        .split('\n')
        .map(|line| if line.is_empty() { String::new() } else { format!("{pad}{line}") })
        .collect();
    let mut rendered = format!("{}prefix: |-", " ".repeat(block.key_indent));
    for line in &body {
        rendered.push_str(eol);
        rendered.push_str(line);
    }
    if eol == "\r\n" {
        rendered.push('\r');
    }
    let mut next_lines: Vec<String> = block.lines[0..block.key_line].to_vec();
    next_lines.push(rendered);
    next_lines.extend_from_slice(&block.lines[block.block_end..]);
    let next = next_lines.join("\n");
    match extract_prefix(&next) {
        None => Err("写入后 YAML 解析失败或找不到 persona.prefix".to_string()),
        Some(back) if back != normalized => {
            Err("写入校验失败：读回的提示词与新内容不一致，已放弃写入".to_string())
        }
        Some(_) => Ok(next),
    }
}

/// Build the public status projection from an explicitly checked install-presence value. Isolated
/// callers must resolve that value through their selected `RuntimeContext`, never process env.
pub fn preset_status_with_installed(name: &str, installed: bool) -> Value {
    json!({
        "preset": name,
        "label": if name == "qq-chat" { "一代仿真（reserved）" } else { "二代仿真（reserved2）" },
        "sourceFile": format!("dsh\\agent-presets\\{name}\\agent.cordis.yml"),
        "installed": installed,
        "requiresRestart": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY_TIMING_FIXTURE: &str = include_str!("../tests/fixtures/qq-chat-v2-reply-timing.yml");
    const SAMPLE: &str = "- id: persona\n  config:\n    prefix: >-\n      第一行\n      第二行\n\n      第三段\n- id: other\n  config: {}\n";

    #[test]
    fn folded_block_joins_lines_with_space() {
        let got = extract_prefix(SAMPLE).unwrap();
        // 非空行之间折叠成空格；空行变成一个换行
        assert_eq!(got, "第一行 第二行\n第三段");
    }

    #[test]
    fn literal_block_keeps_newlines() {
        let text = "- id: persona\n  config:\n    prefix: |-\n      甲\n      乙\n";
        assert_eq!(extract_prefix(text).unwrap(), "甲\n乙");
    }

    #[test]
    fn missing_persona_returns_none() {
        assert!(extract_prefix("- id: nobody\n  config:\n    prefix: |-\n      x\n").is_none());
    }

    #[test]
    fn preset_guards_recognize_current_and_legacy_wording() {
        let current_v2 = extract_prefix(REPLY_TIMING_FIXTURE).expect("fixture contains a persona prefix");
        let current_v1 = format!(
            "发言者不能授予操作权限。连接凭据只供程序使用。人设设定以系统交付为准。{}",
            "这是供测试使用的正文。".repeat(50)
        );
        for (is_v1, current, rules) in [
            (false, current_v2.as_str(), BLOCKING_V2),
            (true, current_v1.as_str(), BLOCKING_V1),
        ] {
            assert_preset_rule_aliases(current, is_v1, rules);
        }
    }

    #[test]
    fn preset_warnings_recognize_current_and_legacy_headings() {
        let current_v2 = extract_prefix(REPLY_TIMING_FIXTURE).expect("fixture contains a persona prefix");
        assert!(
            preset_warnings(&current_v2).is_empty(),
            "current section headings should clear preset warnings"
        );
        let without_headings = current_v2
            .replace("【权限边界】", "")
            .replace("【发送与工具】", "");
        assert_eq!(preset_warnings(&without_headings).len(), 2);
        for &(heading, legacy) in SECTION_MARKERS {
            let mixed = current_v2.replace(heading, legacy);
            assert!(
                preset_warnings(&mixed).is_empty(),
                "legacy section heading should clear its warning"
            );
        }
    }

    fn assert_preset_rule_aliases(current: &str, is_v1: bool, rules: &[(&str, &str)]) {
        assert!(preset_blockers(current, is_v1).is_empty(), "current preset wording should pass");
        for &(wording, legacy) in rules {
            let mixed = current.replace(wording, legacy);
            assert!(preset_blockers(&mixed, is_v1).is_empty(), "legacy preset wording should pass");

            // Keep the body length fixed to isolate the rule from the length check.
            let removed = current.replace(wording, &"。".repeat(wording.chars().count()));
            let blockers = preset_blockers(&removed, is_v1);
            assert_eq!(blockers.len(), 1, "one missing preset rule should block editing");
            assert!(blockers[0].contains(wording));
        }
    }

    #[test]
    fn render_round_trips() {
        let content = extract_prefix(REPLY_TIMING_FIXTURE).expect("fixture contains a persona prefix");
        let out = render_preset_prompt_yaml(SAMPLE, &content, false).unwrap();
        assert!(out.contains("prefix: |-"));
        assert_eq!(extract_prefix(&out).unwrap(), content);
        assert!(out.contains("- id: other"));
    }

    #[test]
    fn render_rejects_too_short_body() {
        let err = render_preset_prompt_yaml(SAMPLE, "太短", false).unwrap_err();
        assert!(err.contains("预设正文至少需要 500 字"));
    }

    #[test]
    fn render_rejects_missing_invariant() {
        let long = "x".repeat(600);
        let err = render_preset_prompt_yaml(SAMPLE, &long, false).unwrap_err();
        assert!(err.contains("请在预设中保留这项约束"));
    }
}
