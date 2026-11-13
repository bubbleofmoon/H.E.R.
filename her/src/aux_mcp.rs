//! The two auxiliary MCP servers the DSH profile launches next to the QQ tool server (`--mcp-serve`):
//! `web-search-safe` (`web_search`, `web_fetch`) and `snowluma-host` (`snowluma_status`). Tool definitions are the
//! real `tools/list` of the frozen JS servers (`spec/aux-baseline.json`); behaviour ports `mcp-web-search-safe.js`
//! and `mcp-host-server.js`.
//!
//! External boundaries (not rewritten here): DeepSeek's Anthropic-compatible search endpoint, Bing, any public
//! web page, DSH's credential file and the SnowLuma OneBot HTTP gateway. Their failures become the documented
//! error texts below; the servers never start, stop or install SnowLuma.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::aux_fetch;
use crate::aux_search::{self, Network};
use crate::aux_text::{js_truthy, sanitize_query};
use crate::mcp::{prepare_call, serve_stdio, ToolDispatch};
use crate::runtime::{RuntimeArea, RuntimeContext};

const BASELINE: &str = include_str!("../spec/aux-baseline.json");
const FETCH_CHARS: usize = 50_000;
const GATEWAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn frozen_tools(server: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(BASELINE)
        .ok()
        .and_then(|v| v[server][1]["result"]["tools"].as_array().cloned())
        .unwrap_or_default()
}

/// The same frozen definitions used by the auxiliary stdio servers.
pub(crate) fn tool_list() -> Vec<Value> {
    let mut tools = frozen_tools("search");
    tools.extend(frozen_tools("host"));
    tools
}

/// In-process entry after the caller validates the captured schema and session authority.
pub(crate) fn call_tool(name: &str, args: &Value, context: &RuntimeContext) -> Value {
    match name {
        "web_search" => web_search_tool(args["query"].as_str().unwrap_or(""), context),
        "web_fetch" => web_fetch_tool(args["url"].as_str().unwrap_or("")),
        "snowluma_status" => outcome(pretty(&snowluma_status(context)), false),
        _ => outcome(format!("Tool {name} not found"), true),
    }
}

/// The selected config (`{}` when a legacy context has none, as JS `loadConfig`); an invalid file is `{}` too.
fn read_config(context: &RuntimeContext) -> Value {
    context
        .read_text(RuntimeArea::Config, "config.json")
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok())
        .unwrap_or_else(|| json!({}))
}

fn outcome(text: String, is_error: bool) -> Value {
    if is_error {
        json!({"content": [{"type": "text", "text": text}], "isError": true})
    } else {
        json!({"content": [{"type": "text", "text": text}]})
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

pub fn serve_web_search(context: &RuntimeContext) -> std::io::Result<()> {
    let tools = Arc::new(frozen_tools("search"));
    let registered = Arc::clone(&tools);
    let context = context.clone();
    let dispatch: ToolDispatch = Arc::new(move |name, arguments| {
        let args = match prepare_call(name, arguments, &registered) {
            Ok(args) => args,
            Err(outcome) => return outcome,
        };
        match name {
            "web_search" => web_search_tool(args["query"].as_str().unwrap_or(""), &context),
            _ => web_fetch_tool(args["url"].as_str().unwrap_or("")),
        }
    });
    serve_stdio("web-search-safe", tools, dispatch)
}

fn web_search_tool(query: &str, context: &RuntimeContext) -> Value {
    let clean = sanitize_query(query);
    if clean.is_empty() {
        return outcome("查询词为空，已拒绝。".to_string(), true);
    }
    let net = Network { resolver: &aux_fetch::system_resolver, transfer: &aux_fetch::curl_transfer };
    match aux_search::web_search(&clean, &read_config(context), context, &net) {
        Ok(result) => outcome(pretty(&result), false),
        Err(message) => outcome(format!("搜索失败：{message}"), true),
    }
}

fn web_fetch_tool(url: &str) -> Value {
    match aux_fetch::fetch_text(url, FETCH_CHARS) {
        Ok(result) => outcome(pretty(&result), false),
        Err(message) => outcome(format!("抓取失败：{message}"), true),
    }
}

// ----- snowluma-host ---------------------------------------------------------------------------------------------------

pub fn serve_host(context: &RuntimeContext) -> std::io::Result<()> {
    let tools = Arc::new(frozen_tools("host"));
    let registered = Arc::clone(&tools);
    let context = context.clone();
    let dispatch: ToolDispatch = Arc::new(move |name, arguments| {
        if let Err(outcome) = prepare_call(name, arguments, &registered) {
            return outcome;
        }
        outcome(pretty(&snowluma_status(&context)), false)
    });
    serve_stdio("snowluma-host", tools, dispatch)
}

/// Where the gateway answers: an isolated runtime uses only its selected OneBot dependency, a legacy one
/// `snowluma.httpUrl` (default `http://127.0.0.1:3000`). `Err`: not a plain-http target (reported as unreachable).
fn gateway_target(context: &RuntimeContext, cfg: &Value) -> Result<(String, String), String> {
    let url = if context.is_isolated() {
        context.dependency_url("onebot").unwrap_or("").to_string()
    } else {
        match cfg.pointer("/snowluma/httpUrl") {
            Some(Value::Null) | None => "http://127.0.0.1:3000".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        }
    };
    let url = url.trim_end_matches('/');
    let rest = url.strip_prefix("http://").ok_or_else(|| "fetch failed".to_string())?;
    let (authority, prefix) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    if authority.is_empty() {
        return Err("fetch failed".to_string());
    }
    let host_port = if authority.rsplit_once(':').is_some_and(|(_, p)| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty()) {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Ok((host_port, prefix))
}

fn v8_json_error(body: &[u8], error: &serde_json::Error) -> String {
    let text = crate::activity::redact_log_line(&String::from_utf8_lossy(body));
    if text.trim().is_empty() {
        return "Unexpected end of JSON input".to_string();
    }
    let ch = text.chars().nth(error.column().saturating_sub(1)).map(|c| c.to_string());
    match ch {
        Some(c) => format!("Unexpected token '{c}', \"{}\" is not valid JSON", crate::activity::preview_chars(&text, 40)),
        None => "Unexpected end of JSON input".to_string(),
    }
}

/// JS `gatewayInfo()`.
fn gateway_info(context: &RuntimeContext, cfg: &Value) -> Map<String, Value> {
    let mut info = Map::new();
    let token = cfg.pointer("/snowluma/accessToken").filter(|v| js_truthy(v)).map(crate::aux_text::js_string).unwrap_or_default();
    let reply = gateway_target(context, cfg).and_then(|(host_port, prefix)| {
        let headers = if token.is_empty() { Vec::new() } else { vec![("Authorization".to_string(), format!("Bearer {token}"))] };
        crate::qq_tools::http_request(&host_port, "GET", &format!("{prefix}/get_login_info"), &headers, None, GATEWAY_TIMEOUT)
    });
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            info.insert("reachable".into(), json!(false));
            info.insert("error".into(), json!(error));
            return info;
        }
    };
    if !(200..300).contains(&reply.status) {
        info.insert("reachable".into(), json!(false));
        info.insert("httpStatus".into(), json!(reply.status));
        if reply.status == 426 {
            info.insert("hint".into(), json!("（HTTP 426：snowluma.httpUrl 可能指向了 WebSocket 端口，请改为 OneBot HTTP API 地址）"));
        }
        return info;
    }
    // `res.json()` decodes UTF-8 (lossily) first; a lone surrogate escape becomes U+FFFD (accepted deviation 18).
    let body: Value = match crate::aux_text::parse_json_replacing_lone_surrogates::<Value>(&String::from_utf8_lossy(&reply.body)) {
        Ok((body, _)) => body,
        Err(error) => {
            info.insert("reachable".into(), json!(false));
            info.insert("error".into(), json!(v8_json_error(&reply.body, &error)));
            return info;
        }
    };
    let online = body.get("status").and_then(Value::as_str) == Some("ok") && body.get("retcode").and_then(Value::as_f64) == Some(0.0);
    info.insert("reachable".into(), json!(true));
    if online {
        info.insert("online".into(), json!(true));
        if let Some(v) = body.pointer("/data/user_id") {
            info.insert("user_id".into(), v.clone());
        }
        if let Some(v) = body.pointer("/data/nickname") {
            info.insert("nickname".into(), v.clone());
        }
    } else {
        info.insert("online".into(), json!(false));
        if let Some(v) = body.get("retcode") {
            info.insert("retcode".into(), v.clone());
        }
        if let Some(v) = body.get("wording") {
            info.insert("wording".into(), v.clone());
        }
    }
    info
}

fn snowluma_status(context: &RuntimeContext) -> Value {
    let cfg = read_config(context);
    let mut payload = gateway_info(context, &cfg);
    if payload.get("reachable") == Some(&json!(false)) {
        let home_configured = match cfg.pointer("/snowluma/homeDir") {
            Some(Value::Null) | None => cfg.pointer("/snowluma/launcherPath").is_some_and(js_truthy),
            Some(v) => js_truthy(v),
        };
        let mut hint = "SnowLuma 网关不可达。请自行启动 SnowLuma（本程序不代装、不代启，原因见项目文档的合规说明 L6）。".to_string();
        if !home_configured {
            hint.push_str(" 若 config.json 的 snowluma.httpUrl 与实际端口不一致，也请一并检查。");
        }
        payload.insert("hint".into(), json!(hint));
    }
    Value::Object(payload)
}
