//! Memory and settings actions behind the console `/api/memory/*` routes, stdio MCP memory tools and
//! harness memory tools. Conversation authority decides who may act; rows are reached only through
//! `memory::MemoryStore`, which applies scope and the private rule.
use std::collections::HashMap;

use serde_json::{json, Value};

use crate::conversation_authority as authority;
use crate::memory::{ForgetError, MemoryStore, NewMemory};
use crate::runtime::RuntimeContext;

/// The MCP memory tools use the same authorization and mutations as the HTTP surface.
/// No console credential is supplied: a session cannot acquire administrator authority.
pub(crate) fn tool_action(context: &RuntimeContext, action: &str, args: &Value) -> Result<Value, String> {
    let cfg = context.load_config().map_err(|e| e.to_string())?;
    let mut body = args.clone();
    if let Some(obj) = body.as_object_mut() {
        // Keep old Rust callers usable while exposing the frozen JS parameter names.
        for (old, new) in [("setting", "settingKey"), ("read_type", "readType"), ("read_id", "readId")] {
            if !obj.contains_key(new) { if let Some(v) = obj.get(old).cloned() { obj.insert(new.to_string(), v); } }
        }
    }
    let headers = HashMap::from([("x-agent-token".to_string(), args["token"].as_str().unwrap_or("").to_string())]);
    handle(action, &headers, &HashMap::new(), &body.to_string(), &cfg, context).map_err(|(_, error)| error)
}

/// One memory or settings action; query parameters and the JSON body merge, body first.
pub(crate) fn handle(
    action: &str,
    headers: &HashMap<String, String>,
    query: &HashMap<String, String>,
    body: &str,
    cfg: &Value,
    context: &RuntimeContext,
) -> Result<Value, (u16, String)> {
    // Serialize with session reset/token rotation, and re-read configuration after waiting.
    // SQLite writers and ACL revocation below share this gate with MCP calls.
    let _gate = if context.is_isolated() {
        Some(crate::console::acquire_social_state_gate(context)?)
    } else {
        None
    };
    let fresh_cfg = if context.is_isolated() {
        Some(context.load_config().map_err(|e| {
            if action == "settings-set" && e == "runtime config is invalid" {
                (400, "selected runtime config is invalid".to_string())
            } else {
                (503, e.to_string())
            }
        })?)
    } else {
        None
    };
    let cfg = fresh_cfg.as_ref().unwrap_or(cfg);
    // JS 的写法：POST 读 body、其它方法用 query 参数。两边都合进来，body 优先。
    let mut merged = serde_json::Map::new();
    for (k, v) in query {
        merged.insert(k.clone(), json!(v));
    }
    let parsed_body = if context.is_isolated() {
        Some(crate::console::parse_object_body(body)?)
    } else {
        serde_json::from_str::<Value>(body).ok()
    };
    if let Some(Value::Object(o)) = parsed_body {
        for (k, v) in o {
            merged.insert(k, v);
        }
    }
    let b = Value::Object(merged);
    let console_ok = crate::console::memory_console_authority(headers, cfg, context);
    let agent_token = headers.get("x-agent-token").cloned().unwrap_or_default();
    let unavailable = |error: String| (503_u16, error);
    // `acl` 在 key 校验**之前**处理：控制台调用可以不带 key（JS 同序）
    if action == "acl" {
        let key0 = b.get("key").and_then(Value::as_str).unwrap_or("").trim();
        let owner_ok = authority::owner_holds_token(context, cfg, key0, &agent_token).map_err(unavailable)?;
        if !console_ok && !owner_ok {
            return Err((403, "ACL 操作只允许管理员私聊或控制台".to_string()));
        }
        let store = open_store(context, cfg)?;
        let admin = store.admin();
        let sub = b.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let rt = b.get("readerType").and_then(|v| v.as_str()).unwrap_or("session");
        let rid = b.get("readerId").and_then(|v| v.as_str()).unwrap_or("");
        let wt = b.get("readType").and_then(|v| v.as_str()).unwrap_or("*");
        let wid = b.get("readId").and_then(|v| v.as_str()).unwrap_or("*");
        if sub == "grant" {
            let level = b.get("level").and_then(|v| v.as_str()).unwrap_or("read");
            let id = admin.grant(rt, rid, wt, wid, level, "console").map_err(|e| (500, e.to_string()))?;
            let acl = admin.acl().map_err(|e| (500, e.to_string()))?;
            return Ok(json!({ "ok": true, "id": id, "acl": acl }));
        }
        if sub == "revoke" {
            let removed = admin.revoke(rt, rid, wt, wid, "console").map_err(|e| (500, e.to_string()))?;
            let acl = admin.acl().map_err(|e| (500, e.to_string()))?;
            return Ok(json!({ "ok": true, "removed": removed, "acl": acl }));
        }
        let acl = admin.acl().map_err(|e| (500, e.to_string()))?;
        return Ok(json!({ "ok": true, "acl": acl }));
    }
    // `settings-*` 只允许管理员私聊（自己的 agent token）；控制台令牌不行 ——
    // 它等于管理端全权，而"对话式配置"的整个设计前提是**只有管理员能改**。
    if action == "settings-get" || action == "settings-set" {
        let key0 = b.get("key").and_then(Value::as_str).unwrap_or("").trim();
        if !authority::owner_holds_token(context, cfg, key0, &agent_token).map_err(unavailable)? {
            return Err((403, "配置读写只允许管理员私聊".to_string()));
        }
        return crate::console::settings_action(action, &b, cfg, context);
    }
    if action == "archive" {
        let key = b.get("key").and_then(Value::as_str).unwrap_or("");
        let owner_ok = authority::owner_holds_token(context, cfg, key, &agent_token).map_err(unavailable)?;
        if !console_ok && !owner_ok {
            return Err((403, "归档操作只允许管理员私聊或控制台".to_string()));
        }
        let id = b.get("id").and_then(Value::as_i64).filter(|id| *id > 0).ok_or((400, "id 无效".to_string()))?;
        let archived = b.get("archived").and_then(Value::as_bool).unwrap_or(true);
        let store = MemoryStore::open(context, cfg).map_err(|e| (500, e))?;
        let changed = store.admin().archive(id, archived, "console").map_err(|e| (500, e.to_string()))?;
        return Ok(json!({"ok":true,"changed":changed,"id":id,"archived":archived}));
    }
    let key = b.get("key").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if key.is_empty() {
        return Err((400, "key 不能为空".to_string()));
    }
    // 其余动作必须带该会话自己的 agent token；控制台令牌在 MCP 进程里也有，
    // 若 search/write 也认它，任何会话都能借 MCP 读到别的会话语料。
    if !authority::holds_token(context, &key, &agent_token).map_err(unavailable)? {
        return Err((403, "agent token 无效（必须是该会话自己的令牌）".to_string()));
    }
    let store = open_store(context, cfg)?;
    let reader = store.for_reader(&key);
    match action {
        "write" => {
            let content = b.get("content").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if content.is_empty() {
                return Err((400, "content 不能为空".to_string()));
            }
            let kind = b.get("kind").and_then(|v| v.as_str()).unwrap_or("fact").to_string();
            let subject = b.get("subject").and_then(|v| v.as_str()).map(|s| s.to_string());
            let tags = b.get("tags").and_then(|v| v.as_str()).map(|s| s.to_string());
            let source = b.get("source").and_then(|v| v.as_str()).unwrap_or("agent").to_string();
            let importance = b.get("importance").and_then(|v| v.as_f64()).unwrap_or(0.5);
            let out = reader.write(&NewMemory {
                owner_type: b.get("ownerType").and_then(|v| v.as_str()).unwrap_or("session"),
                scope: b.get("scope").and_then(|v| v.as_str()),
                kind: &kind,
                subject: subject.as_deref(),
                content: &content,
                tags: tags.as_deref(),
                source: &source,
                importance,
            }).map_err(|e| (500, format!("写入记忆失败：{e}")))?;
            use sha1::{Digest, Sha1};
            let hash = format!("{:x}", Sha1::digest(content.as_bytes()));
            Ok(json!({ "ok": true, "id": out.id, "deduped": out.deduped, "hash": hash }))
        }
        "search" => {
            let query = b.get("query").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let kinds: Vec<String> = b
                .get("kinds")
                .and_then(|v| v.as_str())
                .map(|s| s.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect())
                .unwrap_or_default();
            // query 参数都是**字符串**，只认 as_i64 会让 limit=3 变成默认 8
            let limit = b
                .get("limit")
                .and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                })
                .filter(|n| n.is_finite())
                .unwrap_or(8.0)
                .clamp(1.0, 50.0) as usize;
            let hits = reader.search(&query, &kinds, limit)
                .map_err(|e| (500, format!("检索失败：{}", crate::memory::search_error_message(&e))))?;
            Ok(json!({ "ok": true, "count": hits.len(), "memories": hits }))
        }
        "forget" => {
            let Some(number) = b.get("id").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))).filter(|n| n.is_finite()) else {
                return Err((400, "id 无效".to_string()));
            };
            if number.fract() != 0.0 { return Err((403, "无权删除该条（不在可见范围）".to_string())); }
            let id = number as i64;
            // 先按可见范围确认这条确实看得见，看不见就当无权删（不是"不存在"）
            match reader.forget(id) {
                Ok(Some(ok)) => Ok(json!({ "ok": ok })),
                Ok(None) => Err((403, "无权删除该条（不在可见范围）".to_string())),
                Err(ForgetError::Check(e)) => Err((500, format!("检索失败：{e}"))),
                Err(ForgetError::Delete(e)) => Err((500, format!("删除失败：{e}"))),
            }
        }
        "preview" => {
            // 只读：把「若现在唤醒会注入的记忆段」原样返回，用于验证注入链路。
            // 用的是唤醒提示词里同一个 recall_block（不是另写一份），
            // 否则"预览"和"实际注入"会慢慢走偏，而这个接口的全部意义就是验证后者。
            // 传的是**该会话的** state（同提示词那条链），不是根 state ——
            // 传错会让 recall 拿不到 unread/recentMessages，检索词为空，第一层直接空掉。
            let conv = authority::conversation(context, &key).map_err(unavailable)?.unwrap_or_else(|| json!({}));
            let block = crate::prompt::recall_block_with_runtime(context, cfg, &conv, &key);
            Ok(json!({ "ok": true, "chars": block.encode_utf16().count(), "block": block }))
        }
        "index" => {
            let (total, terms) = reader.index(12).map_err(|e| (500, format!("生成索引失败：{e}")))?;
            Ok(json!({ "ok": true, "total": total, "terms": terms }))
        }
        _ => Err((404, format!("未知动作: {action}"))),
    }
}

fn open_store(context: &RuntimeContext, cfg: &Value) -> Result<MemoryStore, (u16, String)> {
    MemoryStore::open(context, cfg).map_err(|e| (500, format!("打开记忆库失败：{e}")))
}
