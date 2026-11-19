//! The shared, read-only definition of a running HER runtime.
use crate::dsh_credential::Login;
use crate::lifecycle::{
    self, DesiredState, DesiredStore, Effective, OwnerGate, OwnerProbe, SUPERVISOR_LOCK,
};
use crate::runtime::RuntimeContext;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Serialize)]
pub struct Part {
    verdict: &'static str,
    reason: String,
    /// The dependency's exact pinned version from runtime config, when one is configured.
    #[serde(rename = "pinnedVersion", skip_serializing_if = "Option::is_none")]
    pinned_version: Option<String>,
}
impl Part {
    fn new(pass: bool, reason: impl Into<String>) -> Self {
        Self {
            verdict: if pass { "pass" } else { "fail" },
            reason: reason.into(),
            pinned_version: None,
        }
    }
}

#[derive(Serialize)]
pub struct Verdict {
    v: u8,
    pub ok: bool,
    parts: BTreeMap<&'static str, Part>,
    #[serde(rename = "dependencyNotices", skip_serializing_if = "Vec::is_empty")]
    dependency_notices: Vec<serde_json::Value>,
    /// #186: where `rollout.rustGroups` and the allow/deny lists disagree. Reported beside the parts, never a failed
    /// part: a configuration choice does not make a release unhealthy.
    #[serde(rename = "scopeNotices", skip_serializing_if = "Vec::is_empty")]
    scope_notices: Vec<String>,
    /// #174: each model route's state (`harness::route_health::report`), present when a harness model is configured.
    /// Reported beside the parts and never a part: a provider outage does not make a release unhealthy.
    #[serde(rename = "modelRoutes", skip_serializing_if = "Option::is_none")]
    model_routes: Option<serde_json::Value>,
    /// Upgrade guidance for configured CLI candidates. It does not make the other health parts fail.
    #[serde(rename = "cliModelNotice", skip_serializing_if = "Option::is_none")]
    cli_model_notice: Option<&'static str>,
}

fn owner(data: &Path, role: &str, lock: &str) -> Part {
    if lifecycle::validate_existing_owner_gate_paths(data).is_err() {
        return Part::new(false, "owner gate paths are unavailable");
    }
    match OwnerGate::new(data.join("logs").join(lock)).probe_existing() {
        OwnerProbe::Held {
            marker: Some(marker),
        } if marker.role == role => {
            let matches_image = crate::process_identity::current_identity()
                .ok()
                .is_some_and(|me| crate::update::health_image(&me.exe, &marker.identity.exe));
            if matches_image
                && crate::process_identity::check_identity(&marker.identity)
                    == crate::process_identity::Liveness::Matches
            {
                Part::new(true, "supervisor owner process is alive")
            } else {
                Part::new(
                    false,
                    "supervisor owner process identity cannot be verified",
                )
            }
        }
        OwnerProbe::Free { .. } => Part::new(false, "supervisor owner is not held"),
        _ => Part::new(false, "supervisor owner cannot be identified"),
    }
}

/// One snapshot: the named parts of the health verdict (`dsh` and `dshLogin` come from one DSH probe).
pub fn check(root: &Path, deadline: Instant) -> Verdict {
    let mut parts = BTreeMap::new();
    let context = match RuntimeContext::from_root(root) {
        Ok(context) => context,
        Err(reason) => {
            for name in [
                "desired",
                "supervisor",
                "console",
                "snowluma",
                "dsh",
                "dshLogin",
            ] {
                parts.insert(name, Part::new(false, reason));
            }
            return Verdict {
                v: 1,
                ok: false,
                parts,
                dependency_notices: Vec::new(),
                scope_notices: Vec::new(),
                model_routes: None,
                cli_model_notice: None,
            };
        }
    };
    let desired = lifecycle::effective(
        &DesiredStore::new(context.data_dir().join("lifecycle")).read(),
        crate::parity_clock::now_ms(),
    );
    parts.insert(
        "desired",
        match desired {
            Effective::Act {
                state: DesiredState::Running,
                ..
            } => Part::new(true, "desired state is running"),
            Effective::Act { state, .. } => {
                Part::new(false, format!("desired state is {}", state.as_str()))
            }
            Effective::Unknown(_) => Part::new(false, "desired state is unknown"),
        },
    );
    parts.insert(
        "supervisor",
        owner(context.data_dir(), "supervisor", SUPERVISOR_LOCK),
    );
    let cfg = match context.load_config() {
        Ok(cfg) => cfg,
        Err(reason) => {
            for name in ["console", "snowluma", "dsh", "dshLogin", "relay"] {
                parts.insert(name, Part::new(false, reason));
            }
            return Verdict {
                v: 1,
                ok: false,
                parts,
                dependency_notices: crate::supervisor_dependencies::outage_notices(&context),
                scope_notices: Vec::new(),
                model_routes: None,
                cli_model_notice: None,
            };
        }
    };
    // Each peer gets the same absolute deadline; a silent peer cannot hide the others' verdicts.
    let (console, snowluma, dsh, relay) = std::thread::scope(|scope| {
        let console = scope.spawn(|| console_part(&context, &cfg, deadline));
        let snowluma = scope.spawn(|| snowluma_part(&context, &cfg, deadline));
        let dsh = scope.spawn(|| dsh_parts(&context, deadline));
        let relay = scope.spawn(|| {
            if cfg["dependencies"]["relay"].is_null() {
                return None;
            }
            let mut part =
                match crate::supervisor_dependencies::probe_relay(&context, &cfg, deadline) {
                    Ok(()) => Part::new(true, "relay models endpoint is healthy"),
                    Err(reason) => Part::new(false, reason),
                };
            match crate::supervisor_dependencies::pinned_version(&cfg, "relay") {
                Ok(version) => part.pinned_version = version,
                Err(reason) => part = Part::new(false, reason),
            }
            Some(part)
        });
        (
            console
                .join()
                .unwrap_or_else(|_| Part::new(false, "console probe failed")),
            snowluma
                .join()
                .unwrap_or_else(|_| Part::new(false, "SnowLuma probe failed")),
            dsh.join().unwrap_or_else(|_| {
                (
                    Part::new(false, "DSH probe failed"),
                    Part::new(false, "DSH probe failed"),
                )
            }),
            relay
                .join()
                .unwrap_or_else(|_| Some(Part::new(false, "relay probe failed"))),
        )
    });
    let (mut snowluma, (mut dsh, dsh_login)) = (snowluma, dsh);
    for (name, part) in [("snowluma", &mut snowluma), ("dsh", &mut dsh)] {
        match crate::supervisor_dependencies::pinned_version(&cfg, name) {
            Ok(version) => part.pinned_version = version,
            Err(reason) => *part = Part::new(false, reason),
        }
    }
    if let Some(relay) = relay {
        parts.insert("relay", relay);
    }
    parts.insert("console", console);
    parts.insert("snowluma", snowluma);
    parts.insert("dsh", dsh);
    parts.insert("dshLogin", dsh_login);
    Verdict {
        v: 1,
        ok: parts.values().all(|part| part.verdict == "pass"),
        parts,
        dependency_notices: crate::supervisor_dependencies::outage_notices(&context),
        scope_notices: crate::watchdog::rollout_scope_gaps(&cfg).lines(),
        model_routes: crate::harness::route_health::report(&context, &cfg),
        cli_model_notice: None, // RED-B2B
    }
}

fn console_part(context: &RuntimeContext, cfg: &serde_json::Value, deadline: Instant) -> Part {
    let port = match cfg.get("consolePort") {
        None | Some(serde_json::Value::Null) => Some(3100),
        Some(value) => value
            .as_f64()
            .filter(|p| p.fract() == 0.0 && (1.0..=65535.0).contains(p))
            .map(|p| p as u16),
    };
    let Some(port) = port else {
        return Part::new(false, "consolePort is invalid");
    };
    match crate::supervisor::health_console_probe(context, port, deadline) {
        crate::watchdog::ConsoleProbe::Managed { .. } => {
            Part::new(true, "console health and owner are verified")
        }
        crate::watchdog::ConsoleProbe::Absent => Part::new(false, "console is not listening"),
        crate::watchdog::ConsoleProbe::Foreign { .. } => {
            Part::new(false, "console listener has no verified selected owner")
        }
        crate::watchdog::ConsoleProbe::Unknown(reason) => Part::new(false, reason),
    }
}

fn snowluma_part(context: &RuntimeContext, cfg: &serde_json::Value, deadline: Instant) -> Part {
    match crate::dependency_watch::probe_status(context, cfg, Some(deadline)) {
        Err(_) => Part::new(false, "SnowLuma get_status failed or timed out"),
        Ok(body) if body["status"] != "ok" => Part::new(false, "SnowLuma get_status was refused"),
        Ok(body) if body.pointer("/data/online").and_then(|v| v.as_bool()) != Some(true) => {
            Part::new(false, "SnowLuma account is offline or unknown")
        }
        Ok(body) if body.pointer("/data/good").and_then(|v| v.as_bool()) != Some(true) => {
            Part::new(false, "SnowLuma receive chain is unhealthy or unknown")
        }
        Ok(_) => Part::new(true, "SnowLuma is online with a healthy receive chain"),
    }
}

/// `dsh` (DSH answers `settings/describe`) and `dshLogin` (DSH accepts a freshly minted cookie; a rejection is
/// refreshed once, two in a row fail) from one probe.
fn dsh_parts(context: &RuntimeContext, deadline: Instant) -> (Part, Part) {
    let check = crate::dsh_health::probe_login(context, Some(deadline));
    let dsh = match &check.result {
        Ok(()) => Part::new(true, "DSH settings/describe answered"),
        Err(reason) if reason.contains("401") => Part::new(false, "DSH authentication was refused"),
        Err(reason) if reason.contains("credentials") => {
            Part::new(false, "DSH credentials are unavailable or invalid")
        }
        Err(_) => Part::new(false, "DSH settings/describe failed or timed out"),
    };
    let login = match check.login {
        Login::Accepted => Part::new(true, "DSH accepted a freshly minted login cookie"),
        login if login.failing() => Part::new(false, crate::dsh_credential::REJECTED_TWICE_REASON),
        Login::Rejected { .. } => Part::new(
            false,
            "DSH rejected a freshly minted login cookie and no refreshed cookie could be minted",
        ),
        Login::Unchecked => match &check.result {
            Err(reason) if reason.contains("credentials") => {
                Part::new(false, "DSH credentials are unavailable or invalid")
            }
            _ => Part::new(false, "DSH login could not be checked: DSH did not answer"),
        },
    };
    (dsh, login)
}

fn wait_duration(value: &str) -> Option<Duration> {
    let (number, multiplier) = if let Some(n) = value.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = value.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = value.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (value, 1000)
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(Duration::from_millis(
        number.parse::<u64>().ok()?.checked_mul(multiplier)?,
    ))
}

fn output(verdict: &Verdict) -> i32 {
    println!("{}", serde_json::to_string(verdict).unwrap());
    if verdict.ok {
        0
    } else {
        1
    }
}

pub fn assert_running(root: &Path, wait: Option<&str>) -> i32 {
    let Some(wait) = wait_duration(wait.unwrap_or("0")) else {
        eprintln!("[lifecycle] --wait requires a nonnegative integer duration (ms, s, m, h; default seconds)");
        return 2;
    };
    let Some(end) = Instant::now().checked_add(wait) else {
        eprintln!("[lifecycle] --wait duration is too large");
        return 2;
    };
    loop {
        let budget = Instant::now() + crate::update::HEALTH_PROBE_ALLOWANCE;
        let verdict = check(
            root,
            if wait.is_zero() {
                budget
            } else {
                budget.min(end)
            },
        );
        if verdict.ok || wait.is_zero() || Instant::now() >= end {
            return output(&verdict);
        }
        std::thread::sleep(
            Duration::from_millis(100).min(end.saturating_duration_since(Instant::now())),
        );
        if Instant::now() >= end {
            return output(&verdict);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ticket219_cli_health_reports_missing_settings_credential() {
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(format!("her-cli-health-{}", crate::turn_ledger::mint_id(""))),
        );
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
        }
        let _cleanup = Cleanup(root.clone());
        for dir in ["config", "data/logs", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let path = root.join("config/config.json");
        let mut config = json!({
            "consoleToken":"fixture-health-token", "consolePort":1,
            "backends":{"list":[{"id":"fixture-cli","kind":"cli","cmd":"fixture"}]}
        });
        std::fs::write(&path, config.to_string()).unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let report = || serde_json::to_value(check(&root, Instant::now())).unwrap();
        let before = report();
        let notice = before["cliModelNotice"].as_str().unwrap_or_default();
        assert!(notice.contains("cliModel.apiKey") && notice.contains("Settings"),
            "health check omitted CLI credential upgrade guidance");
        config["backends"]["list"][0]["env"] = json!({"OPENAI_API_KEY":"fixture-key"});
        std::fs::write(path, config.to_string()).unwrap();
        assert!(report().get("cliModelNotice").is_none());
    }

    #[test]
    fn t186_health_reports_rollout_scope_notices_beside_the_parts() {
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(format!("her-t186-health-{}", crate::turn_ledger::mint_id(""))),
        );
        for dir in ["config", "data/logs", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let config = json!({
            "consoleToken": "synthetic-health-scope-token",
            "consolePort": 1,
            "allow": {"groups": [420042]},
            "rollout": {"enabled": false, "rustGroups": ["group:420042", "group:420044"]}
        });
        std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        // A complete selected-runtime layout needs `data/social-v2.json`; without it no configuration can be read
        // and so no scope can be compared.
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        RuntimeContext::from_root(&root).expect("the fixture is a complete runtime layout");
        let verdict = serde_json::to_value(check(&root, Instant::now() + Duration::from_secs(5))).unwrap();
        let notices = verdict["scopeNotices"].as_array().cloned().unwrap_or_default();
        assert_eq!(notices.len(), 1, "one served conversation the allow list refuses: {verdict}");
        assert!(notices[0].as_str().unwrap().contains("group:420044"), "{verdict}");
        let parts: Vec<&str> = verdict["parts"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(parts, ["console", "desired", "dsh", "dshLogin", "snowluma", "supervisor"], "a scope gap is never a part");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn route_health_is_reported_beside_the_parts_and_never_fails_one() {
        use crate::harness::route_health::{observe, Seen};
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(format!("her-174-health-{}", crate::turn_ledger::mint_id(""))),
        );
        for dir in ["config", "data/logs", "resources", "user/dsh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let config = json!({
            "consoleToken": "synthetic-health-routes-token",
            "consolePort": 1,
            "harness": {"routes": [{"id": "primary-route", "model": "model-a"}, {"id": "backup-route", "model": "model-b"}]}
        });
        std::fs::write(root.join("config/config.json"), config.to_string()).unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#,
        )
        .unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        let chain = crate::harness::routes::chain(&config).unwrap();
        observe(&context, &chain, 0, Seen::Failed { class: "quota" });
        observe(&context, &chain, 1, Seen::Failed { class: "quota" });
        let verdict = serde_json::to_value(check(&root, Instant::now() + Duration::from_secs(5))).unwrap();
        let routes = verdict["modelRoutes"]["routes"].as_array().cloned().unwrap_or_default();
        let states: Vec<(&str, &str)> = routes
            .iter()
            .map(|route| (route["id"].as_str().unwrap_or(""), route["state"].as_str().unwrap_or("")))
            .collect();
        assert_eq!(states, [("primary-route", "failed"), ("backup-route", "failed")], "{verdict}");
        assert_eq!(verdict["modelRoutes"]["allFailed"], true, "{verdict}");
        let parts: Vec<&str> = verdict["parts"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(parts, ["console", "desired", "dsh", "dshLogin", "snowluma", "supervisor"], "route health is never a part");
        std::fs::remove_dir_all(root).unwrap();
    }
}
