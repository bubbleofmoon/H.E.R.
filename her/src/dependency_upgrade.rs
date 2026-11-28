//! Explicit dependency approvals, discovery and side-by-side upgrade transactions.
//! Process operations use the dependency supervisor's ownership and launch gates.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;

const FILE: &str = "lifecycle/dependency-upgrades.json";
const LOCK: &str = "locks/dependency-upgrades.lock";
const MAX_LOCAL_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RELEASE_JSON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CHECKSUMS_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    version: String,
    source: String,
    configuration: Value,
    directory: Value,
    current: String,
    archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_archive: Option<String>,
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnowlumaPathUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home_dir: Option<(Value, Value)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launcher_path: Option<(Value, Value)>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snowluma_paths: Option<SnowlumaPathUpdate>,
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
    )
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
        "snowluma" => match cfg["dependencies"][name].get("feedUrl") {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) if raw.trim().is_empty() => None,
            Some(Value::String(raw)) => Some(raw.trim().to_string()),
            Some(_) => return Err("dependencies.snowluma.feedUrl must be a string".into()),
        },
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
        local_archive: None,
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
        let source_unavailable = if source.is_none() {
            Some(match name {
                "snowluma" => "Automatic SnowLuma release discovery is disabled because no feedUrl is configured; upgrade from a local verified archive with `her-cli dependencies upgrade snowluma`.",
                _ => "No discoverable source is defined for relay; its pinned folder is administrator-installed.",
            })
        } else {
            None
        };
        dependencies[name] = json!({"current":current,"source":source,"sourceUnavailable":source_unavailable,"latest":found["latest"],"available":found["candidate"]["version"],"error":found["error"]});
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
    if name == "dsh" {
        require_dsh_contract(version)?;
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
        .is_some_and(|install| install.protects(context.runtime_root()).unwrap_or(false));
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
    let base_mismatch = cfg["dependencies"][name] != candidate.configuration
        || cfg["dependencies"]["directory"] != candidate.directory;
    let feed_mismatch = candidate.local_archive.is_none()
        && (source(cfg, name)?.as_deref() != Some(candidate.source.as_str())
            || crate::release::settings(cfg)?["policy"] == "off");
    if base_mismatch || feed_mismatch {
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
    let scan = matching(dependency, Unreadable::Refuse)?;
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
    snowluma_paths: Option<&SnowlumaPathUpdate>,
    reverse: bool,
) -> Result<(), String> {
    let _gate = crate::conversation_state::acquire(context).map_err(|(_, e)| e)?;
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
    if let Some(paths) = snowluma_paths {
        for (field, pair) in [
            ("homeDir", paths.home_dir.as_ref()),
            ("launcherPath", paths.launcher_path.as_ref()),
        ] {
            let Some((old_val, new_val)) = pair else {
                continue;
            };
            let (expected_val, target_val) = if reverse {
                (new_val, old_val)
            } else {
                (old_val, new_val)
            };
            if raw.pointer(&format!("/snowluma/{field}")) == Some(expected_val) {
                if !raw["snowluma"].is_object() {
                    raw["snowluma"] = json!({});
                }
                raw["snowluma"][field] = target_val.clone();
            }
        }
    }
    context.write_atomic(
        RuntimeArea::Config,
        "config.json",
        &serde_json::to_vec_pretty(&raw).map_err(|_| "runtime config cannot serialize")?,
    )
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
            let root = context.runtime_root();
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
    if crate::guarded_file::is_link(&meta) || !(meta.is_file() || meta.is_dir()) {
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
        crate::guarded_fs::restrict_entry(
            &staging,
            &fs::metadata(&staging).map_err(|_| "SnowLuma staging is unavailable")?,
            crate::guarded_fs::AclPolicy::OwnerOnly,
        )
        .map_err(String::from)?;
        let bytes = if let Some(local) = candidate.local_archive.as_deref() {
            read_local_archive(Path::new(local))?
        } else {
            crate::release::fetch(
                candidate
                    .archive
                    .as_deref()
                    .ok_or("approved SnowLuma archive is missing")?,
                256 * 1024 * 1024,
            )?
        };
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
        let relative = strip_path_within(old.script()?, &old.cwd)
            .filter(|rel| !rel.as_os_str().is_empty())
            .ok_or("SnowLuma entry must be inside its working directory for upgrade")?;
        if !plain(&staging.join(&relative))?.is_file() {
            return Err("SnowLuma ZIP does not contain its configured entry".into());
        }
        let bundled_rel = if cfg!(windows) {
            "node.exe"
        } else {
            "bin/node"
        };
        let has_bundled_node = match fs::symlink_metadata(staging.join(bundled_rel)) {
            Ok(meta) if meta.is_file() && !crate::guarded_file::is_link(&meta) => true,
            Ok(_) => return Err("SnowLuma bundled node path is not a regular file".into()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err("SnowLuma bundled node cannot be inspected".into()),
        };
        relay::restrict(&staging)?;
        guard_destination(&folder)?;
        fs::rename(&staging, &folder).map_err(|_| "SnowLuma version cannot be published")?;
        let mut next = cfg["dependencies"]["snowluma"].clone();
        next["version"] = json!(candidate.version);
        next["script"] = json!(folder.join(relative));
        next["workingDirectory"] = json!(folder);
        if has_bundled_node {
            next["program"] = json!(crate::subprocess::external_path(folder.join(bundled_rel))?);
        }
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
    recovery.snowluma_paths = if let Some(folder) = &snow_folder {
        compute_snowluma_path_updates(&cfg, &old.cwd, folder)?
    } else {
        None
    };
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
            recovery.snowluma_paths.as_ref(),
            false,
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
            recovery.reason = Some(reason);
            checkpoint(context, &recovery)?;
            let rollback = rollback(context, &mut recovery, clock);
            Ok(rollback_result(&recovery, rollback))
        }
    }
}

// Some(reason) means the pin was restored, but the restored DSH version cannot run.
fn rollback(
    context: &RuntimeContext,
    transaction: &mut Transaction,
    clock: &Clock,
) -> Result<Option<String>, String> {
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
            change_pin(
                context,
                name,
                &transaction.next,
                &transaction.previous,
                transaction.snowluma_paths.as_ref(),
                true,
            )?;
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
            publish(context, &states)?;
            Ok(None)
        }
        _ => {
            let mut old_cfg = context.load_config().map_err(str::to_string)?;
            old_cfg["dependencies"][name] = transaction.previous.clone();
            old_cfg["dependencies"]["directory"] = transaction.candidate.directory.clone();
            if let Err(reason) = runnable_pin(&old_cfg, name) {
                return Ok(Some(reason));
            }
            let old = existing(context, &old_cfg, name)?;
            ensure(context, &old, &mut states, clock, &desired)?;
            ready_and_healthy(context, &old, &mut states, clock, &desired)?;
            Ok(None)
        }
    }
}

fn rollback_result(transaction: &Transaction, outcome: Result<Option<String>, String>) -> Value {
    let (status, rollback, error) = match outcome {
        Ok(None) => ("rolled-back", "succeeded", None),
        Ok(Some(reason)) => ("failed", "pin-restored", Some(reason)),
        Err(reason) => ("failed", "failed", Some(reason)),
    };
    json!({
        "name": transaction.name,
        "from": transaction.candidate.current,
        "to": transaction.candidate.version,
        "status": status,
        "rollback": rollback,
        "reason": transaction.reason,
        "rollbackError": error,
    })
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
            runnable_pin(&cfg, name)?;
            let next = existing(context, &cfg, name)?;
            let mut states = read_states(context)?;
            let scan = matching(&next, Unreadable::Refuse)?;
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
    let outcome = rollback(context, transaction, clock)?;
    Ok(rollback_result(transaction, Ok(outcome)))
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
            snowluma_paths: None,
        });
        state.result = json!({"name":approval.0,"from":approval.1.current,"to":approval.1.version,"status":"running"});
        save(context, &state)?;
        approval
    };
    let result=transaction(context,name(&raw)?,&candidate,&Clock::new()).unwrap_or_else(|reason|json!({"name":raw,"from":candidate.current,"to":candidate.version,"status":"failed","rollback":"not-needed","reason":reason}));
    let pending = read(context)?.transaction.is_some_and(|tx| {
        tx.phase != Phase::Staging
            && result["status"] != "succeeded"
            && !matches!(result["rollback"].as_str(), Some("succeeded" | "pin-restored"))
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

fn read_plain_bounded(path: &Path, max: u64, label: &str) -> Result<Vec<u8>, String> {
    crate::install_root::check_runtime(path, false)?;
    let meta = fs::symlink_metadata(path).map_err(|_| format!("{label} is unavailable"))?;
    if crate::guarded_file::is_link(&meta) || !meta.is_file() {
        return Err(format!("{label} must be a regular file"));
    }
    if meta.len() > max {
        return Err(format!("{label} exceeds its maximum allowed size"));
    }
    crate::guarded_file::read_bounded_no_follow(path, max, false)
        .map_err(|_| format!("{label} cannot be read"))?
        .ok_or_else(|| format!("{label} is unavailable"))
}

fn read_local_archive(path: &Path) -> Result<Vec<u8>, String> {
    read_plain_bounded(path, MAX_LOCAL_ARCHIVE_BYTES, "local SnowLuma archive")
}

fn strip_verbatim_and_slashes(path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let text = path.to_str()?.replace('/', "\\");
        let stripped = if text
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case(r"\\?\UNC\"))
        {
            format!(r"\\{}", &text[8..])
        } else {
            text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
        };
        Some(PathBuf::from(stripped))
    }
    #[cfg(not(windows))]
    {
        let _ = path.to_str()?;
        Some(path.to_path_buf())
    }
}

fn normalize_path_for_strip(path: &Path, resolve: bool) -> Option<PathBuf> {
    let cleaned = strip_verbatim_and_slashes(path)?;
    if !cleaned.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in cleaned.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop only a normal segment so `..` never erases the root/prefix.
                let can_pop = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if !can_pop {
                    return None;
                }
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if resolve {
                    match fs::symlink_metadata(&out) {
                        Ok(_) => out = strip_verbatim_and_slashes(&fs::canonicalize(&out).ok()?)?,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => return None,
                    }
                }
            }
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
        }
    }
    Some(out)
}

fn strip_clean_components(target: &Path, base: &Path) -> Option<PathBuf> {
    let mut t_comps = target.components();
    let mut b_comps = base.components();
    loop {
        match (b_comps.next(), t_comps.next()) {
            (None, Some(first_rest)) => {
                let mut rel = PathBuf::new();
                rel.push(first_rest.as_os_str());
                for comp in t_comps {
                    rel.push(comp.as_os_str());
                }
                return Some(rel);
            }
            (None, None) => return Some(PathBuf::new()),
            (Some(_), None) => return None,
            (Some(bc), Some(tc)) => {
                let equal = if cfg!(windows) {
                    bc.as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&tc.as_os_str().to_string_lossy())
                } else {
                    bc == tc
                };
                if !equal {
                    return None;
                }
            }
        }
    }
}

fn strip_path_within(target: &Path, base: &Path) -> Option<PathBuf> {
    let b_lex = normalize_path_for_strip(base, false)?;
    match fs::canonicalize(&b_lex) {
        Ok(b_canon) => {
            let b_canon = strip_verbatim_and_slashes(&b_canon)?;
            let t_canon = normalize_path_for_strip(target, true)?;
            strip_clean_components(&t_canon, &b_canon)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let t_lex = normalize_path_for_strip(target, false)?;
            strip_clean_components(&t_lex, &b_lex)
        }
        Err(_) => None,
    }
}

fn compute_snowluma_path_updates(
    cfg: &Value,
    old_cwd: &Path,
    new_cwd: &Path,
) -> Result<Option<SnowlumaPathUpdate>, String> {
    let Some(snowluma) = cfg.get("snowluma").filter(|v| v.is_object()) else {
        return Ok(None);
    };
    let rewrite_field = |field: &str| -> Result<Option<(Value, Value)>, String> {
        let Some(raw) = snowluma.get(field).and_then(Value::as_str) else {
            return Ok(None);
        };
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let Some(rel) = strip_path_within(Path::new(raw), old_cwd) else {
            return Ok(None);
        };
        let rebased = if rel.as_os_str().is_empty() {
            new_cwd.to_path_buf()
        } else {
            new_cwd.join(rel)
        };
        let external = crate::subprocess::external_path(&rebased)?;
        crate::install_root::check_runtime(Path::new(&external), false)?;
        Ok(Some((Value::String(raw.to_string()), json!(external))))
    };
    let update = SnowlumaPathUpdate {
        home_dir: rewrite_field("homeDir")?,
        launcher_path: rewrite_field("launcherPath")?,
    };
    if update.home_dir.is_none() && update.launcher_path.is_none() {
        Ok(None)
    } else {
        Ok(Some(update))
    }
}

fn resolve_from_dir(dir: &Path) -> Result<(PathBuf, String, String), String> {
    crate::install_root::check_runtime(dir, false)?;
    let dir_meta =
        fs::symlink_metadata(dir).map_err(|_| "SnowLuma release directory is unavailable")?;
    if crate::guarded_file::is_link(&dir_meta) || !dir_meta.is_dir() {
        return Err("SnowLuma release directory must be a plain directory".into());
    }
    let rel_bytes = read_plain_bounded(
        &dir.join("release.json"),
        MAX_RELEASE_JSON_BYTES,
        "SnowLuma release.json",
    )?;
    let doc: Value =
        serde_json::from_slice(&rel_bytes).map_err(|_| "SnowLuma release.json is invalid JSON")?;
    if !doc.is_object() {
        return Err("SnowLuma release.json must be a JSON object".into());
    }
    if doc["draft"] != false || doc["prerelease"] != false {
        return Err("SnowLuma release.json must describe a published stable release".into());
    }
    let version = doc["tag_name"]
        .as_str()
        .map(|v| v.strip_prefix('v').unwrap_or(v))
        .filter(|v| exact(v) && !v.contains('-'))
        .ok_or("SnowLuma release.json tag_name must be an exact stable semver")?
        .to_string();
    let zip_name = format!("SnowLuma-v{version}-win-x64.zip");
    let assets: Vec<_> = doc["assets"]
        .as_array()
        .ok_or("SnowLuma release.json assets must be an array")?
        .iter()
        .filter(|asset| asset["name"].as_str() == Some(zip_name.as_str()))
        .collect();
    if assets.len() != 1 {
        return Err("SnowLuma release.json needs one full Windows x64 ZIP asset".into());
    }
    let release_digest = assets[0]["digest"]
        .as_str()
        .and_then(|v| v.strip_prefix("sha256:"))
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("SnowLuma release.json asset needs a sha256 digest")?
        .to_ascii_lowercase();

    let sums_bytes = read_plain_bounded(
        &dir.join("checksums.sha256"),
        MAX_CHECKSUMS_BYTES,
        "SnowLuma checksums.sha256",
    )?;
    let sums_text = String::from_utf8(sums_bytes)
        .map_err(|_| "SnowLuma checksums.sha256 must be valid UTF-8")?;
    let mut matched = Vec::new();
    for line in sums_text.trim_start_matches('\u{feff}').lines() {
        if line.trim().is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() <= 64 || !bytes[..64].iter().all(|b| b.is_ascii_hexdigit()) {
            return Err("SnowLuma checksums.sha256 contains an invalid line".into());
        }
        let hex = &line[..64];
        let rest = &line[64..];
        if !rest.starts_with([' ', '\t']) {
            return Err("SnowLuma checksums.sha256 contains an invalid line".into());
        }
        let after_ws = rest.trim_start_matches([' ', '\t']);
        let file = after_ws.strip_prefix('*').unwrap_or(after_ws);
        if file.is_empty() || file.contains(['\0', '\r', '\n']) {
            return Err("SnowLuma checksums.sha256 contains an invalid line".into());
        }
        if file == zip_name {
            matched.push(hex.to_ascii_lowercase());
        }
    }
    if matched.len() != 1 {
        return Err("SnowLuma checksums.sha256 must contain one entry for the ZIP asset".into());
    }
    if matched[0] != release_digest {
        return Err("SnowLuma checksums.sha256 digest does not match release.json".into());
    }

    let zip_path = dir.join(&zip_name);
    crate::install_root::check_runtime(&zip_path, false)?;
    let zip_meta = fs::symlink_metadata(&zip_path)
        .map_err(|_| "SnowLuma ZIP archive in release directory is unavailable")?;
    if crate::guarded_file::is_link(&zip_meta) || !zip_meta.is_file() {
        return Err("SnowLuma ZIP archive must be a regular file".into());
    }
    Ok((zip_path, release_digest, version))
}

const CLI_USAGE: &str = "\
usage: her-cli dependencies upgrade snowluma --runtime-root <abs> --archive <zip> --sha256 <64-hex> --version <semver>
       her-cli dependencies upgrade snowluma --runtime-root <abs> --from-dir <dir>";

enum CliUpgradeSource {
    FromDir(PathBuf),
    Archive {
        archive: PathBuf,
        digest: String,
        version: String,
    },
}

fn resolve_cli_path(raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|_| "working directory is unavailable")?
            .join(path))
    }
}

fn retry_lock<T>(
    busy_errors: &[&str],
    mut acquire: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match acquire() {
            Ok(guard) => return Ok(guard),
            Err(err) if busy_errors.iter().any(|busy| err == *busy) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) => return Err(err),
        }
    }
}

fn parse_cli_args(args: &[String]) -> Result<Option<(PathBuf, CliUpgradeSource)>, String> {
    if matches!(args, [a] if a == "--help" || a == "-h")
        || matches!(args, [a, b] if a == "upgrade" && (b == "--help" || b == "-h"))
    {
        return Ok(None);
    }
    if args.len() < 2 || args[0] != "upgrade" || args[1] != "snowluma" {
        return Err(CLI_USAGE.into());
    }
    let mut runtime_root: Option<String> = None;
    let mut from_dir: Option<String> = None;
    let mut archive: Option<String> = None;
    let mut sha256: Option<String> = None;
    let mut version: Option<String> = None;
    let rest = &args[2..];
    let mut i = 0;
    while i < rest.len() {
        let take_value = |slot: &mut Option<String>, flag: &str, idx: &mut usize| -> Result<(), String> {
            if slot.is_some() {
                return Err(format!("duplicate {flag}"));
            }
            let Some(val) = rest.get(*idx + 1).filter(|v| !v.is_empty()) else {
                return Err(format!("missing value for {flag}"));
            };
            *slot = Some(val.clone());
            *idx += 1;
            Ok(())
        };
        match rest[i].as_str() {
            "--help" | "-h" => return Ok(None),
            "--runtime-root" => take_value(&mut runtime_root, "--runtime-root", &mut i)?,
            "--from-dir" => take_value(&mut from_dir, "--from-dir", &mut i)?,
            "--archive" => take_value(&mut archive, "--archive", &mut i)?,
            "--sha256" => take_value(&mut sha256, "--sha256", &mut i)?,
            "--version" => take_value(&mut version, "--version", &mut i)?,
            other => return Err(format!("unknown argument: {other}\n{CLI_USAGE}")),
        }
        i += 1;
    }
    let Some(root_raw) = runtime_root else {
        return Err(format!("missing --runtime-root\n{CLI_USAGE}"));
    };
    let root = PathBuf::from(&root_raw);
    if !root.is_absolute() {
        return Err("--runtime-root must be absolute".into());
    }
    let source = if let Some(dir_raw) = from_dir {
        if archive.is_some() || sha256.is_some() || version.is_some() {
            return Err("cannot combine --from-dir with --archive, --sha256, or --version".into());
        }
        CliUpgradeSource::FromDir(resolve_cli_path(&dir_raw)?)
    } else {
        let (Some(archive_raw), Some(sha_raw), Some(ver_raw)) = (archive, sha256, version) else {
            return Err(format!(
                "missing --archive, --sha256, or --version (or use --from-dir)\n{CLI_USAGE}"
            ));
        };
        let normalized_version = ver_raw.strip_prefix('v').unwrap_or(&ver_raw);
        if !exact(normalized_version) || normalized_version.contains('-') {
            return Err("--version must be an exact stable semver".into());
        }
        let normalized_sha = sha_raw.strip_prefix("sha256:").unwrap_or(&sha_raw);
        if normalized_sha.len() != 64 || !normalized_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("--sha256 must be 64 hexadecimal characters".into());
        }
        CliUpgradeSource::Archive {
            archive: resolve_cli_path(&archive_raw)?,
            digest: normalized_sha.to_ascii_lowercase(),
            version: normalized_version.to_string(),
        }
    };
    Ok(Some((root, source)))
}

fn execute_cli_upgrade(
    context: &RuntimeContext,
    source: CliUpgradeSource,
) -> Result<Value, String> {
    let (archive_path, digest, version) = match source {
        CliUpgradeSource::FromDir(dir) => resolve_from_dir(&dir)?,
        CliUpgradeSource::Archive {
            archive,
            digest,
            version,
        } => (archive, digest, version),
    };
    let bytes = read_local_archive(&archive_path)?;
    if format!("{:x}", Sha256::digest(&bytes)) != digest {
        return Err("local SnowLuma archive checksum mismatch".into());
    }
    drop(bytes);

    let cfg = context.load_config().map_err(str::to_string)?;
    if !cfg["dependencies"]["snowluma"].is_object() {
        return Err("dependencies.snowluma must be configured as an object".into());
    }
    let current = pinned_version(&cfg, "snowluma")?;
    if current.as_deref() == Some(version.as_str()) {
        return Err(format!("SnowLuma is already pinned to {version}"));
    }
    let _ = existing(context, &cfg, "snowluma")?;

    let installation_path = context
        .resolve_path(
            RuntimeArea::Data,
            "locks/update-restore.lock",
            PathIntent::WriteFile,
        )
        .map_err(str::to_string)?;
    let _installation = retry_lock(
        &["another release installation or restore is running"],
        || crate::release_store::installation_gate(&installation_path),
    )?;
    let _turn = retry_lock(&["runtime file is already locked"], || {
        context.open_locked_file(RuntimeArea::Data, "locks/turn-update.lock")
    })?;
    let _launch = retry_lock(
        &["a start operation for this runtime is already running", "gate is busy"],
        || os::launch_gate(context),
    )?;

    let cfg = context.load_config().map_err(str::to_string)?;
    if !cfg["dependencies"]["snowluma"].is_object() {
        return Err("dependencies.snowluma must be configured as an object".into());
    }
    let current = pinned_version(&cfg, "snowluma")?;
    if current.as_deref() == Some(version.as_str()) {
        return Err(format!("SnowLuma is already pinned to {version}"));
    }
    let _ = existing(context, &cfg, "snowluma")?;
    let external_archive = crate::subprocess::external_path(&archive_path)?;

    let desired = match DesiredStore::new(context.data_dir().join("lifecycle")).read() {
        lifecycle::DesiredRead::Absent => None,
        lifecycle::DesiredRead::Present(record) => Some(record),
        lifecycle::DesiredRead::Unreadable(_) => {
            return Err("desired state is unreadable".into());
        }
    };
    if !wanted(context) {
        return Err("desired state does not permit dependency upgrade".into());
    }

    let candidate = Candidate {
        version: version.clone(),
        source: "local-archive".into(),
        configuration: cfg["dependencies"]["snowluma"].clone(),
        directory: cfg["dependencies"]["directory"].clone(),
        current: current.unwrap_or_else(|| "unpinned".into()),
        archive: None,
        local_archive: Some(external_archive),
        digest: Some(digest),
    };
    {
        let _gate = gate(context)?;
        let mut state = read(context)?;
        if state.approval.is_some() || state.transaction.is_some() {
            return Err("a dependency upgrade is already pending".into());
        }
        state.transaction = Some(Transaction {
            name: "snowluma".into(),
            candidate: candidate.clone(),
            previous: cfg["dependencies"]["snowluma"].clone(),
            next: Value::Null,
            desired,
            phase: Phase::Staging,
            identity: None,
            reason: None,
            alert_scheduled: false,
            snowluma_paths: None,
        });
        state.result = json!({
            "name": "snowluma",
            "from": candidate.current,
            "to": candidate.version,
            "status": "running",
        });
        save(context, &state)?;
    }

    let result = transaction(context, "snowluma", &candidate, &Clock::new()).unwrap_or_else(
        |reason| {
            json!({
                "name": "snowluma",
                "from": candidate.current,
                "to": candidate.version,
                "status": "failed",
                "rollback": "not-needed",
                "reason": reason,
            })
        },
    );
    let pending = read(context)?.transaction.is_some_and(|tx| {
        tx.phase != Phase::Staging
            && result["status"] != "succeeded"
            && !matches!(result["rollback"].as_str(), Some("succeeded" | "pin-restored"))
    });
    finish(context, "snowluma", &candidate, result.clone(), !pending)?;
    Ok(result)
}

pub fn run_cli(args: &[String]) -> i32 {
    let (root, source) = match parse_cli_args(args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{CLI_USAGE}");
            return 0;
        }
        Err(err) => {
            eprintln!("[dependencies] {err}");
            return 2;
        }
    };
    if let Err(err) = crate::install_root::check_runtime(&root, false) {
        eprintln!("[dependencies] {err}");
        return 2;
    }
    let context = match RuntimeContext::from_root(&root) {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!("[dependencies] {err}");
            return 2;
        }
    };
    match execute_cli_upgrade(&context, source) {
        Ok(result) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
            );
            if result["status"] == "succeeded" {
                0
            } else {
                1
            }
        }
        Err(err) => {
            eprintln!("[dependencies] {err}");
            1
        }
    }
}

#[cfg(test)]
mod dependency_upgrade_tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_temp(tag: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "her462-{tag}-{}-{seq}",
            std::process::id()
        ))
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8], u32)]) -> String {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, bytes, mode) in entries {
            let options = zip::write::FileOptions::default().unix_permissions(*mode);
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
    }

    fn create_dir_link_or_junction(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            crate::subprocess::command_without_console("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|out| out.status.success())
        }
    }

    #[test]
    fn unset_null_and_empty_snowluma_feed_url_disable_discovery_without_http_errors() {
        let root = unique_temp("disabled-feed");
        let fixture = crate::runtime::tests::runtime_fixture(
            root,
            &json!({
                "consoleToken": "fixture-console",
                "update": {"policy": "notify"},
                "dependencies": {
                    "snowluma": {
                        "version": "1.14.22",
                        "program": "C:/node.exe",
                        "script": "C:/snow/index.mjs",
                        "workingDirectory": "C:/snow"
                    }
                }
            }),
            &json!({"onebot":"http://127.0.0.1:1","dsh":"http://127.0.0.1:2","model":"http://127.0.0.1:3"}),
        );
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        for feed_value in [None, Some(Value::Null), Some(json!("")), Some(json!("   "))] {
            let mut cfg = context.load_config().unwrap();
            match feed_value {
                None => {
                    cfg["dependencies"]["snowluma"]
                        .as_object_mut()
                        .unwrap()
                        .remove("feedUrl");
                }
                Some(v) => {
                    cfg["dependencies"]["snowluma"]["feedUrl"] = v;
                }
            }
            context
                .write_atomic(
                    RuntimeArea::Config,
                    "config.json",
                    &serde_json::to_vec(&cfg).unwrap(),
                )
                .unwrap();

            assert_eq!(source(&cfg, "snowluma").unwrap(), None);
            let st = status(&context, &cfg).unwrap();
            assert!(st["dependencies"]["snowluma"]["source"].is_null(), "{st}");
            assert_eq!(
                st["dependencies"]["snowluma"]["sourceUnavailable"],
                "Automatic SnowLuma release discovery is disabled because no feedUrl is configured; upgrade from a local verified archive with `her-cli dependencies upgrade snowluma`."
            );
            assert!(st["dependencies"]["snowluma"]["error"].is_null(), "{st}");

            let checked = check(&context, &cfg).unwrap();
            assert!(checked["dependencies"]["snowluma"]["source"].is_null(), "{checked}");
            assert!(checked["dependencies"]["snowluma"]["error"].is_null(), "{checked}");
            assert!(checked["dependencies"]["snowluma"]["available"].is_null(), "{checked}");
            assert!(checked["dependencies"]["snowluma"]["latest"].is_null(), "{checked}");
        }

        let mut cfg = context.load_config().unwrap();
        cfg["dependencies"]["snowluma"]["feedUrl"] = json!(" https://127.0.0.1:9999/releases ");
        assert_eq!(
            source(&cfg, "snowluma").unwrap().as_deref(),
            Some("https://127.0.0.1:9999/releases")
        );
        let st = status(&context, &cfg).unwrap();
        assert!(st["dependencies"]["snowluma"]["sourceUnavailable"].is_null());

        cfg["dependencies"]["snowluma"]["feedUrl"] = json!(123);
        assert!(source(&cfg, "snowluma").is_err());
    }

    #[test]
    fn strip_path_within_handles_exact_subdir_sibling_slashes_case_and_canonical_symlinks() {
        let base_root = crate::install_root::development_root(unique_temp("strip-within"));
        let old_cwd = base_root.join("deps/snowluma/1.14.22");
        let outside = base_root.join("outside");
        fs::create_dir_all(old_cwd.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();

        assert_eq!(
            strip_path_within(&old_cwd, &old_cwd),
            Some(PathBuf::new())
        );
        assert_eq!(
            strip_path_within(&old_cwd.join("data/home"), &old_cwd),
            Some(PathBuf::from("data/home"))
        );
        assert_eq!(
            strip_path_within(&old_cwd.join("./sub/../home"), &old_cwd),
            Some(PathBuf::from("home"))
        );

        let slash_target = PathBuf::from(old_cwd.join("home").to_string_lossy().replace('\\', "/"));
        assert_eq!(
            strip_path_within(&slash_target, &old_cwd),
            Some(PathBuf::from("home"))
        );

        let sibling = PathBuf::from(format!("{}-extra/home", old_cwd.display()));
        assert_eq!(strip_path_within(&sibling, &old_cwd), None);
        assert_eq!(strip_path_within(&outside.join("home"), &old_cwd), None);
        assert_eq!(strip_path_within(Path::new("relative/home"), &old_cwd), None);

        #[cfg(windows)]
        {
            let ext = crate::subprocess::external_path(&old_cwd).unwrap();
            let verbatim = PathBuf::from(format!(r"\\?\{ext}\Home"));
            assert_eq!(
                strip_path_within(&verbatim, &old_cwd),
                Some(PathBuf::from("Home"))
            );
            let upper_base = PathBuf::from(ext.to_ascii_uppercase().replace('\\', "/"));
            assert_eq!(
                strip_path_within(&old_cwd.join("Launcher.exe"), &upper_base),
                Some(PathBuf::from("Launcher.exe"))
            );
        }

        let escape_link = old_cwd.join("escape");
        if create_dir_link_or_junction(&escape_link, &outside) {
            assert_eq!(
                strip_path_within(&escape_link.join("secret"), &old_cwd),
                None,
                "symlink/junction escaping old_cwd must not match"
            );
        }

        // Pure lexical paths that do not exist on disk still resolve cleanly.
        let virtual_base = base_root.join("virtual/snow/1.14.22");
        assert_eq!(
            strip_path_within(&virtual_base.join("home"), &virtual_base),
            Some(PathBuf::from("home"))
        );
        assert_eq!(
            strip_path_within(&base_root.join("virtual/snow/1.14.22-other/home"), &virtual_base),
            None
        );

        let _ = fs::remove_dir_all(&base_root);
    }

    #[test]
    fn compute_snowluma_path_updates_rewrites_inside_old_cwd_only() {
        let base_root = crate::install_root::development_root(unique_temp("path-updates"));
        let old_cwd = base_root.join("deps/snowluma/1.14.22");
        let new_cwd = base_root.join("deps/snowluma/2.4.2");
        let sibling_cwd = base_root.join("deps/snowluma/1.14.22-extra");
        fs::create_dir_all(&old_cwd).unwrap();
        fs::create_dir_all(&new_cwd).unwrap();

        let cfg_both = json!({
            "snowluma": {
                "accessToken": "keep-token",
                "homeDir": old_cwd.join("home").to_string_lossy().replace('\\', "/"),
                "launcherPath": old_cwd.join("bin/SnowLuma.exe")
            }
        });
        let update = compute_snowluma_path_updates(&cfg_both, &old_cwd, &new_cwd)
            .unwrap()
            .expect("both paths inside old_cwd");
        assert_eq!(
            update.home_dir,
            Some((
                cfg_both["snowluma"]["homeDir"].clone(),
                json!(crate::subprocess::external_path(new_cwd.join("home")).unwrap()),
            ))
        );
        assert_eq!(
            update.launcher_path,
            Some((
                cfg_both["snowluma"]["launcherPath"].clone(),
                json!(crate::subprocess::external_path(new_cwd.join("bin").join("SnowLuma.exe")).unwrap()),
            ))
        );

        let cfg_partial = json!({
            "snowluma": {
                "homeDir": old_cwd,
                "launcherPath": sibling_cwd.join("SnowLuma.exe")
            }
        });
        let partial = compute_snowluma_path_updates(&cfg_partial, &old_cwd, &new_cwd)
            .unwrap()
            .expect("homeDir exact match inside old_cwd");
        assert_eq!(
            partial.home_dir,
            Some((
                cfg_partial["snowluma"]["homeDir"].clone(),
                json!(crate::subprocess::external_path(&new_cwd).unwrap()),
            ))
        );
        assert_eq!(partial.launcher_path, None);

        let cfg_outside = json!({
            "snowluma": {
                "homeDir": base_root.join("external/home"),
                "launcherPath": sibling_cwd.join("SnowLuma.exe")
            }
        });
        assert_eq!(
            compute_snowluma_path_updates(&cfg_outside, &old_cwd, &new_cwd).unwrap(),
            None
        );

        let _ = fs::remove_dir_all(&base_root);
    }

    #[test]
    fn resolve_from_dir_validates_release_json_checksums_and_plain_zip() {
        let dir = crate::install_root::development_root(unique_temp("from-dir"));
        fs::create_dir_all(&dir).unwrap();
        let zip_name = "SnowLuma-v2.4.2-win-x64.zip";
        let zip_path = dir.join(zip_name);
        let digest = write_zip(&zip_path, &[("index.mjs", b"console.log(1);", 0o644)]);

        let write_meta = |draft: bool, prerelease: bool, rel_digest: &str, sums: &str| {
            fs::write(
                dir.join("release.json"),
                json!({
                    "tag_name": "v2.4.2",
                    "draft": draft,
                    "prerelease": prerelease,
                    "assets": [
                        {
                            "name": "SnowLuma-v2.4.2-win-x64-lite.zip",
                            "digest": format!("sha256:{}", "a".repeat(64))
                        },
                        {
                            "name": zip_name,
                            "digest": format!("sha256:{rel_digest}")
                        }
                    ]
                })
                .to_string(),
            )
            .unwrap();
            fs::write(dir.join("checksums.sha256"), sums).unwrap();
        };

        let upper_digest = digest.to_ascii_uppercase();
        let valid_sums = format!(
            "{}  SnowLuma-v2.4.2-win-x64-lite.zip\n{upper_digest} *{zip_name}\n\n",
            "a".repeat(64)
        );
        write_meta(false, false, &upper_digest, &valid_sums);
        let (resolved_zip, resolved_digest, resolved_version) = resolve_from_dir(&dir).unwrap();
        assert_eq!(resolved_zip, zip_path);
        assert_eq!(resolved_digest, digest);
        assert_eq!(resolved_version, "2.4.2");

        // Draft and prerelease rejected.
        write_meta(true, false, &digest, &valid_sums);
        assert!(resolve_from_dir(&dir).is_err());
        write_meta(false, true, &digest, &valid_sums);
        assert!(resolve_from_dir(&dir).is_err());

        // Checksum mismatch between release.json and checksums.sha256 rejected.
        write_meta(
            false,
            false,
            &digest,
            &format!("{}  {zip_name}\n", "0".repeat(64)),
        );
        assert!(resolve_from_dir(&dir).is_err());

        // Malformed checksum line rejected.
        write_meta(false, false, &digest, &format!("not-hex  {zip_name}\n"));
        assert!(resolve_from_dir(&dir).is_err());

        // Missing zip rejected.
        write_meta(false, false, &digest, &valid_sums);
        fs::remove_file(&zip_path).unwrap();
        assert!(resolve_from_dir(&dir).is_err());

        // Non-regular zip (directory / junction) rejected.
        let outside = dir.parent().unwrap().join("from-dir-outside");
        fs::create_dir_all(&outside).unwrap();
        if !create_dir_link_or_junction(&zip_path, &outside) {
            fs::create_dir(&zip_path).unwrap();
        }
        assert!(resolve_from_dir(&dir).is_err());

        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_archive_valid_and_install_snow_bundled_node_rewrite() {
        let root = unique_temp("install-snow-local");
        let old_cwd = root.join("snow-old");
        let deps_dir = root.join("deps");
        let old_node = old_cwd.join(if cfg!(windows) { "node.exe" } else { "node" });
        let old_script = old_cwd.join("index.mjs");
        let snow_cfg = json!({
            "version": "1.14.22",
            "program": old_node,
            "script": old_script,
            "workingDirectory": old_cwd
        });
        let fixture = crate::runtime::tests::runtime_fixture(
            root.clone(),
            &json!({
                "consoleToken": "fixture-console",
                "update": {"policy": "off"},
                "snowluma": {
                    "accessToken": "tok",
                    "homeDir": old_cwd.join("home"),
                    "launcherPath": old_cwd.join("launcher.exe")
                },
                "dependencies": {
                    "directory": deps_dir,
                    "snowluma": snow_cfg
                }
            }),
            &json!({"onebot":"http://127.0.0.1:1","dsh":"http://127.0.0.1:2","model":"http://127.0.0.1:3"}),
        );
        fs::create_dir_all(&old_cwd).unwrap();
        fs::write(&old_node, b"old-node").unwrap();
        fs::write(&old_script, b"console.log('old');").unwrap();

        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let cfg = context.load_config().unwrap();
        let old = existing(&context, &cfg, "snowluma").unwrap();

        let bundled_rel = if cfg!(windows) { "node.exe" } else { "bin/node" };
        let zip_with_node = fixture.0.join("with-node.zip");
        let digest_with_node = write_zip(
            &zip_with_node,
            &[
                ("index.mjs", b"console.log('new');", 0o644),
                (bundled_rel, b"bundled-node-binary", 0o755),
            ],
        );
        let cand_with_node = Candidate {
            version: "2.4.2".into(),
            source: "local-archive".into(),
            configuration: cfg["dependencies"]["snowluma"].clone(),
            directory: cfg["dependencies"]["directory"].clone(),
            current: "1.14.22".into(),
            archive: None,
            local_archive: Some(crate::subprocess::external_path(&zip_with_node).unwrap()),
            digest: Some(digest_with_node),
        };
        assert!(valid(&cfg, "snowluma", &cand_with_node).is_ok());

        let (next, folder) = install_snow(&context, &cfg, &cand_with_node, &old).unwrap();
        assert_eq!(
            next["program"],
            json!(crate::subprocess::external_path(folder.join(bundled_rel)).unwrap())
        );
        assert_eq!(next["version"], "2.4.2");

        // Without bundled node, existing program is preserved.
        let zip_no_node = fixture.0.join("no-node.zip");
        let digest_no_node = write_zip(&zip_no_node, &[("index.mjs", b"console.log('no-node');", 0o644)]);
        let cand_no_node = Candidate {
            version: "2.4.3".into(),
            source: "local-archive".into(),
            configuration: cfg["dependencies"]["snowluma"].clone(),
            directory: cfg["dependencies"]["directory"].clone(),
            current: "1.14.22".into(),
            archive: None,
            local_archive: Some(crate::subprocess::external_path(&zip_no_node).unwrap()),
            digest: Some(digest_no_node),
        };
        let (next_no_node, _) = install_snow(&context, &cfg, &cand_no_node, &old).unwrap();
        assert_eq!(next_no_node["program"], cfg["dependencies"]["snowluma"]["program"]);
    }

    #[test]
    fn cli_rejects_bad_flags_and_pre_lock_checksum_mismatch_with_zero_mutation() {
        let root = unique_temp("cli-precheck");
        let old_cwd = root.join("snow");
        let old_node = old_cwd.join(if cfg!(windows) { "node.exe" } else { "node" });
        let old_script = old_cwd.join("index.mjs");
        let fixture = crate::runtime::tests::runtime_fixture(
            root,
            &json!({
                "consoleToken": "fixture-console",
                "update": {"policy": "off"},
                "dependencies": {
                    "directory": "C:/unused-deps",
                    "snowluma": {
                        "version": "1.14.22",
                        "program": old_node,
                        "script": old_script,
                        "workingDirectory": old_cwd
                    }
                }
            }),
            &json!({"onebot":"http://127.0.0.1:1","dsh":"http://127.0.0.1:2","model":"http://127.0.0.1:3"}),
        );
        fs::create_dir_all(&old_cwd).unwrap();
        fs::write(&old_node, b"node").unwrap();
        fs::write(&old_script, b"console.log(1);").unwrap();
        let zip_path = fixture.0.join("snow.zip");
        let _real_digest = write_zip(&zip_path, &[("index.mjs", b"console.log(2);", 0o644)]);

        let s = |v: &str| v.to_string();
        let rt = fixture.0.to_string_lossy().into_owned();
        let zp = zip_path.to_string_lossy().into_owned();

        assert_eq!(run_cli(&[s("--help")]), 0);
        assert_eq!(run_cli(&[s("upgrade"), s("snowluma"), s("-h")]), 0);
        assert_eq!(run_cli(&[s("upgrade"), s("dsh")]), 2);
        assert_eq!(run_cli(&[s("upgrade"), s("snowluma"), s("--unknown")]), 2);
        assert_eq!(
            run_cli(&[
                s("upgrade"),
                s("snowluma"),
                s("--runtime-root"),
                s("relative/path"),
                s("--from-dir"),
                rt.clone(),
            ]),
            2
        );
        assert_eq!(
            run_cli(&[
                s("upgrade"),
                s("snowluma"),
                s("--runtime-root"),
                rt.clone(),
                s("--from-dir"),
                rt.clone(),
                s("--version"),
                s("2.4.2"),
            ]),
            2
        );
        assert_eq!(
            run_cli(&[
                s("upgrade"),
                s("snowluma"),
                s("--runtime-root"),
                rt.clone(),
                s("--archive"),
                zp.clone(),
                s("--sha256"),
                "0".repeat(64),
                s("--version"),
                s("2.4.2-rc.1"),
            ]),
            2
        );

        let cfg_before = fs::read(fixture.0.join("config/config.json")).unwrap();
        let bad_code = run_cli(&[
            s("upgrade"),
            s("snowluma"),
            s("--runtime-root"),
            rt.clone(),
            s("--archive"),
            zp.clone(),
            s("--sha256"),
            "0".repeat(64),
            s("--version"),
            s("v2.4.2"),
        ]);
        assert_eq!(bad_code, 1);
        assert!(!fixture.0.join("data/lifecycle/dependency-upgrades.json").exists());
        assert!(!fixture.0.join("data/locks/dependency-upgrades.lock").exists());
        assert_eq!(fs::read(fixture.0.join("config/config.json")).unwrap(), cfg_before);

        let same_ver_code = run_cli(&[
            s("upgrade"),
            s("snowluma"),
            s("--runtime-root"),
            rt,
            s("--archive"),
            zp,
            s("--sha256"),
            _real_digest,
            s("--version"),
            s("1.14.22"),
        ]);
        assert_eq!(same_ver_code, 1);
        assert!(!fixture.0.join("data/lifecycle/dependency-upgrades.json").exists());
        assert_eq!(fs::read(fixture.0.join("config/config.json")).unwrap(), cfg_before);
    }

    #[test]
    fn change_pin_atomically_rewrites_and_restores_snowluma_paths_preserving_siblings() {
        let root = unique_temp("change-pin-paths");
        let old_cwd = root.join("deps/snowluma/1.14.22");
        let new_cwd = root.join("deps/snowluma/2.4.2");
        let old_pin = json!({
            "version": "1.14.22",
            "program": old_cwd.join("node.exe"),
            "script": old_cwd.join("index.mjs"),
            "workingDirectory": old_cwd
        });
        let new_pin = json!({
            "version": "2.4.2",
            "program": new_cwd.join("node.exe"),
            "script": new_cwd.join("index.mjs"),
            "workingDirectory": new_cwd
        });
        let fixture = crate::runtime::tests::runtime_fixture(
            root,
            &json!({
                "consoleToken": "fixture-console",
                "snowluma": {
                    "accessToken": "keep-access",
                    "baseUrl": "http://127.0.0.1:3000",
                    "homeDir": old_cwd.join("home"),
                    "launcherPath": old_cwd.join("SnowLuma.exe")
                },
                "dependencies": {
                    "snowluma": old_pin
                }
            }),
            &json!({"onebot":"http://127.0.0.1:1","dsh":"http://127.0.0.1:2","model":"http://127.0.0.1:3"}),
        );
        let context = RuntimeContext::from_root(&fixture.0).unwrap();
        let cfg = context.load_config().unwrap();
        let updates = compute_snowluma_path_updates(&cfg, &old_cwd, &new_cwd)
            .unwrap()
            .expect("updates expected");

        change_pin(
            &context,
            "snowluma",
            &old_pin,
            &new_pin,
            Some(&updates),
            false,
        )
        .unwrap();
        let after_forward = context.load_config().unwrap();
        assert_eq!(after_forward["dependencies"]["snowluma"], new_pin);
        assert_eq!(after_forward["snowluma"]["homeDir"], updates.home_dir.as_ref().unwrap().1);
        assert_eq!(
            after_forward["snowluma"]["launcherPath"],
            updates.launcher_path.as_ref().unwrap().1
        );
        assert_eq!(after_forward["snowluma"]["accessToken"], "keep-access");
        assert_eq!(after_forward["snowluma"]["baseUrl"], "http://127.0.0.1:3000");

        change_pin(
            &context,
            "snowluma",
            &new_pin,
            &old_pin,
            Some(&updates),
            true,
        )
        .unwrap();
        let after_reverse = context.load_config().unwrap();
        assert_eq!(after_reverse["dependencies"]["snowluma"], old_pin);
        assert_eq!(after_reverse["snowluma"]["homeDir"], updates.home_dir.as_ref().unwrap().0);
        assert_eq!(
            after_reverse["snowluma"]["launcherPath"],
            updates.launcher_path.as_ref().unwrap().0
        );
        assert_eq!(after_reverse["snowluma"]["accessToken"], "keep-access");
        assert_eq!(after_reverse["snowluma"]["baseUrl"], "http://127.0.0.1:3000");
    }
}
