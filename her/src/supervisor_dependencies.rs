//! Dependency startup belongs to the supervisor. Children deliberately outlive it.
use crate::lifecycle::{self, DesiredStore, Effective};
use crate::process_identity::{self as identity, Match, ProcessIdentity};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

#[path = "supervisor_relay.rs"]
mod relay;
#[path = "dependency_upgrade.rs"]
pub(crate) mod upgrades;

const STATUS: &str = "lifecycle/dependencies.json";
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";
const NPM_INSTALL_LIMIT: Duration = Duration::from_secs(600);

/// A dependency's pinned version from runtime config: `None` when unpinned. A pin is one exact
/// release version (`1.14.22`, `0.2.1-rc.2`); ranges, tags and build metadata are refused.
pub fn pinned_version(cfg: &Value, name: &str) -> Result<Option<String>, String> {
    static EXACT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let exact = EXACT.get_or_init(|| {
        regex::Regex::new(
            r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$",
        )
        .unwrap()
    });
    match cfg["dependencies"][name].get("version") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(version))
            if exact.is_match(version)
                && version.split_once('-').is_none_or(|(_, prerelease)| {
                    prerelease.split('.').all(|id| {
                        !(id.len() > 1
                            && id.starts_with('0')
                            && id.bytes().all(|b| b.is_ascii_digit()))
                    })
                }) =>
        {
            Ok(Some(version.clone()))
        }
        _ => Err(format!(
            "pinned {} version must be one exact release version",
            match name {
                "dsh" => "DSH",
                "relay" => "relay",
                _ => "SnowLuma",
            }
        )),
    }
}

struct Clock {
    epoch: i64,
    started: Instant,
    #[cfg(feature = "parity-clock")]
    fixture_now: std::cell::Cell<Option<i64>>,
}
impl Clock {
    fn new() -> Self {
        Self {
            epoch: crate::parity_clock::now_ms(),
            started: Instant::now(),
            #[cfg(feature = "parity-clock")]
            fixture_now: std::cell::Cell::new(None),
        }
    }
    fn now(&self, _context: &RuntimeContext) -> i64 {
        #[cfg(feature = "parity-clock")]
        if std::env::var_os("HER_TEST_DEPENDENCY_CLOCK").is_some() {
            for _ in 0..10 {
                if let Ok(Some(value)) =
                    _context.read_optional_text(RuntimeArea::Data, "lifecycle/dependency-clock")
                {
                    if let Ok(now) = value.trim().parse::<i64>() {
                        self.fixture_now.set(Some(now));
                        return now;
                    }
                }
                // A fixture clock replacement never switches timers to host time.
                if let Some(now) = self.fixture_now.get() {
                    return now;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("fixture dependency clock is unavailable");
        }
        self.epoch
            .saturating_add(self.started.elapsed().as_millis() as i64)
    }
}

fn remember(states: &mut Value, name: &str, state: &str, found: &ProcessIdentity, now: i64) {
    if states[name]["identity"] != json!(found) {
        states[name]["healthySince"] = Value::Null;
        states[name]["refusalSince"] = Value::Null;
        states[name]["startedAt"] = json!(now);
        states[name]["ready"] = json!(false);
    }
    states[name]["state"] = json!(state);
    states[name]["identity"] = json!(found);
}

fn backoff_ms(step: u64) -> i64 {
    match step {
        1 => 5_000,
        2 => 30_000,
        _ => 60_000,
    }
}

fn restore_timers(states: &mut Value, name: &str, now: i64) {
    // Preserve bounded durations from the last observation, not the previous process's
    // wall-clock anchor. Downtime cannot prove continuous health or refusal.
    let observed = states[name]["timerAt"].as_i64().unwrap_or(now);
    if let Some(deadline) = states[name]["restartAt"].as_i64() {
        let remaining = deadline.saturating_sub(observed).clamp(
            0,
            backoff_ms(states[name]["restartStep"].as_u64().unwrap_or(0)),
        );
        states[name]["restartAt"] = json!(now.saturating_add(remaining));
    }
    for (field, bound) in [
        ("healthySince", 600_000),
        ("refusalSince", 120_000),
        ("startedAt", 90_000),
        ("wrapperSince", 30_000),
    ] {
        if let Some(since) = states[name][field].as_i64() {
            let elapsed = observed.saturating_sub(since).clamp(0, bound);
            states[name][field] = json!(now.saturating_sub(elapsed));
        }
    }
    if let Some(failures) = states[name]["failures"].as_array() {
        let valid: Vec<_> = failures
            .iter()
            .filter_map(Value::as_i64)
            .filter(|at| *at <= now && now.saturating_sub(*at) <= 600_000)
            .collect();
        states[name]["failures"] = json!(valid);
    }
}

fn failed(states: &mut Value, name: &str, now: i64) {
    let step = states[name]["restartStep"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(1);
    let delay = backoff_ms(step);
    states[name]["state"] = json!("backoff");
    states[name]["reason"] = json!(format!("{name} exited; waiting to restart"));
    states[name]["restartStep"] = json!(step);
    states[name]["restartAt"] = json!(now.saturating_add(delay));
    states[name]["healthySince"] = Value::Null;
    states[name]["refusalSince"] = Value::Null;
    let mut failures: Vec<i64> = states[name]["failures"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .filter(|at| *at <= now && now.saturating_sub(*at) <= 600_000)
        .collect();
    failures.push(now);
    states[name]["failures"] = json!(failures);
}

fn healthy(states: &mut Value, name: &str, now: i64) {
    let since = states[name]["healthySince"].as_i64().unwrap_or(now);
    states[name]["healthySince"] = json!(since);
    states[name]["fingerprint"] = Value::Null;
    states[name]["refusalSince"] = Value::Null;
    states[name]["ready"] = json!(true);
    if states[name]["state"] == "refused" {
        states[name]["state"] = json!("adopted");
    }
    if now.saturating_sub(since) >= 600_000 {
        states[name]["restartStep"] = json!(0);
        states[name]["failures"] = json!([]);
        states[name]["outageAlerted"] = json!(false);
    }
}

fn read_states(context: &RuntimeContext) -> Result<Value, String> {
    let text = context
        .read_optional_text(RuntimeArea::Data, STATUS)
        .map_err(|_| "dependency state is unreadable; no process changed")?;
    let value: Value = match text {
        None => json!({}),
        Some(text) => serde_json::from_str(&text)
            .map_err(|_| "dependency state is invalid; no process changed")?,
    };
    if !value
        .as_object()
        .is_some_and(|entries| entries.values().all(Value::is_object))
    {
        return Err("dependency state is invalid; no process changed".into());
    }
    Ok(value)
}

fn liveness(found: &ProcessIdentity) -> identity::Liveness {
    let observed = identity::check_identity(found);
    if matches!(observed, identity::Liveness::Unknown(_)) {
        // A Windows process being reaped may temporarily deny a new handle.
        // Retry observation only; a persistent denial still authorizes no action.
        std::thread::sleep(Duration::from_millis(50));
        identity::check_identity(found)
    } else {
        observed
    }
}

struct Dependency {
    name: &'static str,
    program: PathBuf,
    script: Option<PathBuf>,
    cwd: PathBuf,
    endpoint: SocketAddr,
    /// Managed DSH: `script` is selected from this versioned install before startup.
    install: Option<Install>,
    relay: Option<relay::Settings>,
}

impl Dependency {
    // Every launch/adoption/restart consumer must require preparation to have selected an entry.
    fn script(&self) -> Result<&Path, String> {
        self.script
            .as_deref()
            .ok_or_else(|| "DSH entry has not been prepared".into())
    }
}

/// npm installs the pinned DSH package into `<dependencies.directory>/dsh/<version>`.
struct Install {
    version: String,
    folder: PathBuf,
    npm: PathBuf,
    registry: Option<String>,
}

fn process_match(process: &identity::Process, dependency: &Dependency) -> Result<Match, String> {
    if let Some(relay) = &dependency.relay {
        let program = identity::matches(process, &dependency.program, None);
        if !matches!(program, Match::Matches) {
            return Ok(program);
        }
        if process.args.get(1..) != Some(relay.arguments.as_slice()) {
            return Ok(Match::Mismatch("relay arguments differ".into()));
        }
        let cwd = std::fs::canonicalize(&process.cwd)
            .map_err(|_| "relay working directory cannot be inspected")?;
        if cwd != dependency.cwd {
            return Ok(Match::Mismatch("relay working directory differs".into()));
        }
        Ok(Match::Matches)
    } else {
        Ok(identity::matches(
            process,
            &dependency.program,
            Some(dependency.script()?),
        ))
    }
}

fn names(cfg: &Value) -> Vec<&'static str> {
    let mut names = vec!["snowluma"];
    if !cfg["dependencies"]["relay"].is_null() {
        names.push("relay");
    }
    names.push("dsh");
    names
}

struct Entry {
    pid: u32,
    parent: u32,
    name: String,
}

struct Scan {
    found: Vec<ProcessIdentity>,
    pending: bool,
}

fn matching(dependency: &Dependency) -> Result<Scan, String> {
    let mut found = Vec::new();
    let mut pending = false;
    let entries = os::processes()?;
    let name = dependency
        .program
        .file_name()
        .ok_or("dependency program has no filename")?
        .to_string_lossy();
    for entry in &entries {
        let dsh_node = dependency.name == "dsh"
            && (entry.name.eq_ignore_ascii_case("node.exe")
                || entry.name.eq_ignore_ascii_case("node"));
        if !entry.name.eq_ignore_ascii_case(&name) && !dsh_node {
            continue;
        }
        let instance = identity::identity_of(entry.pid).map_err(|_| format!("{}: cannot inspect elevated or access-restricted dependency (pid {}); no process changed",dependency.name,entry.pid))?;
        let Some(instance) = instance else { continue };
        let image = identity::Process {
            identity: instance,
            cwd: String::new(),
            args: Vec::new(),
        };
        let configured_program = match identity::matches(&image, &dependency.program, None) {
            Match::Mismatch(_) if !dsh_node => continue,
            Match::Matches => true,
            Match::Mismatch(_) => false,
            _ => {
                return Err(format!(
                    "{}: cannot inspect dependency program; no process changed",
                    dependency.name
                ))
            }
        };
        let process = match identity::identify(entry.pid) {
            Ok(Some(process)) => process,
            Ok(None) => continue,
            // A known foreign image alone is neither a dependency nor evidence
            // of a relevant wrapper. Listener inspection remains fail-closed.
            Err(_) if !configured_program => continue,
            Err(_) => return Err(format!("{}: cannot inspect elevated or access-restricted dependency (pid {}); no process changed",dependency.name,entry.pid)),
        };
        // Wrapper discovery spans Node installations; only exact configured
        // program/script matches below can confer dependency ownership.
        if dependency.name == "dsh"
            && process
                .args
                .iter()
                .any(|s| s == "@deepseek-ai/dsh" || s.starts_with("@deepseek-ai/dsh@"))
            && matches!(
                identity::matches(
                    &process,
                    std::path::Path::new(&process.identity.exe),
                    Some(dependency.script()?)
                ),
                Match::Wrapper
            )
        {
            pending |= unresolved(&process.identity, &entries)?;
            continue;
        }
        match process_match(&process, dependency)? {
            Match::Matches
                if matches!(
                    identity::check_identity(&process.identity),
                    identity::Liveness::Matches
                ) =>
            {
                found.push(process.identity)
            }
            Match::Unknown(_) => {
                return Err(format!(
                    "{}: cannot inspect dependency script; no process changed",
                    dependency.name
                ))
            }
            _ => {}
        }
    }
    Ok(Scan { found, pending })
}

fn unresolved(wrapper: &ProcessIdentity, entries: &[Entry]) -> Result<bool, String> {
    let mut queue = std::collections::VecDeque::from([wrapper.clone()]);
    let mut seen = std::collections::BTreeSet::new();
    let mut resolved = false;
    while let Some(parent) = queue.pop_front() {
        if !seen.insert((parent.pid, parent.start)) {
            continue;
        }
        for entry in entries.iter().filter(|entry| entry.parent == parent.pid) {
            let Some(child)=identity::identity_of(entry.pid).map_err(|_|"cannot inspect elevated or access-restricted DSH wrapper descendant; no process changed")? else { continue };
            if child.start < parent.start {
                continue;
            }
            queue.push_back(child.clone());
            if !entry.name.eq_ignore_ascii_case("node.exe")
                && !entry.name.eq_ignore_ascii_case("node")
            {
                continue;
            }
            let Some(process)=identity::identify(child.pid).map_err(|_|"cannot inspect elevated or access-restricted DSH wrapper descendant; no process changed")? else { continue };
            if !identity::same_identity(&child, &process.identity) {
                continue;
            }
            if identity::resolved_script(&process)?.is_some() {
                resolved = true;
            }
        }
    }
    if resolved {
        return Ok(false);
    }
    match identity::check_identity(wrapper) {
        identity::Liveness::Matches => Ok(true),
        identity::Liveness::Gone | identity::Liveness::Mismatch(_) => Ok(false),
        identity::Liveness::Unknown(_) => Err(
            "cannot inspect elevated or access-restricted DSH wrapper; no process changed".into(),
        ),
    }
}

fn wanted(context: &RuntimeContext) -> bool {
    matches!(lifecycle::effective(&DesiredStore::new(context.data_dir().join("lifecycle")).read(), crate::parity_clock::now_ms()),
        Effective::Act { wants, .. } if wants.jobs)
}

fn same_desired(context: &RuntimeContext, expected: &lifecycle::DesiredRead) -> bool {
    DesiredStore::new(context.data_dir().join("lifecycle")).read() == *expected && wanted(context)
}

fn absolute(value: Option<&str>) -> Result<PathBuf, String> {
    let path = PathBuf::from(
        value
            .filter(|s| !s.is_empty())
            .ok_or("dependency startup path is missing")?,
    );
    if !path.is_absolute() {
        return Err("dependency startup paths must be absolute".into());
    }
    // Validate the external spelling before any child is started.
    crate::subprocess::external_path(&path)?;
    Ok(path)
}

fn configuration(
    context: &RuntimeContext,
    cfg: &Value,
    name: &'static str,
) -> Result<Dependency, String> {
    let legacy = &cfg["tasks"]["start-all"];
    let endpoint = match name {
        "snowluma" => "onebot",
        "relay" => "relay",
        _ => "dsh",
    };
    let url = url::Url::parse(
        context
            .dependency_url(endpoint)
            .ok_or("dependency endpoint is missing")?,
    )
    .map_err(|_| "dependency endpoint is invalid")?;
    let ip: IpAddr = match url.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => ip.into(),
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => ip.into(),
        _ => return Err("dependency endpoint must use a loopback IP address".into()),
    };
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("dependency endpoint must be a plain loopback HTTP origin".into());
    }
    let port = url
        .port_or_known_default()
        .filter(|p| *p != 0)
        .ok_or("dependency port is invalid")?;
    if ["onebot", "dsh", "relay"]
        .into_iter()
        .filter(|other| *other != endpoint)
        .filter_map(|other| context.dependency_url(other))
        .filter_map(|text| url::Url::parse(text).ok())
        .any(|other| other.port_or_known_default() == Some(port))
    {
        return Err("dependency ports must be distinct".into());
    }
    if name == "relay" {
        return relay::configuration(context, cfg, SocketAddr::new(ip, port));
    }
    let value = &cfg["dependencies"][name];
    let mut install = None;
    let (mut program, mut script, cwd) = if !value.is_null() {
        match (name, pinned_version(cfg, name)?) {
            ("dsh", Some(version)) => {
                if !value["workingDirectory"].is_null() {
                    return Err("pinned DSH works in the runtime's dsh/work folder; remove dependencies.dsh.workingDirectory".into());
                }
                let program = absolute(value["program"].as_str())?;
                let cwd = context
                    .resolve_path(RuntimeArea::DshWork, "", PathIntent::CreateDirectory)
                    .map_err(str::to_string)?;
                if value["script"].is_null() {
                    install = Some(managed(context, cfg, &program, &version)?);
                    (program, None, cwd)
                } else {
                    // An existing install is used in place only when it proves the pin.
                    let script = absolute(value["script"].as_str())?;
                    // Guard before reading the package manifests around it.
                    crate::install_root::check_runtime(&script, false)?;
                    let script = configured_version(&script, &version)?;
                    (program, Some(script), cwd)
                }
            }
            _ => (
                absolute(value["program"].as_str())?,
                Some(absolute(value["script"].as_str())?),
                absolute(value["workingDirectory"].as_str())?,
            ),
        }
    } else if name == "snowluma" {
        let folder = absolute(legacy["snowLumaDirectory"].as_str())?;
        (
            folder.join(if cfg!(windows) { "node.exe" } else { "node" }),
            Some(folder.join("index.mjs")),
            folder,
        )
    } else {
        (
            absolute(cfg["tasks"]["nodePath"].as_str())?,
            Some(absolute(legacy["dshScript"].as_str())?),
            absolute(legacy["dshWorkingDirectory"].as_str())?,
        )
    };
    for path in [&program, &cwd].into_iter().chain(script.as_ref()) {
        if !path.as_os_str().is_empty() {
            crate::install_root::check_runtime(path, false)?;
        }
    }
    if !program.is_file()
        || (install.is_none() && !script.as_ref().is_some_and(|script| script.is_file()))
        || !cwd.is_dir()
    {
        return Err(format!(
            "{name} configured program, script or working directory is unavailable"
        ));
    }
    if name == "dsh" {
        let cached_spelling = npx_cache(&program) || script.as_ref().is_some_and(|p| npx_cache(p));
        program = resolved_path(&program)?;
        script = script.map(|p| resolved_path(&p)).transpose()?;
        if cached_spelling || npx_cache(&program) || script.as_ref().is_some_and(|p| npx_cache(p)) {
            if pinned_version(cfg, name)?.is_some() {
                return Err("DSH must not run from an npx cache path".into());
            }
            static WARN: std::sync::Once = std::sync::Once::new();
            WARN.call_once(|| crate::safe_eprintln!("[dependencies] unpinned DSH uses an npx cache path; configure the pin and versioned install at switchover"));
        }
    }
    Ok(Dependency {
        name,
        program,
        script,
        cwd,
        endpoint: SocketAddr::new(ip, port),
        install,
        relay: None,
    })
}

pub(crate) fn probe_relay(
    context: &RuntimeContext,
    cfg: &Value,
    deadline: Instant,
) -> Result<(), String> {
    let dependency = configuration(context, cfg, "relay")?;
    dependency
        .relay
        .as_ref()
        .ok_or("relay configuration is unavailable")?
        .probe(dependency.endpoint, deadline)
}

fn npx_cache(path: &Path) -> bool {
    path.components().any(
        |c| matches!(c, Component::Normal(s) if s.to_string_lossy().eq_ignore_ascii_case("_npx")),
    )
}

fn dependency_directory(context: &RuntimeContext, cfg: &Value) -> Result<PathBuf, String> {
    Ok(match cfg["dependencies"].get("directory") {
        None | Some(Value::Null) => {
            // Production keeps dependencies beside the runtime: <install root>/deps.
            let install = crate::install_root::InstallRoot::configured()?;
            let runtime = context.runtime_root().ok_or("runtime root unavailable")?;
            if std::fs::canonicalize(install.root().join("runtime"))
                .ok()
                .as_deref()
                != Some(runtime)
            {
                return Err(
                    "dependencies.directory is not configured for the dependency install".into(),
                );
            }
            install.root().join("deps")
        }
        Some(value) => absolute(value.as_str())?,
    })
}

fn managed(
    context: &RuntimeContext,
    cfg: &Value,
    program: &Path,
    version: &str,
) -> Result<Install, String> {
    let directory = dependency_directory(context, cfg)?;
    let npm = match cfg["dependencies"]["dsh"].get("npm") {
        None | Some(Value::Null) => {
            let folder = program.parent().ok_or("DSH program has no folder")?;
            if cfg!(windows) {
                folder.join("node_modules/npm/bin/npm-cli.js")
            } else {
                folder
                    .parent()
                    .ok_or("DSH program has no installation folder")?
                    .join("lib/node_modules/npm/bin/npm-cli.js")
            }
        }
        Some(value) => absolute(value.as_str())?,
    };
    let registry = match cfg["dependencies"]["dsh"].get("registry") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let text = value
                .as_str()
                .ok_or("dependencies.dsh.registry is invalid")?;
            let url = url::Url::parse(text).map_err(|_| "dependencies.dsh.registry is invalid")?;
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(
                    "dependencies.dsh.registry must be an http(s) URL without credentials".into(),
                );
            }
            Some(text.to_string())
        }
    };
    for path in [&directory, &npm] {
        crate::install_root::check_runtime(path, false)?;
    }
    if !npm.is_file() {
        return Err("npm for the managed DSH install is unavailable".into());
    }
    let install = Install {
        version: version.to_string(),
        folder: directory.join("dsh").join(version),
        npm,
        registry,
    };
    guard_install(&install)?;
    Ok(install)
}

/// Validate derived destinations before metadata reads, creation, npm or cleanup.
/// Resolve the existing prefix too: the leaf need not exist yet.
fn guard_destination(path: &Path) -> Result<(), String> {
    crate::install_root::check_runtime(path, false)?;
    let mut ancestor = path;
    let mut missing = Vec::new();
    let mut resolved = loop {
        match std::fs::canonicalize(ancestor) {
            Ok(resolved) => break resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or("DSH destination cannot be resolved")?,
                );
                ancestor = ancestor
                    .parent()
                    .ok_or("DSH destination cannot be resolved")?;
            }
            Err(_) => return Err("DSH destination cannot be resolved".into()),
        }
    };
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    crate::install_root::check_runtime(&resolved, false)?;
    if npx_cache(path) || npx_cache(&resolved) {
        return Err("DSH must not run from an npx cache path".into());
    }
    Ok(())
}

fn guard_install(install: &Install) -> Result<(), String> {
    let parent = install
        .folder
        .parent()
        .ok_or("versioned DSH folder is invalid")?;
    for path in [
        parent.to_path_buf(),
        install.folder.clone(),
        parent.join(".npm-cache"),
        parent.join(".npm-userconfig"),
        parent.join(".npm-globalconfig"),
    ] {
        guard_destination(&path)?;
    }
    Ok(())
}

fn resolved_path(path: &Path) -> Result<PathBuf, String> {
    crate::install_root::check_runtime(path, false)?;
    let resolved = std::fs::canonicalize(path).map_err(|_| "DSH path cannot be resolved")?;
    crate::install_root::check_runtime(&resolved, false)?;
    Ok(resolved)
}

fn pinned_path(path: &Path) -> Result<PathBuf, String> {
    let resolved = resolved_path(path)?;
    if npx_cache(path) || npx_cache(&resolved) {
        return Err("DSH must not run from an npx cache path".into());
    }
    Ok(resolved)
}

fn manifest(folder: &Path) -> Result<Value, String> {
    let folder = pinned_path(folder)?;
    let path = pinned_path(&folder.join("package.json"))?;
    if path.parent() != Some(folder.as_path()) {
        return Err("DSH package manifest leaves its package".into());
    }
    let text = std::fs::read_to_string(path).map_err(|_| "DSH package manifest is unavailable")?;
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| "DSH package manifest is invalid".into())
}

/// Prove the resolved entry belongs to this package, never a nested node_modules package.
fn entry_within(script: &Path, package: &Path) -> Result<(), String> {
    let relative = script
        .strip_prefix(package)
        .map_err(|_| "DSH package command entry leaves its package")?;
    if relative
        .components()
        .any(|c| matches!(c, Component::Normal(s) if s.to_string_lossy().eq_ignore_ascii_case("node_modules")))
    {
        return Err("DSH package command entry belongs to a nested package".into());
    }
    Ok(())
}

/// A configured entry is proved using its canonical package, not an enclosing lexical alias.
fn configured_version(script: &Path, version: &str) -> Result<PathBuf, String> {
    let script = pinned_path(script)?;
    for folder in script.ancestors().skip(1) {
        let path = folder.join("package.json");
        crate::install_root::check_runtime(&path, false)?;
        if !path.exists() {
            continue;
        }
        let manifest = manifest(folder)?;
        if manifest["name"] != DSH_PACKAGE {
            return Err(format!("configured DSH script is not inside the {DSH_PACKAGE} package; its version cannot be proven"));
        }
        entry_within(&script, folder)?;
        if manifest["version"] == version {
            return Ok(script);
        }
        return Err(format!(
            "configured DSH install is version {}, not the pinned {version}; refusing to run it",
            manifest["version"].as_str().unwrap_or("unknown")
        ));
    }
    Err(format!("configured DSH script is not inside the {DSH_PACKAGE} package; its version cannot be proven"))
}

/// The command entry of an installed DSH package that must be exactly the pinned version.
fn package_entry(package: &Path, version: &str) -> Result<PathBuf, String> {
    let mismatch =
        || format!("versioned DSH folder does not hold {DSH_PACKAGE} {version}; left alone");
    let package = pinned_path(package)?;
    let manifest = manifest(&package).map_err(|error| format!("{}: {error}", mismatch()))?;
    if manifest["name"] != DSH_PACKAGE || manifest["version"] != version {
        return Err(mismatch());
    }
    let bin = match &manifest["bin"] {
        Value::String(bin) => Some(bin.as_str()),
        Value::Object(bins) => bins
            .get("dsh")
            .or_else(|| (bins.len() == 1).then(|| bins.values().next()).flatten())
            .and_then(Value::as_str),
        _ => None,
    }
    .ok_or("DSH package declares no command entry")?;
    let relative = Path::new(bin);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("DSH package command entry leaves its package".into());
    }
    let script = pinned_path(&package.join(relative))?;
    entry_within(&script, &package)?;
    configured_version(&script, version)?;
    if !script.is_file() {
        return Err("DSH package command entry is missing".into());
    }
    Ok(script)
}

fn dependency_log(
    context: &RuntimeContext,
    stem: &str,
    suffix: &str,
) -> Result<std::fs::File, String> {
    let path = context
        .resolve_path(
            RuntimeArea::Data,
            &format!("logs/dependency-{stem}.{suffix}.log"),
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| "dependency log is unavailable".into())
}

/// Validate each dependency independently, then prepare its install before any launch.
fn prepare(
    context: &RuntimeContext,
    cfg: &Value,
    name: &'static str,
    states: &mut Value,
) -> Result<Option<Dependency>, String> {
    let mut dependency = configuration(context, cfg, name)?;
    if let Some(settings) = &dependency.relay {
        relay::restrict(&settings.folder)?;
        settings.check_arguments()?;
    }
    if !prepare_install(context, &mut dependency, states)? {
        return Ok(None);
    }
    Ok(Some(dependency))
}

/// Staging belongs to the supervisor through npm exit, validation and publication/cleanup.
/// Unknown, live, reused and redirected entries stay untouched.
fn sweep_staging(parent: &Path) -> Result<(), String> {
    guard_destination(parent)?;
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("DSH staging folders cannot be inspected".into()),
    };
    for entry in entries {
        let entry = entry.map_err(|_| "DSH staging folder cannot be inspected")?;
        if !entry.file_name().to_string_lossy().starts_with(".install-") {
            continue;
        }
        let path = entry.path();
        guard_destination(&path)?;
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || redirected(&metadata) {
            continue;
        }
        let marker = path.join(".installer.json");
        guard_destination(&marker)?;
        let Ok(metadata) = std::fs::symlink_metadata(&marker) else {
            continue;
        };
        if !metadata.is_file() || redirected(&metadata) {
            continue;
        }
        let Some(owner) = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|text| serde_json::from_str::<ProcessIdentity>(&text).ok())
        else {
            continue;
        };
        if identity::check_identity(&owner) == identity::Liveness::Gone {
            guard_destination(&path)?;
            std::fs::remove_dir_all(&path)
                .map_err(|_| "stale DSH staging folder cannot be removed")?;
        }
    }
    Ok(())
}

fn redirected(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn record_staging_owner(staging: &Path) -> Result<(), String> {
    use std::io::Write;
    let owner = identity::identity_of(std::process::id())?
        .ok_or("DSH staging supervisor identity is unavailable")?;
    let marker = staging.join(".installer.json");
    guard_destination(&marker)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .map_err(|_| "DSH staging owner record cannot be created")?;
    file.write_all(
        serde_json::to_string(&owner)
            .map_err(|_| "DSH staging owner record is invalid")?
            .as_bytes(),
    )
    .and_then(|()| file.sync_all())
    .map_err(|_| "DSH staging owner record cannot be written".into())
}

fn prepare_install(
    context: &RuntimeContext,
    dependency: &mut Dependency,
    states: &mut Value,
) -> Result<bool, String> {
    let Some(install) = &dependency.install else {
        return Ok(true);
    };
    guard_install(install)?;
    sweep_staging(
        install
            .folder
            .parent()
            .ok_or("versioned DSH folder is invalid")?,
    )?;
    match std::fs::symlink_metadata(&install.folder) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err("versioned DSH path is not a plain folder; left alone".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let retry = &states["dsh"]["installRetry"];
            if retry["version"] == install.version
                && retry["after"]
                    .as_i64()
                    .is_some_and(|at| at > crate::parity_clock::now_ms())
            {
                return Ok(false);
            }
            if !install_dsh(context, &dependency.program, install, states)? {
                return Ok(false);
            }
        }
        Err(_) => return Err("versioned DSH folder cannot be inspected".into()),
    }
    let script = package_entry(
        &install.folder.join("node_modules").join(DSH_PACKAGE),
        &install.version,
    )?;
    crate::install_root::check_runtime(&script, false)?;
    dependency.script = Some(script);
    Ok(true)
}

/// npm installs into a private staging folder that becomes the versioned folder only when it
/// holds exactly the pinned package, so a partial install is never adopted.
fn install_dsh(
    context: &RuntimeContext,
    program: &Path,
    install: &Install,
    states: &mut Value,
) -> Result<bool, String> {
    if !wanted(context) {
        return Ok(false);
    }
    let parent = install
        .folder
        .parent()
        .ok_or("versioned DSH folder is invalid")?;
    guard_install(install)?;
    std::fs::create_dir_all(parent).map_err(|_| "DSH dependency folder cannot be created")?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let staging = parent.join(format!(
        ".install-{}-{}-{nonce}",
        install.version,
        std::process::id()
    ));
    guard_destination(&staging)?;
    std::fs::create_dir(&staging).map_err(|_| "DSH install staging folder cannot be created")?;
    let retry = &states["dsh"]["installRetry"];
    let failures = if retry["version"] == install.version {
        retry["failures"].as_u64().unwrap_or(0)
    } else {
        0
    };
    states["dsh"] = json!({"state":"installing","version":install.version});
    let result = record_staging_owner(&staging)
        .and_then(|()| publish(context, states))
        .and_then(|()| run_npm(context, program, install, &staging))
        .and_then(|completed| {
            if !completed || !wanted(context) {
                return Ok(false);
            }
            guard_install(install)?;
            guard_destination(&staging)?;
            package_entry(
                &staging.join("node_modules").join(DSH_PACKAGE),
                &install.version,
            )?;
            let marker = staging.join(".installer.json");
            guard_destination(&marker)?;
            match std::fs::remove_file(marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("DSH staging owner record cannot be removed".into()),
            }
            std::fs::rename(&staging, &install.folder)
                .or_else(|_| {
                    if install.folder.is_dir() {
                        Ok(())
                    } else {
                        Err("installed DSH could not be moved into its versioned folder"
                            .to_string())
                    }
                })
                .map(|()| true)
        });
    if guard_destination(&staging).is_ok() && staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    if matches!(result, Ok(false)) {
        states["dsh"] = json!({"state":"stopped"});
        publish(context, states)?;
    } else if result.is_err() && wanted(context) {
        let failures = failures.saturating_add(1);
        let seconds = (600_u64 * (1_u64 << failures.saturating_sub(1).min(3))).min(3600);
        states["dsh"]["installRetry"] = json!({"version":install.version,"failures":failures,
            "after":crate::parity_clock::now_ms().saturating_add(seconds as i64 * 1000)});
    }
    result
}

/// These files are never filled from ambient config, and existing non-empty/redirected files
/// are refused rather than truncated. npm cannot load the same path as both user and global.
fn empty_npm_config(path: &Path) -> Result<(), String> {
    guard_destination(path)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata =
                std::fs::symlink_metadata(path).map_err(|_| "npm config cannot be inspected")?;
            #[cfg(windows)]
            let redirected = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_attributes() & 0x400 != 0
            };
            #[cfg(not(windows))]
            let redirected = metadata.file_type().is_symlink();
            if redirected || !metadata.is_file() || metadata.len() != 0 {
                Err("supervisor npm configuration must be a plain empty file".into())
            } else {
                Ok(())
            }
        }
        Err(_) => Err("supervisor npm configuration cannot be created".into()),
    }
}

fn run_npm(
    context: &RuntimeContext,
    program: &Path,
    install: &Install,
    staging: &Path,
) -> Result<bool, String> {
    guard_install(install)?;
    guard_destination(staging)?;
    let external = crate::subprocess::external_path;
    let cache = install
        .folder
        .parent()
        .ok_or("versioned DSH folder is invalid")?
        .join(".npm-cache");
    let parent = install
        .folder
        .parent()
        .ok_or("versioned DSH folder is invalid")?;
    let userconfig = parent.join(".npm-userconfig");
    let globalconfig = parent.join(".npm-globalconfig");
    // Guard every destination together before the first write.
    guard_install(install)?;
    empty_npm_config(&userconfig)?;
    empty_npm_config(&globalconfig)?;
    let mut command = crate::subprocess::command_without_console(external(program)?);
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("npm_config_")
        {
            command.env_remove(name);
        }
    }
    command
        .arg(external(&install.npm)?)
        .args(["install", "--prefix"])
        .arg(external(staging)?)
        .arg("--cache")
        .arg(external(&cache)?)
        .args([
            "--no-save",
            "--no-package-lock",
            "--no-audit",
            "--no-fund",
            "--no-update-notifier",
            "--omit=dev",
        ]);
    let registry = install
        .registry
        .as_deref()
        .unwrap_or("https://registry.npmjs.org/");
    command
        .arg("--registry")
        .arg(registry)
        .arg(format!("--@deepseek-ai:registry={registry}"))
        .arg("--userconfig")
        .arg(external(&userconfig)?)
        .arg("--globalconfig")
        .arg(external(&globalconfig)?);
    command
        .arg(format!("{DSH_PACKAGE}@{}", install.version))
        .current_dir(external(staging)?)
        .stdin(Stdio::null())
        .stdout(dependency_log(context, "dsh-install", "out")?)
        .stderr(dependency_log(context, "dsh-install", "err")?)
        .env_remove("DSH_LAUNCH_TOKEN");
    let failed = |what: &str| {
        format!(
            "{what} DSH {}; see data/logs/dependency-dsh-install.err.log",
            install.version
        )
    };
    let mut tree = os::NpmTree::spawn(command)?;
    let deadline = Instant::now() + NPM_INSTALL_LIMIT;
    loop {
        if !wanted(context) {
            tree.stop();
            return Ok(false);
        }
        match tree.child().try_wait() {
            Ok(Some(status)) if status.success() => return Ok(true),
            Ok(Some(_)) => return Err(failed("npm could not install")),
            Ok(None) if Instant::now() >= deadline => {
                tree.stop();
                return Err(failed("npm timed out after 10 minutes installing"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => {
                tree.stop();
                return Err(failed("npm could not be observed installing"));
            }
        }
    }
}

fn inspected(pid: u32, dependency: &Dependency) -> Result<ProcessIdentity, String> {
    let process = identity::identify(pid).map_err(|_| format!("{}: cannot inspect elevated or access-restricted dependency (pid {pid}); no process changed", dependency.name))?
        .ok_or("dependency process exited during inspection")?;
    match process_match(&process, dependency)? {
        Match::Matches
            if matches!(
                identity::check_identity(&process.identity),
                identity::Liveness::Matches
            ) =>
        {
            Ok(process.identity)
        }
        Match::Unknown(_) => Err(format!(
            "{}: cannot inspect dependency program and script; no process changed",
            dependency.name
        )),
        _ => Err(format!(
            "{}: port {} taken by an unknown process (pid {pid}); left alone",
            dependency.name,
            dependency.endpoint.port()
        )),
    }
}

fn holder(dependency: &Dependency) -> Result<Option<ProcessIdentity>, String> {
    let pids = os::listeners(dependency.endpoint.port())?;
    match pids.as_slice() {
        [] => Ok(None),
        [pid] => inspected(*pid, dependency).map(Some),
        _ => Err(format!(
            "{}: port {} taken by an unknown process (multiple owners); left alone",
            dependency.name,
            dependency.endpoint.port()
        )),
    }
}

fn publish(context: &RuntimeContext, states: &Value) -> Result<(), String> {
    context.write_atomic(RuntimeArea::Data, STATUS, states.to_string().as_bytes())
}

fn ensure(
    context: &RuntimeContext,
    dependency: &Dependency,
    states: &mut Value,
    clock: &Clock,
    desired: &lifecycle::DesiredRead,
) -> Result<(), String> {
    if !same_desired(context, desired) {
        return Err("desired state does not permit dependency startup".into());
    }
    if let Some(found) = holder(dependency)? {
        remember(
            states,
            dependency.name,
            "adopted",
            &found,
            clock.now(context),
        );
        return Ok(());
    }
    let scan = matching(dependency)?;
    if let Some(found) = scan.found.into_iter().next() {
        remember(
            states,
            dependency.name,
            "adopted",
            &found,
            clock.now(context),
        );
        return Ok(());
    }
    if scan.pending {
        let now = clock.now(context);
        let since = states[dependency.name]["wrapperSince"]
            .as_i64()
            .unwrap_or(now);
        states[dependency.name]["wrapperSince"] = json!(since);
        states[dependency.name]["state"] = json!("waiting");
        states[dependency.name]["reason"] = json!("waiting for npm/npx DSH entry resolution");
        if now.saturating_sub(since) >= 30_000 {
            return Err("a pending npm/npx DSH wrapper did not resolve within 30 seconds; refusing to start DSH".into());
        }
        return Ok(());
    }
    states[dependency.name]["wrapperSince"] = Value::Null;
    let gate_path = context
        .resolve_path(
            RuntimeArea::Data,
            "lifecycle/lifecycle.lock",
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    let desired_gate = lifecycle::lock_with_wait(&gate_path, Duration::from_secs(5))?;
    if !same_desired(context, desired) {
        return Err("desired state no longer permits dependency startup".into());
    }
    // Listener ownership can change while acquiring the desired-state gate.
    if let Some(found) = holder(dependency)? {
        remember(
            states,
            dependency.name,
            "adopted",
            &found,
            clock.now(context),
        );
        return publish(context, states);
    }
    let log = |suffix: &str| -> Result<std::fs::File, String> {
        let path = context
            .resolve_path(
                RuntimeArea::Data,
                &format!("logs/dependency-{}.{suffix}.log", dependency.name),
                PathIntent::WriteFile,
            )
            .map_err(str::to_string)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| "dependency log is unavailable".into())
    };
    let mut command = crate::subprocess::command_without_console(crate::subprocess::external_path(
        &dependency.program,
    )?);
    if let Some(settings) = &dependency.relay {
        command.args(&settings.arguments);
    } else {
        command.arg(crate::subprocess::external_path(dependency.script()?)?);
    }
    command
        .current_dir(crate::subprocess::external_path(&dependency.cwd)?)
        .stdin(Stdio::null())
        .stdout(log("out")?)
        .stderr(log("err")?);
    if dependency.relay.is_some() {
        // Upstream output may contain keys or login data. Only HER lifecycle metadata is retained.
        command.stdout(Stdio::null()).stderr(Stdio::null());
    } else if dependency.name == "dsh" {
        let home = context
            .resolve_path(RuntimeArea::DshUser, "", PathIntent::CreateDirectory)
            .map_err(str::to_string)?;
        command
            .args(["web", "--no-open"])
            .env("DSH_HOME", crate::subprocess::external_path(home)?);
        command.env_remove("DSH_LAUNCH_TOKEN");
    } else {
        command.env("SNOWLUMA_HOOK_AUTOLOAD", "1");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A dependency has its own session, never the task executor's kill-on-close group.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command.spawn().map_err(|e| {
        failed(states, dependency.name, clock.now(context));
        format!("{} could not start: {}", dependency.name, e.kind())
    })?;
    let found = identity::identity_of(child.id());
    // Retain/reap only our child; dropping the supervisor does not kill dependencies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let found = found?.ok_or_else(|| {
        failed(states, dependency.name, clock.now(context));
        "started dependency exited before identity could be recorded".to_string()
    })?;
    remember(
        states,
        dependency.name,
        "started",
        &found,
        clock.now(context),
    );
    publish(context, states)?;
    drop(desired_gate);
    Ok(())
}

fn listening(dependency: &Dependency) -> Result<bool, String> {
    let timeout = Duration::from_millis(if cfg!(windows) { 5_000 } else { 500 });
    match std::net::TcpStream::connect_timeout(&dependency.endpoint, timeout) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => Ok(false),
        Err(_) => Err(format!(
            "{}: selected port observation is unknown; no process changed",
            dependency.name
        )),
    }
}

fn check_port(states: &mut Value, dependency: &Dependency) -> Result<bool, String> {
    let result = listening(dependency).and_then(|listening| {
        if listening {
            if let Some(settings) = &dependency.relay {
                settings.probe(
                    dependency.endpoint,
                    Instant::now() + Duration::from_millis(500),
                )?;
            }
        }
        Ok(listening)
    });
    if result.is_err() {
        states[dependency.name]["refusalSince"] = Value::Null;
    }
    result
}

fn stop_hung(
    context: &RuntimeContext,
    dependency: &Dependency,
    found: &ProcessIdentity,
    states: &mut Value,
    clock: &Clock,
    desired: &lifecycle::DesiredRead,
) -> Result<bool, String> {
    let path = context
        .resolve_path(
            RuntimeArea::Data,
            "lifecycle/lifecycle.lock",
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    let _desired_gate = lifecycle::lock_with_wait(&path, Duration::from_secs(5))?;
    if !same_desired(context, desired) {
        return Ok(false);
    }
    if holder(dependency)?.is_some_and(|owner| !identity::same_identity(&owner, found)) {
        return Err(format!(
            "{} listener changed before hang recovery; left alone",
            dependency.name
        ));
    }
    let inspected = inspected(found.pid, dependency)?;
    if !identity::same_identity(&inspected, found) {
        return Err(format!(
            "{} identity changed before hang recovery; left alone",
            dependency.name
        ));
    }
    if check_port(states, dependency)? {
        healthy(states, dependency.name, clock.now(context));
        return Ok(true);
    }
    match identity::kill_verified(found) {
        identity::KillOutcome::Killed | identity::KillOutcome::Gone => {
            failed(states, dependency.name, clock.now(context));
            states[dependency.name]["reason"] =
                json!("dependency hung: selected port refused for two minutes; waiting to restart");
            Ok(false)
        }
        other => Err(format!(
            "{} hung dependency could not be stopped: {other:?}; no restart",
            dependency.name
        )),
    }
}

/// Caller holds the lifecycle gate and has already validated the stopped revision.
pub(crate) fn stop(context: &RuntimeContext) -> Result<(), String> {
    if !matches!(DesiredStore::new(context.data_dir().join("lifecycle")).read(), lifecycle::DesiredRead::Present(record) if record.state == lifecycle::DesiredState::Stopped)
    {
        return Err("explicit stopped desired state is required for dependency stop".into());
    }
    let cfg = context.load_config()?;
    if cfg.get("dependencies").is_none()
        && cfg["tasks"]["start-all"].get("snowLumaDirectory").is_none()
    {
        return Ok(());
    }
    let dependencies = names(&cfg)
        .into_iter()
        .map(|name| {
            let mut dependency = configuration(context, &cfg, name)?;
            // Stop resolves only an existing install; it must never run npm.
            if let Some(install) = &dependency.install {
                guard_install(install)?;
                let script = package_entry(
                    &install.folder.join("node_modules").join(DSH_PACKAGE),
                    &install.version,
                )?;
                crate::install_root::check_runtime(&script, false)?;
                dependency.script = Some(script);
            }
            Ok(dependency)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut states = read_states(context)?;
    let mut targets = Vec::new();
    // Inspect both dependencies before terminating either, including non-listeners.
    for dependency in &dependencies {
        let listener = holder(dependency)?;
        let scan = matching(dependency)?;
        if scan.pending {
            return Err("unresolved npm/npx DSH wrapper; dependency stop refused".into());
        }
        let mut found = scan.found;
        if let Some(listener) = listener {
            if !found
                .iter()
                .any(|recorded| identity::same_identity(recorded, &listener))
            {
                found.push(listener);
            }
        }
        for instance in found {
            targets.push((dependency, instance));
        }
    }
    for (dependency, instance) in targets {
        match liveness(&instance) {
            identity::Liveness::Gone | identity::Liveness::Mismatch(_) => continue,
            identity::Liveness::Unknown(_) => {
                return Err(format!(
                    "{} cannot inspect elevated or access-restricted dependency; stop refused",
                    dependency.name
                ))
            }
            identity::Liveness::Matches => {}
        }
        let verified = inspected(instance.pid, dependency)?;
        if !identity::same_identity(&verified, &instance) {
            return Err("dependency identity changed before stop".into());
        }
        match identity::kill_verified(&instance) {
            identity::KillOutcome::Killed | identity::KillOutcome::Gone => {}
            other => {
                return Err(format!(
                    "{} dependency stop refused: {other:?}",
                    dependency.name
                ))
            }
        }
        states[dependency.name] = json!({"state":"stopped","identity":instance});
        publish(context, &states)?;
    }
    // An already-dead dependency is intentionally stopped too; do not resume its outage.
    for dependency in &dependencies {
        states[dependency.name] =
            json!({"state":"stopped","identity":states[dependency.name]["identity"].clone()});
    }
    publish(context, &states)
}

/// One supervisor-owned worker reconciles dependency state and persisted restart deadlines.
pub fn start(context: RuntimeContext) {
    upgrades::start(context.clone());
    std::thread::spawn(move || {
        let clock = Clock::new();
        let mut fresh = true;
        loop {
            let recovered = match upgrades::apply_pending(&context) {
                Ok(idle) => idle,
                Err(error) => { crate::safe_eprintln!("[dependency-upgrade] {error}"); false }
            };
            if wanted(&context) {
                let blocked = if recovered { Ok(None) } else { upgrades::blocked(&context) };
                if let Ok(blocked) = blocked {
                    reconcile(&context, &clock, fresh, blocked);
                    fresh = false;
                }
            } else {
                // Resume by re-adopting survivors and restoring timers from observed time.
                fresh = true;
            }
            std::thread::sleep(Duration::from_millis(if cfg!(feature = "parity-clock") {
                100
            } else {
                1_000
            }));
        }
    });
}

fn reconcile_one(
    context: &RuntimeContext,
    dependency: &Dependency,
    states: &mut Value,
    clock: &Clock,
    fresh: bool,
    desired: &lifecycle::DesiredRead,
) -> Result<bool, String> {
    let name = dependency.name;
    let now = clock.now(context);
    if states[name]["state"] == "stopped" {
        states[name]["identity"] = Value::Null;
        states[name]["state"] = json!("pending");
    }
    if states[name]["state"] == "backoff" {
        if now
            < states[name]["restartAt"]
                .as_i64()
                .ok_or("dependency restart deadline is invalid")?
        {
            return Ok(false);
        }
        states[name]["identity"] = Value::Null;
        states[name]["state"] = json!("pending");
    } else if let Ok(found) =
        serde_json::from_value::<ProcessIdentity>(states[name]["identity"].clone())
    {
        match liveness(&found) {
            identity::Liveness::Gone | identity::Liveness::Mismatch(_) => {
                failed(states, name, clock.now(context));
                return Ok(false);
            }
            identity::Liveness::Unknown(_) => return Err(format!("{name}: cannot inspect elevated or access-restricted dependency; no process changed")),
            identity::Liveness::Matches => {
                if fresh { states[name]["state"] = json!("adopted"); }
            }
        }
    }
    if let Some(found) = holder(dependency)? {
        if !serde_json::from_value::<ProcessIdentity>(states[name]["identity"].clone())
            .is_ok_and(|recorded| identity::same_identity(&found, &recorded))
        {
            remember(states, name, "adopted", &found, clock.now(context));
        }
    }
    if !states[name]["identity"].is_object() {
        ensure(context, dependency, states, clock, desired)?;
    }
    if !wanted(context) {
        return Ok(false);
    }
    let Ok(found) = serde_json::from_value::<ProcessIdentity>(states[name]["identity"].clone())
    else {
        return Ok(false);
    };
    let verified = inspected(found.pid, dependency)?;
    if !identity::same_identity(&found, &verified) {
        return Err(format!(
            "{name} identity changed during monitoring; left alone"
        ));
    }
    if check_port(states, dependency)? {
        healthy(states, name, clock.now(context));
        return Ok(true);
    }
    let now = clock.now(context);
    let since = states[name]["refusalSince"].as_i64().unwrap_or(now);
    states[name]["refusalSince"] = json!(since);
    states[name]["healthySince"] = Value::Null;
    if now.saturating_sub(since) >= 120_000 {
        return stop_hung(context, dependency, &found, states, clock, desired);
    }
    if states[name]["ready"] != true
        && now.saturating_sub(states[name]["startedAt"].as_i64().unwrap_or(now)) >= 90_000
    {
        states[name]["state"] = json!("refused");
        states[name]["reason"] = json!(format!(
            "{name} did not listen within 90 seconds; left running"
        ));
    }
    Ok(false)
}

// Persist outage notices before attempting delivery through the recovering gateway.
fn checkpoint(
    context: &RuntimeContext,
    cfg: &Value,
    states: &mut Value,
    name: &str,
    error: Option<&str>,
    desired: &lifecycle::DesiredRead,
    previous: &Value,
) -> Result<(), String> {
    let path = context
        .resolve_path(
            RuntimeArea::Data,
            "lifecycle/lifecycle.lock",
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    let _desired_gate = lifecycle::lock_with_wait(&path, Duration::from_secs(5))?;
    // A stop may have published newer dependency state after this pass read it.
    if !same_desired(context, desired) {
        return Ok(());
    }
    let mut pending_created = false;
    let notice = if states[name]["state"] == "backoff" {
        if states[name]["failures"]
            .as_array()
            .is_some_and(|times| times.len() >= 3)
            && states[name]["outageAlerted"] != true
        {
            states[name]["outageAlerted"] = json!(true);
            let first = states[name]["failures"][0].as_i64().unwrap_or(0);
            states[name]["outageNotice"] = json!({
                "key": format!("{name}-outage-{first}"),
                "reason": format!("{name} failed 3 times within 10 minutes; retrying with restart backoff"),
                "status": "pending"
            });
            pending_created = true;
            None
        } else {
            None
        }
    } else if let Some(reason) = error {
        states[name]["state"] = json!("refused");
        states[name]["reason"] = json!(reason);
        states[name]["healthySince"] = Value::Null;
        states[name]["refusalSince"] = Value::Null;
        if states[name]["fingerprint"] != reason {
            states[name]["fingerprint"] = json!(reason);
            Some((name.to_string(), reason.to_string()))
        } else {
            None
        }
    } else {
        None
    };
    if states != previous {
        publish(context, states)?;
    }
    if pending_created {
        record_outage_notice(
            context,
            states,
            name,
            crate::run_record::Outcome::Failed,
            "pending administrator notice; awaiting SnowLuma",
        );
    }
    deliver_outage_notice(context, cfg, states, name)?;
    if let Some((key, reason)) = notice {
        alert(context, cfg, &key, &reason);
    }
    Ok(())
}

fn record_outage_notice(
    context: &RuntimeContext,
    states: &Value,
    name: &str,
    outcome: crate::run_record::Outcome,
    delivery: &str,
) {
    let notice = &states[name]["outageNotice"];
    crate::recovery_log::record(
        context,
        crate::run_record::Trigger::Timer,
        crate::run_record::Component::Alert,
        &format!("dependency-{}", notice["key"].as_str().unwrap_or(name)),
        outcome,
        &format!(
            "{}; {delivery}",
            notice["reason"].as_str().unwrap_or("dependency outage")
        ),
    );
}

fn deliver_outage_notice(
    context: &RuntimeContext,
    cfg: &Value,
    states: &mut Value,
    name: &str,
) -> Result<(), String> {
    if states[name]["outageNotice"]["status"] != "pending" {
        // An interrupted attempt has an unknown outcome and must never be repeated.
        return Ok(());
    }
    let Some(owner) = crate::admin_alert::owner_id(cfg) else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    if !crate::dependency_watch::probe_status(context, cfg, Some(deadline))
        .is_ok_and(|body| body["status"] == "ok" && body["data"]["online"] == true)
    {
        return Ok(());
    }
    let ob = crate::console::media_onebot(context, cfg)?;
    let key = states[name]["outageNotice"]["key"]
        .as_str()
        .ok_or("invalid outage notice")?;
    let scope = context
        .runtime_root()
        .ok_or("outage notice requires selected runtime")?
        .to_string_lossy();
    if !crate::admin_alert::notice_limiter()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .check(
            &scope,
            &format!("dependency-{key}"),
            crate::parity_clock::now_ms(),
        )
        .is_send()
    {
        return Ok(());
    }
    let reason = states[name]["outageNotice"]["reason"]
        .as_str()
        .ok_or("invalid outage notice")?;
    let body =
        crate::admin_alert::prepare_body(reason, |text| crate::activity::redact(context, text));
    // A replacement after this checkpoint cannot distinguish an unsent request from a lost reply.
    states[name]["outageNotice"]["status"] = json!("attempting");
    publish(context, states)?;
    let (status, outcome, detail) =
        match ob.send_one_outcome(&crate::onebot::Target::Private(owner), &body, None, None) {
            Ok(_) => (
                "sent",
                crate::run_record::Outcome::Ok,
                "sent to the administrator",
            ),
            Err(crate::onebot::SendError::NotCommitted(_)) => (
                "pending",
                crate::run_record::Outcome::Failed,
                "pre-send refusal; awaiting retry",
            ),
            Err(crate::onebot::SendError::Unknown(_)) => (
                "unknown",
                crate::run_record::Outcome::Unknown,
                "delivery outcome unknown; never retried",
            ),
        };
    states[name]["outageNotice"]["status"] = json!(status);
    publish(context, states)?;
    record_outage_notice(context, states, name, outcome, detail);
    Ok(())
}

/// Read-only delivery diagnostics for the shared health verdict. Notices do not change health.
pub(crate) fn outage_notices(context: &RuntimeContext) -> Vec<Value> {
    let Ok(states) = read_states(context) else {
        return Vec::new();
    };
    ["snowluma", "relay", "dsh"]
        .into_iter()
        .filter_map(|name| {
            let mut notice = states[name]["outageNotice"].as_object()?.clone();
            notice.insert("dependency".into(), json!(name));
            if notice
                .get("status")
                .is_some_and(|status| status == "attempting")
            {
                notice.insert("status".into(), json!("unknown"));
            }
            Some(Value::Object(notice))
        })
        .collect()
}

fn reconcile(context: &RuntimeContext, clock: &Clock, fresh: bool, blocked: Option<&str>) {
    let desired = DesiredStore::new(context.data_dir().join("lifecycle")).read();
    let Ok(cfg) = context.load_config() else {
        return;
    };
    let mut states = match read_states(context) {
        Ok(states) => states,
        Err(reason) => {
            crate::safe_eprintln!("[dependencies] {reason}");
            return;
        }
    };
    let result = (|| {
        if cfg.get("dependencies").is_none()
            && cfg["tasks"]["start-all"].get("snowLumaDirectory").is_none()
        {
            return Ok(());
        }
        let _launch_gate = os::launch_gate(context)?;
        let mut predecessors_ready = true;
        for name in names(&cfg) {
            if blocked == Some(name) { continue; }
            if !same_desired(context, &desired) {
                break;
            }
            // Startup is ordered; established dependencies continue healing independently.
            if !predecessors_ready
                && !states[name]["identity"].is_object()
                && states[name]["restartStep"].as_u64().unwrap_or(0) == 0
            {
                continue;
            }
            let previous = states.clone();
            if fresh {
                restore_timers(&mut states, name, clock.now(context));
            }
            let ready =
                prepare(context, &cfg, name, &mut states).and_then(|dependency| match dependency {
                    Some(dependency) => {
                        reconcile_one(context, &dependency, &mut states, clock, fresh, &desired)
                    }
                    None => Ok(false),
                });
            if ready.is_err()
                && states[name]["state"] != "backoff"
                && serde_json::from_value::<ProcessIdentity>(states[name]["identity"].clone())
                    .is_ok_and(|found| {
                        matches!(
                            liveness(&found),
                            identity::Liveness::Gone | identity::Liveness::Mismatch(_)
                        )
                    })
            {
                failed(&mut states, name, clock.now(context));
            }
            states[name]["timerAt"] = json!(clock.now(context));
            checkpoint(
                context,
                &cfg,
                &mut states,
                name,
                ready.as_ref().err().map(String::as_str),
                &desired,
                &previous,
            )?;
            predecessors_ready &= matches!(ready, Ok(true));
        }
        Ok::<_, String>(())
    })();
    if let Err(reason) = result {
        let previous = states.clone();
        if let Err(error) = checkpoint(
            context,
            &cfg,
            &mut states,
            "configuration",
            Some(&reason),
            &desired,
            &previous,
        ) {
            crate::safe_eprintln!("[dependencies] {error}");
        }
    }
}

fn alert(context: &RuntimeContext, cfg: &Value, name: &str, reason: &str) {
    crate::safe_eprintln!("[dependencies] {reason}");
    crate::admin_alert::notify_notice(context, cfg, reason, &format!("dependency-{name}"));
    crate::recovery_log::record(
        context,
        crate::run_record::Trigger::Timer,
        crate::run_record::Component::Process,
        &format!("dependency-{name}"),
        crate::run_record::Outcome::Failed,
        reason,
    );
}

#[cfg(windows)]
mod os {
    pub(super) use super::relay::windows_private as private;
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    pub fn processes() -> Result<Vec<Entry>, String> {
        use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err("dependency process table is unavailable".into());
        }
        let result = (|| {
            let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut next = unsafe { Process32FirstW(snapshot, &mut entry) };
            let mut entries = Vec::new();
            while next != 0 {
                let length = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                entries.push(Entry {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..length]),
                });
                next = unsafe { Process32NextW(snapshot, &mut entry) };
            }
            if unsafe { GetLastError() } != 18 {
                return Err("dependency process table is incomplete".into());
            }
            Ok(entries)
        })();
        unsafe {
            CloseHandle(snapshot);
        }
        result
    }
    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetExtendedTcpTable(
            table: *mut c_void,
            size: *mut u32,
            order: i32,
            family: u32,
            class: u32,
            reserved: u32,
        ) -> u32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> HANDLE;
        fn ReleaseMutex(handle: HANDLE) -> i32;
        fn WaitForSingleObject(handle: HANDLE, timeout: u32) -> u32;
    }
    /// npm and its descendants run in a kill-on-close job.
    pub struct NpmTree(crate::backend::windows_cli_lifecycle::CliProcessTree);
    impl NpmTree {
        pub fn spawn(command: std::process::Command) -> Result<Self, String> {
            crate::backend::windows_cli_lifecycle::CliProcessTree::spawn_task(
                command,
                Duration::from_secs(30),
            )
            .map(Self)
        }
        pub fn child(&mut self) -> &mut std::process::Child {
            self.0.child_mut()
        }
        pub fn stop(&mut self) {
            let _ = self.0.stop_task();
        }
    }
    pub struct Gate(HANDLE);
    impl Drop for Gate {
        fn drop(&mut self) {
            unsafe {
                ReleaseMutex(self.0);
                CloseHandle(self.0);
            }
        }
    }
    pub fn launch_gate(context: &RuntimeContext) -> Result<Gate, String> {
        use sha2::{Digest, Sha256};
        let root = crate::subprocess::external_path(
            context.runtime_root().ok_or("runtime root unavailable")?,
        )?
        .replace('/', "\\")
        .to_lowercase();
        let hash = format!("{:x}", Sha256::digest(root.as_bytes())).to_uppercase();
        let name: Vec<u16> = format!("Global\\QQBot-StartAll-{hash}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // The established per-runtime name also serializes competing supervisor instances.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err("dependency launch mutex is unavailable".into());
        }
        match unsafe { WaitForSingleObject(handle, 0) } {
            0 | 128 => Ok(Gate(handle)),
            _ => {
                unsafe {
                    CloseHandle(handle);
                }
                Err("a start operation for this runtime is already running".into())
            }
        }
    }
    pub fn listeners(port: u16) -> Result<Vec<u32>, String> {
        let mut pids = std::collections::BTreeSet::new();
        for (family, stride, port_offset, pid_offset) in
            [(2, 24usize, 8usize, 20usize), (23, 56, 20, 52)]
        {
            let mut size = 0;
            let first =
                unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, family, 3, 0) }; // OWNER_PID_LISTENER
            if first != 122 && first != 0 {
                return Err("cannot inspect dependency listener ownership".into());
            }
            let mut table = Vec::<u32>::new();
            let mut complete = false;
            for _ in 0..4 {
                if size > 32 * 1024 * 1024 {
                    break;
                }
                table.resize((size as usize + 3) / 4, 0);
                let status = unsafe {
                    GetExtendedTcpTable(table.as_mut_ptr() as _, &mut size, 0, family, 3, 0)
                };
                if status == 0 {
                    complete = true;
                    break;
                }
                if status != 122 {
                    break;
                }
            }
            if !complete || table.is_empty() {
                return Err("cannot inspect dependency listener ownership".into());
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(table.as_ptr() as *const u8, table.len() * 4) };
            let count = table[0] as usize;
            if count > (bytes.len().saturating_sub(4)) / stride {
                return Err("dependency listener table is incomplete".into());
            }
            for i in 0..count {
                let row = &bytes[4 + i * stride..4 + (i + 1) * stride];
                let actual = u16::from_be_bytes([row[port_offset], row[port_offset + 1]]);
                if actual == port {
                    pids.insert(u32::from_ne_bytes(
                        row[pid_offset..pid_offset + 4].try_into().unwrap(),
                    ));
                }
            }
        }
        Ok(pids.into_iter().collect())
    }
}

#[cfg(unix)]
mod os {
    pub(super) use super::relay::unix_private as private;
    use super::*;
    /// npm runs as its own process group so a stop reaches its descendants.
    pub struct NpmTree(std::process::Child);
    impl NpmTree {
        pub fn spawn(mut command: std::process::Command) -> Result<Self, String> {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            command
                .spawn()
                .map(Self)
                .map_err(|e| format!("npm could not start: {}", e.kind()))
        }
        pub fn child(&mut self) -> &mut std::process::Child {
            &mut self.0
        }
        pub fn stop(&mut self) {
            // The unreaped leader keeps its PID, so the group id cannot have been reused.
            if let Ok(pid) = i32::try_from(self.0.id()) {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    pub fn processes() -> Result<Vec<Entry>, String> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir("/proc")
            .map_err(|_| "dependency process table is unavailable")?
            .flatten()
        {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(end) = stat.rfind(')') else { continue };
            let Some(parent) = stat[end + 1..]
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
            else {
                continue;
            };
            let name = std::fs::read_link(entry.path().join("exe"))
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .or_else(|| {
                    std::fs::read_to_string(entry.path().join("comm"))
                        .ok()
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_default();
            entries.push(Entry { pid, parent, name });
        }
        Ok(entries)
    }
    pub fn launch_gate(context: &RuntimeContext) -> Result<std::fs::File, String> {
        let path = context
            .resolve_path(
                RuntimeArea::Data,
                "lifecycle/dependencies.lock",
                PathIntent::WriteFile,
            )
            .map_err(str::to_string)?;
        lifecycle::lock_with_wait(&path, Duration::ZERO)
    }
    fn listening_inodes(port: u16) -> Result<std::collections::BTreeSet<String>, String> {
        let mut inodes = std::collections::BTreeSet::new();
        for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let text = std::fs::read_to_string(file)
                .map_err(|_| "cannot inspect dependency listener ownership")?;
            for row in text.lines().skip(1) {
                let fields: Vec<_> = row.split_whitespace().collect();
                if fields.len() > 9
                    && fields[3] == "0A"
                    && fields[1]
                        .rsplit(':')
                        .next()
                        .and_then(|s| u16::from_str_radix(s, 16).ok())
                        == Some(port)
                {
                    inodes.insert(format!("socket:[{}]", fields[9]));
                }
            }
        }
        Ok(inodes)
    }

    fn listener_owners(inodes: &std::collections::BTreeSet<String>) -> Result<Vec<u32>, String> {
        let mut pids = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir("/proc")
            .map_err(|_| "cannot inspect process table")?
            .flatten()
        {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                if std::fs::read_link(fd.path())
                    .ok()
                    .is_some_and(|p| inodes.contains(&p.to_string_lossy().to_string()))
                {
                    pids.insert(pid);
                }
            }
        }
        Ok(pids.into_iter().collect())
    }

    pub fn listeners(port: u16) -> Result<Vec<u32>, String> {
        listeners_with(port, listener_owners)
    }

    fn listeners_with(
        port: u16,
        owners: impl FnOnce(&std::collections::BTreeSet<String>) -> Result<Vec<u32>, String>,
    ) -> Result<Vec<u32>, String> {
        let inodes = listening_inodes(port)?;
        if inodes.is_empty() {
            return Ok(Vec::new());
        }
        let pids = owners(&inodes)?;
        if pids.is_empty() {
            // The socket may close while /proc is scanned. Only a fresh absence proves
            // the port free; a current or replacement unmapped listener stays unknown.
            if listening_inodes(port)?.is_empty() {
                return Ok(Vec::new());
            }
            return Err("dependency port is taken by an unknown process; left alone".into());
        }
        Ok(pids)
    }

    #[cfg(all(test, target_os = "linux"))]
    mod native_tests {
        use super::*;
        use std::net::TcpListener;

        #[test]
        fn a_listener_that_disappears_during_owner_inspection_is_absent() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let result = listeners_with(port, |inodes| {
                assert!(
                    !inodes.is_empty(),
                    "the real listener must be in the TCP snapshot"
                );
                drop(listener);
                Ok(Vec::new())
            });
            assert_eq!(result.unwrap(), Vec::<u32>::new());
        }

        #[test]
        fn an_unmapped_live_listener_is_still_refused() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            assert!(listeners_with(port, |_| Ok(Vec::new())).is_err());
            assert!(!listening_inodes(port).unwrap().is_empty());
        }

        #[test]
        fn a_replaced_listener_without_an_owner_is_still_refused() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let mut replacement = None;
            let result = listeners_with(address.port(), |_| {
                drop(listener);
                replacement = Some(TcpListener::bind(address).unwrap());
                Ok(Vec::new())
            });
            assert!(result.is_err());
            assert!(!listening_inodes(address.port()).unwrap().is_empty());
        }
    }
}
