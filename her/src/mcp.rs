//! Stdio JSON-RPC MCP server. QQ definitions come from the frozen snowluma server;
//! the seven memory/settings tools come from its separate `mcp-memory.js` server.
//! Memory calls share the console action handler without supplying a console credential.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::memory;
use crate::runtime::{RuntimeArea, RuntimeContext};

/// 校验会话令牌：读所选运行时 `social-v2.json` 里的 agentToken。
#[cfg(test)]
fn token_ok(context: &RuntimeContext, key: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let Some(state) = context.read_social_state().ok() else {
        return false;
    };
    let Some(conv) = state.get("conversations").and_then(|c| c.get(key)) else {
        return false;
    };
    let expected = conv
        .get("agentToken")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    // 定长比较，避免按字节提前返回
    expected.len() == token.len()
        && expected
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
fn get_path<'a>(cfg: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = cfg;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

#[cfg(test)]
fn set_path(cfg: &mut Value, path: &str, value: Value) -> bool {
    let mut cur = cfg;
    let segs: Vec<&str> = path.split('.').collect();
    for seg in &segs[..segs.len() - 1] {
        let Some(next) = cur.get_mut(*seg) else {
            return false;
        };
        if !next.is_object() {
            *next = json!({});
        }
        cur = next;
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.insert(segs[segs.len() - 1].to_string(), value);
        return true;
    }
    false
}

/// The memory / settings tools of the JS `mcp-memory.js` server, served on the same server (ticket 17 owns them).
fn memory_tools() -> Value {
    json!({
        "tools": [
            {
                "name": "memory_write",
                "description": "记一条长期记忆。适合：用户的偏好/要求、重要决定、踩过的坑、群里的长期话题。不要记流水账。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string", "description": "会话 key，格式 group:群号 或 private:QQ号"},
                        "token": {"type": "string", "description": "会话令牌"},
                        "content": {"type": "string", "minLength": 1, "description": "记忆正文，一句话说清，别超过 200 字"},
                        "kind": {"type": "string", "enum": ["fact","topic","thought","impression","event","preference"], "description": "类型，默认 fact"},
                        "subject": {"type": "string", "description": "话题名或人名（模糊指代时靠它命中，建议填）"},
                        "tags": {"type": "string", "description": "逗号分隔的标签"},
                        "importance": {"type": "number", "minimum": 0, "maximum": 1, "description": "重要度 0~1，默认 0.5"}
                    },
                    "required": ["key", "token", "content"]
                }
            },
            {
                "name": "memory_search",
                "description": "检索长期记忆。遇到「那个 / 上次 / 之前说的」这类指代不明时，先用话题名试探，不要直接说不记得。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "token": {"type": "string"},
                        "query": {"type": "string", "description": "检索词（话题名/关键词，别用整句话）"},
                        "limit": {"type": "number", "minimum": 1, "maximum": 50, "description": "最多返回几条，默认 8"},
                        "kinds": {"type": "string", "description": "按类型过滤，逗号分隔"}
                    },
                    "required": ["key", "token", "query"]
                }
            },
            {
                "name": "memory_index",
                "description": "看本会话（及已授权范围）里有哪些记忆话题，不返回正文。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"key": {"type": "string"}, "token": {"type": "string"}},
                    "required": ["key", "token"]
                }
            },
            {
                "name": "memory_forget",
                "description": "删掉一条记忆（按 id）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "token": {"type": "string"},
                        "id": {"type": "number", "description": "记忆 id（来自 memory_search 的 #编号）"}
                    },
                    "required": ["key", "token", "id"]
                }
            },
            {
                "name": "memory_acl",
                "description": "管理记忆读取授权（grant / revoke / list）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "token": {"type": "string"},
                        "action": {"type": "string", "enum": ["grant", "revoke", "list"]},
                        "readerType": {"type": "string", "enum": ["session", "person", "team"]},
                        "readerId": {"type": "string"},
                        "readType": {"type": "string", "enum": ["session", "person", "team", "*"]},
                        "readId": {"type": "string"}
                    },
                    "required": ["key", "token", "action"]
                }
            },
            {
                "name": "settings_get",
                "description": "读取可热改的配置项。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"key": {"type": "string"}, "token": {"type": "string"}, "settingKey": {"type": "string"}},
                    "required": ["key", "token"]
                }
            },
            {
                "name": "settings_set",
                "description": "修改一个可热改的配置项（立即生效）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "token": {"type": "string"},
                        "settingKey": {"type": "string"},
                        "value": {"anyOf": [{"type":"string"},{"type":"number"},{"type":"boolean"}]}
                    },
                    "required": ["key", "token", "settingKey", "value"]
                }
            }
        ]
    })
}

/// `tools/list` result: the registered `qq_*` tools (JS registration order, config-gated at start-up),
/// then the memory / settings tools.
pub fn tool_list(cfg: &Value) -> Value {
    let mut tools = crate::qq_tools::registered_tools(cfg);
    if let Some(extra) = memory_tools().get("tools").and_then(|t| t.as_array()) {
        tools.extend(extra.iter().cloned());
    }
    json!({ "tools": tools })
}

#[allow(dead_code)] // Keep the no-context wrapper for existing in-crate callers.
pub fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    call_tool_with_context(name, args, &RuntimeContext::legacy())
}

/// Execute a tool using the caller's explicit runtime selection.
pub fn call_tool_with_context(
    name: &str,
    args: &Value,
    context: &RuntimeContext,
) -> Result<String, String> {
    call_tool_with_source(name, args, context).map(|(text, _)| text)
}

fn call_tool_with_source(name: &str, args: &Value, context: &RuntimeContext) -> Result<(String, Value), String> {
    let action = match name {
        "memory_write" => "write",
        "memory_search" => "search",
        "memory_index" => "index",
        "memory_forget" => "forget",
        "memory_acl" => "acl",
        "settings_get" => "settings-get",
        "settings_set" => "settings-set",
        _ => return Err(format!("未知工具：{name}")),
    };
    let source = crate::console::memory_tool_action(context, action, args)?;
    let result = if matches!(name, "memory_search" | "memory_index") {
        let cfg = context.load_config().map_err(str::to_string)?;
        crate::model_view::strip_chat_with_runtime(context, &source, &cfg)
    } else {
        source.clone()
    };
    Ok((format_tool_result(name, args, &result)?, source))
}

/// Render generated IDs/counters only after filtering the structured chat fields.
pub(crate) fn format_tool_result(name: &str, args: &Value, result: &Value) -> Result<String, String> {
    Ok(match name {
        "memory_write" => {
            if result["deduped"] == true {
                format!("已存在相同记忆（#{}），已更新使用时间。", result["id"])
            } else {
                format!("已记住（#{}）。", result["id"])
            }
        }
        "memory_search" => {
            let hits: Vec<memory::Hit> =
                serde_json::from_value(result["memories"].clone()).map_err(|e| e.to_string())?;
            if hits.is_empty() {
                "没有找到相关记忆。".to_string()
            } else {
                hits.iter()
                    .enumerate()
                    .map(|(i, m)| {
                        format!(
                            "{}. [#{} {}{}] {}",
                            i + 1,
                            m.id,
                            m.kind,
                            m.subject
                                .as_ref()
                                .filter(|s| !s.is_empty())
                                .map(|s| format!(" · {s}"))
                                .unwrap_or_default(),
                            m.content
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "memory_index" => {
            if result["total"] == 0 {
                "记忆库还是空的。".to_string()
            } else {
                let terms = result["terms"]
                    .as_array()
                    .ok_or("invalid memory index result")?
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("、");
                format!(
                    "共 {} 条，话题：{terms}（要细节用 memory_search）",
                    result["total"]
                )
            }
        }
        "memory_forget" => format!("已删除 #{}。", args["id"]),
        "memory_acl" => {
            let rows = result["acl"].as_array().ok_or("invalid memory ACL result")?;
            match args["action"].as_str().unwrap_or("list") {
                "grant" => format!("已授权。当前共 {} 条授权。", rows.len()),
                "revoke" => format!("已撤销。当前共 {} 条授权。", rows.len()),
                _ if rows.is_empty() => "还没有任何授权。".to_string(),
                _ => rows
                    .iter()
                    .map(|a| {
                        format!(
                            "[#{}] {}:{} 可读 {}:{} ({})",
                            a["id"],
                            a["reader_type"].as_str().unwrap_or(""),
                            a["reader_id"].as_str().unwrap_or(""),
                            a["read_type"].as_str().unwrap_or(""),
                            a["read_id"].as_str().unwrap_or(""),
                            a["level"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            }
        }
        "settings_get" => {
            if let Some(path) = args
                .get("settingKey")
                .or_else(|| args.get("setting"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                format!("{path} = {}", result.get("value").map(Value::to_string).unwrap_or_else(|| "undefined".to_string()))
            } else {
                let mut lines = result["settings"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| format!("{k} = {v}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                lines.push_str(&format!(
                    "\n\n不可通过对话修改：{}",
                    result["readonly"].as_str().unwrap_or("")
                ));
                lines
            }
        }
        "settings_set" => format!(
            "已改 {} = {}。{}",
            result["key"].as_str().unwrap_or(""),
            result["value"],
            result["note"].as_str().unwrap_or("")
        ),
        _ => return Err("unknown memory/settings result".into()),
    })
}

/// Protocol versions the JS server (MCP SDK 1.30) negotiates; an unknown request gets the latest.
const SUPPORTED_PROTOCOLS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
const SERVER_NAME: &str = "snowluma-safe";
/// Same string as the JS server so that clients see an identical `serverInfo`.
const SERVER_VERSION: &str = "0.2.0-r4";

/// stdio JSON-RPC loop with the legacy runtime.
pub fn serve() -> std::io::Result<()> {
    serve_with_context(&RuntimeContext::legacy())
}

fn text_outcome(text: String, is_error: bool) -> Value {
    if is_error {
        json!({ "content": [{ "type": "text", "text": text }], "isError": true })
    } else {
        json!({ "content": [{ "type": "text", "text": text }] })
    }
}

fn received_name(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
        Some(Value::String(_)) => "string",
        Some(Value::Number(_)) => "number",
        Some(Value::Bool(_)) => "boolean",
    }
}

/// One zod `invalid_type` issue: `(path, expected, received)`.
type Issue = (Vec<&'static str>, &'static str, &'static str);

fn issues_error(issues: &[Issue]) -> String {
    let list: Vec<Value> = issues
        .iter()
        .map(|(path, expected, received)| {
            json!({
                "expected": expected, "code": "invalid_type", "path": path,
                "message": format!("Invalid input: expected {expected}, received {received}")
            })
        })
        .collect();
    serde_json::to_string_pretty(&Value::Array(list)).unwrap_or_default()
}

/// `field` of an object `params`, checked against a zod `expected` kind (`record` and `object` both mean a JSON
/// object; arrays are neither). `required` fields also report `undefined`.
fn check_field(params: &Value, path: &[&'static str], expected: &'static str, required: bool, issues: &mut Vec<Issue>) {
    let value = path.iter().skip(1).fold(Some(params), |cur, key| cur.and_then(|v| v.get(*key)));
    let ok = match (expected, value) {
        (_, None) => !required,
        ("string", Some(v)) => v.is_string(),
        (_, Some(v)) => v.is_object(),
    };
    if !ok {
        issues.push((path.to_vec(), expected, received_name(value)));
    }
}

/// The SDK's request-schema checks that precede dispatch (params shape of `initialize`, `tools/list`, `tools/call`).
fn request_issues(method: &str, params: Option<&Value>) -> Vec<Issue> {
    let mut issues = Vec::new();
    match method {
        "initialize" => match params {
            Some(p) if p.is_object() => {
                check_field(p, &["params", "protocolVersion"], "string", true, &mut issues);
                check_field(p, &["params", "capabilities"], "object", true, &mut issues);
                match p.get("clientInfo") {
                    Some(info) if info.is_object() => {
                        check_field(p, &["params", "clientInfo", "name"], "string", true, &mut issues);
                        check_field(p, &["params", "clientInfo", "version"], "string", true, &mut issues);
                    }
                    other => issues.push((vec!["params", "clientInfo"], "object", received_name(other))),
                }
            }
            other => issues.push((vec!["params"], "object", received_name(other))),
        },
        "tools/list" => {
            if let Some(p) = params.filter(|p| p.is_object()) {
                check_field(p, &["params", "cursor"], "string", false, &mut issues);
            }
        }
        "tools/call" => match params {
            Some(p) if p.is_object() => {
                check_field(p, &["params", "name"], "string", true, &mut issues);
                check_field(p, &["params", "arguments"], "record", false, &mut issues);
                check_field(p, &["params", "task"], "object", false, &mut issues);
            }
            other => issues.push((vec!["params"], "object", received_name(other))),
        },
        _ => {}
    }
    issues
}

/// Result (or `Err((code, message))`) of one request. `tools` is the start-up registration.
fn handle_request(
    method: &str,
    params: Option<&Value>,
    tools: &[Value],
    server_name: &str,
    dispatch: &ToolDispatch,
) -> Result<Value, (i64, String)> {
    let issues = request_issues(method, params);
    if !issues.is_empty() {
        return Err((-32603, issues_error(&issues)));
    }
    match method {
        "initialize" => {
            let wanted = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = if SUPPORTED_PROTOCOLS.contains(&wanted) {
                wanted
            } else {
                SUPPORTED_PROTOCOLS[0]
            };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": server_name, "version": SERVER_VERSION }
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools })),
        "tools/call" => {
            let params = params.expect("validated above");
            let name = params.get("name").and_then(Value::as_str).expect("validated above");
            Ok(dispatch(name, params.get("arguments")))
        }
        _ => Err((-32601, "Method not found".to_string())),
    }
}

/// The validated arguments of a registered tool, or the ready-made error outcome (unknown tool, invalid input).
pub(crate) fn prepare_call(name: &str, arguments: Option<&Value>, tools: &[Value]) -> Result<Value, Value> {
    let Some(definition) = tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
    else {
        return Err(text_outcome(format!("MCP error -32602: Tool {name} not found"), true));
    };
    let schema = definition
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    crate::mcp_validate::validate(&schema, arguments).map_err(|issues| {
        text_outcome(
            format!("MCP error -32602: Input validation error: Invalid arguments for tool {name}: {}", issues.join("\n")),
            true,
        )
    })
}

/// `tools/call` for a registered tool: validation, session defaults, dispatch, result shape.
fn call_registered(
    name: &str,
    arguments: Option<&Value>,
    tools: &[Value],
    context: &RuntimeContext,
) -> Value {
    let mut args = match prepare_call(name, arguments, tools) {
        Ok(args) => args,
        Err(outcome) => return outcome,
    };
    if name.starts_with("qq_") {
        // Callers that cannot know the session (non-DSH clients) inject it through the environment.
        let env_key = std::env::var("QQ_SESSION_KEY")
            .unwrap_or_default()
            .trim()
            .to_string();
        let env_token = std::env::var("QQ_SESSION_TOKEN")
            .unwrap_or_default()
            .trim()
            .to_string();
        if let Some(obj) = args.as_object_mut() {
            let absent = |obj: &serde_json::Map<String, Value>, k: &str| !matches!(obj.get(k), Some(Value::String(s)) if !s.is_empty());
            if !env_token.is_empty() && absent(obj, "token") {
                obj.insert("token".to_string(), Value::String(env_token));
            }
            if !env_key.is_empty() && absent(obj, "key") {
                obj.insert("key".to_string(), Value::String(env_key));
            }
        }
    }
    call_prepared(name, &args, context)
}

/// Shared in-process dispatch after schema validation and explicit session binding.
pub(crate) fn call_prepared(name: &str, args: &Value, context: &RuntimeContext) -> Value {
    call_prepared_inner(name, args, context, false)
}

/// Canonical harness tool facts keep source data for later field-aware model copies.
pub(crate) fn call_prepared_recorded(name: &str, args: &Value, context: &RuntimeContext) -> Value {
    call_prepared_inner(name, args, context, true)
}

fn call_prepared_inner(name: &str, args: &Value, context: &RuntimeContext, recorded: bool) -> Value {
    if name.starts_with("qq_") {
        let outcome = if recorded {
            crate::qq_tools::call_tool_recorded(name, args, context)
        } else {
            crate::qq_tools::call_tool(name, args, context)
        };
        let mut result = if outcome.is_error {
            json!({ "content": outcome.content, "isError": true })
        } else {
            json!({ "content": outcome.content })
        };
        if outcome.delivery_unknown {
            result["_meta"] = json!({"herDelivery":"unknown"});
        }
        return result;
    }
    match call_tool_with_source(name, args, context) {
        Ok((text, source)) => {
            let mut outcome = text_outcome(text, false);
            if recorded && name.starts_with("memory_") {
                // Only formatter inputs: never persist the bound agent token or arbitrary arguments.
                let args: serde_json::Map<String, Value> = ["id", "action"].into_iter()
                    .filter_map(|key| args.get(key).map(|value| (key.into(), value.clone()))).collect();
                outcome["_meta"] = json!({"herMemoryResult":{"v":1,"name":name,"args":args,"data":source}});
            }
            outcome
        },
        Err(e) => {
            let prefix = match name {
                "memory_write" => "写入失败",
                "memory_search" => "检索失败",
                "memory_forget" => "删除失败",
                "memory_acl" => "权限操作失败",
                "settings_set" => "改配置失败",
                _ => "读取失败",
            };
            text_outcome(format!("{prefix}：{e}"), false)
        }
    }
}

/// stdio JSON-RPC loop bound to an explicit runtime context. Requests run concurrently (a long
/// `qq_wait_for_messages` must not hold up another session's tool calls); responses are written whole.
pub fn serve_with_context(context: &RuntimeContext) -> std::io::Result<()> {
    use std::sync::Arc;
    let cfg = match context.read_text(RuntimeArea::Config, "config.json") {
        Ok(text) => serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text))
            .unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    let tools: Arc<Vec<Value>> = Arc::new(
        tool_list(&cfg)
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    );
    let context = context.clone();
    let registered = Arc::clone(&tools);
    let dispatch: ToolDispatch = Arc::new(move |name, arguments| call_registered(name, arguments, &registered, &context));
    serve_stdio(SERVER_NAME, tools, dispatch)
}

/// How a server answers `tools/call` for a tool name (after the protocol layer has parsed the request).
pub(crate) type ToolDispatch = std::sync::Arc<dyn Fn(&str, Option<&Value>) -> Value + Send + Sync>;

/// The stdio JSON-RPC protocol shared by every MCP server of this binary (SDK-compatible envelope handling).
pub(crate) fn serve_stdio(
    server_name: &'static str,
    tools: std::sync::Arc<Vec<Value>>,
    dispatch: ToolDispatch,
) -> std::io::Result<()> {
    use std::sync::{Arc, Mutex};
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        // Unparsable lines and messages that are not JSON-RPC 2.0 requests are dropped silently (as the SDK does).
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            continue;
        }
        let Some(method) = req
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        // Notifications (no id) get no answer.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        // Frozen SDK RequestIdSchema: string | number().int(). Invalid envelopes
        // are dropped before dispatch, so an uncorrelatable request cannot send.
        if !id.is_string()
            && !id
                .as_f64()
                .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
        {
            continue;
        }
        // The SDK's request envelope: `params` (and its `_meta`) must be objects, otherwise the message is dropped.
        // `params: null` is the one exception kept from ticket 14: it is answered (the JS server stays silent).
        let params = req.get("params").cloned();
        if params.as_ref().is_some_and(|p| !p.is_null() && (!p.is_object() || p.get("_meta").is_some_and(|m| !m.is_object()))) {
            continue;
        }
        workers.retain(|w| !w.is_finished());
        const MAX_IN_FLIGHT_REQUESTS: usize = 64;
        if workers.len() >= MAX_IN_FLIGHT_REQUESTS {
            if let Ok(mut stdout) = out.lock() {
                let resp = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "Too many in-flight MCP requests"}});
                let _ = writeln!(stdout, "{resp}");
                let _ = stdout.flush();
            }
            continue;
        }
        let (tools, worker_out, dispatch) = (Arc::clone(&tools), Arc::clone(&out), Arc::clone(&dispatch));
        let worker_id = id.clone();
        let worker = std::thread::Builder::new().name("mcp-request".into()).spawn(move || {
            let resp = match handle_request(&method, params.as_ref(), &tools, server_name, &dispatch) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": worker_id, "result": result }),
                Err((code, message)) => json!({ "jsonrpc": "2.0", "id": worker_id, "error": { "code": code, "message": message } }),
            };
            if let Ok(mut stdout) = worker_out.lock() {
                let _ = writeln!(stdout, "{resp}");
                let _ = stdout.flush();
            }
        });
        match worker {
            Ok(worker) => workers.push(worker),
            Err(_) => {
                if let Ok(mut stdout) = out.lock() {
                    let resp = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "MCP request worker unavailable"}});
                    let _ = writeln!(stdout, "{resp}");
                    let _ = stdout.flush();
                }
            }
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_merges_memory_and_qq() {
        let v = tool_list(&json!({}));
        let tools = v.get("tools").and_then(|t| t.as_array()).unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name")?.as_str())
            .collect();
        let mem = names
            .iter()
            .filter(|n| n.starts_with("memory_") || n.starts_with("settings_"))
            .count();
        let qq = names.iter().filter(|n| n.starts_with("qq_")).count();
        assert_eq!(mem, 7, "memory 系列应有 7 个工具");
        assert_eq!(qq, 38, "qq_* 应有 38 个工具（JS 全部注册）");
        assert_eq!(tools.len(), 45, "合计 45 个");
    }

    #[test]
    fn settings_path_helpers() {
        let mut cfg = json!({"socialV2": {"send": {"maxSendPerMinute": 8}}});
        assert_eq!(
            get_path(&cfg, "socialV2.send.maxSendPerMinute"),
            Some(&json!(8))
        );
        assert!(set_path(
            &mut cfg,
            "socialV2.send.maxSendPerMinute",
            json!(5)
        ));
        assert_eq!(
            get_path(&cfg, "socialV2.send.maxSendPerMinute"),
            Some(&json!(5))
        );
        assert!(!set_path(&mut cfg, "不存在的.路径", json!(1)));
    }

    #[test]
    fn invalid_token_rejected() {
        assert!(!token_ok(&RuntimeContext::legacy(), "group:1", ""));
    }
}
