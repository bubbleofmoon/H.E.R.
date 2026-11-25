//! Release discovery and verified side-by-side installation. Handover belongs to herupdate.
use crate::release_store::{
    directory, installation_gate, sums, version, write_new, IMAGES,
};
use crate::{install_root::InstallRoot, runtime::RuntimeContext};
use crate::release_signature::{self, MAX_CHECKSUM_BYTES, MAX_SIGNATURE_BYTES, SIGNATURE_NAME};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub const DEFAULT_FEED: &str = "https://api.github.com/repos/bubbleofmoon/H.E.R./releases";
pub const CHECK_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;

pub fn settings(cfg: &Value) -> Result<Value, String> {
    let mut value = match cfg.get("update") {
        None | Some(Value::Null) => json!({}),
        Some(value) if value.is_object() => value.clone(),
        _ => return Err("update must be an object".into()),
    };
    if value.get("policy").is_none_or(Value::is_null) {
        value["policy"] = "notify".into();
    }
    if value.get("feedUrl").is_none_or(Value::is_null) {
        value["feedUrl"] = DEFAULT_FEED.into();
    }
    if !matches!(value["policy"].as_str(), Some("off" | "notify" | "auto")) {
        return Err("update.policy must be off, notify or auto".into());
    }
    validate_url(
        value["feedUrl"]
            .as_str()
            .ok_or("update.feedUrl must be a URL")?,
    )?;
    Ok(value)
}

pub(crate) fn validate_url(raw: &str) -> Result<(), String> {
    crate::guarded_fetch::validate_url_syntax(
        raw,
        crate::guarded_fetch::FetchPolicy::ReleaseHttpsOrLoopbackFixture,
    )
    .map(|_| ())
}

/// Guarded release download over `guarded_fetch` (HTTPS or loopback HTTP fixture, public IP
/// pinning, revalidated redirects, bounded time and bytes, hardened curl).
pub(crate) fn fetch(raw: &str, max: usize) -> Result<Vec<u8>, String> {
    validate_url(raw)?;
    let options = crate::guarded_fetch::FetchOptions {
        policy: crate::guarded_fetch::FetchPolicy::ReleaseHttpsOrLoopbackFixture,
        max_bytes: max,
        max_redirects: 5,
        budget: crate::guarded_fetch::BudgetMode::PerHop {
            hop_timeout: Duration::from_secs(30),
            attempt_cap: Some(Duration::from_secs(10)),
        },
        connect_timeout: Some(Duration::from_secs(5)),
        max_addresses: 4,
        min_remaining: Duration::ZERO,
        headers: vec![("user-agent".into(), "HER-release-manager".into())],
        strip_fragment_on_redirect: true,
        pass_max_filesize: true,
        accept: None,
        #[cfg(test)]
        allow_loopback_pin_for_test: false,
    };
    crate::guarded_fetch::fetch_bytes_with(
        raw,
        &options,
        &crate::guarded_fetch::SystemDnsResolver,
        &crate::guarded_fetch::CurlTransfer,
    )
    .map(|out| out.bytes)
}

/// Preserve the managed npm install's existing HTTP(S), credential-free registry contract.
pub(crate) fn validate_registry_url(raw: &str) -> Result<(), String> {
    crate::guarded_fetch::validate_url_syntax(
        raw,
        crate::guarded_fetch::FetchPolicy::RegistryHttpOrHttps,
    )
    .map(|_| ())
}

pub(crate) fn fetch_registry(raw: &str, max: usize) -> Result<Vec<u8>, String> {
    validate_registry_url(raw)?;
    let options = crate::guarded_fetch::FetchOptions {
        policy: crate::guarded_fetch::FetchPolicy::RegistryHttpOrHttps,
        max_bytes: max,
        max_redirects: 5,
        budget: crate::guarded_fetch::BudgetMode::PerHop {
            hop_timeout: Duration::from_secs(30),
            attempt_cap: Some(Duration::from_secs(10)),
        },
        connect_timeout: Some(Duration::from_secs(5)),
        max_addresses: 4,
        min_remaining: Duration::ZERO,
        headers: vec![("user-agent".into(), "HER-release-manager".into())],
        strip_fragment_on_redirect: true,
        pass_max_filesize: true,
        accept: None,
        #[cfg(test)]
        allow_loopback_pin_for_test: false,
    };
    crate::guarded_fetch::fetch_bytes_with(
        raw,
        &options,
        &crate::guarded_fetch::SystemDnsResolver,
        &crate::guarded_fetch::CurlTransfer,
    )
    .map(|out| out.bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

fn required_assets(release: &Release) -> Result<(), String> {
    for name in IMAGES.into_iter().chain(["SHA256SUMS"]) {
        let assets = release
            .assets
            .iter()
            .filter(|asset| asset.name == name)
            .collect::<Vec<_>>();
        if assets.len() != 1 {
            return Err("release requires exactly one of each image and SHA256SUMS".into());
        }
        validate_url(&assets[0].url)?;
    }
    Ok(())
}

/// Feed metadata is untrusted. Re-fetch this proof again at installation time.
fn signed_list(release: &Release, authority: &crate::update::Authority) -> Result<(Vec<u8>, Vec<u8>), String> {
    let signatures = release.assets.iter().filter(|asset| asset.name == SIGNATURE_NAME)
        .collect::<Vec<_>>();
    if signatures.len() != 1 {
        return Err(release_signature::REFUSED.into());
    }
    let checksum = release.assets.iter().find(|asset| asset.name == "SHA256SUMS")
        .ok_or(release_signature::REFUSED)?;
    let checksum = fetch(&checksum.url, MAX_CHECKSUM_BYTES).map_err(|_| release_signature::REFUSED)?;
    let signature = fetch(&signatures[0].url, MAX_SIGNATURE_BYTES).map_err(|_| release_signature::REFUSED)?;
    authority.verify_candidate(&release.tag, &checksum, &signature)?;
    // Only authenticated bytes reach the checksum parser.
    sums(&checksum)?;
    Ok((checksum, signature))
}

fn signature_notice(context: &RuntimeContext, cfg: &Value, error: &str) {
    if error == release_signature::REFUSED {
        crate::admin_alert::notify_notice(context, cfg,
            "HER 更新被拒绝：发布校验清单签名缺失、无效或不受信任。",
            "release-signature-refused");
    }
}

pub fn check_feed(cfg: &Value) -> Result<Option<Release>, String> {
    if settings(cfg)?["policy"] == "off" { return Ok(None); }
    // A CLI check is one admission: it alerts its lost current once per invocation.
    check_feed_with_authority(cfg, &crate::update::admitted_authority(&InstallRoot::configured()?, None, &mut None)?)
}
fn check_feed_with_authority(cfg: &Value, authority: &crate::update::Authority) -> Result<Option<Release>, String> {
    let floor = authority.floor();
    let settings = settings(cfg)?;
    if settings["policy"] == "off" {
        return Ok(None);
    }
    let bytes = fetch(settings["feedUrl"].as_str().unwrap(), 4 * 1024 * 1024)?;
    let feed: Value = serde_json::from_slice(&bytes).map_err(|_| "release feed is invalid JSON")?;
    let list = feed
        .as_array()
        .ok_or("release feed must be a GitHub releases array")?;
    let mut candidates = Vec::new();
    for item in list {
        let Some(tag) = item["tag_name"].as_str() else {
            continue;
        };
        let Some(number) = version(tag) else { continue };
        if floor.and_then(version).is_some_and(|old| number <= old) {
            continue;
        }
        if item["draft"] != false || item["prerelease"] != false {
            continue;
        }
        let assets = item["assets"]
            .as_array()
            .ok_or("release assets are invalid")?
            .iter()
            .map(|asset| {
                Ok(Asset {
                    name: asset["name"]
                        .as_str()
                        .ok_or("release asset name missing")?
                        .into(),
                    url: asset["browser_download_url"]
                        .as_str()
                        .ok_or("release asset URL missing")?
                        .into(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let release = Release {
            tag: tag.into(),
            assets,
        };
        if required_assets(&release).is_ok() {
            candidates.push((number, release));
        }
    }
    candidates.sort_by_key(|(number, _)| *number);
    let release = candidates.pop().map(|(_, release)| release);
    if let Some(release) = &release {
        signed_list(release, authority)?;
    }
    Ok(release)
}

pub(crate) fn current(install: &InstallRoot) -> Option<String> {
    std::fs::canonicalize(install.root().join("current"))
        .ok()
        .and_then(|path| {
            let releases = std::fs::canonicalize(install.root().join("releases")).ok()?;
            (path.parent() == Some(releases.as_path()))
                .then(|| path.file_name()?.to_str().map(str::to_string))
                .flatten()
        })
}

/// A fully verified release directory; this does not change current or last-good.
#[derive(Debug, Serialize)]
pub struct InstalledRelease {
    pub tag: String,
    pub path: PathBuf,
}

struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// This reserved namespace is the ownership marker, including before the first file write.
const STAGING_PREFIX: &str = ".staging-her-release-v1-";

fn recover_staging(root: &InstallRoot, releases: &Path) -> Result<(), String> {
    let protected =
        ["current", "last-good"].map(|name| fs::canonicalize(root.root().join(name)).ok());
    for entry in fs::read_dir(releases).map_err(|_| "release staging cannot be listed")? {
        let entry = entry.map_err(|_| "release staging cannot be inspected")?;
        let name = entry.file_name();
        let Some(suffix) = name
            .to_str()
            .and_then(|name| name.strip_prefix(STAGING_PREFIX))
        else {
            continue;
        };
        let parts = suffix.split('-').collect::<Vec<_>>();
        if parts.len() != 3
            || parts
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| "release staging cannot be inspected")?;
        if !metadata.is_dir() || crate::guarded_file::is_link(&metadata) {
            continue;
        }
        let resolved = fs::canonicalize(&path).map_err(|_| "release staging cannot be resolved")?;
        if protected.iter().flatten().any(|kept| kept == &resolved) {
            continue;
        }
        // Every installer holds the same gate throughout staging; an owned directory
        // seen while holding it cannot belong to another active installer.
        fs::remove_dir_all(path).map_err(|_| "abandoned release staging cannot be removed")?;
    }
    Ok(())
}

/// Serializes staging with restore and with every installer for this install root.
/// The returned path contains the three images, exact SHA256SUMS and its signature.
/// `lost_alerted` dedups the lost-current admission alert (the supervisor passes its
/// discovery state's memory; the CLI passes `None` and alerts once per invocation).
pub fn install(
    context: &RuntimeContext,
    release: &Release,
    lost_alerted: &mut Option<String>,
) -> Result<InstalledRelease, String> {
    let root = installation()?;
    version(&release.tag).ok_or("release tag must be vX.Y.Z")?;
    required_assets(release)?;
    let update_gate =
        installation_gate(&root.root().join("runtime/data/locks/update-restore.lock"))?;
    install_with_gate(context, &root, release, lost_alerted, &update_gate)
}

fn install_with_gate(
    context: &RuntimeContext,
    root: &InstallRoot,
    release: &Release,
    lost_alerted: &mut Option<String>,
    update_gate: &fs::File,
) -> Result<InstalledRelease, String> {
    version(&release.tag).ok_or("release tag must be vX.Y.Z")?;
    required_assets(release)?;
    if !newer(release, current(root).as_deref()) {
        return Err(release_signature::REFUSED.into());
    }
    let releases = root.root().join("releases");
    directory(&releases)?;
    let _gate = installation_gate(&releases.join(".install.lock"))?;
    recover_staging(root, &releases)?;
    let target = releases.join(&release.tag);
    if fs::symlink_metadata(&target).is_ok() {
        return Err("release folder already exists; it will not be overwritten".into());
    }
    let asset = |name: &str| {
        &release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .expect("required_assets checked")
            .url
    };
    let authority = crate::update::admitted_authority(root, Some(update_gate), lost_alerted)?;
    let (checksum, signature) = signed_list(release, &authority).inspect_err(|error| {
        if let Ok(cfg) = context.load_config() {
            signature_notice(context, &cfg, error);
        }
    })?;
    let hashes = sums(&checksum)?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staged = releases.join(format!(
        "{STAGING_PREFIX}{}-{}-{}",
        std::process::id(),
        crate::parity_clock::now_ms().unsigned_abs(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir(&staged).map_err(|_| "release staging directory cannot be created")?;
    let stage = Stage(staged);
    for name in IMAGES {
        let bytes = fetch(asset(name), 512 * 1024 * 1024)?;
        if format!("{:x}", Sha256::digest(&bytes)) != hashes[name] {
            return Err(format!("release checksum mismatch: {name}"));
        }
        write_new(&stage.0.join(name), &bytes)?;
    }
    write_new(&stage.0.join("SHA256SUMS"), &checksum)?;
    write_new(&stage.0.join(SIGNATURE_NAME), &signature)?;
    directory(&releases)?;
    if fs::symlink_metadata(&target).is_ok() {
        return Err("release folder already exists; it will not be overwritten".into());
    }
    crate::release_store::sync_directory(&stage.0)?;
    fs::rename(&stage.0, &target).map_err(|_| "verified release cannot be published")?;
    crate::release_store::sync_directory(&releases)?;
    Ok(InstalledRelease {
        tag: release.tag.clone(),
        path: target,
    })
}

/// The destination is protected independently of the configuration/runtime selection.
pub(crate) fn installation() -> Result<InstallRoot, String> {
    let root = InstallRoot::configured()?;
    let image = std::env::current_exe().map_err(|_| "executable identity is unavailable")?;
    root.require_release_image(&image).map_err(|_| {
        "release installation requires an executable under the install root's releases".to_string()
    })?;
    Ok(root)
}

#[derive(Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CheckState {
    version: u8,
    trust: String,
    feed_url: String,
    last_checked_at: Option<i64>,
    available: Option<Release>,
    notified: Vec<String>,
    error: Option<String>,
    approval: Option<Approval>,
    update_error: Option<String>,
    auto_attempted: Option<String>,
    /// Lost current already alerted under this authority stamp.
    lost_alerted: Option<String>,
}
impl Default for CheckState {
    fn default() -> Self {
        Self {
            version: 3,
            trust: String::new(),
            feed_url: String::new(),
            last_checked_at: None,
            available: None,
            notified: Vec::new(),
            error: None,
            approval: None,
            update_error: None,
            auto_attempted: None,
            lost_alerted: None,
        }
    }
}
fn read_state(context: &RuntimeContext) -> Result<CheckState, String> {
    read_state_for(&InstallRoot::configured()?, context)
}
fn read_state_for(root: &InstallRoot, context: &RuntimeContext) -> Result<CheckState, String> {
    let trust = cache_authority_for(root)?;
    let Some(text) = context
        .read_optional_text(crate::runtime::RuntimeArea::Data, "releases.json")
        .map_err(str::to_string)?
    else {
        return Ok(CheckState { trust, ..CheckState::default() });
    };
    let state: CheckState =
        serde_json::from_str(&text).map_err(|_| "release check state is invalid")?;
    if matches!(state.version, 1 | 2) || state.version == 3 && state.trust != trust {
        return Ok(CheckState { trust, ..CheckState::default() });
    }
    if state.version != 3 {
        return Err("release check state version is unsupported".into());
    }
    Ok(state)
}
/// Refused authority has its own stamp, so reads and dedup work without alerting.
const REFUSED_STAMP: &str = "refused";
fn stamp_of(authority: &Result<crate::update::Authority, String>) -> String {
    authority
        .as_ref()
        .map_or_else(|_| REFUSED_STAMP.into(), |authority| authority.stamp.clone())
}
fn cache_authority_for(root: &InstallRoot) -> Result<String, String> {
    Ok(stamp_of(&crate::update::authority(root)))
}
fn save_state(context: &RuntimeContext, state: &CheckState) -> Result<(), String> {
    save_state_for(&InstallRoot::configured()?, context, state)
}
fn save_state_for(
    root: &InstallRoot,
    context: &RuntimeContext,
    state: &CheckState,
) -> Result<(), String> {
    // Never stamp decisions with authority newer than the one that verified them.
    if state.trust != cache_authority_for(root)? {
        return Err("current release changed during discovery".into());
    }
    context.write_atomic(
        crate::runtime::RuntimeArea::Data,
        "releases.json",
        &serde_json::to_vec(state).map_err(|_| "release state cannot be serialized")?,
    )?;
    crate::release_store::sync_directory(&context.data_dir())
}
fn newer(release: &Release, current: Option<&str>) -> bool {
    version(&release.tag).is_some_and(|next| current.and_then(version).is_none_or(|old| next > old))
}
/// Availability is durable, but a current-link change or feed/policy edit takes effect immediately.
pub fn status(context: &RuntimeContext, cfg: &Value) -> Result<Value, String> {
    let settings = settings(cfg)?;
    let state = read_state(context)?;
    let current = current(&InstallRoot::configured()?);
    let active_feed = settings["policy"] != "off"
        && state.feed_url == settings["feedUrl"].as_str().unwrap();
    let available = state
        .available
        .filter(|release| active_feed && newer(release, current.as_deref()));
    let error = state.error.filter(|_| active_feed);
    Ok(
        json!({"ok":true,"settings":settings,"current":current,"available":available,"lastCheckedAt":state.last_checked_at,"error":error,"approval":state.approval,"updateError":state.update_error}),
    )
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Approval {
    current: String,
    release: Release,
    feed_url: String,
    #[serde(default)]
    automatic: bool,
}

fn approval_installation(context: &RuntimeContext) -> Result<InstallRoot, String> {
    let root = installation()?;
    let runtime =
        fs::canonicalize(root.root().join("runtime")).map_err(|_| "install runtime unavailable")?;
    if context.runtime_root() != Some(runtime.as_path()) {
        return Err("update approval requires the install runtime".into());
    }
    Ok(root)
}

/// Canonical lock acquisition hierarchy across release, turn, and configuration gates:
///
/// 1. `runtime/data/locks/update-restore.lock` (`restore_gate`)
/// 2. `locks/turn-update.lock`
/// 3. `locks/release-check.lock` (`release_check_gate`)
/// 4. `locks/social-v2.lock` (`SOCIAL_STATE_GATE` / `social_state::acquire_config_gate` / `conversation_state::acquire`)
///
/// Every caller across `release.rs`, `update.rs`, `handover.rs`, `dependency_upgrade.rs`,
/// `engine_switch.rs`, `restore.rs`, `console.rs`, and `chat.rs` acquires locks strictly in
/// ascending order (`1 -> 2 -> 3 -> 4`) and never waits on a lower-numbered lock while holding
/// a higher-numbered one. Callers already holding shared `(2) turn-update.lock` (such as
/// `chat::handle_update_command` via `approve_authorized_under_turn_guard`) acquire `(3) -> (4)`
/// and only probe `(1)` non-blockingly via `gate_contended` without holding or waiting on `(1)`.
const GATE_RETRY_WINDOW: Duration = Duration::from_millis(500);
const GATE_RETRY_STEP: Duration = Duration::from_millis(20);

fn retry_contended_gate<T>(
    is_contended: impl Fn(&str) -> bool,
    mut attempt: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let deadline = Instant::now() + GATE_RETRY_WINDOW;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if is_contended(&error) && Instant::now() < deadline => {
                std::thread::sleep(GATE_RETRY_STEP);
            }
            Err(error) => return Err(error),
        }
    }
}

fn restore_gate(root: &InstallRoot) -> Result<fs::File, String> {
    retry_contended_gate(
        |error| error == "another release installation or restore is running",
        || {
            crate::release_store::installation_gate(
                &root.root().join("runtime/data/locks/update-restore.lock"),
            )
        },
    )
}

fn release_check_gate(context: &RuntimeContext) -> Result<fs::File, String> {
    retry_contended_gate(
        |error| error == "runtime file is already locked",
        || {
            context.open_locked_file(
                crate::runtime::RuntimeArea::Data,
                "locks/release-check.lock",
            )
        },
    )
}

pub enum ApproveError<E> {
    Release(String),
    Authorize(E),
}

/// Approval binds to the version displayed by discovery; it never silently chooses a newer tag.
///
/// Follows the canonical 4-lock hierarchy (`1 -> 3 -> 4`): acquires `restore_gate` (1) then
/// `release_check_gate` (3) before running `authorize` (which acquires `SOCIAL_STATE_GATE` (4)
/// and returns `(gate, latest_cfg)`), then drops `(4) -> (3) -> (1)` on return.
pub fn approve_authorized<G, E, F>(
    context: &RuntimeContext,
    tag: &str,
    authorize: F,
) -> Result<Value, ApproveError<E>>
where
    F: FnOnce() -> Result<(G, Value), E>,
{
    let root = approval_installation(context).map_err(ApproveError::Release)?;
    approve_authorized_in_root(&root, context, tag, authorize)
}

/// Approval variant for callers that already hold shared `(2) turn-update.lock` (`turn_guard`):
/// follows `2 -> 3 -> 4` and probes `(1) update-restore.lock` non-blockingly via `gate_contended`
/// without holding or waiting on `(1)`.
pub fn approve_authorized_under_turn_guard<G, E, F>(
    context: &RuntimeContext,
    tag: &str,
    authorize: F,
) -> Result<Value, ApproveError<E>>
where
    F: FnOnce() -> Result<(G, Value), E>,
{
    let root = approval_installation(context).map_err(ApproveError::Release)?;
    approve_authorized_under_turn_guard_in_root(&root, context, tag, authorize)
}

fn approve_authorized_in_root<G, E, F>(
    root: &InstallRoot,
    context: &RuntimeContext,
    tag: &str,
    authorize: F,
) -> Result<Value, ApproveError<E>>
where
    F: FnOnce() -> Result<(G, Value), E>,
{
    // Do not queue a second attempt while a helper is judging this candidate.
    // In particular, a drained /update reply must not reapprove it for rollback.
    let _installation = restore_gate(root).map_err(ApproveError::Release)?;
    approve_with_release_check(root, context, tag, false, authorize)
}

fn approve_authorized_under_turn_guard_in_root<G, E, F>(
    root: &InstallRoot,
    context: &RuntimeContext,
    tag: &str,
    authorize: F,
) -> Result<Value, ApproveError<E>>
where
    F: FnOnce() -> Result<(G, Value), E>,
{
    approve_with_release_check(root, context, tag, true, authorize)
}

fn approve_with_release_check<G, E, F>(
    root: &InstallRoot,
    context: &RuntimeContext,
    tag: &str,
    probe_restore_gate: bool,
    authorize: F,
) -> Result<Value, ApproveError<E>>
where
    F: FnOnce() -> Result<(G, Value), E>,
{
    let _gate = release_check_gate(context).map_err(ApproveError::Release)?;
    let (_config_gate, cfg) = authorize().map_err(ApproveError::Authorize)?;
    if probe_restore_gate
        && crate::release_store::gate_contended(
            &root.root().join("runtime/data/locks/update-restore.lock"),
        )
    {
        return Err(ApproveError::Release(
            "another release installation or restore is running".into(),
        ));
    }
    approve_with_gates(root, context, &cfg, tag).map_err(ApproveError::Release)
}

fn approve_with_gates(
    root: &InstallRoot,
    context: &RuntimeContext,
    cfg: &Value,
    tag: &str,
) -> Result<Value, String> {
    let mut state = read_state_for(root, context)?;
    let settings = settings(cfg)?;
    let current = current(root).ok_or("current release unavailable")?;
    let release = state
        .available
        .as_ref()
        .filter(|release| {
            settings["policy"] != "off"
                && settings["feedUrl"] == state.feed_url
                && release.tag == tag
                && newer(release, Some(&current))
        })
        .ok_or("displayed release is no longer available; refresh before approving")?
        .clone();
    if state.approval.is_some() {
        return Err("an approved update is already pending".into());
    }
    state.approval = Some(Approval {
        current: current.clone(),
        release,
        feed_url: state.feed_url.clone(),
        automatic: false,
    });
    state.update_error = None;
    save_state_for(root, context, &state)?;
    Ok(json!({"ok":true,"current":current,"approved":tag}))
}

/// A turn holds shared ownership until its complete pipeline (including final sends) returns.
/// Auto admission holds exclusive ownership through staging; the helper owns retirement/judgment.
pub async fn turn_guard(context: &RuntimeContext) -> Result<Option<fs::File>, String> {
    if !context.is_isolated() {
        return Ok(None);
    }
    loop {
        match context
            .open_shared_locked_file(crate::runtime::RuntimeArea::Data, "locks/turn-update.lock")
        {
            Ok(file) => return Ok(Some(file)),
            Err(error) if error == "runtime file is already locked" => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(format!("conversation admission is unavailable: {error}")),
        }
    }
}

fn ledger_idle(context: &RuntimeContext) -> Result<bool, String> {
    let records = crate::turn_ledger::read_records(context)?;
    let (turns, _) = crate::turn_ledger::fold(&records);
    // Paused/reattaching work may still be executing remotely; uncertainty is never idle.
    Ok(!turns.iter().any(|turn| {
        turn.state.is_open()
            || turn.state == crate::turn_ledger::State::Paused
            || turn.reattaching && !turn.state.is_final()
    }))
}

/// Only the supervisor consumes approvals. Auto attempts once per admitted discovery check;
/// an administrator can explicitly retry a candidate that was already staged.
///
/// Follows the canonical 4-lock hierarchy (`1 -> 2 -> 3`): peeks state under (3); for auto
/// acquires `update-restore.lock` (1) BEFORE `locks/turn-update.lock` (2) outside (3),
/// re-reads/claims under (3) strictly in `1 -> 2 -> 3` order, downloads/verifies under `(1, 2)`
/// (or outside all gates for manual), drops (2), then holds `(1) -> (3)` to publish and pass
/// the held `update-restore.lock` directly into `handover_with_policy_gate`.
fn apply_approval(context: &RuntimeContext) -> Result<(), String> {
    // Step A (Peek): check whether a manual approval or eligible auto candidate exists.
    let need_auto_fence = {
        let _gate = release_check_gate(context)?;
        let state = read_state(context)?;
        let cfg = context.load_config().map_err(str::to_string)?;
        let settings = settings(&cfg)?;
        if let Some(approval) = &state.approval {
            approval.automatic
        } else if settings["policy"] == "auto" && settings["feedUrl"] == state.feed_url {
            let root = approval_installation(context)?;
            let current = current(&root).ok_or("current release unavailable")?;
            if state.available.as_ref().is_some_and(|release| {
                newer(release, Some(&current))
                    && state.auto_attempted.as_deref() != Some(&release.tag)
            }) {
                true
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        }
    };

    // Step B (Auto restore + turn fence): acquire update-restore.lock (1) BEFORE
    // turn-update.lock (2), both outside release-check.lock (3).
    let (mut update_gate, mut idle) = if need_auto_fence {
        let root = approval_installation(context)?;
        let gate = match installation_gate(&root.root().join("runtime/data/locks/update-restore.lock"))
        {
            Ok(gate) => gate,
            Err(error) if error == "another release installation or restore is running" => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let guard = match context
            .open_locked_file(crate::runtime::RuntimeArea::Data, "locks/turn-update.lock")
        {
            Ok(guard) => guard,
            Err(error) if error == "runtime file is already locked" => return Ok(()),
            Err(error) => return Err(error),
        };
        if !ledger_idle(context)? {
            return Ok(());
        }
        (Some(gate), Some(guard))
    } else {
        (None, None)
    };

    // Step C (Confirm & Claim): re-read and claim under release-check.lock (strictly 1 -> 2 -> 3).
    let (approval, settings, orig_lost_alerted) = {
        let _gate = release_check_gate(context)?;
        let mut state = read_state(context)?;
        let cfg = context.load_config().map_err(str::to_string)?;
        let settings = settings(&cfg)?;
        let root = approval_installation(context)?;
        let current_tag = current(&root);
        if let Some(existing) = state.approval.clone() {
            if current_tag.as_deref() != Some(&existing.current)
                || settings["policy"] == "off"
                || settings["feedUrl"] != existing.feed_url
                || existing.automatic && settings["policy"] != "auto"
            {
                let reason =
                    "approved release or update settings changed; approve again".to_string();
                state.approval = None;
                state.update_error = Some(reason.clone());
                save_state(context, &state)?;
                return Err(reason);
            }
            if existing.automatic {
                if idle.is_none() || update_gate.is_none() {
                    return Ok(());
                }
                state.approval = None;
                state.auto_attempted = Some(existing.release.tag.clone());
                save_state(context, &state)?;
            }
            let orig = state.lost_alerted.clone();
            (existing, settings, orig)
        } else {
            if !need_auto_fence
                || settings["policy"] != "auto"
                || settings["feedUrl"] != state.feed_url
            {
                return Ok(());
            }
            let current_tag = current_tag.ok_or("current release unavailable")?;
            let Some(release) = state.available.as_ref().filter(|release| {
                newer(release, Some(&current_tag))
                    && state.auto_attempted.as_deref() != Some(&release.tag)
            }) else {
                return Ok(());
            };
            let approval = Approval {
                current: current_tag,
                release: release.clone(),
                feed_url: state.feed_url.clone(),
                automatic: true,
            };
            // Persist only auto_attempted (never state.approval) so a failed auto attempt cannot
            // remain stuck as pending approval across retries.
            state.auto_attempted = Some(approval.release.tag.clone());
            save_state(context, &state)?;
            let orig = state.lost_alerted.clone();
            (approval, settings, orig)
        }
    };
    if !approval.automatic {
        drop(idle.take());
        drop(update_gate.take());
    }

    // Step D (Download/Verify): auto reuses held update_gate (1) under idle (2) without re-acquiring (1);
    // manual runs outside both (1) and (3).
    let mut lost_alerted = orig_lost_alerted.clone();
    let install_result: Result<(), String> = (|| {
        let root = approval_installation(context)?;
        let path = root.root().join("releases").join(&approval.release.tag);
        if fs::symlink_metadata(&path).is_ok() {
            crate::update::verify_staged(&root, &approval.release.tag, update_gate.as_ref())?;
        } else if let Some(gate) = update_gate.as_ref() {
            install_with_gate(context, &root, &approval.release, &mut lost_alerted, gate)?;
        } else {
            install(context, &approval.release, &mut lost_alerted)?;
        }
        // Configuration can change while a download is in progress.
        let latest = settings_of_context(context)?;
        if latest != settings {
            return Err("update settings changed during installation; approve again".into());
        }
        Ok(())
    })();
    // Release the staging turn fence before Step E; keep update_gate (1) for Step E (`1 -> 3`).
    drop(idle);

    // Step E (Publish & Atomic Handover Fence): hold update-restore.lock (1) -> release_check_gate (3),
    // verify manual approval still stands, then pass the held gate directly to launcher.
    let record_error = |error: &str, lost: &Option<String>| -> Result<(), String> {
        let _gate = release_check_gate(context)?;
        let mut state = read_state(context)?;
        if lost != &orig_lost_alerted {
            state.lost_alerted = lost.clone();
        }
        if !approval.automatic {
            state.approval = None;
        }
        state.update_error = Some(error.to_string());
        save_state(context, &state)
    };
    if let Err(error) = install_result {
        record_error(&error, &lost_alerted)?;
        return Err(error);
    }
    let gate = match update_gate {
        Some(gate) => gate,
        None => match approval_installation(context).and_then(|root| restore_gate(&root)) {
            Ok(gate) => gate,
            Err(error) => {
                record_error(&error, &lost_alerted)?;
                return Err(error);
            }
        },
    };
    let should_handover = {
        let _gate = release_check_gate(context)?;
        let mut state = read_state(context)?;
        let lost_changed = lost_alerted != orig_lost_alerted;
        if lost_changed {
            state.lost_alerted = lost_alerted.clone();
        }
        if !approval.automatic
            && !state.approval.as_ref().is_some_and(|pending| {
                pending.release.tag == approval.release.tag
                    && pending.feed_url == approval.feed_url
            })
        {
            // Concurrent poll() cleared pending approval (e.g. on feed failure) during download.
            if lost_changed {
                save_state(context, &state)?;
            }
            false
        } else {
            state.approval = None;
            state.update_error = None;
            save_state(context, &state)?;
            true
        }
    };
    if !should_handover {
        return Ok(());
    }
    if let Err(error) =
        handover_with_policy_gate(context, &approval.release.tag, approval.automatic, Some(gate))
    {
        let _gate = release_check_gate(context)?;
        let mut state = read_state(context)?;
        state.update_error = Some(error.clone());
        save_state(context, &state)?;
        return Err(error);
    }
    Ok(())
}

fn settings_of_context(context: &RuntimeContext) -> Result<Value, String> {
    settings(&context.load_config().map_err(str::to_string)?)
}

/// Follows the canonical 4-lock hierarchy (`1 -> 3`): releases `release-check.lock` (3) across
/// network feed I/O, acquires `restore_gate` (1) before `release_check_gate` (3) when publishing
/// post-I/O state, and emits `signature_notice` / `notify_notice` only after dropping both gates.
fn poll(context: &RuntimeContext, cfg: &Value, now: i64) -> Result<(), String> {
    let settings = settings(cfg)?;
    if settings["policy"] == "off" {
        return Ok(());
    }
    let feed = settings["feedUrl"].as_str().unwrap();
    let should_check = {
        let _gate = release_check_gate(context)?;
        let mut state = read_state(context)?;
        if state.feed_url != feed {
            state.feed_url = feed.into();
            state.last_checked_at = None;
            state.available = None;
            state.error = None;
        }
        let due = state
            .last_checked_at
            .is_none_or(|last| now.saturating_sub(last) >= CHECK_INTERVAL_MS);
        if due {
            // Persist admission before I/O: restarts and failures cannot turn into a request storm.
            state.last_checked_at = Some(now);
            state.auto_attempted = None;
            save_state(context, &state)?;
        }
        due
    };
    if should_check {
        let mut pending_signature_notice: Option<String> = None;
        loop {
            let root = InstallRoot::configured()?;
            let authority = crate::update::authority(&root);
            let result = match &authority {
                Ok(authority) => check_feed_with_authority(cfg, authority),
                Err(error) => Err(error.clone()),
            };
            // Installation/handover can proceed during network I/O. Publish only while the
            // exact verifying context and policy/feed are still current (order: 1 -> 3).
            let _installation = restore_gate(&root)?;
            let _gate = release_check_gate(context)?;
            let current_settings = settings_of_context(context)?;
            if current_settings["policy"] == "off" || current_settings["feedUrl"] != feed {
                break;
            }
            let stamp = stamp_of(&authority);
            if stamp != cache_authority_for(&root)? {
                continue;
            }
            let mut state = read_state_for(&root, context)?;
            state.trust = stamp;
            state.feed_url = feed.into();
            state.last_checked_at = Some(now);
            // This check is admission. A lost current alerts once per lost tag whatever
            // the feed did; a refusal once per new cached discovery error.
            if authority.is_ok() {
                crate::update::admission_alert(
                    &root,
                    &authority,
                    Some(&_installation),
                    &mut state.lost_alerted,
                )?;
            }
            match result {
                Ok(available) => {
                    state.available = available;
                    state.error = authority
                        .as_ref()
                        .ok()
                        .and_then(|authority| authority.lost.as_ref())
                        .map(|_| crate::update::LOST_CURRENT.to_string());
                }
                Err(error) => {
                    state.available = None;
                    state.approval = None;
                    if state.error.as_ref() != Some(&error) {
                        if authority.is_err() {
                            crate::update::admission_alert(
                                &root,
                                &authority,
                                Some(&_installation),
                                &mut state.lost_alerted,
                            )?;
                        } else {
                            pending_signature_notice = Some(error.clone());
                        }
                    }
                    state.error = Some(error);
                }
            }
            save_state_for(&root, context, &state)?;
            break;
        }
        if let Some(error) = pending_signature_notice {
            let latest = context.load_config().map_err(str::to_string)?;
            signature_notice(context, &latest, &error);
        }
    }
    // Re-read policy after network I/O so an off/feed edit suppresses stale notices.
    let latest = context.load_config().map_err(str::to_string)?;
    let latest_settings = self::settings(&latest)?;
    if latest_settings["policy"] == "notify" && latest_settings["feedUrl"] == settings["feedUrl"] {
        let claim = {
            let _gate = release_check_gate(context)?;
            let mut state = read_state(context)?;
            if state.feed_url == latest_settings["feedUrl"].as_str().unwrap() {
                let current = current(&InstallRoot::configured()?);
                if let Some(release) = state
                    .available
                    .as_ref()
                    .filter(|release| newer(release, current.as_deref()))
                {
                    let tag = release.tag.clone();
                    if !state.notified.contains(&tag)
                        && crate::admin_alert::owner_id(&latest).is_some()
                    {
                        // Claim before admission/delivery so a crash leaves an uncertain send
                        // deduplicated. Only a definite non-admission releases the claim.
                        state.notified.push(tag.clone());
                        save_state(context, &state)?;
                        Some((tag, current))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some((tag, current)) = claim {
            let admitted = crate::admin_alert::notify_notice(
                context,
                &latest,
                &format!(
                    "HER 更新可用：{} → {tag}。请在控制台查看更新策略。",
                    current.as_deref().unwrap_or("未知版本")
                ),
                &format!("release-available:{tag}"),
            );
            if !admitted {
                let _gate = release_check_gate(context)?;
                let mut state = read_state(context)?;
                state.notified.retain(|notified| notified != &tag);
                save_state(context, &state)?;
            }
        }
    }
    Ok(())
}

/// This supervisor's release tag: the folder of its own installed image (`releases/<tag>/`).
fn own_release_tag() -> Result<String, String> {
    let me = crate::process_identity::current_identity()?;
    let image = fs::canonicalize(&me.exe).map_err(|_| "installed supervisor image unavailable")?;
    image
        .parent()
        .and_then(Path::file_name)
        .and_then(|tag| tag.to_str())
        .map(str::to_string)
        .ok_or_else(|| "installed supervisor tag unavailable".to_string())
}

/// Installed runtimes check by default; legacy/development runtimes opt in with an update block.
/// The worker discovers releases and consumes explicit approvals independently of task scheduling.
pub fn start(context: RuntimeContext) {
    let installed = InstallRoot::configured().ok().is_some_and(|install| {
        context
            .runtime_root()
            .is_some_and(|root| install.protects(root).unwrap_or(false))
    });
    std::thread::spawn(move || {
        let mut last_error = None;
        let mut health_at = None::<Instant>;
        let mut own_tag = None::<String>;
        loop {
            let result = (|| -> Result<(), String> {
                let cfg = context.load_config().map_err(str::to_string)?;
                if installed {
                    let root = InstallRoot::configured()?;
                    let install_runtime = fs::canonicalize(root.root().join("runtime")).ok();
                    let is_runtime = context
                        .runtime_root()
                        .is_some_and(|path| install_runtime.as_deref() == Some(path));
                    if is_runtime
                        && health_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
                    {
                        health_at = Some(Instant::now());
                        let tag = own_release_tag()?;
                        if crate::update::start_count(&root, &tag, crate::update::wall_clock_ms())?
                            > 0
                            && crate::health_check::check(
                                root.root().join("runtime").as_path(),
                                Instant::now() + Duration::from_secs(5),
                            )
                            .ok
                        {
                            crate::update::mark_healthy(&root, &tag)?;
                        }
                    }
                    if is_runtime {
                        // Resolved once (a failure is retried next round). An unresolved own
                        // release (`None`) also defers a rollback in progress.
                        if own_tag.is_none() {
                            own_tag = own_release_tag().ok();
                        }
                        if let Some(alert) = crate::update::claim_alert(
                            &root,
                            crate::update::wall_clock_ms(),
                            own_tag.as_deref(),
                        )? {
                            let text = format!(
                                "HER 更新失败：{} → {}；回退状态：{}；原因：{}",
                                alert.failed, alert.restored, alert.rollback, alert.reason
                            );
                            if crate::admin_alert::notify_notice_wait(
                                &context,
                                &cfg,
                                &text,
                                &format!("release-rollback:{}", alert.id),
                            ) {
                                crate::update::complete_alert(&root, &alert.id)?;
                            }
                        }
                    }
                }
                if installed || cfg.get("update").is_some() {
                    poll(&context, &cfg, crate::parity_clock::now_ms())?;
                    apply_approval(&context)?;
                }
                Ok(())
            })();
            match result {
                Err(error) => {
                    if last_error.as_ref() != Some(&error) {
                        crate::safe_eprintln!("[release] {error}");
                    }
                    last_error = Some(error);
                }
                Ok(()) => last_error = None,
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

pub fn run_cli(args: &[String]) -> i32 {
    if args.first().is_some_and(|arg| arg == "handover") {
        let result = (|| -> Result<Value, String> {
            if args.len() != 5 || args[1] != "--runtime-root" || args[3] != "--tag" {
                return Err("usage: her-cli release handover --runtime-root <install runtime> --tag <installed tag>".into());
            }
            let root = installation()?;
            if !Path::new(&args[2]).is_absolute() {
                return Err("handover runtime must be absolute".into());
            }
            let runtime = fs::canonicalize(&args[2]).map_err(|_| "install runtime unavailable")?;
            if runtime
                != fs::canonicalize(root.root().join("runtime"))
                    .map_err(|_| "install runtime unavailable")?
            {
                return Err("handover requires the install root's runtime".into());
            }
            let context = RuntimeContext::from_root(&runtime).map_err(str::to_string)?;
            Ok(json!({"ok":true,"updaterPid":handover(&context, &args[4])?}))
        })();
        return match result {
            Ok(value) => {
                println!("{value}");
                0
            }
            Err(error) => {
                eprintln!("[release] {error}");
                2
            }
        };
    }
    if args.first().is_some_and(|arg| arg == "retire-current") {
        return retire_current_cli(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "retire") {
        return retire_cli(&args[1..]);
    }
    let result = (|| -> Result<Value, String> {
        if args.len() != 3
            || !matches!(args[0].as_str(), "check" | "install")
            || args[1] != "--runtime-root"
        {
            return Err("usage: her-cli release check|install --runtime-root <absolute root> [--install-root <root>]".into());
        }
        let root = Path::new(&args[2]);
        crate::install_root::check_runtime(root, false)?;
        if args[0] == "install" {
            installation()?;
        }
        let context = RuntimeContext::from_root(root).map_err(str::to_string)?;
        let cfg = context.load_config().map_err(str::to_string)?;
        let current = current(&InstallRoot::configured()?);
        let available = check_feed(&cfg)
            .inspect_err(|error| signature_notice(&context, &cfg, error))?;
        if args[0] == "install" {
            let release =
                available.ok_or("no eligible newer release is available (or updates are off)")?;
            Ok(json!({"ok":true,"installed":install(&context,&release,&mut None)?}))
        } else {
            Ok(json!({"ok":true,"current":current,"available":available}))
        }
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            eprintln!("[release] {error}");
            2
        }
    }
}

// The fixed updater retires the old release through the candidate CLI, so the previous
// release need not implement this protocol; rollback's retire-current uses the failed
// release's CLI unless it fails verification, then verified last-good's. No
// desired-state or dependency mutation.
fn retire_cli(args: &[String]) -> i32 {
    let automatic = args.first().is_some_and(|arg| arg == "--auto");
    let args = if automatic { &args[1..] } else { args };
    let mut stopped = false;
    let result = (|| -> Result<(), String> {
        use crate::lifecycle::{
            collect_verified_owners, retire_verified_owners, ImagePolicy, LiveLifecycle,
            OwnerRole, ProbeMode,
        };
        use crate::process_identity::ProcessIdentity;
        if args.len() != 6
            || args[0] != "--old-pid"
            || args[2] != "--old-start"
            || args[4] != "--old-exe"
        {
            return Err("invalid release retirement arguments".into());
        }
        let root = installation()?;
        let old = ProcessIdentity {
            pid: args[1].parse().map_err(|_| "invalid PID")?,
            start: args[3].parse().map_err(|_| "invalid start")?,
            exe: args[5].clone(),
        };
        let current = crate::update::link_tag(&root, "current")?
            .ok_or("current release unavailable")?;
        let policy = ImagePolicy::ReleaseTag {
            root: root.clone(),
            tag: current,
        };
        if !policy.matches("", &old.exe) {
            return Err("retirement image is not current".into());
        }
        let data = root.root().join("runtime/data");
        let mut backend = LiveLifecycle;
        let owners = collect_verified_owners(
            &mut backend,
            &data,
            &[OwnerRole::Supervisor, OwnerRole::Live, OwnerRole::Console],
            &policy,
            ProbeMode::Existing,
            Some(&old),
        )?;
        #[cfg(feature = "parity-clock")]
        let fault_after = crate::update::handover_control(&root)?
            .and_then(|(_, value)| value["retireFailAfter"].as_u64());
        #[cfg(not(feature = "parity-clock"))]
        let fault_after: Option<u64> = None;
        if fault_after == Some(0) {
            return Err("injected retirement failure".into());
        }
        if automatic {
            // The fixed helper retains exclusive turn admission until this child exits.
            // Manual approval intentionally only drains active pipelines, including sends.
            let context = RuntimeContext::from_root(&root.root().join("runtime"))
                .map_err(str::to_string)?;
            let refusal = match ledger_idle(&context) {
                Ok(true) => None,
                Ok(false) => Some("automatic handover deferred: conversation turns are not idle; approve again".to_string()),
                Err(error) => Some(format!("automatic handover deferred: idle state unavailable: {error}; approve again")),
            };
            if let Some(reason) = refusal {
                // Helper already holds update-restore.lock (1) and turn-update.lock (2) -> (3).
                let _gate = release_check_gate(&context)?;
                let mut state = read_state(&context)?;
                state.update_error = Some(reason.clone());
                save_state(&context, &state)?;
                return Err(reason);
            }
        }
        retire_verified_owners(
            &mut backend,
            &data,
            &owners,
            ProbeMode::Existing,
            Duration::from_secs(5),
            Duration::from_millis(20),
            fault_after,
        )
        .map_err(|failure| {
            stopped = failure.stopped;
            failure.reason
        })
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[release] {error}");
            if stopped {
                3
            } else {
                2
            }
        }
    }
}

/// Approval/idle admission belongs to the caller; this interface accepts only an
/// already installed release and the installation's own runtime.
pub fn handover(context: &RuntimeContext, target: &str) -> Result<u32, String> {
    handover_with_policy_gate(context, target, false, None)
}

fn handover_with_policy_gate(
    context: &RuntimeContext,
    target: &str,
    automatic: bool,
    gate: Option<fs::File>,
) -> Result<u32, String> {
    use crate::lifecycle::{
        verify_owner, ImagePolicy, LiveLifecycle, OwnerObservation, OwnerRole, ProbeMode,
    };
    let root = installation()?;
    if context.runtime_root()
        != Some(
            fs::canonicalize(root.root().join("runtime"))
                .map_err(|_| "install runtime unavailable")?
                .as_path(),
        )
    {
        return Err("handover requires the install runtime".into());
    }
    let current_tag = current(&root).ok_or("current image unavailable")?;
    let old = match verify_owner(
        &mut LiveLifecycle,
        &context.data_dir(),
        OwnerRole::Supervisor,
        &ImagePolicy::ReleaseTag {
            root: root.clone(),
            tag: current_tag,
        },
        ProbeMode::Existing,
    ) {
        OwnerObservation::Verified(owner) => owner.marker.identity,
        OwnerObservation::Mismatch {
            marker: Some(_),
            liveness: Some(_),
            ..
        } => return Err("supervisor identity cannot be verified".into()),
        OwnerObservation::Mismatch {
            marker: Some(marker),
            liveness: None,
            ..
        } if marker.role == OwnerRole::Supervisor.as_str() => {
            return Err("supervisor is not the current release".into());
        }
        _ => return Err("supervisor owner cannot be identified".into()),
    };
    match gate {
        Some(gate) => crate::update::launch_with_gate(&root, target, &old, automatic, gate),
        None => {
            debug_assert!(!automatic);
            crate::update::launch(&root, target, &old)
        }
    }
}

fn retire_current_cli(args: &[String]) -> i32 {
    let result = (|| -> Result<Vec<String>, String> {
        use crate::lifecycle::{
            verify_owner, ImagePolicy, LiveLifecycle, OwnerObservation, OwnerRole, ProbeMode,
        };
        if args.len() != 2 || args[0] != "--tag" {
            return Err("invalid retirement tag".into());
        }
        let root = installation()?;
        if current(&root).as_deref() != Some(&args[1]) {
            return Err("retirement tag is not current".into());
        }
        let image = crate::update::release_path(&root, &args[1])?.join("her.exe");
        let data = root.root().join("runtime/data");
        let policy = ImagePolicy::ReleaseTag {
            root: root.clone(),
            tag: args[1].clone(),
        };
        let identity = match verify_owner(
            &mut LiveLifecycle,
            &data,
            OwnerRole::Supervisor,
            &policy,
            ProbeMode::Existing,
        ) {
            OwnerObservation::Verified(owner) => owner.marker.identity,
            OwnerObservation::Free { .. } => crate::process_identity::ProcessIdentity {
                pid: 0,
                start: 0,
                exe: image.to_string_lossy().into(),
            },
            _ => return Err("current supervisor cannot be identified".into()),
        };
        Ok(vec![
            "--old-pid".into(),
            identity.pid.to_string(),
            "--old-start".into(),
            identity.start.to_string(),
            "--old-exe".into(),
            identity.exe,
        ])
    })();
    match result {
        Ok(args) => retire_cli(&args),
        Err(error) => {
            eprintln!("[release] {error}");
            2
        }
    }
}

/// Count installed current starts before parsing runtime config. A duplicate
/// launch while an owner is held is not an unhealthy supervisor start.
pub fn before_supervisor_start(runtime: &Path) -> Result<bool, String> {
    use crate::lifecycle::{LifecycleBackend as _, LiveLifecycle, OwnerProbe, OwnerRole, ProbeMode};
    let root = InstallRoot::configured()?;
    let selected = root.root().join("runtime");
    if runtime != selected
        && fs::canonicalize(runtime)
            .ok()
            .zip(fs::canonicalize(&selected).ok())
            .is_none_or(|(a, b)| a != b)
    {
        return Ok(false);
    }
    let me = crate::process_identity::current_identity()?;
    root.require_release_image(Path::new(&me.exe))?;
    let image = fs::canonicalize(&me.exe).map_err(|_| "supervisor image unavailable")?;
    let own_tag = image
        .parent()
        .and_then(Path::file_name)
        .and_then(|tag| tag.to_str());
    // Install-root guards also support local/bootstrap folders. Only published
    // versioned releases participate in automatic update recovery bookkeeping.
    if own_tag.and_then(version).is_none() {
        return Ok(false);
    }
    let (current, last_good, now) = crate::update::startup_selection(&root)?;
    let Some(tag) = current else {
        return Ok(false);
    };
    if image.parent()
        != Some(
            fs::canonicalize(crate::update::release_path(&root, &tag)?)
                .map_err(|_| "current release unavailable")?
                .as_path(),
        )
    {
        return Ok(false);
    }
    if matches!(
        LiveLifecycle.probe_owner(
            &selected.join("data"),
            OwnerRole::Supervisor,
            ProbeMode::Existing,
        ),
        OwnerProbe::Held { .. }
    ) {
        return Ok(false);
    }
    let count = crate::update::record_start(&root, &tag, now)?;
    if count < 3 || last_good.as_deref().is_none_or(|good| good == tag) {
        return Ok(false);
    }
    match crate::update::launch_recovery(&root, &tag, &me) {
        Ok(_) => Ok(true),
        Err(error) if error == "another release installation or restore is running" => Ok(true),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempInstall(PathBuf);
    impl TempInstall {
        fn new(name: &str) -> (Self, InstallRoot, RuntimeContext) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let base = crate::install_root::development_root(std::env::temp_dir().join(format!(
                "her-release-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )));
            for dir in [
                "releases/v0.0.0",
                "runtime/config",
                "runtime/data/locks",
                "runtime/resources",
            ] {
                fs::create_dir_all(base.join(dir)).unwrap();
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(
                base.join("releases").join("v0.0.0"),
                base.join("current"),
            )
            .unwrap();
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let out = std::process::Command::new("cmd.exe")
                    .args(["/d", "/c", "mklink", "/J"])
                    .arg(base.join("current"))
                    .arg(base.join("releases").join("v0.0.0"))
                    .creation_flags(0x0800_0000)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{out:?}");
            }
            let runtime = base.join("runtime");
            fs::write(
                runtime.join("config/config.json"),
                json!({
                    "consoleToken": "fixture-token",
                    "update": {
                        "policy": "notify",
                        "feedUrl": "http://127.0.0.1:9999/feed"
                    }
                })
                .to_string(),
            )
            .unwrap();
            fs::write(runtime.join("data/social-v2.json"), "{}").unwrap();
            fs::write(
                runtime.join("dependencies.json"),
                json!({
                    "dsh": "http://127.0.0.1:1",
                    "model": "http://127.0.0.1:1",
                    "onebot": "http://127.0.0.1:1"
                })
                .to_string(),
            )
            .unwrap();
            let root = InstallRoot::new(&base).unwrap();
            let context = RuntimeContext::from_root(&runtime).unwrap();
            (Self(base), root, context)
        }
    }
    impl Drop for TempInstall {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn seed_available(root: &InstallRoot, context: &RuntimeContext, tag: &str) -> Value {
        let cfg = context.load_config().unwrap();
        let state = CheckState {
            version: 3,
            trust: cache_authority_for(root).unwrap(),
            feed_url: "http://127.0.0.1:9999/feed".into(),
            last_checked_at: Some(1),
            available: Some(Release {
                tag: tag.into(),
                assets: Vec::new(),
            }),
            notified: Vec::new(),
            error: None,
            approval: None,
            update_error: None,
            auto_attempted: None,
            lost_alerted: None,
        };
        save_state_for(root, context, &state).unwrap();
        cfg
    }

    #[test]
    fn approve_retries_transient_restore_lock_and_rejects_persistent_hold() {
        let (_guard, root, context) = TempInstall::new("approve-lock-retry");
        let cfg = seed_available(&root, &context, "v1.2.3");
        let restore_lock = root.root().join("runtime/data/locks/update-restore.lock");

        // Brief (~60ms) transient hold on update-restore.lock succeeds within the 500ms window.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder_path = restore_lock.clone();
        let holder = std::thread::spawn(move || {
            let gate = crate::release_store::installation_gate(&holder_path).unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(60));
            drop(gate);
        });
        locked_rx.recv().unwrap();
        let approved = approve_authorized_in_root(&root, &context, "v1.2.3", || {
            Ok::<_, (u16, String)>(((), cfg.clone()))
        });
        holder.join().unwrap();
        let approved = approved.ok().unwrap();
        assert_eq!(approved["ok"], true);
        assert_eq!(approved["approved"], "v1.2.3");

        // Brief (~60ms) transient hold on locks/release-check.lock also succeeds.
        let cfg = seed_available(&root, &context, "v1.2.4");
        let (check_tx, check_rx) = std::sync::mpsc::channel();
        let check_context = context.clone();
        let check_holder = std::thread::spawn(move || {
            let gate = check_context
                .open_locked_file(crate::runtime::RuntimeArea::Data, "locks/release-check.lock")
                .unwrap();
            check_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(60));
            drop(gate);
        });
        check_rx.recv().unwrap();
        let approved = approve_authorized_in_root(&root, &context, "v1.2.4", || {
            Ok::<_, (u16, String)>(((), cfg.clone()))
        });
        check_holder.join().unwrap();
        let approved = approved.ok().unwrap();
        assert_eq!(approved["ok"], true);
        assert_eq!(approved["approved"], "v1.2.4");

        // A persistently held update-restore.lock (>500ms) still fails with 409 text.
        let cfg = seed_available(&root, &context, "v1.2.5");
        let _held = crate::release_store::installation_gate(&restore_lock).unwrap();
        let started = Instant::now();
        let err = approve_authorized_in_root(&root, &context, "v1.2.5", || {
            Ok::<_, (u16, String)>(((), cfg.clone()))
        })
        .err()
        .unwrap();
        assert!(matches!(
            err,
            ApproveError::Release(ref message) if message == "another release installation or restore is running"
        ));
        assert!(started.elapsed() >= Duration::from_millis(450));
    }

    #[test]
    fn approve_authorized_acquires_release_gates_before_authorize_and_preserves_status() {
        let (_guard, root, context) = TempInstall::new("approve-order-status");
        let _cfg = seed_available(&root, &context, "v1.2.3");
        let restore_lock = root.root().join("runtime/data/locks/update-restore.lock");

        let err = approve_authorized_in_root(&root, &context, "v1.2.3", || {
            // Both outer gates (1) update-restore.lock and (3) release-check.lock must already be held
            // when the caller acquires (4) SOCIAL_STATE_GATE.
            assert!(crate::release_store::installation_gate(&restore_lock).is_err());
            assert!(context
                .open_locked_file(
                    crate::runtime::RuntimeArea::Data,
                    "locks/release-check.lock",
                )
                .is_err());
            Err::<((), Value), (u16, String)>((503, "selected runtime social state is busy".into()))
        })
        .err()
        .unwrap();
        assert!(matches!(
            err,
            ApproveError::Authorize((503, ref msg)) if msg == "selected runtime social state is busy"
        ));
    }

    #[test]
    fn approve_under_turn_guard_probes_restore_gate_without_waiting() {
        let (_guard, root, context) = TempInstall::new("approve-under-turn-guard");
        let cfg = seed_available(&root, &context, "v1.2.3");
        let restore_lock = root.root().join("runtime/data/locks/update-restore.lock");

        let held = crate::release_store::installation_gate(&restore_lock).unwrap();
        let started = Instant::now();
        let err = approve_authorized_under_turn_guard_in_root(&root, &context, "v1.2.3", || {
            assert!(context
                .open_locked_file(
                    crate::runtime::RuntimeArea::Data,
                    "locks/release-check.lock",
                )
                .is_err());
            Ok::<_, String>(((), cfg.clone()))
        })
        .err()
        .unwrap();
        assert!(matches!(
            err,
            ApproveError::Release(ref message) if message == "another release installation or restore is running"
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(held);

        let approved =
            approve_authorized_under_turn_guard_in_root(&root, &context, "v1.2.3", || {
                Ok::<_, String>(((), cfg.clone()))
            })
            .ok()
            .unwrap();
        assert_eq!(approved["ok"], true);
        assert_eq!(approved["approved"], "v1.2.3");
    }

    #[test]
    fn release_fetch_enforces_private_host_policy_and_curl_hardening() {
        assert!(validate_url("https://169.254.169.254/feed").is_err(), "link-local metadata IP must be refused");
        assert!(validate_url("https://10.0.0.1/feed").is_err(), "RFC1918 private IP must be refused");
        assert!(validate_url("https://printer.local/feed").is_err(), ".local host must be refused");
        assert!(validate_registry_url("http://10.0.0.1/npm").is_err(), "private registry IP must be refused");
        assert!(validate_registry_url("http://printer.local/npm").is_err(), ".local registry host must be refused");
        assert!(validate_url(DEFAULT_FEED).is_ok());
        assert!(validate_registry_url("http://registry.example.invalid/npm/?view=versions").is_ok());

        let url = url::Url::parse("https://api.github.com/repos/bubbleofmoon/H.E.R./releases?token=s").unwrap();
        let req = crate::guarded_fetch::TransferRequest {
            url: &url,
            pin: "140.82.112.5".parse().unwrap(),
            headers: vec![("user-agent".into(), "HER-release-manager".into())],
            body: None,
            budget: Duration::from_secs(30),
            connect_timeout: Some(Duration::from_secs(5)),
            max_filesize: Some(1024),
        };
        let (command, config) = crate::guarded_fetch::build_curl_command(&req).unwrap();
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.first().map(String::as_str), Some("-q"));
        assert!(args.windows(2).any(|w| w == ["-K", "-"]), "release curl must read URL via -K -: {args:?}");
        assert!(!args.iter().any(|a| a.contains("api.github.com/repos")), "release URL must not appear in CLI args: {args:?}");
        assert!(args.windows(2).any(|w| w == ["--noproxy", "*"]), "release curl must pass --noproxy *: {args:?}");
        assert!(config.contains("url = \"https://api.github.com/repos/bubbleofmoon/H.E.R./releases?token=s\""));
    }

    #[test]
    fn release_fetch_loopback_fixture_enforces_size_cap_and_refuses_private_redirects() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(3) {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let response = match path {
                    "/ok" => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string(),
                    "/big" => "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcdefghij".to_string(),
                    "/redir-private" => "HTTP/1.1 302 Found\r\nLocation: https://169.254.169.254/latest\r\nConnection: close\r\n\r\n".to_string(),
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        assert_eq!(fetch(&format!("http://127.0.0.1:{port}/ok"), 16).unwrap(), b"ok");
        assert!(fetch(&format!("http://127.0.0.1:{port}/big"), 4).is_err(), "oversized release response must fail");
        assert!(fetch(&format!("http://127.0.0.1:{port}/redir-private"), 1024).is_err(), "redirect to private IP must fail");
    }
}
