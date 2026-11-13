//! P7：QQ 收藏表情同步。
//!
//! JS 版在 `sticker-lib.js` 里做三件事：拉取（`fetch_custom_face_detail`）、
//! 合并（保留本地认知：localNote / tags / useCount）、落盘。
//! 这里逐条对齐，重点是把「合并」写成纯函数——它全是状态迁移，最该被测。
//!
//! 一条硬规则（JS 里踩过坑）：**「文件不存在」和「文件读坏了」必须分开**。
//! 读坏了当成空库再合并写回，会把 AI 攒的备注/标签/使用次数永久清零。

use serde_json::{Value, json};
use crate::runtime::{RuntimeArea, RuntimeContext};
use std::path::Path;

pub const STICKER_FILE: &str = "C:/qqbot/qq-bridge/state/stickers.json";
const MAX_FETCH: usize = 500;
const DEFAULT_COUNT: usize = 100;
const TTL_MS: i64 = 60_000;

fn now_ms() -> i64 {
    crate::parity_clock::now_ms() as i64
}

pub(crate) fn now_iso() -> String {
    iso_at(now_ms())
}

/// 把毫秒时间戳格式化成 JS `toISOString()` 的形态（UTC，毫秒三位）。
pub(crate) fn iso_at(ms: i64) -> String {
    // 桥接用的是 ISO 字符串；这里保持同样的形态，两边文件可以互相覆盖。
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        ms.rem_euclid(1000)
    )
}

pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 清洗文本字段：去控制字符、裁首尾空白、按字符数截断。
fn sanitize(v: Option<&Value>, max: usize) -> String {
    let s = match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let cleaned: String = s.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() > max {
        trimmed.chars().take(max).collect()
    } else {
        trimmed.to_string()
    }
}

fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    v.get(name).filter(|x| !x.is_null())
}

/// 规范化一条表情记录（对应 JS 的 normalizeStickerEntry）。没有 id 就丢弃。
pub fn normalize(raw: &Value) -> Option<Value> {
    let id = {
        let a = sanitize(field(raw, "id"), 200);
        if !a.is_empty() {
            a
        } else {
            let b = sanitize(field(raw, "emoji_id"), 200);
            if !b.is_empty() {
                b
            } else {
                sanitize(field(raw, "resId"), 200)
            }
        }
    };
    if id.is_empty() {
        return None;
    }
    let res_id = {
        let r = sanitize(field(raw, "resId"), 200);
        if r.is_empty() { id.clone() } else { r }
    };
    let tags: Vec<Value> = field(raw, "tags")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .map(|t| sanitize(Some(t), 24))
                .filter(|t| !t.is_empty())
                .take(20)
                .map(Value::String)
                .collect()
        })
        .unwrap_or_default();
    let source = match field(raw, "source").and_then(|s| s.as_str()) {
        Some("manual") => "manual",
        Some("ai") => "ai",
        _ => "qq",
    };
    // 先算好：json! 里的 `{ ... }` 会被当成嵌套 JSON 对象，不能直接写代码块。
    let created_at = {
        let s = sanitize(field(raw, "createdAt"), 40);
        if s.is_empty() { now_iso() } else { s }
    };
    let updated_at = {
        let s = sanitize(field(raw, "updatedAt"), 40);
        if s.is_empty() { now_iso() } else { s }
    };
    Some(json!({
        "id": id,
        "resId": res_id,
        "url": sanitize(field(raw, "url"), 2000),
        "md5": sanitize(field(raw, "md5"), 100).to_uppercase(),
        "desc": sanitize(field(raw, "desc"), 200),
        "localNote": sanitize(field(raw, "localNote"), 200),
        "tags": tags,
        "usage": sanitize(field(raw, "usage"), 200),
        "source": source,
        "useCount": field(raw, "useCount").and_then(|v| v.as_i64()).unwrap_or(0).max(0),
        "lastUsedAt": field(raw, "lastUsedAt").and_then(|v| v.as_i64()).unwrap_or(0),
        "lastContext": sanitize(field(raw, "lastContext"), 200),
        "createdAt": created_at,
        "updatedAt": updated_at
    }))
}

/// JS 的 `stickerUrlKey`：只认 `http(s)://…` 或 `host/path`，归一成 `host/path`
/// （丢协议、丢查询串、去尾斜杠、host 转小写）。
///
/// 没有用 url crate：这里只需要 host+path，手写反而少一层依赖和失败分支。
fn url_key(value: &str) -> String {
    let raw = value.trim();
    if raw.is_empty() {
        return String::new();
    }
    let lower = raw.to_ascii_lowercase();
    let has_scheme = lower.starts_with("http://") || lower.starts_with("https://");
    if !has_scheme {
        // 无协议时要求 `host/path`：host 段不能为空、不能有空白
        match raw.split_once('/') {
            Some((host, _)) if !host.is_empty() && !host.chars().any(|c| c.is_whitespace()) => {}
            _ => return String::new(),
        }
    }
    let with_scheme = if has_scheme {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let Some((_, rest)) = with_scheme.split_once("://") else {
        return String::new();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') || authority.chars().any(|c| c.is_whitespace()) {
        return String::new();
    }
    let host = authority.to_ascii_lowercase();
    // 查询串/锚点不属于 pathname
    let path = path.split(['?', '#']).next().unwrap_or("");
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        host
    } else {
        format!("{host}/{trimmed}")
    }
}

/// JS 的 `findSticker`：id / resId / md5 精确匹配优先，再按 URL 归一化匹配。
/// 返回下标（JS 返回对象，Rust 侧还要按下标替换）。
pub fn find(entries: &[Value], r: &str) -> Option<usize> {
    let raw = r.trim();
    if raw.is_empty() {
        return None;
    }
    let md5 = raw.to_uppercase();
    for (i, e) in entries.iter().enumerate() {
        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let res_id = e.get("resId").and_then(|v| v.as_str()).unwrap_or("");
        if id == raw || res_id == raw {
            return Some(i);
        }
        if let Some(m) = e.get("md5").and_then(|v| v.as_str()) {
            if !m.is_empty() && m == md5 {
                return Some(i);
            }
        }
    }
    let key = url_key(raw);
    if key.is_empty() {
        return None;
    }
    entries.iter().position(|e| {
        url_key(e.get("url").and_then(|v| v.as_str()).unwrap_or("")) == key
    })
}

/// 对应 JS 的 `applyStickerNote`：改 localNote / tags / usage，其余字段保留。
///
/// `None` 表示没找到这个表情（调用方回 404）；`Some((新列表, 新条目))` 才需要落盘。
pub fn apply_note(
    entries: &[Value],
    id: &str,
    note: Option<&str>,
    tags: Option<Vec<String>>,
    usage: Option<&str>,
) -> Option<(Vec<Value>, Value)> {
    let list: Vec<Value> = entries.iter().filter_map(normalize).collect();
    let idx = find(&list, id)?;
    let target = list[idx].clone();
    let mut next = target.clone();
    if let Some(n) = note {
        next["localNote"] = json!(n);
    }
    if let Some(t) = tags {
        let cleaned: Vec<String> = t
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .take(20)
            .collect();
        next["tags"] = json!(cleaned);
    }
    if let Some(u) = usage {
        next["usage"] = json!(u);
    }
    let source = target.get("source").and_then(|v| v.as_str()).unwrap_or("");
    next["source"] = json!(if source.is_empty() { "ai" } else { source });
    next["updatedAt"] = json!(now_iso());
    let normalized = normalize(&next)?;
    let mut out = list;
    out[idx] = normalized.clone();
    Some((out, normalized))
}

/// `formatStickerList` 的 Rust 版：控制台/工具用的表情列表视图。
///
/// 注意返回的是**视图字段**（不含 createdAt/updatedAt/lastContext），
/// 与 JS 的 `formatStickerList()` 逐字段一致；多带字段会让前端渲染出多余列。
pub fn format_list(entries: &[Value], query: &str, limit: i64) -> Value {
    let list: Vec<Value> = entries.iter().filter_map(normalize).collect();
    let q = query.trim().to_lowercase();
    let filtered: Vec<Value> = if q.is_empty() {
        list.clone()
    } else {
        list.iter()
            .filter(|e| {
                let mut haystack = String::new();
                for k in ["desc", "localNote", "usage", "id", "resId", "md5"] {
                    haystack.push_str(e.get(k).and_then(|v| v.as_str()).unwrap_or(""));
                    haystack.push(' ');
                }
                if let Some(tags) = e.get("tags").and_then(|v| v.as_array()) {
                    for t in tags {
                        haystack.push_str(t.as_str().unwrap_or(""));
                        haystack.push(' ');
                    }
                }
                haystack.to_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    };
    let max = if limit <= 0 { 48 } else { limit.clamp(1, 500) };
    let items: Vec<Value> = filtered
        .iter()
        .take(max as usize)
        .map(|e| {
            json!({
                "id": e.get("id").cloned().unwrap_or(Value::Null),
                "resId": e.get("resId").cloned().unwrap_or(Value::Null),
                "md5": e.get("md5").cloned().unwrap_or(Value::Null),
                "url": e.get("url").cloned().unwrap_or(Value::Null),
                "desc": e.get("desc").cloned().unwrap_or_else(|| json!("")),
                "localNote": e.get("localNote").cloned().unwrap_or_else(|| json!("")),
                "tags": e.get("tags").cloned().unwrap_or_else(|| json!([])),
                "usage": e.get("usage").cloned().unwrap_or_else(|| json!("")),
                "useCount": e.get("useCount").cloned().unwrap_or_else(|| json!(0)),
                "lastUsedAt": e.get("lastUsedAt").cloned().unwrap_or_else(|| json!(0)),
                "source": e.get("source").cloned().unwrap_or_else(|| json!("qq")),
            })
        })
        .collect();
    json!({
        "total": list.len(),
        "matched": filtered.len(),
        "truncated": filtered.len() > max as usize,
        "stickers": items,
    })
}

/// 合并 QQ 拉回来的列表与本地库（对应 JS 的 mergeStickerLibrary）。///
/// **本地认知优先**：localNote / tags / usage / useCount / lastUsedAt / lastContext
/// 一律保留旧值，只更新 QQ 侧会变的字段（url / md5 / desc）。
/// `complete=true` 时才清理「QQ 那边已删除」的 qq 来源条目；
/// 拉取被 count 截断时必须传 false，否则会把没返回的表情误判成已删除。
pub fn merge(existing: &[Value], fetched: &[Value], complete: bool) -> Vec<Value> {
    let mut out: Vec<Value> = existing.iter().filter_map(normalize).collect();

    let fetched_ids: Vec<String> = fetched
        .iter()
        .map(|item| {
            let a = sanitize(field(item, "emoji_id"), 200);
            if !a.is_empty() {
                a
            } else {
                let b = sanitize(field(item, "resId"), 200);
                if !b.is_empty() { b } else { sanitize(field(item, "id"), 200) }
            }
        })
        .filter(|s| !s.is_empty())
        .collect();

    for item in fetched {
        let id = {
            let a = sanitize(field(item, "emoji_id"), 200);
            if !a.is_empty() {
                a
            } else {
                let b = sanitize(field(item, "resId"), 200);
                if !b.is_empty() { b } else { sanitize(field(item, "id"), 200) }
            }
        };
        if id.is_empty() {
            continue;
        }
        let old = out.iter().find(|e| e.get("id").and_then(|v| v.as_str()) == Some(&id)).cloned();

        // 逐字段构造：QQ 侧字段优先，本地认知一律沿用旧值。
        let pick = |name: &str, item: &Value, old: Option<&Value>, local_only: bool| -> Value {
            if local_only {
                return old
                    .and_then(|o| o.get(name))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            match field(item, name) {
                Some(v) if v.as_str().map(|s| !s.is_empty()).unwrap_or(true) => v.clone(),
                _ => old
                    .and_then(|o| o.get(name))
                    .cloned()
                    .unwrap_or(Value::Null),
            }
        };

        let merged = normalize(&json!({
            "id": id,
            "resId": pick("resId", item, old.as_ref(), false),
            "url": pick("url", item, old.as_ref(), false),
            "md5": pick("md5", item, old.as_ref(), false),
            "desc": pick("desc", item, old.as_ref(), false),
            "localNote": pick("localNote", item, old.as_ref(), true),
            "tags": pick("tags", item, old.as_ref(), true),
            "usage": pick("usage", item, old.as_ref(), true),
            "source": old
                .as_ref()
                .and_then(|o| o.get("source"))
                .cloned()
                .unwrap_or_else(|| Value::String("qq".into())),
            "useCount": pick("useCount", item, old.as_ref(), true),
            "lastUsedAt": pick("lastUsedAt", item, old.as_ref(), true),
            "lastContext": pick("lastContext", item, old.as_ref(), true),
            "createdAt": pick("createdAt", item, old.as_ref(), true),
            "updatedAt": now_iso(),
        }));
        let Some(merged) = merged else { continue };

        match out
            .iter()
            .position(|e| e.get("id").and_then(|v| v.as_str()) == Some(&id))
        {
            Some(idx) => out[idx] = merged,
            None => out.push(merged),
        }
    }

    if complete {
        out.retain(|e| {
            let source = e.get("source").and_then(|s| s.as_str()).unwrap_or("qq");
            let id = e.get("id").and_then(|i| i.as_str()).unwrap_or("");
            source != "qq" || fetched_ids.iter().any(|f| f == id)
        });
    }
    out
}

/// 读取本地表情库。返回 `Ok(None)` 表示文件不存在，`Err` 表示文件存在但读不动/读坏了。
///
/// 这个区分是故意的：读坏了绝不能当成空库——否则下一次合并会把它覆盖成空。
#[cfg(test)]
mod legacy_load_scope {
    use super::RuntimeContext;
    use std::cell::RefCell;
    use std::sync::Arc;

    std::thread_local! {
        static CONTEXTS: RefCell<Vec<Arc<RuntimeContext>>> = RefCell::new(Vec::new());
    }

    struct ScopeGuard;

    impl Drop for ScopeGuard {
        fn drop(&mut self) {
            CONTEXTS.with(|contexts| {
                let popped = contexts.borrow_mut().pop();
                debug_assert!(popped.is_some(), "legacy load scope stack underflow");
            });
        }
    }

    pub(super) fn current() -> Option<Arc<RuntimeContext>> {
        CONTEXTS.with(|contexts| contexts.borrow().last().cloned())
    }

    pub(super) fn with_context<T>(context: RuntimeContext, action: impl FnOnce() -> T) -> T {
        CONTEXTS.with(|contexts| contexts.borrow_mut().push(Arc::new(context)));
        let _scope = ScopeGuard;
        action()
    }
}

pub fn load() -> Result<Option<Vec<Value>>, String> {
    #[cfg(test)]
    if let Some(context) = legacy_load_scope::current() {
        return load_for_runtime(context.as_ref());
    }
    load_for_runtime(&RuntimeContext::legacy())
}

pub fn load_for_runtime(context: &RuntimeContext) -> Result<Option<Vec<Value>>, String> {
    let text = if context.is_isolated() {
        context
            .read_optional_text(RuntimeArea::Data, "stickers.json")
            .map_err(|error| format!("读表情库失败：{error}"))?
    } else {
        read_legacy_sticker_text(Path::new(STICKER_FILE))?
    };
    let Some(text) = text else {
        return Ok(None);
    };
    parse_sticker_text(&text).map(Some)
}

fn parse_sticker_text(text: &str) -> Result<Vec<Value>, String> {
    let cleaned = text.trim_start_matches('\u{feff}');
    let parsed: Value = serde_json::from_str(cleaned).map_err(|e| format!("表情库 JSON 损坏：{e}"))?;
    match parsed {
        Value::Array(a) => Ok(a),
        other => Err(format!("表情库结构异常（顶层是 {}）", if other.is_object() { "对象" } else { "非数组" })),
    }
}

fn read_legacy_sticker_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读表情库失败：{error}")),
    }
}

pub fn save(entries: &[Value]) -> Result<(), String> {
    save_for_runtime(&RuntimeContext::legacy(), entries)
}

pub fn save_for_runtime(context: &RuntimeContext, entries: &[Value]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    context
        .write_atomic(RuntimeArea::Data, "stickers.json", text.as_bytes())
        .map_err(|e| format!("写表情库失败：{e}"))
}

/// The result of a sticker-store transaction. `Changed` persists the supplied in-memory update;
/// `Unchanged` returns its value without replacing the selected file.
pub enum StoreUpdate<T> {
    Changed(T),
    Unchanged(T),
}

/// Read the freshest selected sticker array, apply an in-memory update, and persist it while
/// holding the selected-root gate. The callback must remain synchronous and must not perform
/// network or other external I/O. Legacy callers retain their original path and do not acquire
/// an isolated-runtime gate.
pub fn mutate_for_runtime<T>(
    context: &RuntimeContext,
    update: impl FnOnce(&mut Vec<Value>) -> Result<StoreUpdate<T>, String>,
) -> Result<T, String> {
    let _gate = if context.is_isolated() {
        Some(acquire_sticker_mutation_gate(context)?)
    } else {
        None
    };
    let mut entries = load_for_runtime(context)?.unwrap_or_default();
    match update(&mut entries)? {
        StoreUpdate::Changed(result) => {
            save_for_runtime(context, &entries)?;
            Ok(result)
        }
        StoreUpdate::Unchanged(result) => Ok(result),
    }
}

fn acquire_sticker_mutation_gate(context: &RuntimeContext) -> Result<std::fs::File, String> {
    const LOCK_FILE: &str = "locks/stickers.lock";
    const WAIT_BUDGET: std::time::Duration = std::time::Duration::from_millis(300);
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

    let deadline = std::time::Instant::now() + WAIT_BUDGET;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("runtime file is already locked".to_string());
        }
        match context.open_locked_file(RuntimeArea::Data, LOCK_FILE) {
            Ok(gate) if std::time::Instant::now() < deadline => return Ok(gate),
            Ok(gate) => {
                drop(gate);
                return Err("runtime file is already locked".to_string());
            }
            Err(error) if error == "runtime file is already locked" => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(error);
                }
                std::thread::sleep(remaining.min(RETRY_INTERVAL));
            }
            Err(error) => return Err(error),
        }
    }
}

/// 从配置里取拉取数量与 TTL（对应 JS 的 maxListCount / syncTtlMs）。
fn limits(cfg: &Value) -> (usize, i64) {
    let s = cfg.get("socialV2").and_then(|v| v.get("sticker"));
    let count = s
        .and_then(|x| x.get("maxListCount"))
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_COUNT as u64);
    let count = count.clamp(1, MAX_FETCH as u64) as usize;
    let ttl = s
        .and_then(|x| x.get("syncTtlMs"))
        .and_then(|v| v.as_i64())
        .unwrap_or(TTL_MS)
        .max(0);
    (count, ttl)
}

pub fn enabled(cfg: &Value) -> bool {
    cfg.get("socialV2")
        .and_then(|v| v.get("sticker"))
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 同步一次。返回 (拉到的条数, 合并后的条数)。
pub fn sync(onebot: &crate::onebot::OneBot, cfg: &Value) -> Result<(usize, usize), String> {
    sync_for_runtime(&RuntimeContext::legacy(), onebot, cfg)
}

/// Build the OneBot client from the selected runtime and synchronize its selected sticker store.
/// Isolated callers must use this entry point so the HTTP endpoint can never be inherited from
/// candidate config or silently fall back to the legacy local gateway. `sync` and
/// `sync_for_runtime` remain available for compatibility with callers that already supply a
/// client explicitly.
pub fn sync_with_runtime(
    context: &RuntimeContext,
    cfg: &Value,
) -> Result<(usize, usize), String> {
    let http_url = if context.is_isolated() {
        context
            .dependency_url("onebot")
            .ok_or_else(|| "runtime OneBot dependency is unavailable".to_string())?
            .to_string()
    } else {
        cfg.get("snowluma")
            .and_then(|value| value.get("httpUrl"))
            .and_then(Value::as_str)
            .unwrap_or("http://127.0.0.1:3000")
            .to_string()
    };
    let token = cfg
        .get("snowluma")
        .and_then(|value| value.get("accessToken"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let onebot = if context.is_isolated() {
        crate::onebot::OneBot::try_new_url(&http_url, token)?
    } else {
        crate::onebot::OneBot::new(&http_url, token)
    };
    sync_for_runtime(context, &onebot, cfg)
}

pub fn sync_for_runtime(
    context: &RuntimeContext,
    onebot: &crate::onebot::OneBot,
    cfg: &Value,
) -> Result<(usize, usize), String> {
    if !enabled(cfg) {
        return Err("贴纸功能已关闭".to_string());
    }
    let (count, _ttl) = limits(cfg);
    let resp = onebot
        .request("fetch_custom_face_detail", json!({ "count": count }))
        .ok_or("fetch_custom_face_detail 无响应")?;
    if resp.get("status").and_then(|s| s.as_str()) != Some("ok")
        || resp.get("retcode").and_then(|r| r.as_i64()) != Some(0)
    {
        return Err(format!(
            "fetch_custom_face_detail 失败：{}",
            resp.get("wording").and_then(|w| w.as_str()).unwrap_or("unknown")
        ));
    }
    // 只有拿到合法数组才允许合并；data 异常时不能拿空数组清空本地库。
    let fetched = resp
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("fetch_custom_face_detail 返回 data 不是数组，已放弃同步")?
        .clone();

    let complete = fetched.len() < count;
    let fetched_count = fetched.len();
    let merged_count = mutate_for_runtime(context, |existing| {
        // The selected store is loaded after the network response while holding its persistent
        // gate, so local notes written during the fetch participate in this merge.
        let merged = merge(existing, &fetched, complete);
        let merged_count = merged.len();
        *existing = merged;
        Ok(StoreUpdate::Changed(merged_count))
    })?;
    Ok((fetched_count, merged_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, use_count: i64, note: &str) -> Value {
        json!({
            "id": id, "resId": id, "url": "https://x/y.gif", "md5": "abc",
            "desc": "旧备注", "localNote": note, "tags": ["可爱"],
            "usage": "卖萌", "source": "qq", "useCount": use_count,
            "lastUsedAt": 111, "lastContext": "上次", "createdAt": "2026-01-01T00:00:00.000Z",
            "updatedAt": "2026-01-01T00:00:00.000Z"
        })
    }

    struct StickerRuntimeRoot(std::path::PathBuf);

    impl Drop for StickerRuntimeRoot {
        fn drop(&mut self) {
            let Ok(temp_root) = std::fs::canonicalize(std::env::temp_dir()) else {
                return;
            };
            let Ok(root) = std::fs::canonicalize(&self.0) else {
                return;
            };
            if root.parent() != Some(temp_root.as_path())
                || !root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("her-sticker-runtime-"))
            {
                return;
            }
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn sticker_runtime_root(
        label: &str,
        endpoint: &str,
        token: &str,
        legacy_http_url: &str,
    ) -> (StickerRuntimeRoot, RuntimeContext) {
        static NEXT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = StickerRuntimeRoot(crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-sticker-runtime-{label}-{}-{sequence}",
            std::process::id()
        ))));
        std::fs::create_dir(&root.0).expect("unique sticker fixture root");
        std::fs::create_dir_all(root.0.join("config")).unwrap();
        std::fs::create_dir_all(root.0.join("data")).unwrap();
        std::fs::create_dir_all(root.0.join("resources")).unwrap();
        let config = json!({
            "consoleToken": "fixture-console-token-abcdefghij",
            "snowluma": { "httpUrl": legacy_http_url, "accessToken": token },
            "socialV2": { "sticker": { "enabled": true, "maxListCount": 20 } }
        });
        std::fs::write(
            root.0.join("config/config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();
        std::fs::write(root.0.join("data/social-v2.json"), b"{}")
            .unwrap();
        std::fs::write(
            root.0.join("dependencies.json"),
            serde_json::to_vec(&json!({
                "dsh": "http://127.0.0.1:49101",
                "model": "http://127.0.0.1:49102",
                "onebot": endpoint
            }))
            .unwrap(),
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root.0).expect("synthetic sticker runtime");
        (root, context)
    }

    #[test]
    fn legacy_load_wrapper_reads_only_scoped_selected_root_and_preserves_both_files() {
        let (root_a, context_a) = sticker_runtime_root(
            "legacy load selected A 中文 space",
            "http://127.0.0.1:49111",
            "legacy-load-a-fixture-token",
            "http://127.0.0.1:1",
        );
        let (root_b, context_b) = sticker_runtime_root(
            "legacy load decoy B 中文 space",
            "http://127.0.0.1:49112",
            "legacy-load-b-fixture-token",
            "http://127.0.0.1:1",
        );
        let path_a = root_a.0.join("data/stickers.json");
        let path_b = root_b.0.join("data/stickers.json");
        let entries_a = vec![entry("selected-a", 3, "A local note")];
        let bytes_a = serde_json::to_vec(&entries_a).unwrap();
        let bytes_b_invalid = b"{ decoy B must not be read by A }";
        std::fs::write(&path_a, &bytes_a).unwrap();
        std::fs::write(&path_b, bytes_b_invalid).unwrap();

        let loaded_a = super::legacy_load_scope::with_context(context_a, load)
            .expect("selected A should load despite malformed decoy B")
            .expect("selected A exists");
        assert_eq!(loaded_a, entries_a);
        assert_eq!(std::fs::read(&path_a).unwrap(), bytes_a);
        assert_eq!(std::fs::read(&path_b).unwrap(), bytes_b_invalid);

        let bytes_a_invalid = b"{ decoy A must not be read by B }";
        let entries_b = vec![entry("selected-b", 9, "B local note")];
        let bytes_b = serde_json::to_vec(&entries_b).unwrap();
        std::fs::write(&path_a, bytes_a_invalid).unwrap();
        std::fs::write(&path_b, &bytes_b).unwrap();

        let loaded_b = super::legacy_load_scope::with_context(context_b, load)
            .expect("selected B should load despite malformed decoy A")
            .expect("selected B exists");
        assert_eq!(loaded_b, entries_b);
        assert_eq!(std::fs::read(&path_a).unwrap(), bytes_a_invalid);
        assert_eq!(std::fs::read(&path_b).unwrap(), bytes_b);
    }

    struct FakeOnebot {
        url: String,
        request: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        worker: Option<std::thread::JoinHandle<Result<(), String>>>,
    }

    impl Drop for FakeOnebot {
        fn drop(&mut self) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    impl FakeOnebot {
        fn once(response: Value) -> Self {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
                .expect("bind synthetic OneBot listener");
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let url = format!("http://{address}");
            let request = std::sync::Arc::new(std::sync::Mutex::new(None));
            let captured = request.clone();
            let worker = std::thread::spawn(move || {
                use std::io::Write;

                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                || error.raw_os_error() == Some(10035) =>
                        {
                            if std::time::Instant::now() >= deadline {
                                return Err("timed out waiting for fake OneBot request".into());
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => return Err(format!("fake OneBot accept failed: {error}")),
                    }
                };
                stream.set_nonblocking(false).map_err(|error| format!("set fake OneBot blocking read: {error}"))?;
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .map_err(|error| format!("set fake OneBot read timeout: {error}"))?;
                let raw = read_http_request(&mut stream)?;
                *captured.lock().unwrap() = Some(raw);
                let body = response.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .map_err(|error| format!("write fake OneBot response: {error}"))?;
                Ok(())
            });
            Self {
                url,
                request,
                worker: Some(worker),
            }
        }

        fn finish(mut self) -> String {
            self.worker
                .take()
                .expect("fake OneBot worker exists")
                .join()
                .expect("fake OneBot worker did not panic")
                .unwrap_or_else(|error| panic!("fake OneBot worker failed: {error}"));
            let request = self.request.lock().unwrap().clone();
            request.expect("fake OneBot captured one request")
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> Result<String, String> {
        use std::io::Read;

        let mut bytes = Vec::new();
        let mut buffer = [0u8; 2048];
        loop {
            if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&bytes[..header_end])
                    .map_err(|error| format!("fake request headers are not UTF-8: {error}"))?;
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .ok_or_else(|| "fake request has no valid content length".to_string())?;
                let request_end = header_end + 4 + content_length;
                if bytes.len() >= request_end {
                    bytes.truncate(request_end);
                    return String::from_utf8(bytes)
                        .map_err(|error| format!("fake request is not UTF-8: {error}"));
                }
            }
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("read fake OneBot request: {error}"))?;
            if read == 0 {
                return Err("fake OneBot request ended before its body".into());
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
    }

    fn assert_onebot_request(request: &str, token: &str) {
        assert!(request.starts_with("POST /fetch_custom_face_detail HTTP/1.1\r\n"));
        let headers = request.split_once("\r\n\r\n").unwrap().0;
        let authorization = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_string())
            })
            .expect("fake OneBot request has Authorization header");
        assert_eq!(authorization, format!("Bearer {token}"));
        assert!(request.ends_with(r#"{"count":20}"#));
    }

    #[test]
    fn isolated_sync_uses_selected_gateway_credentials_and_sticker_store() {
        let fake_a = FakeOnebot::once(json!({
            "status": "ok",
            "retcode": 0,
            "data": [{"emoji_id":"same","url":"https://fake-a.invalid/a.gif","md5":"face-a","desc":"fetched from A"}]
        }));
        let fake_b = FakeOnebot::once(json!({
            "status": "ok",
            "retcode": 0,
            "data": [{"emoji_id":"same","url":"https://fake-b.invalid/b.gif","md5":"face-b","desc":"fetched from B"}]
        }));
        let (root_a, context_a) = sticker_runtime_root(
            "selected A 中文 space",
            &fake_a.url,
            "selected-a-token-fixture",
            &fake_b.url,
        );
        let (root_b, context_b) = sticker_runtime_root(
            "decoy B 中文 space",
            &fake_b.url,
            "selected-b-token-fixture",
            "http://127.0.0.1:1",
        );
        let selected_a = root_a.0.join("data/stickers.json");
        let selected_b = root_b.0.join("data/stickers.json");
        std::fs::write(&selected_a, serde_json::to_vec(&[entry("same", 8, "A local note")]).unwrap())
            .unwrap();
        std::fs::write(&selected_b, serde_json::to_vec(&[entry("same", 12, "B local note")]).unwrap())
            .unwrap();
        let a_before = std::fs::read(&selected_a).unwrap();
        let b_before = std::fs::read(&selected_b).unwrap();

        let result_a = sync_with_runtime(&context_a, &context_a.load_config().unwrap());
        let request_a = fake_a.finish();
        assert_onebot_request(&request_a, "selected-a-token-fixture");
        assert_eq!(result_a.unwrap(), (1, 1));
        let stickers_a = load_for_runtime(&context_a).unwrap().unwrap();
        assert_eq!(stickers_a[0]["url"], "https://fake-a.invalid/a.gif");
        assert_eq!(stickers_a[0]["desc"], "fetched from A");
        assert_eq!(stickers_a[0]["localNote"], "A local note");
        assert_eq!(stickers_a[0]["useCount"], 8);
        let a_after_sync = std::fs::read(&selected_a).unwrap();
        assert_ne!(a_after_sync, a_before, "A's selected store must be updated");
        assert_eq!(std::fs::read(&selected_b).unwrap(), b_before);

        let result_b = sync_with_runtime(&context_b, &context_b.load_config().unwrap());
        let request_b = fake_b.finish();
        assert_onebot_request(&request_b, "selected-b-token-fixture");
        assert_eq!(result_b.unwrap(), (1, 1));
        let stickers_b = load_for_runtime(&context_b).unwrap().unwrap();
        assert_eq!(stickers_b[0]["url"], "https://fake-b.invalid/b.gif");
        assert_eq!(stickers_b[0]["desc"], "fetched from B");
        assert_eq!(stickers_b[0]["localNote"], "B local note");
        assert_eq!(stickers_b[0]["useCount"], 12);
        assert_eq!(std::fs::read(&selected_a).unwrap(), a_after_sync);
    }

    #[test]
    fn isolated_sync_does_not_write_while_selected_sticker_gate_is_owned() {
        let fake = FakeOnebot::once(json!({
            "status": "ok",
            "retcode": 0,
            "data": [{"emoji_id":"same","url":"https://fake.invalid/updated.gif","desc":"remote update"}]
        }));
        let (root, context) = sticker_runtime_root(
            "busy selected sticker gate 中文",
            &fake.url,
            "selected-busy-gate-token-fixture",
            "http://127.0.0.1:1",
        );
        let store = root.0.join("data/stickers.json");
        let original = serde_json::to_vec(&[entry("same", 7, "keep local note")]).unwrap();
        std::fs::write(&store, &original).unwrap();
        let _gate = context
            .open_locked_file(RuntimeArea::Data, "locks/stickers.lock")
            .expect("test owns the selected sticker mutation gate");

        // Sync must perform the network fetch before trying to acquire the local mutation gate.
        // With the selected gate busy, it must then fail without replacing the selected store.
        let result = sync_with_runtime(&context, &context.load_config().unwrap());
        let request = fake.finish();
        assert_onebot_request(&request, "selected-busy-gate-token-fixture");
        assert_eq!(
            std::fs::read(&store).unwrap(),
            original,
            "a sync that cannot acquire its selected mutation gate must not overwrite the store"
        );
        let error = result.expect_err("sync must report the held selected sticker gate");
        assert!(
            error.contains("already locked"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn isolated_mutations_from_two_contexts_preserve_both_fields() {
        let (root, context_a) = sticker_runtime_root(
            "same root mutation A 中文",
            "http://127.0.0.1:1",
            "selected-same-root-token-fixture",
            "http://127.0.0.1:1",
        );
        let context_b = RuntimeContext::from_root(&root.0).expect("second selected context");
        let store = root.0.join("data/stickers.json");
        std::fs::write(
            &store,
            serde_json::to_vec(&[entry("same", 0, "initial note")]).unwrap(),
        )
        .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let update_a = std::thread::spawn(move || {
            mutate_for_runtime(&context_a, |entries| {
                entries[0]["localNote"] = json!("written by A");
                entered_tx
                    .send(())
                    .map_err(|error| format!("signal A gate ownership: {error}"))?;
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(|error| format!("wait for A gate release: {error}"))?;
                Ok(StoreUpdate::Changed(()))
            })
        });
        if let Err(error) = entered_rx.recv_timeout(std::time::Duration::from_secs(3)) {
            let _ = release_tx.send(());
            let _ = update_a.join();
            panic!("A did not enter its selected-store transaction: {error}");
        }

        let closure_b_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closure_b_ran_worker = closure_b_ran.clone();
        let context_b_worker = context_b.clone();
        let (result_b_tx, result_b_rx) = std::sync::mpsc::channel();
        let update_b = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let result = mutate_for_runtime(&context_b_worker, |entries| {
                closure_b_ran_worker.store(true, std::sync::atomic::Ordering::SeqCst);
                entries[0]["useCount"] = json!(17);
                Ok(StoreUpdate::Changed(()))
            });
            let _ = result_b_tx.send((result, started.elapsed()));
        });
        let busy_while_a_owned = result_b_rx.recv_timeout(std::time::Duration::from_secs(2));

        // Always release and join A before asserting, so a failed observation cannot strand the
        // test thread or race fixture cleanup with an owner that still holds the OS gate.
        let release_result = release_tx.send(());
        let result_a = update_a.join();
        let result_b_after_release = if busy_while_a_owned.is_ok() {
            None
        } else {
            Some(result_b_rx.recv_timeout(std::time::Duration::from_secs(2)))
        };
        let join_b = update_b.join();

        assert!(release_result.is_ok(), "could not release A's selected gate");
        assert!(result_a.is_ok(), "A mutation thread panicked");
        assert!(result_a.unwrap().is_ok(), "A's mutation failed");
        assert!(join_b.is_ok(), "B mutation thread panicked");
        assert!(
            busy_while_a_owned.is_ok(),
            "B must return bounded busy while A owns the gate; after-release result: {result_b_after_release:?}"
        );
        let (busy_result, busy_elapsed) = busy_while_a_owned.unwrap();
        assert!(
            busy_result
                .as_ref()
                .is_err_and(|error| error.as_str() == "runtime file is already locked"),
            "B should receive the exact selected-gate busy result: {busy_result:?}"
        );
        assert!(
            !closure_b_ran.load(std::sync::atomic::Ordering::SeqCst),
            "B's mutation closure must not run without gate ownership"
        );
        assert!(
            busy_elapsed >= std::time::Duration::from_millis(250)
                && busy_elapsed < std::time::Duration::from_secs(2),
            "B's busy wait was not bounded near the 300 ms gate budget: {busy_elapsed:?}"
        );

        mutate_for_runtime(&context_b, |entries| {
            entries[0]["useCount"] = json!(17);
            Ok(StoreUpdate::Changed(()))
        })
        .expect("B's one bounded retry after A releases the gate");

        let saved = load_for_runtime(&context_b).unwrap().unwrap();
        assert_eq!(saved[0]["localNote"], "written by A");
        assert_eq!(saved[0]["useCount"], 17);
    }

    #[test]
    fn isolated_mutation_error_and_noop_do_not_write_and_release_gate() {
        let (root, context) = sticker_runtime_root(
            "mutation no write 中文",
            "http://127.0.0.1:1",
            "selected-mutation-noop-token-fixture",
            "http://127.0.0.1:1",
        );
        let store = root.0.join("data/stickers.json");
        let original = serde_json::to_vec(&[entry("same", 3, "original")]).unwrap();
        std::fs::write(&store, &original).unwrap();

        let rejected: Result<(), String> = mutate_for_runtime(&context, |entries| {
            entries[0]["localNote"] = json!("must not persist");
            Err("synthetic mutation rejection".to_string())
        });
        assert_eq!(rejected.unwrap_err(), "synthetic mutation rejection");
        assert_eq!(std::fs::read(&store).unwrap(), original);
        drop(
            context
                .open_locked_file(RuntimeArea::Data, "locks/stickers.lock")
                .expect("gate is released after callback error"),
        );

        let unchanged = mutate_for_runtime(&context, |entries| {
            Ok(StoreUpdate::Unchanged(entries.len()))
        })
        .unwrap();
        assert_eq!(unchanged, 1);
        assert_eq!(std::fs::read(&store).unwrap(), original);
        drop(
            context
                .open_locked_file(RuntimeArea::Data, "locks/stickers.lock")
                .expect("gate is released after unchanged update"),
        );
    }

    #[test]
    fn isolated_mutation_rejects_malformed_store_without_overwrite_and_releases_gate() {
        let (root, context) = sticker_runtime_root(
            "malformed mutation store 中文",
            "http://127.0.0.1:1",
            "selected-malformed-mutation-token-fixture",
            "http://127.0.0.1:1",
        );
        let store = root.0.join("data/stickers.json");
        let malformed = br#"{"not":"an array"}"#;
        std::fs::write(&store, malformed).unwrap();

        let result = mutate_for_runtime(&context, |_entries| Ok(StoreUpdate::Changed(())));
        assert!(result.unwrap_err().contains("结构异常"));
        assert_eq!(std::fs::read(&store).unwrap(), malformed);
        drop(
            context
                .open_locked_file(RuntimeArea::Data, "locks/stickers.lock")
                .expect("gate is released after malformed selected-store error"),
        );
    }

    #[test]
    fn isolated_sticker_gate_busy_retry_is_bounded() {
        let (root, context) = sticker_runtime_root(
            "bounded sticker gate 中文",
            "http://127.0.0.1:1",
            "selected-bounded-gate-token-fixture",
            "http://127.0.0.1:1",
        );
        let _gate = context
            .open_locked_file(RuntimeArea::Data, "locks/stickers.lock")
            .expect("test owns the selected sticker mutation gate");
        let started = std::time::Instant::now();
        let result = mutate_for_runtime(&context, |_entries| Ok(StoreUpdate::Changed(())));
        let elapsed = started.elapsed();
        assert!(result.unwrap_err().contains("already locked"));
        assert!(
            elapsed >= std::time::Duration::from_millis(250),
            "elapsed: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "elapsed: {elapsed:?}"
        );
    }

    #[test]
    fn isolated_sync_preserves_corrupt_selected_store_without_overwriting() {
        let fake = FakeOnebot::once(json!({
            "status": "ok",
            "retcode": 0,
            "data": [{"emoji_id":"same","url":"https://fake.invalid/new.gif"}]
        }));
        let (root, context) = sticker_runtime_root(
            "corrupt selected store 中文",
            &fake.url,
            "selected-corrupt-token-fixture",
            "http://127.0.0.1:1",
        );
        let store = root.0.join("data/stickers.json");
        let malformed = br#"{"this is not an array"#;
        std::fs::write(&store, malformed).unwrap();

        let result = sync_with_runtime(&context, &context.load_config().unwrap());
        let request = fake.finish();
        assert_onebot_request(&request, "selected-corrupt-token-fixture");
        assert!(result.unwrap_err().contains("JSON 损坏"));
        assert_eq!(std::fs::read(&store).unwrap(), malformed);
    }

    #[test]
    fn isolated_sync_rejects_non_array_gateway_data_without_overwriting() {
        let fake = FakeOnebot::once(json!({
            "status": "ok",
            "retcode": 0,
            "data": {"unexpected":"object"}
        }));
        let (root, context) = sticker_runtime_root(
            "bad gateway data 中文",
            &fake.url,
            "selected-gateway-token-fixture",
            "http://127.0.0.1:1",
        );
        let store = root.0.join("data/stickers.json");
        let original = serde_json::to_vec(&[entry("keep", 4, "preserve me")]).unwrap();
        std::fs::write(&store, &original).unwrap();

        let result = sync_with_runtime(&context, &context.load_config().unwrap());
        let request = fake.finish();
        assert_onebot_request(&request, "selected-gateway-token-fixture");
        assert!(result.unwrap_err().contains("data 不是数组"));
        assert_eq!(std::fs::read(&store).unwrap(), original);
    }

    #[test]
    fn legacy_sticker_loader_contract_is_testable_without_touching_production_files() {
        let path = crate::install_root::development_root(std::env::temp_dir().join(format!(
            "her-sticker-legacy-fixture-{}-{}",
            std::process::id(),
            now_ms()
        )));
        assert_eq!(read_legacy_sticker_text(&path).unwrap(), None);
        std::fs::write(&path, r#"[{"id":"fixture-sticker"}]"#).unwrap();
        let source = read_legacy_sticker_text(&path).unwrap().unwrap();
        assert_eq!(parse_sticker_text(&source).unwrap()[0]["id"], "fixture-sticker");
        std::fs::write(&path, "not-json").unwrap();
        let corrupt = read_legacy_sticker_text(&path).unwrap().unwrap();
        assert!(parse_sticker_text(&corrupt).unwrap_err().contains("JSON 损坏"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn normalize_requires_id() {
        assert!(normalize(&json!({"url":"x"})).is_none());
        let n = normalize(&json!({"emoji_id":"e1","md5":"ab"})).unwrap();
        assert_eq!(n["id"], "e1");
        assert_eq!(n["md5"], "AB", "md5 统一大写");
        assert_eq!(n["source"], "qq");
    }

    #[test]
    fn format_list_filters_and_truncates() {
        let a = normalize(&entry("a", 1, "好图")).unwrap();
        let b = normalize(&entry("b", 2, "")).unwrap();
        let list = vec![a, b];

        let all = format_list(&list, "", 48);
        assert_eq!(all["total"], 2);
        assert_eq!(all["matched"], 2);
        assert_eq!(all["truncated"], false);
        assert_eq!(all["stickers"].as_array().unwrap().len(), 2);
        // 视图字段是固定的一组，别把 createdAt 之类的内部字段漏出去
        let first = &all["stickers"][0];
        assert!(first.get("createdAt").is_none());
        assert_eq!(first["source"], "qq");

        // 查询命中：按 localNote 匹配
        let hit = format_list(&list, "好图", 48);
        assert_eq!(hit["matched"], 1);
        assert_eq!(hit["stickers"][0]["id"], "a");

        // 查询不命中
        let miss = format_list(&list, "查无此词", 48);
        assert_eq!(miss["matched"], 0);

        // 截断标记
        let cut = format_list(&list, "", 1);
        assert_eq!(cut["total"], 2);
        assert_eq!(cut["stickers"].as_array().unwrap().len(), 1);
        assert_eq!(cut["truncated"], true);
    }
    #[test]
    fn url_key_matches_js_normalisation() {
        assert_eq!(url_key("https://A.com/x/y/"), "a.com/x/y");
        assert_eq!(url_key("http://a.com/x/y"), "a.com/x/y");
        assert_eq!(url_key("a.com/x/y"), "a.com/x/y", "无协议时补 https");
        assert_eq!(url_key("https://a.com"), "a.com", "只有 host 时不留尾斜杠");
        assert_eq!(url_key("https://a.com/x?q=1#f"), "a.com/x", "查询串/锚点不算 pathname");
        assert_eq!(url_key(""), "");
        assert_eq!(url_key("不是网址"), "");
        assert_eq!(url_key("https://user:pw@a.com/x"), "", "带账号密码的不认");
    }

    #[test]
    fn find_prefers_exact_then_url() {
        // find 的契约和 JS 一样：传进来的必须是**已归一化**的列表
        // （apply_note 内部就是这么做的）。别拿裸 JSON 直接喂它。
        let list: Vec<Value> = [entry("a", 1, ""), entry("b", 2, "")]
            .iter()
            .filter_map(normalize)
            .collect();
        assert_eq!(find(&list, "a"), Some(0));
        assert_eq!(find(&list, "ABC"), Some(0), "md5 全大写后匹到 a（abc）");
        assert_eq!(find(&list, "nope"), None);
        assert_eq!(find(&list, "https://X/y.gif?z=1"), Some(0), "URL 归一化后匹配");
    }

    #[test]
    fn apply_note_keeps_other_fields_and_touches_only_given_ones() {
        let list = vec![entry("a", 7, "旧认知")];
        let (next, updated) = apply_note(&list, "a", Some("新认知"), None, None).unwrap();
        assert_eq!(updated["localNote"], "新认知");
        assert_eq!(updated["tags"], json!(["可爱"]), "没传 tags 就不该被清空");
        assert_eq!(updated["useCount"], 7);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0]["localNote"], "新认知");

        let (_, t) = apply_note(&list, "a", None, Some(vec!["  ".into(), "梗".into()]), None).unwrap();
        assert_eq!(t["tags"], json!(["梗"]));
        assert_eq!(t["localNote"], "旧认知", "没传 note 就不该被改");

        assert!(apply_note(&list, "查无此表情", Some("x"), None, None).is_none());
    }

    #[test]
    fn merge_preserves_local_knowledge() {
        // 本地攒的 useCount / localNote / tags 绝不能被 QQ 返回的数据冲掉
        let existing = vec![entry("e1", 7, "这是我最爱的一张")];
        let fetched = vec![json!({"emoji_id":"e1","url":"https://x/new.gif","md5":"def","desc":"QQ 新备注"})];
        let out = merge(&existing, &fetched, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["useCount"], 7, "使用次数必须保留");
        assert_eq!(out[0]["localNote"], "这是我最爱的一张");
        assert_eq!(out[0]["tags"][0], "可爱");
        assert_eq!(out[0]["url"], "https://x/new.gif", "QQ 侧字段要更新");
        assert_eq!(out[0]["md5"], "DEF");
        assert_eq!(out[0]["desc"], "QQ 新备注");
    }

    #[test]
    fn merge_adds_new_stickers() {
        let out = merge(&[], &[json!({"emoji_id":"new1","url":"u"})], true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "new1");
        assert_eq!(out[0]["useCount"], 0);
    }

    #[test]
    fn incomplete_fetch_never_prunes() {
        // count 截断时（complete=false）不能判「已删除」
        let existing = vec![entry("e1", 1, ""), entry("e2", 2, "")];
        let fetched = vec![json!({"emoji_id":"e1"})];
        let out = merge(&existing, &fetched, false);
        assert_eq!(out.len(), 2, "不完整的结果不许清理本地");
    }

    #[test]
    fn complete_fetch_prunes_deleted_qq_stickers() {
        let existing = vec![entry("e1", 1, ""), entry("e2", 2, "")];
        let fetched = vec![json!({"emoji_id":"e1"})];
        let out = merge(&existing, &fetched, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "e1");
    }

    #[test]
    fn complete_fetch_keeps_manual_entries() {
        // 手动加的表情不受 QQ 端删除影响
        let mut manual = entry("m1", 0, "");
        manual["source"] = json!("manual");
        let existing = vec![manual, entry("e1", 1, "")];
        let out = merge(&existing, &[json!({"emoji_id":"e1"})], true);
        assert_eq!(out.len(), 2, "manual 来源必须保留");
    }

    #[test]
    fn sanitize_strips_control_and_truncates() {
        assert_eq!(sanitize(Some(&json!("a\u{0}b\tc")), 100), "abc");
        assert_eq!(sanitize(Some(&json!("123456")), 3), "123");
    }

    #[test]
    fn load_reports_missing_vs_broken() {
        let (root, context) = sticker_runtime_root(
            "load missing vs broken",
            "http://127.0.0.1:49111",
            "load-missing-broken-fixture-token",
            "http://127.0.0.1:1",
        );
        let path = root.0.join("data/stickers.json");
        super::legacy_load_scope::with_context(context, || {
            assert_eq!(load().unwrap(), None, "a missing store is distinct from an empty one");
            std::fs::write(&path, b"[]").unwrap();
            assert_eq!(load().unwrap(), Some(vec![]));

            let entries = vec![entry("fixture", 2, "preserve local note")];
            let valid = serde_json::to_vec(&entries).unwrap();
            std::fs::write(&path, &valid).unwrap();
            assert_eq!(load().unwrap(), Some(entries));
            assert_eq!(std::fs::read(&path).unwrap(), valid);

            for (bytes, error) in [
                (b"not-json".as_slice(), "JSON 损坏"),
                (b"{}".as_slice(), "结构异常"),
            ] {
                std::fs::write(&path, bytes).unwrap();
                assert!(load().unwrap_err().contains(error));
                assert_eq!(std::fs::read(&path).unwrap(), bytes, "a broken store is never overwritten");
            }
        });
    }
}
