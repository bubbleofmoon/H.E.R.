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
