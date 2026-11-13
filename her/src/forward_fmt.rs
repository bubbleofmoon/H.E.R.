// 合并转发消息的**纯函数**移植（对应 JS 的 `src/forward.js`）：
//
// * 从 OneBot 消息段里提取并清洗 forward id
// * 把 `get_forward_msg` 的返回格式化成适合 AI 阅读的紧凑结构
// * 暴露每条消息里的图片/表情元数据与嵌套 forward id
//
// 两条安全原则照抄：
// 1. forward id 只允许安全字符、长度受限 —— 它会被当作参数传给 OneBot 并写进日志；
// 2. 只负责格式化，**不访问网络**；"id 必须来自本会话已见消息"由调用方校验。
//
// 这里最容易被写错的是**按 UTF-16 码元截断**：JS 的 `slice` 切在代理对中间会留下
// 孤立高代理，写进提示词就渲染成乱码或替换符。所以下面先用 `encode_utf16()` 展开、
// 切完再拼回去，并且和 `splitForQQ` 一样处理"切点被逼到 0"的边界。

use serde_json::{json, Value};

/// forward id 白名单：常见 QQ 资源 id 字符，拒绝空白与控制字符。
pub fn sanitize_forward_id(value: &str) -> String {
    let s = value.trim();
    if s.is_empty() || s.chars().count() > 256 {
        return String::new();
    }
    let ok = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.' || c == '-' || c == '/' || c == '=' || c == '+' || c == '@');
    if ok {
        s.to_string()
    } else {
        String::new()
    }
}

/// 从单个 forward 消息段的 `data` 里取 id。
pub fn forward_id_from_data(data: &Value) -> String {
    for k in ["id", "res_id", "file_id", "forward_id"] {
        if let Some(v) = data.get(k) {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => continue,
            };
            let clean = sanitize_forward_id(&s);
            if !clean.is_empty() {
                return clean;
            }
        }
    }
    String::new()
}

fn segment_text(seg: &Value) -> String {
    let Some(kind) = seg.get("type").and_then(|t| t.as_str()) else {
        return String::new();
    };
    let d = seg.get("data").cloned().unwrap_or_else(|| json!({}));
    let s = |k: &str| -> String {
        match d.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };
    match kind {
        "text" => s("text"),
        "at" => {
            if s("qq") == "all" {
                "@全体成员".to_string()
            } else {
                format!("@{}", s("qq"))
            }
        }
        "face" => format!("[表情{}]", s("id")),
        "image" => "[图片]".to_string(),
        "record" => "[语音]".to_string(),
        "video" => "[视频]".to_string(),
        "file" => format!("[文件{}]", s("name")),
        "reply" => "[引用]".to_string(),
        "forward" => "[转发]".to_string(),
        "json" => "[卡片消息]".to_string(),
        other => format!("[{other}]"),
    }
}

/// CQ 码不能原样塞给 AI —— 统一换成 `[媒体]` 占位符。
fn sanitize_cq_string(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        // 匹配 "[CQ:" ... "]"（大小写不敏感）
        if bytes[i] == '['
            && i + 3 < bytes.len()
            && bytes[i + 1].eq_ignore_ascii_case(&'c')
            && bytes[i + 2].eq_ignore_ascii_case(&'q')
            && bytes[i + 3] == ':'
        {
            let mut j = i + 4;
            while j < bytes.len() && bytes[j] != ']' {
                j += 1;
            }
            if j < bytes.len() {
                out.push_str("[媒体]");
                i = j + 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => sanitize_cq_string(s),
        Value::Array(arr) => {
            let joined: String = arr.iter().map(segment_text).collect();
            sanitize_cq_string(joined.trim())
        }
        Value::Object(o) => {
            if o.contains_key("type") {
                return sanitize_cq_string(segment_text(content).trim());
            }
            if let Some(Value::String(t)) = o.get("text") {
                return sanitize_cq_string(t);
            }
            if let Some(Value::Array(arr)) = o.get("content") {
                let joined: String = arr.iter().map(segment_text).collect();
                return sanitize_cq_string(joined.trim());
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn node_data(node: &Value) -> Value {
    node.get("data")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn node_text(node: &Value) -> String {
    let data = node_data(node);
    let top = node.get("message").or_else(|| node.get("content")).or_else(|| node.get("text"));
    let inner = data.get("message").or_else(|| data.get("content")).or_else(|| data.get("text"));
    let a = top.map(content_to_text).unwrap_or_default();
    if !a.is_empty() {
        return a;
    }
    inner.map(content_to_text).unwrap_or_default()
}

fn first_non_null_str(vals: &[Option<&Value>]) -> Option<String> {
    for v in vals {
        match v {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(Value::Number(n)) => return Some(n.to_string()),
            Some(Value::Bool(b)) => return Some(b.to_string()),
            _ => {}
        }
    }
    None
}

fn sender_name(node: &Value) -> String {
    let data = node_data(node);
    if let Some(sender) = node.get("sender") {
        if sender.is_object() {
            if let Some(name) = first_non_null_str(&[
                sender.get("nickname"),
                sender.get("card"),
                sender.get("user_id"),
            ]) {
                return truncate_text(&name, 100);
            }
        } else if let Some(Value::String(s)) = Some(sender) {
            if !s.is_empty() {
                return truncate_text(s, 100);
            }
        }
    }
    let name = first_non_null_str(&[
        data.get("name"),
        data.get("nickname"),
        data.get("card"),
        data.get("uin"),
        data.get("user_id"),
        node.get("name"),
        node.get("nickname"),
    ])
    .unwrap_or_default();
    truncate_text(&name, 100)
}

fn sender_user_id(node: &Value) -> Option<String> {
    let data = node_data(node);
    let sender = node.get("sender");
    if let Some(s) = sender {
        if s.is_object() {
            if let Some(id) = s.get("user_id") {
                return Some(match id {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                });
            }
        }
    }
    first_non_null_str(&[
        data.get("uin"),
        data.get("user_id"),
        node.get("uin"),
        node.get("user_id"),
    ])
}

fn node_time(node: &Value) -> Option<i64> {
    let data = node_data(node);
    for v in [node.get("time"), data.get("time")].into_iter().flatten() {
        if let Some(n) = v.as_i64() {
            return Some(n);
        }
        if let Some(f) = v.as_f64() {
            return Some(f as i64);
        }
    }
    None
}
fn node_message_id(node: &Value) -> Option<String> {
    let data = node_data(node);
    first_non_null_str(&[
        node.get("message_id"),
        node.get("messageId"),
        data.get("message_id"),
        data.get("id"),
    ])
}

fn node_message_seq(node: &Value) -> Option<String> {
    let data = node_data(node);
    first_non_null_str(&[
        node.get("message_seq"),
        node.get("messageSeq"),
        data.get("message_seq"),
        data.get("seq"),
    ])
}

fn node_content(node: &Value) -> Vec<Value> {
    let data = node_data(node);
    let top = node.get("message").or_else(|| node.get("content")).or_else(|| node.get("text"));
    let inner = data.get("message").or_else(|| data.get("content")).or_else(|| data.get("text"));
    for cand in [top, inner] {
        if let Some(Value::Array(a)) = cand {
            return a.clone();
        }
    }
    // 单个消息段也可能承载图片或嵌套转发
    // 单个消息段也可能承载图片或嵌套转发
    for v in [top, inner].into_iter().flatten() {
        if v.is_object() {
            if v.get("type").is_some() {
                return vec![v.clone()];
            }
            if let Some(Value::Array(a)) = v.get("content") {
                return a.clone();
            }
        }
    }
    Vec::new()
}

fn segment_media(seg: &Value) -> Option<Value> {
    let kind = seg.get("type").and_then(|t| t.as_str())?;
    let d = seg.get("data").cloned().unwrap_or_else(|| json!({}));
    let s = |k: &str| -> String {
        match d.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };
    match kind {
        "image" | "mface" => {
            let file = if s("file").is_empty() {
                if s("file_id").is_empty() {
                    s("filename")
                } else {
                    s("file_id")
                }
            } else {
                s("file")
            };
            let url = if s("url").is_empty() {
                s("src")
            } else {
                s("url")
            };
            let emoji_id = if d.get("emoji_id").is_some_and(|v| !v.is_null()) {
                s("emoji_id")
            } else {
                s("emojiId")
            };
            if file.is_empty() && url.is_empty() && emoji_id.is_empty() {
                None
            } else {
                let mut o = serde_json::Map::new();
                o.insert(
                    "kind".into(),
                    json!(if kind == "mface" { "sticker" } else { "image" }),
                );
                if !file.is_empty() {
                    o.insert("file".into(), json!(file));
                }
                if !url.is_empty() {
                    o.insert("url".into(), json!(url));
                }
                let sub_type = if kind == "mface" {
                    "1".to_string()
                } else if d.get("sub_type").is_some_and(|v| !v.is_null()) {
                    s("sub_type")
                } else {
                    s("subType")
                };
                let package = if d.get("emoji_package_id").is_some_and(|v| !v.is_null()) {
                    s("emoji_package_id")
                } else {
                    s("emojiPackageId")
                };
                for (key, value) in [
                    ("subType", sub_type),
                    ("summary", sanitize_media_summary(&s("summary"))),
                    ("emojiId", emoji_id),
                    ("emojiPackageId", package),
                    ("emojiKey", s("key")),
                ] {
                    if !value.is_empty() {
                        o.insert(key.into(), json!(value));
                    }
                }
                Some(Value::Object(o))
            }
        }
        "face" => {
            let face_id = if s("id").is_empty() {
                s("face_id")
            } else {
                s("id")
            };
            if face_id.is_empty() {
                None
            } else {
                let mut media = json!({ "kind": "face", "faceId": face_id });
                for (key, value) in [
                    ("url", s("url")),
                    ("summary", sanitize_media_summary(&s("summary"))),
                ] {
                    if !value.is_empty() {
                        media[key] = json!(value);
                    }
                }
                Some(media)
            }
        }
        _ => None,
    }
}

fn sanitize_media_summary(value: &str) -> String {
    let mut clean = String::new();
    let mut control_run = false;
    for c in value.chars() {
        let control = c <= '\u{001f}' || matches!(c, '\u{007f}' | '\u{2028}' | '\u{2029}');
        if !control || !control_run {
            clean.push(if control { ' ' } else { c });
        }
        control_run = control;
    }
    clean.trim().chars().take(60).collect()
}

fn segment_nested_forward_id(seg: &Value) -> Option<String> {
    if seg.get("type").and_then(|t| t.as_str()) != Some("forward") {
        return None;
    }
    let data = seg.get("data").cloned().unwrap_or_else(|| json!({}));
    let id = forward_id_from_data(&data);
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

pub fn collect_segment_meta(content: &[Value]) -> (Vec<Value>, Vec<String>) {
    let mut media = Vec::new();
    let mut nested: Vec<String> = Vec::new();
    for seg in content {
        if let Some(m) = segment_media(seg) {
            media.push(m);
        }
        if let Some(id) = segment_nested_forward_id(seg) {
            if !nested.contains(&id) {
                nested.push(id);
            }
        }
    }
    (media, nested)
}

/// 按**字符**截断（JS 的 `truncateText`）。
fn truncate_text(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 按 UTF-16 码元截断，且不切断代理对 —— 与 JS `slice` + 手工前移的判据一致。
fn cut_utf16(text: &str, max: usize) -> (String, bool) {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return (text.to_string(), false);
    }
    let mut cut = max;
    // 切点前一个码元若是高代理，说明正好劈开了一对，前移一位
    if cut > 0 && (0xD800..=0xDBFF).contains(&units[cut - 1]) {
        cut -= 1;
    }
    // 切点被逼到 0：至少保留一个完整码点，否则预览会变成空串
    if cut == 0 {
        let first = units.first().copied().unwrap_or(0);
        let second = units.get(1).copied().unwrap_or(0);
        cut = if (0xD800..=0xDBFF).contains(&first) && (0xDC00..=0xDFFF).contains(&second) {
            2
        } else {
            1
        };
    }
    (String::from_utf16_lossy(&units[..cut]), true)
}

/// `formatForwardResponse` 的移植。`max_messages` 默认 50、`max_chars` 默认 500。
pub fn format_forward_response(data: &Value, max_messages: Option<i64>, max_chars: Option<i64>) -> Value {
    let max_messages = max_messages.unwrap_or(50).clamp(1, 100) as usize;
    let max_chars = max_chars.unwrap_or(500).clamp(1, 2000) as usize;
    let raw: Vec<Value> = match data {
        Value::Array(a) => a.clone(),
        other => other
            .get("messages")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let total = raw.len();
    let truncated_by_count = total > max_messages;
    let mut text_truncated = false;
    let mut messages = Vec::new();
    for (index, node) in raw.iter().take(max_messages).enumerate() {
        let full = node_text(node);
        let (text, cut_happened) = cut_utf16(&full, max_chars);
        if cut_happened {
            text_truncated = true;
        }
        let content = node_content(node);
        let (media, nested) = collect_segment_meta(&content);
        messages.push(json!({
            "index": index + 1,
            "sender": sender_name(node),
            "userId": sender_user_id(node),
            "time": node_time(node),
            "messageId": node_message_id(node),
            "messageSeq": node_message_seq(node),
            "text": text,
            "media": media,
            "nestedForwardIds": nested,
        }));
    }
    json!({
        "total": total,
        "truncated": truncated_by_count || text_truncated,
        "textTruncated": text_truncated,
        "messages": messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_id_rejects_bad_chars() {
        assert_eq!(sanitize_forward_id("abc-123_xyz:./=+@"), "abc-123_xyz:./=+@");
        assert_eq!(sanitize_forward_id("a b"), "");
        assert_eq!(sanitize_forward_id(""), "");
        assert_eq!(sanitize_forward_id(&"x".repeat(257)), "");
    }

    #[test]
    fn segment_text_covers_common_types() {
        assert_eq!(segment_text(&json!({"type":"text","data":{"text":"hi"}})), "hi");
        assert_eq!(segment_text(&json!({"type":"at","data":{"qq":"all"}})), "@全体成员");
        assert_eq!(segment_text(&json!({"type":"at","data":{"qq":"123"}})), "@123");
        assert_eq!(segment_text(&json!({"type":"image","data":{}})), "[图片]");
        assert_eq!(segment_text(&json!({"type":"face","data":{"id":"9"}})), "[表情9]");
        assert_eq!(segment_text(&json!({"type":"weird","data":{}})), "[weird]");
    }

    #[test]
    fn cq_codes_become_placeholder() {
        assert_eq!(sanitize_cq_string("[CQ:image,file=a.jpg]hi"), "[媒体]hi");
        assert_eq!(sanitize_cq_string("[cq:face,id=1]"), "[媒体]");
        assert_eq!(sanitize_cq_string("no cq here"), "no cq here");
        // 没有闭合方括号时原样保留，不能吞掉正文
        assert_eq!(sanitize_cq_string("[CQ:broken"), "[CQ:broken");
    }

    #[test]
    fn utf16_cut_never_splits_surrogate_pair() {
        // "😀" 占 2 个 UTF-16 码元；max=1 时不能切出半个
        let (out, cut) = cut_utf16("😀ab", 1);
        assert!(cut);
        assert_eq!(out, "😀");
        // 普通字符按码元切
        let (out, _) = cut_utf16("abcdef", 3);
        assert_eq!(out, "abc");
        // 未超长时不截断
        let (out, cut) = cut_utf16("ab", 5);
        assert!(!cut);
        assert_eq!(out, "ab");
    }

    #[test]
    fn format_keeps_media_and_nested_ids() {
        let data = json!({"messages": [
            {"sender": {"nickname": "甲"}, "time": 1700000000, "message_id": 7,
             "message": [{"type":"text","data":{"text":"看图"}},
                         {"type":"image","data":{"url":"https://x/y.png"}},
                         {"type":"forward","data":{"id":"fwd-1"}}]}
        ]});
        let out = format_forward_response(&data, None, None);
        assert_eq!(out["total"], 1);
        assert_eq!(out["truncated"], false);
        assert_eq!(out["messages"][0]["sender"], "甲");
        assert_eq!(out["messages"][0]["text"], "看图[图片][转发]");
        assert_eq!(out["messages"][0]["media"][0]["kind"], "image");
        assert_eq!(out["messages"][0]["nestedForwardIds"][0], "fwd-1");
    }

    #[test]
    fn review_segment_metadata_keeps_market_stickers_and_emoji_fallback_fields() {
        let (media, forwards) = collect_segment_meta(&[
            json!({"type":"image","data":{"file":"cache.png","sub_type":0,"subType":1,"summary":"  hi\n\u{0001}there\u{2028}! ","emoji_id":"aabbccddeeff00112233445566778899","emoji_package_id":42,"key":"market-key"}}),
            json!({"type":"mface","data":{"url":"https://public.example/sticker.gif","summary":"market","emojiId":"112233445566778899aabbccddeeff00","emojiPackageId":"package"}}),
            json!({"type":"image","data":{"subType":1,"emojiId":"112233445566778899aabbccddeeff00"}}),
            json!({"type":"face","data":{"id":14,"url":"https://public.example/face.png","summary":"smile"}}),
            json!({"type":"forward","data":{"id":"fwd-review"}}),
        ]);
        assert_eq!(forwards, vec!["fwd-review"]);
        assert_eq!(media.len(), 4);
        assert_eq!(
            media[0],
            json!({"kind":"image","file":"cache.png","subType":"0","summary":"hi there !","emojiId":"aabbccddeeff00112233445566778899","emojiPackageId":"42","emojiKey":"market-key"})
        );
        assert_eq!(
            media[1],
            json!({"kind":"sticker","url":"https://public.example/sticker.gif","subType":"1","summary":"market","emojiId":"112233445566778899aabbccddeeff00","emojiPackageId":"package"})
        );
        assert_eq!(media[2]["emojiId"], "112233445566778899aabbccddeeff00");
        assert_eq!(media[2]["subType"], "1");
        assert_eq!(media[3]["summary"], "smile");
        assert_eq!(media[3]["url"], "https://public.example/face.png");
        let (media, _) = collect_segment_meta(&[
            json!({"type":"image","data":{"file":"a","summary":"a".repeat(90)}}),
        ]);
        assert_eq!(media[0]["summary"], "a".repeat(60));
    }

    #[test]
    fn format_truncates_by_count() {
        let msgs: Vec<Value> = (0..3).map(|i| json!({"message": format!("m{i}")})).collect();
        let out = format_forward_response(&json!({ "messages": msgs }), Some(2), None);
        assert_eq!(out["total"], 3);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["messages"].as_array().unwrap().len(), 2);
    }
}
