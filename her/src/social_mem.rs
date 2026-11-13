//! 二代「轻量记忆」的状态改写：进行中的话题 / 想说未说 / 群友印象。
//!
//! 逐条移植自 `qq-bridge/src/bridge.js` 的 `appendMemoryV2` / `memory-update` /
//! `memory-remove` / `memory-clear`，以及 `md-to-plain.js` 的 `truncateText`。
//!
//! 为什么单独一个模块：这些改动看着小，但全是「上限 + 去重 + 淘汰」的细节，
//! 散在 HTTP handler 里既测不了也改不动。`memory_formatted` 复用 `prompt` 里那份
//! （管理台和 prompt 用的是同一个渲染器，分开写迟早不一致）。

use crate::role::js_trim;
use serde_json::{json, Map, Value};

/// JS 的 `truncateText(value, max)`：按 UTF-16 码元截断，且不切出孤立代理。
pub fn truncate_text(value: &str, max: i64) -> String {
    let limit = if max <= 0 { 0 } else { max as usize };
    if limit == 0 {
        return String::new();
    }
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= limit {
        return value.to_string();
    }
    let mut cut = limit;
    let code = units[cut - 1];
    if (0xd800..=0xdbff).contains(&code) {
        cut -= 1; // 切在高代理上：前移一个码元
    }
    if cut == 0 {
        // limit=1 且开头就是代理对：至少保留一个完整码点
        let first = units[0];
        let second = *units.get(1).unwrap_or(&0);
        cut = if (0xd800..=0xdbff).contains(&first) && (0xdc00..=0xdfff).contains(&second) {
            2
        } else {
            1
        };
    }
    String::from_utf16_lossy(&units[..cut])
}

/// JS 的 `truncateTextTail(value, max)`：保留最后 `max` 个 UTF-16 码元；切点落在代理对中间时整对保留。
pub fn truncate_text_tail(value: &str, max: i64) -> String {
    let limit = if max <= 0 { 0 } else { max as usize };
    if limit == 0 {
        return String::new();
    }
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= limit {
        return value.to_string();
    }
    let mut start = units.len() - limit;
    if (0xdc00..=0xdfff).contains(&units[start]) {
        // &str 里没有孤儿低代理：前一个码元必然是同一对的高代理
        start -= 1;
    }
    String::from_utf16_lossy(&units[start..])
}

/// JS 的 `redactKnownTokensOnly`：把已知会话令牌替换成 `***`。
pub fn redact_tokens(text: &str, tokens: &[String]) -> String {
    let mut s = text.to_string();
    for token in tokens {
        if !token.is_empty() && s.contains(token.as_str()) {
            s = s.replace(token.as_str(), "***");
        }
    }
    s
}

/// Impression limits, read from `socialV2.context` (`impressionPromptLimit`, `impressionMaxTraits`,
/// `impressionMaxMembers`). The console shows the same defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImpressionPolicy {
    /// How many impressions a prompt or memory view lists.
    pub prompt_members: usize,
    /// Traits kept per member; the oldest are replaced first.
    pub max_traits: usize,
    /// Members kept per conversation; the longest unseen are pruned first.
    pub max_members: usize,
}

impl Default for ImpressionPolicy {
    fn default() -> Self {
        Self { prompt_members: 10, max_traits: 10, max_members: 50 }
    }
}

impl ImpressionPolicy {
    pub fn from_cfg(cfg: &Value) -> Self {
        let default = Self::default();
        let bounded = |name: &str, fallback: usize, max: f64| {
            cfg.pointer(&format!("/socialV2/context/{name}"))
                .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
                .filter(|n| n.is_finite())
                .map_or(fallback, |n| n.round().clamp(1.0, max) as usize)
        };
        Self {
            prompt_members: bounded("impressionPromptLimit", default.prompt_members, 50.0),
            max_traits: bounded("impressionMaxTraits", default.max_traits, 20.0),
            max_members: bounded("impressionMaxMembers", default.max_members, 500.0),
        }
    }
}

/// The impressions a prompt lists: members speaking in the unread/recent window first (most recent
/// speaker first, matched by QQ id or display name), then the rest by `lastSeenAt`. The result keeps
/// storage order so a briefing section changes only when the selected set changes.
pub fn select_impressions<'a>(st: &'a Value, limit: usize) -> Vec<(&'a String, &'a Value)> {
    let Some(map) = st.get("memberImpressions").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut speakers: Vec<(i64, Vec<String>)> = ["unread", "recentMessages"]
        .iter()
        .filter_map(|field| st.get(*field).and_then(Value::as_array))
        .flatten()
        .filter(|m| m.get("isSelf").and_then(Value::as_bool) != Some(true))
        .map(|m| (num_of(m.get("time")), [m.get("userId"), m.get("sender")].into_iter().flatten()
            .filter(|v| !v.is_null()).map(|v| str_of(Some(v))).filter(|s| !s.is_empty()).collect()))
        .collect();
    speakers.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
    let rank = |name: &str| speakers.iter().position(|(_, ids)| ids.iter().any(|id| id == name));
    let entries: Vec<(&String, &Value)> = map.iter().collect();
    let mut chosen: Vec<(usize, Option<usize>, i64)> = entries
        .iter()
        .enumerate()
        .map(|(index, (name, entry))| (index, rank(name), num_of(entry.get("lastSeenAt"))))
        .collect();
    chosen.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.2.cmp(&a.2).then(b.0.cmp(&a.0)),
    });
    chosen.truncate(limit);
    chosen.sort_by_key(|(index, _, _)| *index);
    chosen.into_iter().map(|(index, _, _)| entries[index]).collect()
}

/// A member with an impression spoke: count the observed interaction and refresh `lastSeenAt`.
/// Members are matched by QQ id or display name.
pub fn note_member_seen(st: &mut Value, user_id: Option<&str>, sender: Option<&str>, now: i64) {
    let Some(map) = st.get_mut("memberImpressions").and_then(Value::as_object_mut) else {
        return;
    };
    for (name, entry) in map.iter_mut() {
        if [user_id, sender].into_iter().flatten().any(|id| !id.is_empty() && id == name) {
            entry["interactionCount"] = json!(num_of(entry.get("interactionCount")) + 1);
            entry["lastSeenAt"] = json!(now);
        }
    }
}

fn arr_mut<'a>(st: &'a mut Value, field: &str) -> &'a mut Vec<Value> {
    if !st.get(field).is_some_and(|v| v.is_array()) {
        st[field] = json!([]);
    }
    st[field].as_array_mut().expect("刚刚设置成数组")
}

fn obj_mut<'a>(st: &'a mut Value, field: &str) -> &'a mut Map<String, Value> {
    if !st.get(field).is_some_and(|v| v.is_object()) {
        st[field] = json!({});
    }
    st[field].as_object_mut().expect("刚刚设置成对象")
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn num_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f as i64).unwrap_or(0),
        _ => 0,
    }
}

/// 一次 append 的输入（shallow，别和 `serde_json::Value` 混用）。
#[derive(Debug, Clone, Default)]
pub struct AppendExtra {
    pub participants: Vec<String>,
    pub pending_question: String,
    pub motivation: String,
    pub expires_at_ms: i64,
    pub target: String,
}

pub fn append_extra_from(extra: &Value) -> AppendExtra {
    AppendExtra {
        participants: extra
            .get("participants")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(|x| str_of(Some(x))).collect())
            .unwrap_or_default(),
        pending_question: str_of(extra.get("pendingQuestion")),
        motivation: str_of(extra.get("motivation")),
        expires_at_ms: num_of(extra.get("expiresAtMs")),
        target: js_trim(&str_of(extra.get("target"))).to_string(),
    }
}

/// `appendMemoryV2`：追加/合并一条轻量记忆。
pub fn append_memory(
    st: &mut Value,
    category: &str,
    content: &str,
    extra: &AppendExtra,
    tokens: &[String],
    now: i64,
    policy: &ImpressionPolicy,
) {
    let text = js_trim(&redact_tokens(content, tokens)).to_string();

    // 轻量清理：过期想法移除；超过 24h 未提起的话题移除（避免无限膨胀）
    let thoughts = arr_mut(st, "pendingThoughts");
    thoughts.retain(|t| {
        let expires = num_of(t.get("expiresAt"));
        expires == 0 || now < expires
    });
    let topics = arr_mut(st, "activeTopics");
    topics.retain(|t| {
        let last = num_of(t.get("lastMentionAt"));
        last == 0 || now - last < 24 * 60 * 60 * 1000
    });

    if category == "activeTopic" && !text.is_empty() {
        let topics = arr_mut(st, "activeTopics");
        let existing = topics
            .iter_mut()
            .find(|t| str_of(t.get("text")) == text);
        match existing {
            Some(t) => {
                t["lastMentionAt"] = json!(now);
                if !extra.participants.is_empty() {
                    let mut merged: Vec<String> = t
                        .get("participants")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().map(|p| str_of(Some(p))).collect())
                        .unwrap_or_default();
                    for p in &extra.participants {
                        let p = redact_tokens(p, tokens);
                        if !merged.contains(&p) {
                            merged.push(p);
                        }
                    }
                    merged.truncate(10);
                    t["participants"] = json!(merged);
                }
                if !extra.pending_question.is_empty() {
                    t["pendingQuestion"] =
                        json!(truncate_text(&redact_tokens(&extra.pending_question, tokens), 200));
                }
            }
            None => {
                let participants: Vec<String> = extra
                    .participants
                    .iter()
                    .map(|p| redact_tokens(p, tokens))
                    .take(10)
                    .collect();
                topics.push(json!({
                    "text": truncate_text(&text, 200),
                    "lastMentionAt": now,
                    "participants": participants,
                    "pendingQuestion": truncate_text(&redact_tokens(&extra.pending_question, tokens), 200),
                }));
            }
        }
        let len = topics.len();
        if len > 20 {
            topics.drain(0..len - 20);
        }
    } else if category == "pendingThought" && !text.is_empty() {
        let expires = now + if extra.expires_at_ms != 0 { extra.expires_at_ms } else { 2 * 60 * 60 * 1000 };
        let thoughts = arr_mut(st, "pendingThoughts");
        let existing = thoughts.iter_mut().find(|t| str_of(t.get("text")) == text);
        match existing {
            Some(t) => {
                t["createdAt"] = json!(now);
                t["expiresAt"] = json!(expires);
                if !extra.motivation.is_empty() {
                    t["motivation"] = json!(truncate_text(&redact_tokens(&extra.motivation, tokens), 50));
                }
            }
            None => {
                let motivation = if extra.motivation.is_empty() {
                    "curiosity".to_string()
                } else {
                    extra.motivation.clone()
                };
                thoughts.push(json!({
                    "text": truncate_text(&text, 200),
                    "createdAt": now,
                    "expiresAt": expires,
                    "motivation": truncate_text(&redact_tokens(&motivation, tokens), 50),
                }));
            }
        }
        let len = thoughts.len();
        if len > 20 {
            thoughts.drain(0..len - 20);
        }
    } else if category == "memberImpression" {
        let target = extra.target.clone();
        if target.is_empty() {
            return;
        }
        let impressions = obj_mut(st, "memberImpressions");
        let old = impressions.get(&target).cloned().unwrap_or_else(|| json!({}));
        let mut traits: Vec<String> = old
            .get("traits")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(|x| str_of(Some(x))).collect())
            .unwrap_or_default();
        // A repeated trait moves to the newest slot; beyond the limit the oldest trait is replaced.
        let trait_text = truncate_text(&text, 50);
        if !text.is_empty() {
            traits.retain(|t| *t != trait_text);
            traits.push(trait_text);
        }
        if traits.len() > policy.max_traits {
            traits.drain(..traits.len() - policy.max_traits);
        }
        // Only a member speaking refreshes `lastSeenAt` (`note_member_seen`); a write starts it for a new entry.
        let last_seen = if old.get("lastSeenAt").is_some() { num_of(old.get("lastSeenAt")) } else { now };
        impressions.insert(
            target,
            json!({
                "traits": traits,
                "interactionCount": num_of(old.get("interactionCount")).max(0),
                "lastSeenAt": last_seen,
            }),
        );
        cap_impressions(impressions, policy.max_members);
    }
}

fn cap_impressions(impressions: &mut Map<String, Value>, max_members: usize) {
    if impressions.len() <= max_members {
        return;
    }
    let mut entries: Vec<(String, i64)> = impressions
        .iter()
        .map(|(k, v)| (k.clone(), num_of(v.get("lastSeenAt"))))
        .collect();
    entries.sort_by_key(|(_, ts)| *ts);
    for (k, _) in entries.iter().take(entries.len() - max_members) {
        impressions.remove(k);
    }
}

/// `memory-remove`：按内容删话题/想法，按名字删印象。
pub fn remove_memory(st: &mut Value, category: &str, content: &str, target: &str) {
    if category == "activeTopic" {
        if let Some(arr) = st.get_mut("activeTopics").and_then(|v| v.as_array_mut()) {
            arr.retain(|t| str_of(t.get("text")) != content);
        }
    } else if category == "pendingThought" {
        if let Some(arr) = st.get_mut("pendingThoughts").and_then(|v| v.as_array_mut()) {
            arr.retain(|t| str_of(t.get("text")) != content);
        }
    } else if category == "memberImpression" {
        if let Some(map) = st.get_mut("memberImpressions").and_then(|v| v.as_object_mut()) {
            map.remove(target);
        }
    }
}

/// `memory-clear`：category 为空 = 全清。
pub fn clear_memory(st: &mut Value, category: &str) {
    if category.is_empty() || category == "activeTopic" {
        st["activeTopics"] = json!([]);
    }
    if category.is_empty() || category == "pendingThought" {
        st["pendingThoughts"] = json!([]);
    }
    if category.is_empty() || category == "memberImpression" {
        st["memberImpressions"] = json!({});
    }
}

/// `memory-update` 的输入。
#[derive(Debug, Clone, Default)]
pub struct UpdateExtra {
    pub pending_question: Option<String>,
    pub participants: Option<Vec<String>>,
    pub motivation: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub target: Option<String>,
}

fn opt_str(extra: &Value, field: &str, tokens: &[String]) -> Option<String> {
    let v = extra.get(field)?;
    if v.is_null() {
        return None;
    }
    Some(js_trim(&redact_tokens(&str_of(Some(v)), tokens)).to_string())
}

pub fn update_extra_from(extra: &Value, tokens: &[String]) -> UpdateExtra {
    let participants = extra
        .get("participants")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|p| js_trim(&redact_tokens(&str_of(Some(p)), tokens)).to_string()).collect());
    UpdateExtra {
        pending_question: opt_str(extra, "pendingQuestion", tokens),
        participants,
        motivation: opt_str(extra, "motivation", tokens),
        expires_at_ms: extra.get("expiresAtMs").filter(|v| !v.is_null()).map(|v| num_of(Some(v))),
        target: opt_str(extra, "target", tokens),
    }
}

/// 返回 Err((状态码, 文案))，与 JS 的 400/404 对齐。
pub fn update_memory(
    st: &mut Value,
    category: &str,
    old_content: &str,
    target: &str,
    new_content: Option<&str>,
    extra: &UpdateExtra,
    now: i64,
    policy: &ImpressionPolicy,
) -> Result<(), (u16, String)> {
    match category {
        "activeTopic" => {
            let topics = arr_mut(st, "activeTopics");
            let Some(t) = topics.iter_mut().find(|t| str_of(t.get("text")) == old_content) else {
                return Err((404, "找不到要编辑的 activeTopic".to_string()));
            };
            if let Some(c) = new_content {
                t["text"] = json!(truncate_text(c, 200));
            }
            if let Some(q) = &extra.pending_question {
                t["pendingQuestion"] = json!(truncate_text(q, 200));
            }
            if let Some(ps) = &extra.participants {
                t["participants"] = json!(ps.iter().take(10).cloned().collect::<Vec<_>>());
            }
        }
        "pendingThought" => {
            let thoughts = arr_mut(st, "pendingThoughts");
            let Some(t) = thoughts.iter_mut().find(|t| str_of(t.get("text")) == old_content) else {
                return Err((404, "找不到要编辑的 pendingThought".to_string()));
            };
            if let Some(c) = new_content {
                t["text"] = json!(truncate_text(c, 200));
            }
            if let Some(m) = &extra.motivation {
                t["motivation"] = json!(truncate_text(m, 50));
            }
            if let Some(ms) = extra.expires_at_ms {
                t["expiresAt"] = json!(now + ms.max(0));
            }
        }
        "memberImpression" => {
            let impressions = obj_mut(st, "memberImpressions");
            // 必须命中已有条目：否则任意 target 都能凭空建一条。
            let Some(old) = impressions.get(target).cloned() else {
                return Err((404, "找不到要编辑的群友印象（新印象请用 qq_memory_append 添加）".to_string()));
            };
            let new_target = extra
                .target
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| target.to_string());
            let new_target = truncate_text(&new_target, 64);
            if new_target.is_empty() {
                return Err((400, "非法的群友名字".to_string()));
            }
            let mut im = old;
            if let Some(c) = new_content {
                // The newest traits are kept, as `append_memory` does. `interactionCount` is only ever counted
                // from speaking (`note_member_seen`), so a write cannot set it.
                let mut traits: Vec<String> = c
                    .split(['，', ',', '、'])
                    .map(|s| js_trim(s).to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if traits.len() > policy.max_traits {
                    traits.drain(..traits.len() - policy.max_traits);
                }
                im["traits"] = json!(traits);
            }
            if new_target != target {
                impressions.remove(target);
            }
            impressions.insert(new_target, im);
            cap_impressions(impressions, policy.max_members);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(text: &str) -> Value {
        json!({ "text": text, "lastMentionAt": 1_000, "participants": [], "pendingQuestion": "" })
    }

    #[test]
    fn truncate_text_matches_js_semantics() {
        assert_eq!(truncate_text("abcdef", 3), "abc");
        assert_eq!(truncate_text("abc", 3), "abc");
        assert_eq!(truncate_text("abc", 0), "");
        // 切点落在代理对中间：前移一个码元，不留孤立高代理
        assert_eq!(truncate_text("a🐟b", 2), "a");
        // limit=1 且开头就是代理对：至少保留一个完整码点
        assert_eq!(truncate_text("🐟b", 1), "🐟");
    }

    #[test]
    fn truncate_text_tail_matches_js_semantics() {
        assert_eq!(truncate_text_tail("abcdef", 3), "def");
        assert_eq!(truncate_text_tail("abc", 3), "abc");
        assert_eq!(truncate_text_tail("abc", 0), "");
        // 切点落在代理对中间：整对保留（宁可多一个码元）
        assert_eq!(truncate_text_tail("a🐟b", 2), "🐟b");
    }

    #[test]
    fn append_dedupes_topic_and_caps_at_20() {
        let mut st = json!({ "activeTopics": [], "pendingThoughts": [], "memberImpressions": {} });
        let extra = AppendExtra::default();
        for i in 0..25 {
            append_memory(&mut st, "activeTopic", &format!("话题{i}"), &extra, &[], 1_000 + i, &ImpressionPolicy::default());
        }
        let arr = st["activeTopics"].as_array().unwrap();
        assert_eq!(arr.len(), 20, "话题上限 20，超了从头丢");
        assert_eq!(str_of(arr[19].get("text")), "话题24");

        append_memory(&mut st, "activeTopic", "话题24", &extra, &[], 9_999, &ImpressionPolicy::default());
        let arr = st["activeTopics"].as_array().unwrap();
        assert_eq!(arr.len(), 20, "同名话题是合并，不是新增");
        assert_eq!(num_of(arr[19].get("lastMentionAt")), 9_999);
    }

    #[test]
    fn append_prunes_expired_thoughts() {
        let mut st = json!({
            "activeTopics": [],
            "pendingThoughts": [
                { "text": "过期", "createdAt": 0, "expiresAt": 500, "motivation": "x" },
                { "text": "有效", "createdAt": 0, "expiresAt": 0, "motivation": "x" }
            ],
            "memberImpressions": {}
        });
        append_memory(&mut st, "pendingThought", "新的", &AppendExtra::default(), &[], 1_000, &ImpressionPolicy::default());
        let arr = st["pendingThoughts"].as_array().unwrap();
        let texts: Vec<String> = arr.iter().map(|t| str_of(t.get("text"))).collect();
        assert!(!texts.contains(&"过期".to_string()));
        assert!(texts.contains(&"有效".to_string()));
        assert!(texts.contains(&"新的".to_string()));
    }

    #[test]
    fn impression_keeps_traits_unique_and_counts_only_observed_interactions() {
        let policy = ImpressionPolicy::default();
        let mut st = json!({ "activeTopics": [], "pendingThoughts": [], "memberImpressions": {} });
        let mut extra = AppendExtra { target: "小明".into(), ..Default::default() };
        append_memory(&mut st, "memberImpression", "爱玩梗", &extra, &[], 1_000, &policy);
        append_memory(&mut st, "memberImpression", "爱玩梗", &extra, &[], 2_000, &policy);
        append_memory(&mut st, "memberImpression", "话少", &extra, &[], 3_000, &policy);
        let im = &st["memberImpressions"]["小明"];
        assert_eq!(num_of(im.get("interactionCount")), 0, "tool writes are not interactions");
        let traits: Vec<String> = im
            .get("traits")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|t| str_of(Some(t)))
            .collect();
        assert_eq!(traits, vec!["爱玩梗".to_string(), "话少".to_string()]);

        // Speaking is the interaction: matched by QQ id or display name, others untouched.
        append_memory(&mut st, "memberImpression", "x", &AppendExtra { target: "10001".into(), ..Default::default() }, &[], 3_500, &policy);
        note_member_seen(&mut st, Some("10001"), Some("别人"), 4_000);
        note_member_seen(&mut st, None, Some("小明"), 5_000);
        note_member_seen(&mut st, Some(""), Some("路人"), 6_000);
        assert_eq!(num_of(st["memberImpressions"]["小明"].get("interactionCount")), 1);
        assert_eq!(num_of(st["memberImpressions"]["小明"].get("lastSeenAt")), 5_000);
        assert_eq!(num_of(st["memberImpressions"]["10001"].get("interactionCount")), 1);

        // The JS prototype names are ordinary names in a Rust map.
        extra.target = "__proto__".into();
        append_memory(&mut st, "memberImpression", "坏", &extra, &[], 7_000, &policy);
        assert!(st["memberImpressions"].get("__proto__").is_some());
    }

    #[test]
    fn trait_replacement_drops_the_oldest_and_a_repeat_refreshes() {
        let policy = ImpressionPolicy { max_traits: 3, ..ImpressionPolicy::default() };
        let mut st = json!({ "activeTopics": [], "pendingThoughts": [], "memberImpressions": {} });
        let extra = AppendExtra { target: "小明".into(), ..Default::default() };
        for (i, t) in ["a", "b", "c", "d"].iter().enumerate() {
            append_memory(&mut st, "memberImpression", t, &extra, &[], 1_000 + i as i64, &policy);
        }
        let traits = |st: &Value| st["memberImpressions"]["小明"]["traits"].clone();
        assert_eq!(traits(&st), json!(["b", "c", "d"]), "the newest trait is kept, the oldest replaced");
        append_memory(&mut st, "memberImpression", "b", &extra, &[], 2_000, &policy);
        append_memory(&mut st, "memberImpression", "e", &extra, &[], 2_001, &policy);
        assert_eq!(traits(&st), json!(["d", "b", "e"]), "a repeated trait is refreshed, not evicted first");
        // The previous latest trait survives an update when the list was already full at ten.
        let policy = ImpressionPolicy::default();
        let mut st = json!({ "memberImpressions": {"小明": {"traits": (0..11).map(|i| format!("t{i}")).collect::<Vec<_>>(), "interactionCount": 0, "lastSeenAt": 1}} });
        append_memory(&mut st, "memberImpression", "新", &extra, &[], 3_000, &policy);
        let kept = st["memberImpressions"]["小明"]["traits"].as_array().unwrap().clone();
        assert_eq!(kept.len(), 10);
        assert_eq!(kept[8], json!("t10"), "the latest old trait is not discarded");
        assert_eq!(kept[9], json!("新"));
    }

    #[test]
    fn prompt_selection_prefers_speakers_then_recent_not_storage_order() {
        let mut impressions = serde_json::Map::new();
        for i in 0..15 {
            impressions.insert(format!("人{i}"), json!({"traits": ["t"], "interactionCount": 0, "lastSeenAt": 100 + i}));
        }
        // 人0 was inserted first and is not among the newest keys, but is speaking now.
        let st = json!({
            "memberImpressions": impressions,
            "unread": [{"sender": "人0", "userId": "9", "time": 500}],
            "recentMessages": [{"sender": "陌生人", "userId": "人3", "time": 400}, {"sender": "人1", "time": 300, "isSelf": true}],
        });
        let names = |limit| select_impressions(&st, limit).iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>();
        let ten = names(10);
        assert_eq!(ten.len(), 10);
        assert!(ten.contains(&"人0".to_string()) && ten.contains(&"人3".to_string()), "speakers selected: {ten:?}");
        assert!(!ten.contains(&"人1".to_string()), "the bot's own message is not a speaker: {ten:?}");
        assert!(ten.contains(&"人14".to_string()) && ten.contains(&"人13".to_string()), "then most recently seen: {ten:?}");
        let mut sorted = ten.clone();
        sorted.sort_by_key(|n| n[3..].parse::<u32>().unwrap());
        assert_eq!(ten, sorted, "selected members keep storage order");
        assert_eq!(names(1), ["人0"], "the latest speaker wins a one-slot limit");
        assert_eq!(select_impressions(&json!({}), 10).len(), 0);
    }

    #[test]
    fn impression_policy_reads_bounded_context_settings() {
        assert_eq!(ImpressionPolicy::from_cfg(&json!({})), ImpressionPolicy::default());
        let cfg = json!({"socialV2": {"context": {"impressionPromptLimit": "4", "impressionMaxTraits": 99, "impressionMaxMembers": 0}}});
        assert_eq!(ImpressionPolicy::from_cfg(&cfg), ImpressionPolicy { prompt_members: 4, max_traits: 20, max_members: 1 });
    }

    #[test]
    fn impressions_cap_at_50_by_last_seen() {
        let policy = ImpressionPolicy::default();
        let mut st = json!({ "activeTopics": [], "pendingThoughts": [], "memberImpressions": {} });
        for i in 0..60 {
            let extra = AppendExtra { target: format!("人{i}"), ..Default::default() };
            append_memory(&mut st, "memberImpression", "t", &extra, &[], 1_000 + i, &policy);
        }
        let map = st["memberImpressions"].as_object().unwrap();
        assert_eq!(map.len(), 50);
        assert!(!map.contains_key("人0"), "最旧的应该被淘汰");
        assert!(map.contains_key("人59"));
    }

    #[test]
    fn update_member_impression_requires_existing_entry() {
        let mut st = json!({ "activeTopics": [], "pendingThoughts": [], "memberImpressions": {} });
        let err = update_memory(&mut st, "memberImpression", "", "不存在", None, &UpdateExtra::default(), 1, &ImpressionPolicy::default())
            .unwrap_err();
        assert_eq!(err.0, 404);
    }

    #[test]
    fn a_tool_write_never_refreshes_last_seen_of_an_existing_member() {
        let policy = ImpressionPolicy::default();
        let mut st = json!({ "memberImpressions": {"小明": {"traits": ["话少"], "interactionCount": 4, "lastSeenAt": 100}} });
        let extra = AppendExtra { target: "小明".into(), ..Default::default() };
        append_memory(&mut st, "memberImpression", "爱玩梗", &extra, &[], 9_000, &policy);
        assert_eq!(num_of(st["memberImpressions"]["小明"].get("lastSeenAt")), 100, "only speaking refreshes lastSeenAt");
        assert_eq!(num_of(st["memberImpressions"]["小明"].get("interactionCount")), 4);
        let fresh = AppendExtra { target: "新人".into(), ..Default::default() };
        append_memory(&mut st, "memberImpression", "新", &fresh, &[], 9_000, &policy);
        assert_eq!(num_of(st["memberImpressions"]["新人"].get("lastSeenAt")), 9_000, "a brand-new entry starts now");
    }

    #[test]
    fn an_explicit_update_keeps_the_newest_traits_and_cannot_set_the_interaction_count() {
        let policy = ImpressionPolicy { max_traits: 3, ..ImpressionPolicy::default() };
        let mut st = json!({ "memberImpressions": {"小明": {"traits": ["旧"], "interactionCount": 3, "lastSeenAt": 5}} });
        let extra = update_extra_from(&json!({"interactionCount": 99}), &[]);
        update_memory(&mut st, "memberImpression", "", "小明", Some("甲，乙,丙、丁，戊"), &extra, 7_000, &policy).unwrap();
        let im = &st["memberImpressions"]["小明"];
        assert_eq!(im["traits"], json!(["丙", "丁", "戊"]), "the newest traits are kept, as append does");
        assert_eq!(num_of(im.get("interactionCount")), 3, "a write is not an interaction");
        assert_eq!(num_of(im.get("lastSeenAt")), 5);
    }

    #[test]
    fn remove_and_clear_behave_like_js() {
        let mut st = json!({
            "activeTopics": [topic("甲"), topic("乙")],
            "pendingThoughts": [{ "text": "想", "createdAt": 1, "expiresAt": 0, "motivation": "x" }],
            "memberImpressions": { "小明": { "traits": ["话少"], "interactionCount": 1, "lastSeenAt": 1 } }
        });
        remove_memory(&mut st, "activeTopic", "甲", "");
        assert_eq!(st["activeTopics"].as_array().unwrap().len(), 1);
        remove_memory(&mut st, "memberImpression", "", "小明");
        assert!(st["memberImpressions"].as_object().unwrap().is_empty());

        clear_memory(&mut st, "pendingThought");
        assert!(st["pendingThoughts"].as_array().unwrap().is_empty());
        assert_eq!(st["activeTopics"].as_array().unwrap().len(), 1, "只清指定类别");

        clear_memory(&mut st, "");
        assert!(st["activeTopics"].as_array().unwrap().is_empty());
        assert!(st["memberImpressions"].as_object().unwrap().is_empty());
    }

    #[test]
    fn redact_replaces_known_tokens_only() {
        let out = redact_tokens("我的令牌是 abc123，别外传", &["abc123".to_string()]);
        assert_eq!(out, "我的令牌是 ***，别外传");
        assert_eq!(redact_tokens("无关内容", &["abc123".to_string()]), "无关内容");
    }
}
