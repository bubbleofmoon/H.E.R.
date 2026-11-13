//! SnowLuma token self-heal (ticket 25, AC3; frozen JS `bridge.js` `healTokenIfStale`/`applyHealedTokens` and
//! `snowluma-conn.js` `discoverSnowLumaConnection`/`persistSnowLumaEndpoint`).
//!
//! SnowLuma keeps one OneBot config per account (`<home>/config/onebot_<uin>.json`, a different HTTP and WebSocket
//! token each). After the account changes, the token in `config.json` is refused and the gateway link reconnects
//! forever with the stale one. On a failed connect or a close the bridge asks each candidate token for
//! `get_login_info`, takes the first that works together with the same account's WebSocket token, and writes
//! the tokens (and, unless `snowluma.followDiscoveredEndpoint` is `false`, the endpoint) back into the **selected**
//! `config.json` under the social-state gate. Attempts are throttled to one per 10 s because a close storm fires
//! many events. A discovered endpoint is followed wherever it points, as in JS.

use crate::runtime::RuntimeContext;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// JS `now - tokenHealAt < 10000`.
pub const THROTTLE_MS: i64 = 10_000;

#[derive(Debug, Default)]
pub struct Throttle {
    last_ms: Option<i64>,
}

impl Throttle {
    /// May a heal attempt start now? (One per [`THROTTLE_MS`].)
    pub fn allow(&mut self, now_ms: i64) -> bool {
        if self.last_ms.is_some_and(|last| now_ms - last < THROTTLE_MS) {
            return false;
        }
        self.last_ms = Some(now_ms);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub token: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub uin: String,
    pub http: Option<Endpoint>,
    pub ws: Option<Endpoint>,
}

/// JS `snowLumaHomeCandidates`: `snowluma.homeDir`, the directory of `snowluma.launcherPath`, `SNOWLUMA_HOME`.
pub fn home_candidates(cfg: &Value) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !out.contains(&p) {
            out.push(p);
        }
    };
    let sl = cfg.get("snowluma").unwrap_or(cfg);
    if let Some(home) = sl.get("homeDir").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        push(PathBuf::from(home));
    }
    if let Some(launcher) = sl.get("launcherPath").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        if let Some(dir) = Path::new(launcher).parent() {
            push(dir.to_path_buf());
        }
    }
    if let Ok(home) = std::env::var("SNOWLUMA_HOME") {
        if !home.is_empty() {
            push(PathBuf::from(home));
        }
    }
    out
}

/// JS `readOneBotTokens`: the per-account configs under `<home>/config`, sorted by uin. A broken file is skipped.
pub fn read_onebot_tokens(home: &Path) -> Vec<Account> {
    let Ok(entries) = std::fs::read_dir(home.join("config")) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let lower = name.to_ascii_lowercase();
        let Some(uin) = lower.strip_prefix("onebot_").and_then(|r| r.strip_suffix(".json")) else { continue };
        if uin.is_empty() || !uin.bytes().all(|b| b.is_ascii_digit()) || !entry.path().is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let Ok(doc) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else { continue };
        let endpoint = |list: &str, default_port: u16| -> Option<Endpoint> {
            let first = doc.pointer(&format!("/networks/{list}/0"))?;
            if !first.is_object() {
                return None;
            }
            Some(Endpoint {
                token: first.get("accessToken").and_then(Value::as_str).unwrap_or("").to_string(),
                host: first.get("host").and_then(Value::as_str).filter(|h| !h.is_empty()).unwrap_or("127.0.0.1").to_string(),
                port: first.get("port").and_then(Value::as_u64).filter(|p| *p > 0 && *p <= 65535).map(|p| p as u16).unwrap_or(default_port),
            })
        };
        out.push(Account { uin: uin.to_string(), http: endpoint("httpServers", 3000), ws: endpoint("wsServers", 3001) });
    }
    out.sort_by(|a, b| a.uin.cmp(&b.uin));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub http_token: String,
    pub base_url: String,
    pub self_id: String,
    pub source: String,
    pub ws_token: Option<String>,
    pub ws_url: Option<String>,
}

/// One `get_login_info`; the account data on success.
fn probe(base_url: &str, token: &str, timeout: Duration) -> Result<Value, String> {
    let ob = crate::onebot::OneBot::try_new_url(base_url, token.to_string())?;
    let body = ob
        .request_with_timeout("get_login_info", json!({}), timeout)
        .ok_or_else(|| "no answer".to_string())?;
    if body.get("status").and_then(Value::as_str) == Some("ok") && body.get("retcode").and_then(Value::as_i64) == Some(0) {
        Ok(body.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(format!(
            "refused (retcode {})",
            body.get("retcode").map(Value::to_string).unwrap_or_else(|| "?".to_string())
        ))
    }
}

/// JS `discoverSnowLumaConnection`: candidates are the configured token first, then every account file's HTTP token;
/// the first that answers `get_login_info` wins and brings its own account's WebSocket token and endpoint.
pub fn discover(cfg: &Value, base_url: &str, timeout: Duration) -> Result<Discovered, String> {
    let sl = cfg.get("snowluma").unwrap_or(&Value::Null);
    let explicit = sl.get("accessToken").and_then(Value::as_str).unwrap_or("").to_string();
    let mut candidates: Vec<(String, String, String)> = Vec::new(); // (token, source, url)
    fn add(list: &mut Vec<(String, String, String)>, token: String, source: String, url: String) {
        if !list.iter().any(|(t, _, u)| *t == token && *u == url) {
            list.push((token, source, url));
        }
    }
    if !explicit.is_empty() {
        add(&mut candidates, explicit.clone(), "config".to_string(), base_url.to_string());
    }
    let mut ws_info: Vec<(String, String, String)> = Vec::new(); // (uin, token, url)
    for home in home_candidates(cfg) {
        for account in read_onebot_tokens(&home) {
            if let Some(http) = &account.http {
                add(&mut candidates, http.token.clone(), format!("onebot_{}.json", account.uin), format!("http://{}:{}", http.host, http.port));
            }
            if let Some(ws) = account.ws.as_ref().filter(|w| !w.token.is_empty()) {
                ws_info.push((account.uin.clone(), ws.token.clone(), format!("ws://{}:{}", ws.host, ws.port)));
            }
        }
    }
    if explicit.is_empty() && candidates.is_empty() {
        add(&mut candidates, String::new(), "no-token".to_string(), base_url.to_string());
    }
    let mut last = String::from("no candidate");
    for (token, source, url) in &candidates {
        match probe(url, token, timeout) {
            Ok(data) => {
                let uin = source
                    .strip_prefix("onebot_")
                    .and_then(|r| r.strip_suffix(".json"))
                    .map(str::to_string)
                    .unwrap_or_else(|| data.get("user_id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default());
                let ws = ws_info.iter().find(|(u, _, _)| *u == uin);
                return Ok(Discovered {
                    http_token: token.clone(),
                    base_url: url.clone(),
                    self_id: data.get("user_id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or(uin),
                    source: source.clone(),
                    ws_token: ws.map(|(_, t, _)| t.clone()),
                    ws_url: ws.map(|(_, _, u)| u.clone()),
                });
            }
            Err(error) => last = format!("{source}: {error}"),
        }
    }
    Err(format!("no candidate token is accepted by the OneBot HTTP API ({last})"))
}

/// What the live connection should use after a successful heal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Healed {
    pub http_token: String,
    pub ws_token: String,
    pub ws_url: String,
    /// Fields written to `config.json` (names only, never values).
    pub persisted: Vec<&'static str>,
}

/// Which endpoints a heal uses for the connection and which it writes to `config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPlan {
    pub connect_ws: String,
    pub persist_ws: Option<String>,
    pub persist_http: Option<String>,
}

/// JS `applyHealedTokens`: with `followDiscoveredEndpoint` on (the default) any endpoint an account file named is followed
/// and written, wherever it points; with it off only tokens are healed. An endpoint nothing discovered is the live
/// connection's own (it may come from `dependencies.json`) and is never copied into `config.json`.
pub fn plan_endpoints(found: &Discovered, follow: bool, current_ws: &str) -> EndpointPlan {
    if !follow {
        return EndpointPlan { connect_ws: current_ws.to_string(), persist_ws: None, persist_http: None };
    }
    let discovered_http = found.source.starts_with("onebot_").then(|| found.base_url.clone());
    match &found.ws_url {
        Some(url) => EndpointPlan { connect_ws: url.clone(), persist_ws: Some(url.clone()), persist_http: discovered_http },
        None => EndpointPlan { connect_ws: current_ws.to_string(), persist_ws: None, persist_http: discovered_http },
    }
}

/// JS `healTokenIfStale` + `applyHealedTokens` for the selected runtime. `current_ws` is the connection's present
/// endpoint. Blocking (HTTP probes, file write). Never touches any config but the selected one.
pub fn heal(context: &RuntimeContext, cfg: &Value, base_url: &str, current_ws: &str, timeout: Duration) -> Result<Healed, String> {
    let found = discover(cfg, base_url, timeout)?;
    let follow = cfg.pointer("/snowluma/followDiscoveredEndpoint") != Some(&Value::Bool(false));
    let plan = plan_endpoints(&found, follow, current_ws);
    let ws_token = found.ws_token.clone().unwrap_or_else(|| found.http_token.clone());
    let persisted = crate::console::persist_snowluma_endpoint(
        context,
        &found.http_token,
        plan.persist_ws.as_deref(),
        plan.persist_http.as_deref(),
    )?;
    let ws_url = plan.connect_ws;
    Ok(Healed { http_token: found.http_token, ws_token, ws_url, persisted })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let home = crate::install_root::development_root(std::env::temp_dir().join(format!("her-heal-{tag}-{}", crate::turn_ledger::mint_id(""))));
        std::fs::create_dir_all(home.join("config")).unwrap();
        home
    }

    #[test]
    fn heal_attempts_are_throttled_to_one_per_ten_seconds() {
        let mut t = Throttle::default();
        assert!(t.allow(1_000_000));
        assert!(!t.allow(1_009_999), "a close storm fires many events");
        assert!(t.allow(1_010_000));
    }

    #[test]
    fn account_files_give_http_and_ws_tokens_sorted_by_uin_and_a_broken_file_is_skipped() {
        let home = temp_home("tokens");
        std::fs::write(
            home.join("config/onebot_222.json"),
            r#"{"networks":{"httpServers":[{"accessToken":"http-b","host":"127.0.0.1","port":3010}],"wsServers":[{"accessToken":"ws-b","port":3011}]}}"#,
        )
        .unwrap();
        std::fs::write(home.join("config/onebot_111.json"), r#"{"networks":{"httpServers":[{"accessToken":"http-a"}]}}"#).unwrap();
        std::fs::write(home.join("config/onebot_333.json"), "{not json").unwrap();
        std::fs::write(home.join("config/other.json"), "{}").unwrap();
        let accounts = read_onebot_tokens(&home);
        assert_eq!(accounts.iter().map(|a| a.uin.as_str()).collect::<Vec<_>>(), vec!["111", "222"]);
        assert_eq!(accounts[0].http.as_ref().unwrap().port, 3000, "default HTTP port");
        assert_eq!(accounts[0].ws, None);
        assert_eq!(accounts[1].ws.as_ref().unwrap(), &Endpoint { token: "ws-b".into(), host: "127.0.0.1".into(), port: 3011 });
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn home_candidates_come_from_the_config_only_and_are_deduplicated() {
        let cfg = json!({"snowluma": {"homeDir": "/opt/sl", "launcherPath": "/opt/sl/launcher"}});
        assert_eq!(home_candidates(&cfg), vec![PathBuf::from("/opt/sl")]);
    }

    fn found(ws_url: Option<&str>, source: &str) -> Discovered {
        Discovered {
            http_token: "http-t".into(),
            base_url: "http://10.1.2.3:3010".into(),
            self_id: "401".into(),
            source: source.into(),
            ws_token: ws_url.map(|_| "ws-t".to_string()),
            ws_url: ws_url.map(str::to_string),
        }
    }

    /// Review 5 (matches JS `applyHealedTokens`): with `followDiscoveredEndpoint` on, any discovered endpoint is followed
    /// and written, loopback or not.
    #[test]
    fn a_discovered_endpoint_is_followed_and_persisted_even_when_it_is_not_loopback() {
        let plan = plan_endpoints(&found(Some("ws://10.1.2.3:3011"), "onebot_401.json"), true, "ws://127.0.0.1:3001");
        assert_eq!(plan.connect_ws, "ws://10.1.2.3:3011");
        assert_eq!(plan.persist_ws.as_deref(), Some("ws://10.1.2.3:3011"));
        assert_eq!(plan.persist_http.as_deref(), Some("http://10.1.2.3:3010"));
    }

    /// Review 5: nothing discovered for the WebSocket: the live endpoint (which may come from `dependencies.json`) is kept
    /// for the connection and is never written into `config.json`; the HTTP endpoint is written only when an account file
    /// supplied it.
    #[test]
    fn an_endpoint_that_was_not_discovered_is_never_written_into_the_config() {
        let plan = plan_endpoints(&found(None, "config"), true, "ws://127.0.0.1:3001");
        assert_eq!(plan.connect_ws, "ws://127.0.0.1:3001");
        assert_eq!(plan.persist_ws, None, "the dependency endpoint must not be copied into config.json");
        assert_eq!(plan.persist_http, None, "the configured token's own URL is not a discovery");
        let plan = plan_endpoints(&found(None, "onebot_401.json"), true, "ws://127.0.0.1:3001");
        assert_eq!((plan.persist_ws, plan.persist_http.as_deref()), (None, Some("http://10.1.2.3:3010")));
    }

    #[test]
    fn follow_false_keeps_the_configured_endpoint_and_writes_no_url() {
        let plan = plan_endpoints(&found(Some("ws://10.1.2.3:3011"), "onebot_401.json"), false, "ws://127.0.0.1:3001");
        assert_eq!(plan, EndpointPlan { connect_ws: "ws://127.0.0.1:3001".into(), persist_ws: None, persist_http: None });
    }
}
