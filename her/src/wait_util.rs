//! `POST /api/socialV2/wait` 的纯函数部分（对应 JS 的 v2-wait.js + suggestQuietMsV2）。
//!
//! 这些只是「软信号」：判断对方话有没有说完、建议等多久。
//! 真正等不等、等多久，最终仍是模型决定 —— 别把这些当成硬规则。

use serde_json::Value;

fn text_of(m: &Value) -> &str {
    m.get("tail")
        .or_else(|| m.get("plain"))
        .or_else(|| m.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn is_self(m: &Value) -> bool {
    m.get("isSelf").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 常见「话说一半」的结尾。
const UNFINISHED_TAILS: [&str; 26] = [
    "你知道", "等一下", "我跟你讲", "其实吧", "但是", "所以说", "然后", "那个", "就是", "我想说",
    "对了", "等我", "等等", "我看看", "还有", "再说", "主要是", "毕竟", "因为", "所以", "但是吧",
    "回头", "回头说", "待会", "晚点", "等会",
];

/// 对应 JS 的 `looksLikeUnfinished`。
///
/// 顺序很重要：**先判「已说完」的终止标点**，再判未完成特征。
/// 否则「……然后。」会被末尾的「然后」误判成没说完。
pub fn looks_like_unfinished(text: &str) -> bool {
    let s = text.trim();
    if s.is_empty() {
        return false;
    }
    let last = s.chars().last().unwrap_or(' ');
    if "。！？!?…～~".contains(last) {
        return false;
    }
    if UNFINISHED_TAILS.iter().any(|t| s.ends_with(t)) {
        return true;
    }
    if "，、；：,;:".contains(last) {
        return true;
    }
    false
}

/// 对应 JS 的 `suggestQuietMsV2`：给「还要再等多久」一个建议值。
pub fn suggest_quiet_ms(recent: &[Value], now: i64, default_quiet_ms: i64) -> i64 {
    let Some(last) = recent.iter().rev().find(|m| !is_self(m)) else {
        return default_quiet_ms;
    };
    if looks_like_unfinished(text_of(last)) {
        return 12_000;
    }
    let burst = recent
        .iter()
        .filter(|m| !is_self(m) && now - m.get("time").and_then(|t| t.as_i64()).unwrap_or(0) < 15_000)
        .count();
    if burst >= 3 {
        return 12_000;
    }
    default_quiet_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unfinished_heuristics() {
        assert!(!looks_like_unfinished(""));
        assert!(!looks_like_unfinished("好的。"));
        assert!(!looks_like_unfinished("真的吗？"));
        assert!(!looks_like_unfinished("等等……"));
        assert!(looks_like_unfinished("我跟你讲"));
        assert!(looks_like_unfinished("但是"));
        assert!(looks_like_unfinished("就是这样，"));
        // 终止标点优先：以「然后」结尾但句末是句号 → 说完了
        assert!(!looks_like_unfinished("然后。"));
        assert!(looks_like_unfinished("然后"));
    }

    #[test]
    fn suggest_quiet_uses_burst_and_unfinished() {
        let now = 1_000_000i64;
        let quiet = |list: Vec<Value>| suggest_quiet_ms(&list, now, 8000);
        let msg = |text: &str, time: i64, me: bool| json!({"text": text, "time": time, "isSelf": me});

        assert_eq!(quiet(vec![]), 8000, "没有消息用默认值");
        assert_eq!(quiet(vec![msg("好的。", now - 60_000, false)]), 8000);
        assert_eq!(quiet(vec![msg("我跟你讲", now - 60_000, false)]), 12000);
        // 15 秒内连发 3 条别人说的 → 也建议多等
        let burst = vec![
            msg("a", now - 1000, false),
            msg("b", now - 2000, false),
            msg("c", now - 3000, false),
        ];
        assert_eq!(quiet(burst), 12000);
        // 自己发的不计入 burst
        let own = vec![
            msg("a", now - 1000, true),
            msg("b", now - 2000, true),
            msg("c", now - 3000, true),
        ];
        assert_eq!(quiet(own), 8000);
    }
}
