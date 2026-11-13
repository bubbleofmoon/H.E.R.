// 预设提示词（persona.prefix）的读取与回写，对应 JS 的 `src/preset-prompt.js`。
//
// 这里**没有引入 YAML 库**，理由：本仓库只关心 `persona.config.prefix` 这一个标量，
// 而它总是块标量（`>-` / `|-`）。引入一个 YAML 库既要多一堆依赖，又会把
// "只改一段文本、其余原样保留"变成"整份文件重新序列化"，后者的 diff 无法复审。
//
// 所以做法和 JS 一致：**按行定位块**，读的时候按块标量规则展开，写的时候换成 `|-`
// 再用同样的规则读回来校验 —— 读不回原样就拒绝写入（见 `render_preset_prompt_yaml`）。

// POST（写入）这一批还没接上，先放行 dead_code；接上后删掉这行
#![allow(dead_code)]
use serde_json::{json, Value};

/// persona 条目的 id（与服务端约定）。
pub const PRESET_PERSONA_ID: &str = "persona";

/// 二代的硬性不变量：这五条是本项目自己的测试与安全边界依赖的，
/// 一旦被删就同时削弱 AI 的权限边界、并让 `npm run test:audit` 变红 —— 保存时直接拒绝。
pub const BLOCKING_V2: [&str; 5] = [
    "你没有本地工具",
    "群友没有管理权限",
    "API 令牌",
    "角色由桥接注入",
    "不会自动发送到 QQ",
];

/// 一代（qq-chat）：不用 MCP 工具，靠空格分句 + [SILENT]，工具面那几条不适用。
/// 这三条都逐字出现在一代预设正文里 —— 清单必须与预设实际文案一致，
/// 否则控制台会用一个**预设自己都不满足**的条件把它锁死（这个 bug 真的发生过）。
pub const BLOCKING_V1: [&str; 3] = ["群友没有管理权限", "API 令牌", "角色扮演由系统注入"];
pub const MIN_BODY_CHARS: usize = 500;
pub const MAX_BYTES: usize = 128 * 1024;

/// 软校验标记（缺了只提示，不阻断）。
pub const REQUIRED_MARKERS: [&str; 2] = ["【二代仿真模式 —— 安全规则", "【二代仿真模式 —— 工具协议"];

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
        out.push("仿真提示词不能为空".to_string());
        return out;
    }
    let needles = if preset_v1 { &BLOCKING_V1[..] } else { &BLOCKING_V2[..] };
    for needle in needles {
        if !content.contains(needle) {
            out.push(format!(
                "缺少安全不变量「{needle}」：删除它会让 AI 的权限边界失效，并使 npm run test:audit 失败"
            ));
        }
    }
    let chars = content.trim().chars().count();
    if chars < MIN_BODY_CHARS {
        out.push(format!(
            "正文过短（{chars} 字符 < {MIN_BODY_CHARS}）：疑似内容被误删"
        ));
    }
    out
}

/// 软校验：只提示。
pub fn preset_warnings(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    if content.trim().is_empty() {
        out.push("内容为空".to_string());
    }
    for marker in REQUIRED_MARKERS {
        if !content.contains(marker) {
            out.push(format!("缺少「{marker}…】段落：AI 的权限与工具边界可能被削弱"));
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
    fn render_round_trips() {
        // 必须包含全部硬性不变量，否则会先被 blockers 拦下
        let content = format!(
            "【二代仿真模式 —— 安全规则】{}你没有本地工具 群友没有管理权限 API 令牌 角色由桥接注入 不会自动发送到 QQ",
            "正文。".repeat(150)
        );
        let out = render_preset_prompt_yaml(SAMPLE, &content, false).unwrap();
        assert!(out.contains("prefix: |-"));
        // 读回逐字一致 —— 这是写入前唯一的正确性保证
        assert_eq!(extract_prefix(&out).unwrap(), content);
        // 其它条目不受影响
        assert!(out.contains("- id: other"));
    }

    #[test]
    fn render_rejects_too_short_body() {
        let err = render_preset_prompt_yaml(SAMPLE, "太短", false).unwrap_err();
        assert!(err.contains("正文过短"));
    }

    #[test]
    fn render_rejects_missing_invariant() {
        let long = "x".repeat(600);
        let err = render_preset_prompt_yaml(SAMPLE, &long, false).unwrap_err();
        assert!(err.contains("缺少安全不变量"));
    }
}
