//! Explicit dependency approvals, discovery and side-by-side upgrade transactions.
//! Process operations use the dependency supervisor's ownership and launch gates.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;

const FILE: &str = "lifecycle/dependency-upgrades.json";
const LOCK: &str = "locks/dependency-upgrades.lock";
const SNOWLUMA_RELEASES: &str = "https://api.github.com/repos/SnowLuma/SnowLuma/releases";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    version: String,
    source: String,
    configuration: Value,
    directory: Value,
    current: String,
    archive: Option<String>,
    digest: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct State {
    checked_at: Option<i64>,
    check_id: String,
    configuration: Value,
    dependencies: Value,
    approval: Option<(String, Candidate)>,
    error: Option<String>,
    notified: Vec<String>,
    result: Value,
    alert: Option<PendingAlert>,
    transaction: Option<Transaction>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Phase {
    Staging,
    RetiringOld,
    Pinning,
    Starting,
    Judging,
    RetiringCandidate,
    RestoringPin,
    RestartingOld,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Transaction {
    name: String,
    candidate: Candidate,
    previous: Value,
    next: Value,
    desired: Option<lifecycle::DesiredRecord>,
    phase: Phase,
    identity: Option<ProcessIdentity>,
    reason: Option<String>,
    alert_scheduled: bool,
}
fn checkpoint(context: &RuntimeContext, transaction: &Transaction) -> Result<(), String> {
    let _gate = gate(context)?;
    let mut state = read(context)?;
    state.transaction = Some(transaction.clone());
    save(context, &state)
}
fn gate(context: &RuntimeContext) -> Result<fs::File, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match context.open_locked_file(RuntimeArea::Data, LOCK) {
            Ok(gate) => return Ok(gate),
            Err(error)
                if error == "runtime file is already locked" && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    }
}
fn read(context: &RuntimeContext) -> Result<State, String> {
    context
        .read_optional_text(RuntimeArea::Data, FILE)
        .map_err(str::to_string)?
        .map(|text| {
            serde_json::from_str(&text).map_err(|_| "dependency upgrade state is invalid".into())
        })
        .unwrap_or_else(|| Ok(State::default()))
}
fn save(context: &RuntimeContext, state: &State) -> Result<(), String> {
    context.write_atomic(
        RuntimeArea::Data,
        FILE,
        &serde_json::to_vec(state).map_err(|_| "dependency upgrade state cannot serialize")?,
    )?;
    sync_parent(context, RuntimeArea::Data, FILE)
}
fn sync_parent(context: &RuntimeContext, area: RuntimeArea, file: &str) -> Result<(), String> {
    // Windows atomic replacement already uses MOVEFILE_WRITE_THROUGH.
    #[cfg(unix)]
    {
        let path = context
            .resolve_path(area, file, PathIntent::ReadFile)
            .map_err(str::to_string)?;
        fs::File::open(path.parent().ok_or("dependency state path has no parent")?)
            .and_then(|parent| parent.sync_all())
            .map_err(|_| "dependency state directory could not be synced")?;
    }
    let _ = (context, area, file);
    Ok(())
}
fn name(raw: &str) -> Result<&'static str, String> {
    match raw {
        "snowluma" => Ok("snowluma"),
        "dsh" => Ok("dsh"),
        "relay" => Ok("relay"),
        _ => Err("unknown dependency".into()),
    }
}
fn source(cfg: &Value, name: &str) -> Result<Option<String>, String> {
    let raw = match name {
        "dsh" => {
            let registry = cfg["dependencies"]["dsh"]["registry"]
                .as_str()
                .unwrap_or("https://registry.npmjs.org/");
            crate::release::validate_registry_url(registry)?;
            let mut url =
                url::Url::parse(registry).map_err(|_| "dependencies.dsh.registry is invalid")?;
            let path = format!("{}/@deepseek-ai%2fdsh", url.path().trim_end_matches('/'));
            url.set_path(&path);
            url.set_fragment(None);
            Some(url.to_string())
        }
        "snowluma" => Some(
            cfg["dependencies"][name]["feedUrl"]
                .as_str()
                .unwrap_or(SNOWLUMA_RELEASES)
                .to_string(),
        ),
        _ => None,
    };
    if name != "dsh" {
        if let Some(raw) = &raw {
            crate::release::validate_url(raw)?;
        }
    }
    Ok(raw)
}
fn exact(raw: &str) -> bool {
    pinned_version(&json!({"dependencies":{"dsh":{"version":raw}}}), "dsh").is_ok()
}
// Semver precedence, including numeric prerelease identifiers, without lossy integer parsing.
fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let number = |a: &str, b: &str| a.len().cmp(&b.len()).then_with(|| a.cmp(b));
    let (a_core, a_pre) = a
        .split_once('-')
        .map_or((a, None), |(core, pre)| (core, Some(pre)));
    let (b_core, b_pre) = b
        .split_once('-')
        .map_or((b, None), |(core, pre)| (core, Some(pre)));
    for (a, b) in a_core.split('.').zip(b_core.split('.')) {
        let order = number(a, b);
        if order != Ordering::Equal {
            return order;
        }
    }
    match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let a: Vec<_> = a.split('.').collect();
            let b: Vec<_> = b.split('.').collect();
            for (a, b) in a.iter().zip(&b) {
                let numeric = |s: &str| s.bytes().all(|c| c.is_ascii_digit());
                let order = match (numeric(a), numeric(b)) {
                    (true, true) => number(a, b),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    _ => a.cmp(b),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
            a.len().cmp(&b.len())
        }
    }
}
fn discover(cfg: &Value, name: &str, source: &str) -> Result<Value, String> {
    let bytes = if name == "dsh" {
        crate::release::fetch_registry(source, 8 * 1024 * 1024)?
    } else {
        crate::release::fetch(source, 8 * 1024 * 1024)?
    };
    let doc: Value =
        serde_json::from_slice(&bytes).map_err(|_| "dependency feed is invalid JSON")?;
    let current = pinned_version(cfg, name)?;
    let (version, release) = if name == "dsh" {
        let latest = doc["dist-tags"]["latest"]
            .as_str()
            .filter(|v| exact(v))
            .ok_or("npm metadata has no exact latest version")?;
        if !doc["versions"][latest].is_object() {
            return Err("npm metadata is missing the latest version".into());
        }
        (latest.to_string(), None)
    } else {
        let mut releases: Vec<_> = doc
            .as_array()
            .ok_or("SnowLuma feed must be a GitHub release array")?
            .iter()
            .filter_map(|release| {
                if release["draft"] != false || release["prerelease"] != false {
                    return None;
                }
                let version = release["tag_name"]
                    .as_str()
                    .map(|v| v.strip_prefix('v').unwrap_or(v))
                    .filter(|v| exact(v) && !v.contains('-'))?;
                Some((version.to_string(), release))
            })
            .collect();
        releases.sort_by(|a, b| compare(&b.0, &a.0));
        let Some((version, release)) = releases.into_iter().next() else {
            return Ok(json!({"latest":null}));
        };
        (version, Some(release))
    };
    let Some(current) = current.filter(|old| compare(&version, old).is_gt()) else {
        return Ok(json!({"latest":version,"candidate":null}));
    };
    // Only the newest stable release's installation metadata matters. Historical releases
    // may predate asset digests; an unusable newest release is still reported, never approved.
    let package = (|| -> Result<(Option<String>, Option<String>), String> {
        let Some(release) = release else {
            return Ok((None, None));
        };
        let assets: Vec<_> = release["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|asset| {
                asset["name"]
                    .as_str()
                    .is_some_and(|name| name == format!("SnowLuma-v{version}-win-x64.zip"))
            })
            .collect();
        if assets.len() != 1 {
            return Err("SnowLuma release needs one full Windows x64 ZIP asset".into());
        }
        let archive = assets[0]["browser_download_url"]
            .as_str()
            .ok_or("SnowLuma archive URL is missing")?;
        crate::release::validate_url(archive)?;
        let digest = assets[0]["digest"]
            .as_str()
            .and_then(|v| v.strip_prefix("sha256:"))
            .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("SnowLuma ZIP needs a sha256 asset digest")?;
        Ok((Some(archive.into()), Some(digest.to_ascii_lowercase())))
    })();
    let (archive, digest) = match package {
        Ok(package) => package,
        Err(error) => return Ok(json!({"latest":version,"candidate":null,"error":error})),
    };
    let candidate = Candidate {
        version: version.clone(),
        source: source.into(),
        configuration: cfg["dependencies"][name].clone(),
        directory: cfg["dependencies"]["directory"].clone(),
        current,
        archive,
        digest,
    };
    Ok(json!({"latest":version,"candidate":candidate}))
}

pub fn status(context: &RuntimeContext, cfg: &Value) -> Result<Value, String> {
    let state = read(context)?;
    let mut dependencies = json!({});
    for name in ["snowluma", "dsh", "relay"] {
        let source = source(cfg, name)?;
        let current = pinned_version(cfg, name)?;
        let matches = state.configuration == cfg["dependencies"]
            && crate::release::settings(cfg)?["policy"] != "off";
        let found = if matches {
            state.dependencies[name].clone()
        } else {
            Value::Null
        };
        dependencies[name] = json!({"current":current,"source":source,"sourceUnavailable":if source.is_none(){Some("No discoverable source is defined for relay; its pinned folder is administrator-installed.")}else{None},"latest":found["latest"],"available":found["candidate"]["version"],"error":found["error"]});
    }
    Ok(
        json!({"ok":true,"dependencies":dependencies,"approval":state.approval.as_ref().map(|(name,c)|json!({"name":name,"current":c.current,"version":c.version})),"lastCheckedAt":state.checked_at,"updateError":state.error,"result":state.result}),
    )
}
pub fn check(context: &RuntimeContext, cfg: &Value) -> Result<Value, String> {
    if crate::release::settings(cfg)?["policy"] == "off" {
        return status(context, cfg);
    }
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let check_id = format!(
        "{}-{}-{}",
        std::process::id(),
        crate::update::wall_clock_ms(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    {
        let _gate = gate(context)?;
        let mut state = read(context)?;
        state.checked_at = Some(crate::parity_clock::now_ms());
        state.check_id = check_id.clone();
        if state.configuration != cfg["dependencies"] {
            state.dependencies = json!({});
        }
        state.configuration = cfg["dependencies"].clone();
        save(context, &state)?;
    }
    // No state/config/launch gate spans network I/O. Slow feeds cannot hold dependency recovery,
    // approvals or a transaction's final result publication.
    let mut dependencies = json!({});
    for name in ["snowluma", "dsh", "relay"] {
        if cfg["dependencies"][name].is_null() {
            continue;
        }
        dependencies[name] =
            match source(cfg, name).and_then(|s| s.map(|s| discover(cfg, name, &s)).transpose()) {
                Ok(Some(found)) => found,
                Ok(None) => Value::Null,
                Err(error) => json!({"error":error}),
            };
    }
    let _gate = gate(context)?;
    let mut state = read(context)?;
    let latest = context.load_config().map_err(str::to_string)?;
    if latest["dependencies"] != cfg["dependencies"]
        || crate::release::settings(&latest)?["policy"] == "off"
    {
        return Err("dependency settings changed during discovery".into());
    }
    if state.check_id == check_id {
        state.dependencies = dependencies;
        save(context, &state)?;
    }
    status(context, &latest)
}

pub fn approve(
    context: &RuntimeContext,
    cfg: &Value,
    raw: &str,
    version: &str,
) -> Result<Value, String> {
    let name = name(raw)?;
    let installation_path = context
        .resolve_path(
            RuntimeArea::Data,
            "locks/update-restore.lock",
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    let _installation = crate::release_store::installation_gate(&installation_path)?;
    let _gate = gate(context)?;
    let mut state = read(context)?;
    if state.approval.is_some() || state.transaction.is_some() {
        return Err("a dependency upgrade is already pending".into());
    }
    let candidate: Candidate =
        serde_json::from_value(state.dependencies[name]["candidate"].clone()).map_err(|_| {
            "displayed dependency upgrade is no longer available; refresh before approving"
        })?;
    if candidate.version != version
        || candidate.configuration != cfg["dependencies"][name]
        || candidate.directory != cfg["dependencies"]["directory"]
        || source(cfg, name)?.as_deref() != Some(&candidate.source)
        || crate::release::settings(cfg)?["policy"] == "off"
    {
        return Err("displayed dependency upgrade changed; refresh before approving".into());
    }
    state.approval = Some((name.into(), candidate.clone()));
    state.error = None;
    save(context, &state)?;
    Ok(json!({"ok":true,"name":name,"current":candidate.current,"approved":version}))
}
fn poll(context: &RuntimeContext, installed: bool) -> Result<(), String> {
    let cfg = context.load_config().map_err(str::to_string)?;
    let state = read(context)?;
    if (installed || cfg.get("update").is_some())
        && cfg.get("dependencies").is_some()
        && crate::release::settings(&cfg)?["policy"] != "off"
        && (state.configuration != cfg["dependencies"]
            || state.checked_at.is_none_or(|at| {
                crate::parity_clock::now_ms().saturating_sub(at)
                    >= crate::release::CHECK_INTERVAL_MS
            }))
    {
        check(context, &cfg)?;
    }
    deliver_alert(context)?;
    notify_available(context)?;
    Ok(())
}

/// Discovery and notice I/O never run on the dependency reconciliation thread.
pub(super) fn start(context: RuntimeContext) {
    let installed = crate::install_root::InstallRoot::configured()
        .ok()
        .is_some_and(|install| {
            context
                .runtime_root()
                .is_some_and(|root| install.protects(root).unwrap_or(false))
        });
    std::thread::spawn(move || {
        let mut last_error = None;
        loop {
            let result = if wanted(&context) {
                poll(&context, installed)
            } else {
                deliver_alert(&context)
            };
            match result {
                Err(error) => {
                    if last_error.as_ref() != Some(&error) {
                        crate::safe_eprintln!("[dependency-upgrade] {error}");
                    }
                    last_error = Some(error);
                }
                Ok(()) => last_error = None,
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingAlert {
    text: String,
    key: String,
    retry_after: i64,
}
fn valid(cfg: &Value, name: &str, candidate: &Candidate) -> Result<(), String> {
    if cfg["dependencies"][name] != candidate.configuration
        || cfg["dependencies"]["directory"] != candidate.directory
        || source(cfg, name)?.as_deref() != Some(candidate.source.as_str())
        || crate::release::settings(cfg)?["policy"] == "off"
    {
        return Err("approved dependency configuration changed before execution".into());
    }
    Ok(())
}
fn existing(
    context: &RuntimeContext,
    cfg: &Value,
    name: &'static str,
) -> Result<Dependency, String> {
    let mut dependency = configuration(context, cfg, name)?;
    if let Some(install) = &dependency.install {
        guard_install(install)?;
        dependency.script = Some(package_entry(
            &install.folder.join("node_modules").join(DSH_PACKAGE),
            &install.version,
        )?);
    }
    Ok(dependency)
}
fn stop_selected(
    context: &RuntimeContext,
    dependency: &Dependency,
    desired: Option<&lifecycle::DesiredRead>,
) -> Result<(), String> {
    let path = context
        .resolve_path(
            RuntimeArea::Data,
            "lifecycle/lifecycle.lock",
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    let _desired = lifecycle::lock_with_wait(&path, Duration::from_secs(5))?;
    let current = DesiredStore::new(context.data_dir().join("lifecycle")).read();
    if matches!(current, lifecycle::DesiredRead::Unreadable(_))
        || desired.is_some_and(|expected| !same_desired(context, expected))
    {
        return Err("desired state changed or is unreadable during dependency upgrade".into());
    }
    // A listener never grants authority. Refuse all ambiguous observations before the first stop.
    let listener = holder(dependency)?;
    let scan = matching(dependency)?;
    if scan.pending {
        return Err("unresolved dependency wrapper; upgrade stop refused".into());
    }
    let mut found = scan.found;
    if let Some(listener) = listener {
        if !found.iter().any(|p| identity::same_identity(p, &listener)) {
            found.push(listener);
        }
    }
    for instance in &found {
        let verified = inspected(instance.pid, dependency)?;
        if !identity::same_identity(instance, &verified) {
            return Err("dependency identity changed before upgrade stop".into());
        }
    }
    for instance in found {
        match identity::kill_verified(&instance) {
            identity::KillOutcome::Killed | identity::KillOutcome::Gone => {}
            _ => return Err("verified dependency could not be stopped for upgrade".into()),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match liveness(&instance) {
                identity::Liveness::Gone | identity::Liveness::Mismatch(_) => break,
                identity::Liveness::Unknown(_) => {
                    return Err("dependency exit is unknown; upgrade refused".into())
                }
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => return Err("dependency did not exit before upgrade deadline".into()),
            }
        }
    }
    Ok(())
}
fn change_pin(
    context: &RuntimeContext,
    name: &str,
    expected: &Value,
    replacement: &Value,
) -> Result<(), String> {
    let _gate = crate::console::acquire_social_state_gate(context).map_err(|(_, e)| e)?;
    let current = context.load_config().map_err(str::to_string)?;
    if &current["dependencies"][name] != expected {
        return Err("dependency configuration changed; pin replacement refused".into());
    }
    let text = context
        .read_optional_text(RuntimeArea::Config, "config.json")
        .map_err(str::to_string)?
        .ok_or("runtime config is unavailable")?;
    let mut raw: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| "runtime config is invalid")?;
    raw["dependencies"][name] = replacement.clone();
    context.write_atomic(
        RuntimeArea::Config,
        "config.json",
        &serde_json::to_vec_pretty(&raw).map_err(|_| "runtime config cannot serialize")?,
    )?;
    sync_parent(context, RuntimeArea::Config, "config.json")
}
fn ready_and_healthy(
    context: &RuntimeContext,
    dependency: &Dependency,
    states: &mut Value,
    clock: &Clock,
    desired: &lifecycle::DesiredRead,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if !same_desired(context, desired) {
            return Err("desired state changed before dependency health check".into());
        }
        if let Some(found) = holder(dependency)? {
            remember(
                states,
                dependency.name,
                "started",
                &found,
                clock.now(context),
            );
            healthy(states, dependency.name, clock.now(context));
            publish(context, states)?;
            let root = context
                .runtime_root()
                .ok_or("runtime root is unavailable")?;
            if crate::health_check::check(
                root,
                Instant::now() + crate::update::HEALTH_PROBE_ALLOWANCE,
            )
            .ok
            {
                return Ok(());
            }
            return Err("dependency upgrade failed the shared health check".into());
        }
        if let Ok(found) =
            serde_json::from_value::<ProcessIdentity>(states[dependency.name]["identity"].clone())
        {
            if matches!(
                liveness(&found),
                identity::Liveness::Gone | identity::Liveness::Mismatch(_)
            ) {
                return Err("new dependency exited before its health check".into());
            }
        }
        if Instant::now() >= deadline {
            return Err("dependency readiness timed out before its health check".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn plain(path: &Path) -> Result<fs::Metadata, String> {
    guard_destination(path)?;
    crate::release_store::directory(path.parent().ok_or("dependency path has no parent")?)?;
    let meta = fs::symlink_metadata(path).map_err(|_| "dependency files are unavailable")?;
    if redirected(&meta) || !(meta.is_file() || meta.is_dir()) {
        return Err("dependency files must not be redirected".into());
    }
    Ok(meta)
}
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let meta = plain(from)?;
    guard_destination(to)?;
    if meta.is_dir() {
        crate::release_store::directory(to)?;
        for entry in fs::read_dir(from).map_err(|_| "SnowLuma state cannot be read")? {
            let entry = entry.map_err(|_| "SnowLuma state cannot be read")?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        crate::release_store::directory(to.parent().ok_or("SnowLuma state path is invalid")?)?;
        if fs::symlink_metadata(to).is_ok() && !plain(to)?.is_file() {
            return Err("SnowLuma state destination is not a regular file".into());
        }
        fs::copy(from, to)
            .map_err(|_| "SnowLuma config/data cannot be carried to the new version")?;
    }
    Ok(())
}
fn install_snow(
    context: &RuntimeContext,
    cfg: &Value,
    candidate: &Candidate,
    old: &Dependency,
) -> Result<(Value, PathBuf), String> {
    let folder = dependency_directory(context, cfg)?
        .join("snowluma")
        .join(&candidate.version);
    guard_destination(&folder)?;
    if fs::symlink_metadata(&folder).is_ok() {
        return Err("SnowLuma version folder is occupied; no installed version was changed".into());
    }
    let parent = folder.parent().ok_or("SnowLuma version path is invalid")?;
    crate::release_store::directory(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let staging = parent.join(format!(".upgrade-{}-{nonce}", std::process::id()));
    guard_destination(&staging)?;
    fs::create_dir(&staging).map_err(|_| "SnowLuma staging cannot be created")?;
    let result = (|| {
        os::private(
            &staging,
            &fs::metadata(&staging).map_err(|_| "SnowLuma staging is unavailable")?,
        )?;
        let bytes = crate::release::fetch(
            candidate
                .archive
                .as_deref()
                .ok_or("approved SnowLuma archive is missing")?,
            256 * 1024 * 1024,
        )?;
        if Some(format!("{:x}", Sha256::digest(&bytes)).as_str()) != candidate.digest.as_deref() {
            return Err("SnowLuma archive checksum mismatch".into());
        }
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|_| "SnowLuma ZIP is invalid")?;
        if archive.len() > 100_000 {
            return Err("SnowLuma ZIP has too many entries".into());
        }
        let mut total = 0u64;
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|_| "SnowLuma ZIP entry is invalid")?;
            let relative = entry
                .enclosed_name()
                .ok_or("SnowLuma ZIP entry leaves its version folder")?
                .to_path_buf();
            let spelling = relative.to_string_lossy();
            if spelling.contains(['\\', ':'])
                || !seen.insert(spelling.to_lowercase())
                || entry
                    .unix_mode()
                    .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000))
            {
                return Err("SnowLuma ZIP contains redirected or duplicate entries".into());
            }
            total = total
                .checked_add(entry.size())
                .filter(|n| *n <= 2 * 1024 * 1024 * 1024)
                .ok_or("SnowLuma ZIP exceeds its expanded size limit")?;
            let path = staging.join(relative);
            guard_destination(&path)?;
            if entry.is_dir() {
                crate::release_store::directory(&path)?;
            } else {
                crate::release_store::directory(
                    path.parent().ok_or("SnowLuma ZIP path is invalid")?,
                )?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|_| "SnowLuma ZIP file cannot be created")?;
                std::io::copy(&mut entry, &mut file)
                    .map_err(|_| "SnowLuma ZIP cannot be extracted")?;
                file.sync_all()
                    .map_err(|_| "SnowLuma ZIP file cannot be synced")?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // Preserve only owner execution from the archive; config/data stay private.
                    let mode = 0o600 | (entry.unix_mode().unwrap_or(0) & 0o100);
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                        .map_err(|_| "SnowLuma ZIP executable permissions cannot be applied")?;
                }
            }
        }
        let relative = old
            .script()?
            .strip_prefix(&old.cwd)
            .map_err(|_| "SnowLuma entry must be inside its working directory for upgrade")?;
        if !plain(&staging.join(relative))?.is_file() {
            return Err("SnowLuma ZIP does not contain its configured entry".into());
        }
        relay::restrict(&staging)?;
        guard_destination(&folder)?;
        fs::rename(&staging, &folder).map_err(|_| "SnowLuma version cannot be published")?;
        let mut next = cfg["dependencies"]["snowluma"].clone();
        next["version"] = json!(candidate.version);
        next["script"] = json!(folder.join(relative));
        next["workingDirectory"] = json!(folder);
        // Node is an independently configured interpreter; keep its exact program path.
        Ok((next, folder.clone()))
    })();
    if staging.exists() && guard_destination(&staging).is_ok() {
        let _ = fs::remove_dir_all(staging);
    }
    result
}
// Parity builds expose a confined, bounded pause for seam-1 supervisor crash tests.
fn transaction_barrier(context: &RuntimeContext, point: &str) -> Result<(), String> {
    #[cfg(feature = "parity-clock")]
    {
        let control = "lifecycle/dependency-upgrade-pause";
        if context
            .read_optional_text(RuntimeArea::Data, control)
            .map_err(str::to_string)?
            .as_deref()
            == Some(point)
        {
            context.write_atomic(
                RuntimeArea::Data,
                "lifecycle/dependency-upgrade-reached",
                point.as_bytes(),
            )?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while context
                .read_optional_text(RuntimeArea::Data, control)
                .map_err(str::to_string)?
                .as_deref()
                == Some(point)
            {
                if Instant::now() >= deadline {
                    return Err("dependency upgrade fixture pause timed out".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    let _ = (context, point);
    Ok(())
}
fn transaction(
    context: &RuntimeContext,
    name: &'static str,
    candidate: &Candidate,
    clock: &Clock,
) -> Result<Value, String> {
    let cfg = context.load_config().map_err(str::to_string)?;
    valid(&cfg, name, candidate)?;
    let desired = DesiredStore::new(context.data_dir().join("lifecycle")).read();
    if !same_desired(context, &desired) {
        return Err("desired state does not permit dependency upgrade".into());
    }
    let old = existing(context, &cfg, name)?;
    let previous = read_states(context)?;
    let mut states = previous.clone();
    let mut next_cfg = cfg.clone();
    let snow_folder = if name == "snowluma" {
        let (next, folder) = install_snow(context, &cfg, candidate, &old)?;
        next_cfg["dependencies"][name] = next;
        Some(folder)
    } else if name == "dsh" {
        next_cfg["dependencies"][name]["version"] = json!(candidate.version);
        let next = next_cfg["dependencies"][name]
            .as_object_mut()
            .ok_or("DSH configuration is invalid")?;
        next.remove("script");
        next.remove("workingDirectory");
        None
    } else {
        return Err("this dependency has no upgrade installer".into());
    };
    // Stage before stopping anything. DSH's installer publishes its own progress; restore the
    // running process snapshot after staging, including when npm refuses or exits with bad output.
    let prepared = prepare(context, &next_cfg, name, &mut states);
    publish(context, &previous)?;
    let next = prepared?.ok_or("dependency staging was deferred or cancelled")?;
    let mut recovery = read(context)?
        .transaction
        .ok_or("dependency transaction is missing")?;
    recovery.next = next_cfg["dependencies"][name].clone();
    recovery.phase = Phase::RetiringOld;
    checkpoint(context, &recovery)?;
    let result = (|| {
        let latest = context.load_config().map_err(str::to_string)?;
        valid(&latest, name, candidate)?;
        stop_selected(context, &old, Some(&desired))?;
        if let Some(folder) = &snow_folder {
            for area in ["config", "data"] {
                let from = old.cwd.join(area);
                match fs::symlink_metadata(&from) {
                    Ok(_) => copy_tree(&from, &folder.join(area))?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("SnowLuma config/data cannot be inspected".into()),
                }
            }
            relay::restrict(folder)?;
        }
        recovery.phase = Phase::Pinning;
        checkpoint(context, &recovery)?;
        change_pin(
            context,
            name,
            &cfg["dependencies"][name],
            &next_cfg["dependencies"][name],
        )?;
        transaction_barrier(context, "pin-changed")?;
        states = previous.clone();
        states[name] = json!({"state":"pending"});
        publish(context, &states)?;
        recovery.phase = Phase::Starting;
        checkpoint(context, &recovery)?;
        ensure(context, &next, &mut states, clock, &desired)?;
        recovery.phase = Phase::Judging;
        recovery.identity = serde_json::from_value(states[name]["identity"].clone()).ok();
        checkpoint(context, &recovery)?;
        transaction_barrier(context, "candidate-started")?;
        ready_and_healthy(context, &next, &mut states, clock, &desired)
    })();
    match result {
        Ok(()) => Ok(
            json!({"name":name,"from":candidate.current,"to":candidate.version,"status":"succeeded","rollback":"not-needed"}),
        ),
        Err(reason) => {
            recovery.reason = Some(reason.clone());
            checkpoint(context, &recovery)?;
            let rollback = rollback(context, &mut recovery, clock);
            Ok(
                json!({"name":name,"from":candidate.current,"to":candidate.version,"status":if rollback.is_ok(){"rolled-back"}else{"failed"},"rollback":if rollback.is_ok(){"succeeded"}else{"failed"},"reason":reason,"rollbackError":rollback.err()}),
            )
        }
    }
}

fn rollback(
    context: &RuntimeContext,
    transaction: &mut Transaction,
    clock: &Clock,
) -> Result<(), String> {
    let name = name(&transaction.name)?;
    let cfg = context.load_config().map_err(str::to_string)?;
    let pin = &cfg["dependencies"][name];
    if pin != &transaction.previous && pin != &transaction.next {
        return Err(
            "dependency configuration changed; recovery requires administrator resolution".into(),
        );
    }
    if !matches!(
        transaction.phase,
        Phase::RestoringPin | Phase::RestartingOld
    ) {
        transaction.phase = Phase::RetiringCandidate;
        checkpoint(context, transaction)?;
        if pin == &transaction.next {
            let mut next_cfg = cfg.clone();
            next_cfg["dependencies"][name] = transaction.next.clone();
            next_cfg["dependencies"]["directory"] = transaction.candidate.directory.clone();
            let next = existing(context, &next_cfg, name)?;
            stop_selected(context, &next, None)?;
        }
        transaction.phase = Phase::RestoringPin;
        checkpoint(context, transaction)?;
    }
    if transaction.phase == Phase::RestoringPin {
        if pin == &transaction.next {
            change_pin(context, name, &transaction.next, &transaction.previous)?;
        }
        transaction.phase = Phase::RestartingOld;
        checkpoint(context, transaction)?;
    }
    let desired = DesiredStore::new(context.data_dir().join("lifecycle")).read();
    let mut states = read_states(context)?;
    states[name] = json!({"state":"pending"});
    publish(context, &states)?;
    match lifecycle::effective(&desired, crate::parity_clock::now_ms()) {
        Effective::Unknown(_) => {
            Err("desired state is unreadable; rollback startup deferred".into())
        }
        Effective::Act { wants, .. } if !wants.jobs => {
            states[name] = json!({"state":"stopped"});
            publish(context, &states)
        }
        _ => {
            let mut old_cfg = context.load_config().map_err(str::to_string)?;
            old_cfg["dependencies"][name] = transaction.previous.clone();
            old_cfg["dependencies"]["directory"] = transaction.candidate.directory.clone();
            let old = existing(context, &old_cfg, name)?;
            ensure(context, &old, &mut states, clock, &desired)?;
            ready_and_healthy(context, &old, &mut states, clock, &desired)
        }
    }
}
fn recover(
    context: &RuntimeContext,
    transaction: &mut Transaction,
    clock: &Clock,
) -> Result<Value, String> {
    let name = name(&transaction.name)?;
    if transaction.phase == Phase::Staging {
        return Ok(
            json!({"name":name,"from":transaction.candidate.current,"to":transaction.candidate.version,"status":"failed","rollback":"not-needed","reason":"dependency upgrade interrupted during staging"}),
        );
    }
    if transaction.reason.is_none() {
        let desired = transaction.desired.clone().map_or(
            lifecycle::DesiredRead::Absent,
            lifecycle::DesiredRead::Present,
        );
        let judgment = (|| {
            if !matches!(transaction.phase, Phase::Starting | Phase::Judging)
                || !same_desired(context, &desired)
            {
                return Err("dependency upgrade interrupted before proven health".into());
            }
            let cfg = context.load_config().map_err(str::to_string)?;
            if cfg["dependencies"][name] != transaction.next {
                return Err("candidate pin changed before recovery judgment".into());
            }
            let next = existing(context, &cfg, name)?;
            let mut states = read_states(context)?;
            let scan = matching(&next)?;
            if holder(&next)?.is_none() && scan.found.is_empty() && !scan.pending {
                return Err("candidate exited before recovery health judgment".into());
            }
            ready_and_healthy(context, &next, &mut states, clock, &desired)
        })();
        match judgment {
            Ok(()) => {
                return Ok(
                    json!({"name":name,"from":transaction.candidate.current,"to":transaction.candidate.version,"status":"succeeded","rollback":"not-needed"}),
                )
            }
            Err(reason) => {
                transaction.reason = Some(reason);
                checkpoint(context, transaction)?;
            }
        }
    }
    rollback(context, transaction, clock)?;
    Ok(
        json!({"name":name,"from":transaction.candidate.current,"to":transaction.candidate.version,"status":"rolled-back","rollback":"succeeded","reason":transaction.reason}),
    )
}
pub(super) fn apply_pending(context: &RuntimeContext) -> Result<bool, String> {
    let state = read(context)?;
    if state.approval.is_none() && state.transaction.is_none() {
        return Ok(true);
    }
    let path = context
        .resolve_path(
            RuntimeArea::Data,
            "locks/update-restore.lock",
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    let Ok(_installation) = crate::release_store::installation_gate(&path) else {
        return Ok(false);
    };
    let _turn = match context.open_locked_file(RuntimeArea::Data, "locks/turn-update.lock") {
        Ok(gate) => gate,
        Err(e) if e == "runtime file is already locked" => return Ok(false),
        Err(e) => return Err(e),
    };
    let _launch = os::launch_gate(context)?;
    if let Some(mut transaction) = read(context)?.transaction {
        let result = recover(context, &mut transaction, &Clock::new());
        match result {
            Ok(result) => finish(
                context,
                &transaction.name,
                &transaction.candidate,
                result,
                true,
            )?,
            Err(reason) => {
                finish(
                    context,
                    &transaction.name,
                    &transaction.candidate,
                    json!({"name":transaction.name,"from":transaction.candidate.current,"to":transaction.candidate.version,"status":"failed","rollback":"failed","reason":transaction.reason,"rollbackError":reason}),
                    false,
                )?;
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if !wanted(context) {
        return Ok(true);
    }
    let (raw, candidate) = {
        let _gate = gate(context)?;
        let mut state = read(context)?;
        let Some(approval) = state.approval.take() else {
            return Ok(false);
        };
        let desired = DesiredStore::new(context.data_dir().join("lifecycle")).read();
        let desired = match desired {
            lifecycle::DesiredRead::Absent => None,
            lifecycle::DesiredRead::Present(record) => Some(record),
            lifecycle::DesiredRead::Unreadable(_) => {
                return Err("desired state is unreadable".into())
            }
        };
        state.transaction = Some(Transaction {
            name: approval.0.clone(),
            candidate: approval.1.clone(),
            previous: approval.1.configuration.clone(),
            next: Value::Null,
            desired,
            phase: Phase::Staging,
            identity: None,
            reason: None,
            alert_scheduled: false,
        });
        state.result = json!({"name":approval.0,"from":approval.1.current,"to":approval.1.version,"status":"running"});
        save(context, &state)?;
        approval
    };
    let result=transaction(context,name(&raw)?,&candidate,&Clock::new()).unwrap_or_else(|reason|json!({"name":raw,"from":candidate.current,"to":candidate.version,"status":"failed","rollback":"not-needed","reason":reason}));
    let pending = read(context)?.transaction.is_some_and(|tx| {
        tx.phase != Phase::Staging
            && result["status"] != "succeeded"
            && result["rollback"] != "succeeded"
    });
    finish(context, &raw, &candidate, result, !pending)?;
    Ok(!pending)
}
/// An unfinished transaction fences its selected dependency, while siblings can still heal.
pub(super) fn blocked(context: &RuntimeContext) -> Result<Option<&'static str>, String> {
    read(context)?
        .transaction
        .map(|tx| name(&tx.name))
        .transpose()
}
fn finish(
    context: &RuntimeContext,
    raw: &str,
    candidate: &Candidate,
    result: Value,
    complete: bool,
) -> Result<(), String> {
    let _gate = gate(context)?;
    let mut state = read(context)?;
    state.error = result["reason"].as_str().map(str::to_string);
    if result["status"] != "succeeded"
        && !state
            .transaction
            .as_ref()
            .is_some_and(|tx| tx.alert_scheduled)
    {
        if let Some(tx) = state.transaction.as_mut() {
            tx.alert_scheduled = true;
        }
        state.alert = Some(PendingAlert {
            text: format!(
                "依赖升级失败：{raw} {} → {}；回退状态：{}；原因：{}",
                candidate.current,
                candidate.version,
                result["rollback"].as_str().unwrap_or("unknown"),
                result["reason"].as_str().unwrap_or("unknown")
            ),
            key: format!(
                "dependency-upgrade-{raw}-{}",
                crate::update::wall_clock_ms()
            ),
            retry_after: 0,
        });
    }
    if complete {
        state.transaction = None;
    }
    state.result = result;
    save(context, &state)
}
fn deliver_alert(context: &RuntimeContext) -> Result<(), String> {
    let (text, key) = {
        let _gate = gate(context)?;
        let mut state = read(context)?;
        let Some(alert) = state.alert.as_mut() else {
            return Ok(());
        };
        let now = crate::parity_clock::now_ms();
        if now < alert.retry_after {
            return Ok(());
        }
        alert.retry_after = now.saturating_add(60_000);
        let copy = (alert.text.clone(), alert.key.clone());
        save(context, &state)?;
        copy
    };
    let cfg = context.load_config().map_err(str::to_string)?;
    if crate::admin_alert::notify_notice_wait(context, &cfg, &text, &key) {
        let _gate = gate(context)?;
        let mut state = read(context)?;
        if state.alert.as_ref().is_some_and(|a| a.key == key) {
            state.alert = None;
            save(context, &state)?;
        }
    }
    Ok(())
}
fn notify_available(context: &RuntimeContext) -> Result<(), String> {
    let cfg = context.load_config().map_err(str::to_string)?;
    if crate::admin_alert::owner_id(&cfg).is_none() {
        return Ok(());
    }
    if crate::release::settings(&cfg)?["policy"] == "off" {
        return Ok(());
    }
    let _gate = gate(context)?;
    let mut state = read(context)?;
    if state.configuration != cfg["dependencies"] {
        return Ok(());
    }
    for name in ["snowluma", "dsh"] {
        let Some(version) = state.dependencies[name]["candidate"]["version"].as_str() else {
            continue;
        };
        let key = format!("dependency-available-{name}-{version}");
        if state.notified.contains(&key) {
            continue;
        }
        let text = format!(
            "依赖更新可用：{name} {} → {version}；需要管理员通过 /update deps 或控制台批准。",
            pinned_version(&cfg, name)?.unwrap_or_default()
        );
        state.notified.push(key.clone());
        save(context, &state)?;
        if !crate::admin_alert::notify_notice(context, &cfg, &text, &key) {
            state.notified.retain(|found| found != &key);
            save(context, &state)?;
        }
    }
    Ok(())
}
