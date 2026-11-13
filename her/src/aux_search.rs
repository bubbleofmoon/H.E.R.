//! `web_search`: DeepSeek's native web search (the same endpoint, model and server-side tool DSH uses), with Bing as
//! fallback. Port of the frozen `web-search.js`. The API key is only sent to a validated https endpoint, never
//! follows a redirect, and never appears in a log, an error or a result.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Map, Value};
use url::Url;

use crate::aux_fetch::{self, TransferFn};
use crate::aux_text::{clamped_int, collapse_js_space, js_string, js_trim, js_truthy, slice_utf16};
use crate::runtime::{RuntimeArea, RuntimeContext};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/anthropic/v1";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const CACHE_MAX_ENTRIES: usize = 64;
const SEARCH_TOOL_TYPE: &str = "web_search_20250305";

#[derive(Clone, Debug, PartialEq)]
pub struct SearchConfig {
    pub provider: String,
    pub api_key: String,
    pub api_key_env: String,
    pub base_url: String,
    pub model: String,
    pub max_uses: i64,
    pub max_tokens: i64,
    pub max_results: i64,
    pub timeout_ms: i64,
}

fn text_field(raw: &Value, name: &str) -> Option<String> {
    raw.get(name).and_then(Value::as_str).map(str::to_string)
}

/// The selected `config.json` text as the JS reads it: BOM dropped; unreadable or invalid JSON is `{}`.
pub fn parse_config_text(text: &str) -> Value {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)).unwrap_or_else(|_| json!({}))
}

/// JS `loadSearchConfig`: the `webSearch` section of the selected config with the JS defaults and clamps.
pub fn load_search_config(config: &Value) -> SearchConfig {
    let empty = json!({});
    let raw = config.get("webSearch").filter(|v| v.is_object()).unwrap_or(&empty);
    let provider = match raw.get("provider").and_then(Value::as_str) {
        Some(p @ ("deepseek" | "bing")) => p.to_string(),
        _ => "auto".to_string(),
    };
    let trimmed = |name: &str| text_field(raw, name).map(|s| js_trim(&s).to_string());
    SearchConfig {
        provider,
        api_key: trimmed("apiKey").unwrap_or_default(),
        api_key_env: trimmed("apiKeyEnv").filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string()),
        base_url: trimmed("baseURL").map(|s| s.trim_end_matches('/').to_string()).unwrap_or_default(),
        model: trimmed("model").filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        max_uses: clamped_int(raw.get("maxUses"), 5, 1, 5),
        max_tokens: clamped_int(raw.get("maxTokens"), 4096, 256, 8192),
        max_results: clamped_int(raw.get("maxResults"), 8, 1, 20),
        timeout_ms: clamped_int(raw.get("timeoutMs"), 35000, 5000, 120000),
    }
}

// ----- credentials ----------------------------------------------------------------------------------------------------

/// The scalar a YAML block value spells, when js-yaml would load it as a string (`None`: number, bool, null, empty).
fn yaml_string_scalar(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(inner) = t.strip_prefix('\'') {
        let mut out = String::new();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                    continue;
                }
                return Some(out);
            }
            out.push(c);
        }
        return None;
    }
    if t.starts_with('"') {
        let mut end = None;
        let mut escaped = false;
        for (i, c) in t.char_indices().skip(1) {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(i);
                break;
            }
        }
        return serde_json::from_str::<String>(&t[..=end?]).ok();
    }
    // plain scalar: a comment starts at " #"; "a: b" on one line is a YAML error (the whole file fails to load)
    let plain = t.split(" #").next().unwrap_or(t).trim_end();
    if plain.contains(": ") || plain.ends_with(':') {
        return None;
    }
    let lower = plain.to_ascii_lowercase();
    // js-yaml core schema: these plain scalars load as null / bool / number, not as a string.
    let numeric = plain.replace('_', "").parse::<f64>().is_ok_and(|n| n.is_finite())
        && plain.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E' | b'_'))
        || lower.strip_prefix("0x").is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_hexdigit()))
        || lower.strip_prefix("0o").is_some_and(|d| !d.is_empty() && d.chars().all(|c| ('0'..='7').contains(&c)))
        || matches!(lower.as_str(), ".inf" | "+.inf" | "-.inf" | ".nan");
    if numeric || matches!(lower.as_str(), "true" | "false" | "null" | "~") || plain.is_empty() {
        return None;
    }
    Some(plain.to_string())
}

/// `refs.<name>` of a DSH `.credentials.yaml` (block mapping; only the named ref is ever read).
pub fn credential_from_yaml(text: &str, name: &str) -> String {
    let name = js_trim(name);
    if name.is_empty() {
        return String::new();
    }
    let mut in_refs = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            let Some(rest) = line.trim_end().strip_prefix("refs:") else {
                in_refs = false;
                continue;
            };
            let rest = rest.trim();
            if let Some(flow) = rest.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
                in_refs = false;
                for pair in flow.split(',') {
                    if let Some((key, value)) = pair.split_once(':') {
                        if key.trim().trim_matches(|c| c == '"' || c == '\'') == name {
                            return yaml_string_scalar(value).map(|s| js_trim(&s).to_string()).unwrap_or_default();
                        }
                    }
                }
                continue;
            }
            in_refs = rest.is_empty() || rest.starts_with('#');
            continue;
        }
        if !in_refs {
            continue;
        }
        let body = line.trim_start();
        let Some((key, value)) = body.split_once(':') else { continue };
        let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
        if key == name {
            return yaml_string_scalar(value).map(|s| js_trim(&s).to_string()).unwrap_or_default();
        }
    }
    String::new()
}

fn dsh_credential(context: &RuntimeContext, name: &str) -> String {
    let text = if context.is_isolated() {
        context.read_text(RuntimeArea::DshUser, ".credentials.yaml").ok()
    } else {
        let home = std::env::var_os("DSH_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| std::path::PathBuf::from(h).join(".dsh"))
            });
        home.and_then(|h| std::fs::read_to_string(h.join(".credentials.yaml")).ok())
    };
    text.map(|t| credential_from_yaml(&t, name)).unwrap_or_default()
}

/// Config literal, then the named environment variable, then DSH's credential store.
fn resolve_api_key(context: &RuntimeContext, config: &SearchConfig) -> Option<String> {
    let literal = js_trim(&config.api_key);
    if !literal.is_empty() {
        return Some(literal.to_string());
    }
    let from_env = std::env::var(&config.api_key_env).map(|v| js_trim(&v).to_string()).unwrap_or_default();
    if !from_env.is_empty() {
        return Some(from_env);
    }
    let from_dsh = dsh_credential(context, &config.api_key_env);
    (!from_dsh.is_empty()).then_some(from_dsh)
}

// ----- result mapping -------------------------------------------------------------------------------------------------

/// A failure with the stable code the JS attaches (`NO_KEY`, `HTTP_ERROR`, ...).
#[derive(Debug)]
pub struct SearchError {
    pub code: &'static str,
    pub message: String,
}

impl SearchError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

fn normalize_search_url(raw: &str) -> String {
    let s = js_trim(raw);
    let lower = s.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return String::new();
    }
    // DeepSeek appends `#1`-style rank anchors; real fragments are kept.
    let cleaned = match s.rfind('#') {
        Some(i) if i + 1 < s.len() && s[i + 1..].bytes().all(|b| b.is_ascii_digit()) => &s[..i],
        _ => s,
    };
    match Url::parse(cleaned) {
        Ok(url) if (url.scheme() == "http" || url.scheme() == "https") && url.username().is_empty() && url.password().is_none() => url.to_string(),
        _ => String::new(),
    }
}

/// JS `mapDeepSeekResponse`.
pub fn map_deepseek_response(response: &Value, max_results: usize) -> Result<Vec<Value>, SearchError> {
    let blocks: &[Value] = response.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let result_blocks: Vec<&Value> = blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("web_search_tool_result")).collect();
    if result_blocks.is_empty() {
        return Err(SearchError::new("NO_RESULT_BLOCK", "DeepSeek 没有返回 web_search_tool_result（可能未触发联网搜索）"));
    }
    let mut snippets: HashMap<String, String> = HashMap::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        for cite in block.get("citations").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            let url = normalize_search_url(&cite.get("url").map(js_string_or_empty).unwrap_or_default());
            if url.is_empty() || snippets.contains_key(&url) {
                continue;
            }
            let text = js_trim(&cite.get("cited_text").map(js_string_or_empty).unwrap_or_default()).to_string();
            if !text.is_empty() {
                snippets.insert(url, text);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for block in result_blocks {
        for item in block.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            if item.get("type").and_then(Value::as_str) != Some("web_search_result") {
                continue;
            }
            let url = normalize_search_url(&item.get("url").map(js_string_or_empty).unwrap_or_default());
            if url.is_empty() || !seen.insert(url.clone()) {
                continue;
            }
            let title = slice_utf16(js_trim(&item.get("title").map(js_string_or_empty).unwrap_or_default()), 200);
            let snippet = slice_utf16(js_trim(snippets.get(&url).map(String::as_str).unwrap_or("")), 500);
            let mut entry = Map::new();
            entry.insert("title".into(), json!(title));
            entry.insert("url".into(), json!(url));
            if !snippet.is_empty() {
                entry.insert("snippet".into(), json!(snippet));
            }
            if let Some(age) = item.get("page_age").filter(|v| js_truthy(v)) {
                entry.insert("publishedAt".into(), json!(slice_utf16(&js_string(age), 40)));
            }
            results.push(Value::Object(entry));
            if results.len() >= max_results {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

/// `String(value ?? '')`.
fn js_string_or_empty(value: &Value) -> String {
    if value.is_null() {
        String::new()
    } else {
        js_string(value)
    }
}

fn decode_entities(text: &str) -> String {
    static PATTERNS: OnceLock<(Regex, Regex, Regex, Regex)> = OnceLock::new();
    let (cdata, hex, dec, tags) = PATTERNS.get_or_init(|| {
        (
            Regex::new(r"(?s)<!\[CDATA\[(.*?)\]\]>").expect("cdata"),
            Regex::new(r"(?i)&#x([0-9a-f]+);").expect("hex"),
            Regex::new(r"&#(\d+);").expect("dec"),
            Regex::new(r"<[^>]+>").expect("tags"),
        )
    });
    let code_point = |digits: &str, radix: u32| -> String {
        u32::from_str_radix(digits, radix).ok().filter(|c| *c > 0 && *c <= 0x10ffff).and_then(char::from_u32).map(String::from).unwrap_or_default()
    };
    let s = cdata.replace_all(text, "$1");
    let s = hex.replace_all(&s, |c: &regex::Captures| code_point(&c[1], 16));
    let s = dec.replace_all(&s, |c: &regex::Captures| code_point(&c[1], 10));
    let s = s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ").replace("&amp;", "&");
    let s = tags.replace_all(&s, " ");
    js_trim(&collapse_js_space(&s)).to_string()
}

fn pick_tag(block: &str, tag: &str) -> String {
    let pattern = format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>");
    Regex::new(&pattern).ok().and_then(|re| re.captures(block).map(|c| decode_entities(&c[1]))).unwrap_or_default()
}

/// JS `parseBingRss`.
pub fn parse_bing_rss(xml: &str, max_results: usize) -> Vec<Value> {
    static ITEM: OnceLock<Regex> = OnceLock::new();
    let item = ITEM.get_or_init(|| Regex::new(r"(?is)<item>(.*?)</item>").expect("item"));
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in item.captures_iter(xml) {
        let block = &m[1];
        let url = normalize_search_url(&pick_tag(block, "link"));
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let title = slice_utf16(&pick_tag(block, "title"), 200);
        let snippet = slice_utf16(&pick_tag(block, "description"), 500);
        let mut entry = Map::new();
        entry.insert("title".into(), json!(if title.is_empty() { url.clone() } else { title }));
        entry.insert("url".into(), json!(url));
        if !snippet.is_empty() {
            entry.insert("snippet".into(), json!(snippet));
        }
        results.push(Value::Object(entry));
        if results.len() >= max_results {
            break;
        }
    }
    results
}

/// JS `parseBingHtml`.
pub fn parse_bing_html(html: &str, max_results: usize) -> Vec<Value> {
    static PATTERNS: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    let (href, h2, p) = PATTERNS.get_or_init(|| {
        (
            Regex::new(r#"(?i)<a[^>]+href="(https?://[^"]+)""#).expect("href"),
            Regex::new(r"(?is)<h2[^>]*>(.*?)</h2>").expect("h2"),
            Regex::new(r"(?is)<p[^>]*>(.*?)</p>").expect("p"),
        )
    });
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in html.split("<li class=\"b_algo\"").skip(1) {
        let Some(m) = href.captures(block) else { continue };
        let url = normalize_search_url(&decode_entities(&m[1]));
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let title = h2.captures(block).map(|c| slice_utf16(&decode_entities(&c[1]), 200)).unwrap_or_default();
        let snippet = p.captures(block).map(|c| slice_utf16(&decode_entities(&c[1]), 500)).unwrap_or_default();
        let mut entry = Map::new();
        entry.insert("title".into(), json!(if title.is_empty() { url.clone() } else { title }));
        entry.insert("url".into(), json!(url));
        if !snippet.is_empty() {
            entry.insert("snippet".into(), json!(snippet));
        }
        results.push(Value::Object(entry));
        if results.len() >= max_results {
            break;
        }
    }
    results
}

// ----- providers ------------------------------------------------------------------------------------------------------

/// Everything the providers need from the outside world; production values come from `Network::system()`.
pub struct Network<'a> {
    pub resolver: &'a aux_fetch::Resolver<'a>,
    pub transfer: &'a TransferFn<'a>,
}

fn validate_search_endpoint(base_url: &str, net: &Network) -> Result<(Url, Vec<IpAddr>), String> {
    let url = Url::parse(js_trim(base_url)).map_err(|_| "webSearch.baseURL 无效".to_string())?;
    if url.scheme() != "https" {
        return Err("webSearch.baseURL 必须是 https（API key 只能发给加密端点）".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("webSearch.baseURL 不能包含凭据".to_string());
    }
    let host = url.host_str().unwrap_or("").to_string();
    let addresses = aux_fetch::resolve_safe_addresses_with(&host, net.resolver)?;
    Ok((url, addresses))
}

fn deepseek_search(query: &str, config: &SearchConfig, context: &RuntimeContext, net: &Network) -> Result<Vec<Value>, SearchError> {
    let key = resolve_api_key(context, config).ok_or_else(|| SearchError::new("NO_KEY", "没有可用的 DeepSeek API key"))?;
    let base_url = if !config.base_url.is_empty() {
        config.base_url.clone()
    } else {
        std::env::var("DEEPSEEK_SEARCH_BASE_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
    };
    let plain = |message: String| SearchError::new("", message);
    let (url, addresses) = validate_search_endpoint(&base_url, net).map_err(plain)?;
    let endpoint = Url::parse(&format!("{}/messages", url.as_str().trim_end_matches('/'))).map_err(|_| plain("webSearch.baseURL 无效".to_string()))?;
    let body = json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "messages": [{"role": "user", "content": [{"type": "text", "text": format!("Perform a web search for the query: {query}")}]}],
        "tools": [{"type": SEARCH_TOOL_TYPE, "name": "web_search", "max_uses": config.max_uses}],
    })
    .to_string();
    let headers = vec![
        ("x-api-key".to_string(), key.clone()),
        ("authorization".to_string(), format!("Bearer {key}")),
        ("anthropic-version".to_string(), DEFAULT_API_VERSION.to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("user-agent".to_string(), "qq-bridge-web-search/0.2.0".to_string()),
    ];
    let (status, text) = aux_fetch::post_json_with(&endpoint, &addresses, headers, &body, Duration::from_millis(config.timeout_ms as u64), MAX_RESPONSE_BYTES, net.transfer).map_err(plain)?;
    if !(200..300).contains(&status) {
        // only the HTTP status and a short error text from the body; never the request headers (they hold the key)
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|parsed| {
                let message = match parsed.get("error") {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(e) => e.get("message").and_then(Value::as_str).map(str::to_string),
                    None => None,
                }
                .or_else(|| parsed.get("message").and_then(Value::as_str).map(str::to_string));
                message.map(|m| slice_utf16(&m, 200))
            })
            .unwrap_or_default();
        let message = if detail.is_empty() { format!("DeepSeek 搜索接口 HTTP {status}") } else { format!("DeepSeek 搜索接口 HTTP {status}：{detail}") };
        return Err(SearchError::new("HTTP_ERROR", message));
    }
    let parsed: Value = serde_json::from_str(&text).map_err(|_| SearchError::new("BAD_BODY", "DeepSeek 搜索接口返回了无法解析的响应"))?;
    map_deepseek_response(&parsed, config.max_results as usize)
}

fn bing_url(extra: &[(&str, &str)], query: &str, count: i64) -> String {
    let mut url = Url::parse("https://cn.bing.com/search").expect("static url");
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query);
        for (k, v) in extra {
            pairs.append_pair(k, v);
        }
        pairs.append_pair("count", &count.min(20).to_string());
        pairs.append_pair("setlang", "zh-hans");
        pairs.append_pair("cc", "CN");
    }
    url.to_string()
}

fn bing_search(query: &str, config: &SearchConfig, net: &Network) -> Result<Vec<Value>, String> {
    let max = config.max_results as usize;
    if let Ok(res) = aux_fetch::fetch_text_with(&bing_url(&[("format", "rss")], query, config.max_results), 512_000, net.resolver, net.transfer) {
        let status = res["statusCode"].as_u64().unwrap_or(0);
        if (200..300).contains(&status) {
            let results = parse_bing_rss(res["body"].as_str().unwrap_or(""), max);
            if !results.is_empty() {
                return Ok(results);
            }
        }
    }
    let res = aux_fetch::fetch_text_with(&bing_url(&[], query, config.max_results), 512_000, net.resolver, net.transfer)?;
    let status = res["statusCode"].as_u64().unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(format!("Bing 搜索 HTTP {status}"));
    }
    Ok(parse_bing_html(res["body"].as_str().unwrap_or(""), max))
}

type CacheEntry = (Instant, Value);
static CACHE: OnceLock<Mutex<(Vec<String>, HashMap<String, CacheEntry>)>> = OnceLock::new();

/// JS `webSearch(query)`: provider choice, DeepSeek -> Bing fallback, empty-result note, five-minute cache.
pub fn web_search(query: &str, config_value: &Value, context: &RuntimeContext, net: &Network) -> Result<Value, String> {
    let q = js_trim(query);
    if q.is_empty() {
        return Err("查询词为空".to_string());
    }
    let config = load_search_config(config_value);
    let wanted = config.provider.clone();
    let cache_key = format!("{wanted}|{q}|{}", config.max_results);
    let cache = CACHE.get_or_init(|| Mutex::new((Vec::new(), HashMap::new())));
    if let Some((at, value)) = cache.lock().ok().and_then(|c| c.1.get(&cache_key).cloned()) {
        if at.elapsed() < CACHE_TTL {
            return Ok(value);
        }
    }
    let attempt_deepseek = || -> Result<Value, SearchError> {
        deepseek_search(q, &config, context, net).map(|results| json!({"query": q, "provider": "deepseek", "results": results}))
    };
    let attempt_bing = || -> Result<Value, String> { bing_search(q, &config, net).map(|results| json!({"query": q, "provider": "bing", "results": results})) };
    let mut value = match wanted.as_str() {
        "deepseek" => attempt_deepseek().map_err(|e| e.message)?,
        "bing" => attempt_bing()?,
        _ => match attempt_deepseek() {
            Ok(v) => v,
            Err(error) => {
                let reason = if error.code == "NO_KEY" { "未配置 DeepSeek 搜索凭据".to_string() } else { format!("DeepSeek 搜索不可用：{}", error.message) };
                let mut bing = attempt_bing().map_err(|bing_error| format!("{reason}；Bing 兜底也失败：{bing_error}"))?;
                bing["fallbackFrom"] = json!("deepseek");
                bing["note"] = json!(format!("{reason}，已回退 Bing"));
                bing
            }
        },
    };
    if value["results"].as_array().is_some_and(Vec::is_empty) {
        let note = [value.get("note").and_then(Value::as_str).unwrap_or(""), "没有搜到结果（可换个说法/加限定词；仍为空就不要再重复搜索）"]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("；");
        value["note"] = json!(note);
    }
    if let Ok(mut guard) = cache.lock() {
        let (order, map) = &mut *guard;
        if !map.contains_key(&cache_key) {
            order.push(cache_key.clone());
        }
        map.insert(cache_key, (Instant::now(), value.clone()));
        while order.len() > CACHE_MAX_ENTRIES {
            let oldest = order.remove(0);
            map.remove(&oldest);
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/aux-oracle.json")).expect("aux oracle")
    }

    #[test]
    fn bing_rss_matches_the_frozen_js() {
        for case in oracle()["bingRss"].as_array().unwrap() {
            let got = parse_bing_rss(case["xml"].as_str().unwrap(), case["max"].as_u64().unwrap() as usize);
            assert_eq!(Value::Array(got), case["out"], "xml: {}", case["xml"]);
        }
    }

    #[test]
    fn bing_html_matches_the_frozen_js() {
        for case in oracle()["bingHtml"].as_array().unwrap() {
            let got = parse_bing_html(case["html"].as_str().unwrap(), case["max"].as_u64().unwrap() as usize);
            assert_eq!(Value::Array(got), case["out"], "html: {}", case["html"]);
        }
    }

    #[test]
    fn deepseek_responses_map_like_the_frozen_js() {
        for case in oracle()["deepseek"].as_array().unwrap() {
            let got = map_deepseek_response(&case["response"], case["max"].as_u64().unwrap() as usize);
            match (got, case.get("error")) {
                (Ok(results), None) => assert_eq!(Value::Array(results), case["out"], "response: {}", case["response"]),
                (Err(e), Some(expected)) => {
                    assert_eq!(json!(e.message), *expected);
                    assert_eq!(json!(e.code), case["code"]);
                }
                (got, _) => panic!("outcome differs for {}: {got:?}", case["response"]),
            }
        }
    }

    #[test]
    fn search_config_matches_the_frozen_js() {
        for case in oracle()["config"].as_array().unwrap() {
            let cfg = load_search_config(&parse_config_text(case["text"].as_str().unwrap()));
            let want = &case["out"];
            let got = json!({
                "provider": cfg.provider, "apiKey": cfg.api_key, "apiKeyEnv": cfg.api_key_env, "baseURL": cfg.base_url,
                "model": cfg.model, "maxUses": cfg.max_uses, "maxTokens": cfg.max_tokens, "maxResults": cfg.max_results,
                "timeoutMs": cfg.timeout_ms,
            });
            assert_eq!(&got, want, "config text: {:?}", case["text"]);
        }
    }

    #[test]
    fn dsh_credentials_read_like_the_frozen_js() {
        for case in oracle()["credentials"].as_array().unwrap() {
            let got = credential_from_yaml(case["yaml"].as_str().unwrap(), case["name"].as_str().unwrap());
            assert_eq!(json!(got), case["out"], "yaml: {:?} name: {:?}", case["yaml"], case["name"]);
        }
    }
}
