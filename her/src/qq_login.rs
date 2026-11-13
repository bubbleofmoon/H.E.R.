//! Read-only SnowLuma login observation. Offline is distinct from login-needed.
use crate::runtime::{RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const EPISODE_FILE: &str = "qq-login-episode.json";
const RETRY_MS: i64 = 60_000;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Episode {
    v: u8,
    active: bool,
    episode: u64,
    checks: u64,
    state: String,
    #[serde(default)]
    notified: bool,
    #[serde(default)]
    retry_after_ms: i64,
}

/// Supervisor-owned polling remains available even if the bridge is not running.
pub fn start(context: RuntimeContext) {
    let failure_context = context.clone();
    if std::thread::Builder::new()
        .name("qq-login".into())
        .spawn(move || loop {
            if let Ok(cfg) = context.load_config() {
                if observe(&context, &cfg).is_err() {
                    record_failure(&context);
                }
            }
            std::thread::sleep(Duration::from_secs(30));
        })
        .is_err()
    {
        record_failure(&failure_context);
    }
}

fn record_failure(context: &RuntimeContext) {
    crate::recovery_log::record(
        context,
        crate::run_record::Trigger::Timer,
        crate::run_record::Component::Snowluma,
        "qq-login",
        crate::run_record::Outcome::Failed,
        "QQ login episode state is unavailable; notice withheld",
    );
}

fn load_episode(context: &RuntimeContext) -> Result<Episode, String> {
    let mut episode = match context.read_optional_text(RuntimeArea::Data, EPISODE_FILE)? {
        Some(text) => {
            serde_json::from_str::<Episode>(&text).map_err(|_| "invalid login episode")?
        }
        None => Episode {
            v: 2,
            active: false,
            episode: 0,
            checks: 0,
            state: "unknown".into(),
            notified: false,
            retry_after_ms: 0,
        },
    };
    if !matches!(episode.v, 1 | 2)
        || !matches!(
            episode.state.as_str(),
            "unknown" | "logged-in" | "needs-login"
        )
    {
        return Err("unsupported login episode".into());
    }
    if episode.v == 1 {
        // Legacy active claims have no delivery receipt. Keep them pending;
        // episodes already closed by confirmed recovery stay closed.
        episode.v = 2;
        episode.notified = !episode.active;
    }
    Ok(episode)
}

fn save_episode(context: &RuntimeContext, episode: &Episode) -> Result<(), String> {
    let bytes = serde_json::to_vec(episode).map_err(|_| "invalid login episode")?;
    context.write_atomic(RuntimeArea::Data, EPISODE_FILE, &bytes)
}

fn observe(context: &RuntimeContext, cfg: &Value) -> Result<(), String> {
    let snapshot = probe(context, cfg);
    let state = snapshot["state"]
        .as_str()
        .ok_or("invalid login observation")?;
    let gate = context.authority_gate()?;
    let mut episode = load_episode(context)?;
    let claim = state == "needs-login" && !episode.active;
    if claim {
        episode.episode = episode
            .episode
            .checked_add(1)
            .ok_or("login episode overflow")?;
        episode.active = true;
        episode.notified = false;
    } else if state == "logged-in" {
        episode.active = false;
    }
    // Recovery rearms detection but never erases an undelivered notice. A new
    // episode coalesces any older pending notice without bypassing its retry bound.
    episode.state = state.into();
    episode.checks = episode
        .checks
        .checked_add(1)
        .ok_or("login check overflow")?;
    let now = crate::parity_clock::now_ms();
    let attempt = episode.episode > 0 && !episode.notified && now >= episode.retry_after_ms;
    if attempt {
        // Persist before sending so rapid supervisor restarts cannot hammer QQ.
        episode.retry_after_ms = now.saturating_add(RETRY_MS);
    }
    save_episode(context, &episode)?;
    drop(gate);
    if attempt {
        let text = if state == "logged-in" {
            "HER detected QQ needs login earlier. QQ is now logged in; this notice was pending while delivery was unavailable."
        } else {
            "QQ needs login. Open HER and choose Open QQ login window to sign in in the native QQ window."
        };
        if crate::admin_alert::notify_notice_wait(
            context,
            cfg,
            text,
            &format!("qq-login-episode-{}", episode.episode),
        ) {
            let _gate = context.authority_gate()?;
            let mut current = load_episode(context)?;
            if current.episode == episode.episode {
                current.notified = true;
                save_episode(context, &current)?;
            }
        }
    }
    Ok(())
}

pub fn probe(context: &RuntimeContext, cfg: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(2);
    let processes = webui_processes(context, cfg);
    let online = crate::dependency_watch::probe_status(context, cfg, Some(deadline))
        .ok()
        .filter(|body| body["status"] == "ok" && body["retcode"] == 0)
        .and_then(|body| body["data"]["online"].as_bool());
    let mut loaded = Vec::new();
    let mut logged_in = false;
    let mut unknown = false;
    for process in processes.as_ref().into_iter().flatten() {
        if process["connected"] != true {
            continue;
        }
        match (
            process["injected"].as_bool(),
            process["loggedIn"].as_bool(),
            process["status"].as_str(),
        ) {
            (Some(true), Some(false), Some("loaded")) => loaded.push(process),
            (Some(true), Some(true), Some("online")) => logged_in = true,
            _ => unknown = true,
        }
    }
    // Never collapse an offline/failed probe into a login requirement. A loaded,
    // connected hook is the explicit login-needed signal in SnowLuma 1.14.22.
    let state = if unknown
        || (!loaded.is_empty() && (logged_in || online == Some(true)))
        || (logged_in && online == Some(false))
    {
        "unknown"
    } else if !loaded.is_empty() {
        "needs-login"
    } else if logged_in || online == Some(true) {
        "logged-in"
    } else {
        "unknown"
    };
    let targets = if state == "needs-login" {
        loaded
            .into_iter()
            .filter_map(verified_target)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    json!({"ok":true,"state":state,"processes":targets})
}

fn webui_processes(context: &RuntimeContext, cfg: &Value) -> Option<Vec<Value>> {
    let endpoint = cfg
        .pointer("/snowluma/webuiUrl")?
        .as_str()?
        .trim_end_matches('/');
    // Reuse the strict endpoint validator: explicit port, literal loopback HTTP,
    // no URL credentials, path, query or redirect. There is no guessed endpoint.
    crate::onebot::OneBot::try_new_url(endpoint, String::new()).ok()?;
    let token = context
        .read_optional_text(RuntimeArea::Data, "snowluma-webui-token")
        .ok()??;
    let token = token.trim();
    if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control) {
        return None;
    }
    let reply = crate::qq_tools::http_request(
        endpoint.strip_prefix("http://")?,
        "GET",
        "/api/processes",
        &[("Authorization".into(), format!("Bearer {token}"))],
        None,
        Duration::from_secs(1),
    )
    .ok()?;
    if reply.status != 200 || reply.body.len() > 1024 * 1024 {
        return None;
    }
    let body: Value = serde_json::from_slice(&reply.body).ok()?;
    body["list"].as_array().cloned()
}

fn verified_target(process: &Value) -> Option<Value> {
    let pid = u32::try_from(process["pid"].as_u64()?)
        .ok()
        .filter(|pid| *pid != 0)?;
    let reported = std::path::Path::new(process["path"].as_str()?);
    if !reported.is_absolute()
        || !reported
            .file_name()?
            .to_string_lossy()
            .eq_ignore_ascii_case("QQ.exe")
    {
        return None;
    }
    let native = crate::process_identity::identity_of(pid).ok()??;
    if !std::path::Path::new(&native.exe)
        .file_name()?
        .to_string_lossy()
        .eq_ignore_ascii_case("QQ.exe")
    {
        return None;
    }
    if std::fs::canonicalize(reported).ok()? != std::fs::canonicalize(&native.exe).ok()? {
        return None;
    }
    // Only PID/start cross the console boundary; neither paths nor account IDs do.
    Some(json!({"pid":native.pid,"start":native.start}))
}
