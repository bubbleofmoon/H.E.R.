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
    io::Read,
    path::Path,
    process::Stdio,
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
    let url = url::Url::parse(raw).map_err(|_| "release URL is invalid")?;
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
    {
        return Err("release URL requires HTTPS or a loopback HTTP fixture, without credentials or fragment".into());
    }
    Ok(())
}

/// Curl owns TLS, runs hidden, has no inherited curlrc, and is bounded in time and bytes.
pub(crate) fn fetch(raw: &str, max: usize) -> Result<Vec<u8>, String> {
    validate_url(raw)?;
    transfer(raw,max,"=https")
}

/// Preserve the managed npm install's existing HTTP(S), credential-free registry contract.
pub(crate) fn validate_registry_url(raw: &str) -> Result<(), String> {
    let url=url::Url::parse(raw).map_err(|_|"dependencies.dsh.registry is invalid")?;
    if !matches!(url.scheme(),"http"|"https") || !url.username().is_empty() || url.password().is_some() {
        return Err("dependencies.dsh.registry must be an http(s) URL without credentials".into());
    }
    Ok(())
}
pub(crate) fn fetch_registry(raw: &str, max: usize) -> Result<Vec<u8>, String> {
    validate_registry_url(raw)?;
    transfer(raw,max,"=https,http")
}
fn transfer(raw: &str, max: usize, redirects: &str) -> Result<Vec<u8>, String> {
    let mut child =
        crate::subprocess::command_without_console(if cfg!(windows) { "curl.exe" } else { "curl" })
            .args([
                "-q",
                "--silent",
                "--fail",
                "--location",
                "--max-redirs",
                "5",
                "--proto",
                "=https,http",
                "--proto-redir",
                redirects,
                "--connect-timeout",
                "5",
                "--max-time",
                "30",
                "--user-agent",
                "HER-release-manager",
                "--max-filesize",
                &max.to_string(),
                "--url",
                raw,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "release transfer could not start")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("release transfer pipe unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(max as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(35);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("release transfer timed out or became unavailable");
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "release transfer reader failed")?
        .map_err(|_| "release transfer read failed")?;
    if !status?.success() || bytes.len() > max {
        return Err("release transfer failed or exceeded its size limit".into());
    }
    Ok(bytes)
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
    let _update = installation_gate(&root.root().join("runtime/data/locks/update-restore.lock"))?;
    if !newer(release, current(&root).as_deref()) {
        return Err(release_signature::REFUSED.into());
    }
    let releases = root.root().join("releases");
    directory(&releases)?;
    let _gate = installation_gate(&releases.join(".install.lock"))?;
    recover_staging(&root, &releases)?;
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
    let authority = crate::update::admitted_authority(&root, Some(&_update), lost_alerted)?;
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
    let trust = cache_authority()?;
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
fn cache_authority() -> Result<String, String> {
    Ok(stamp_of(&crate::update::authority(&InstallRoot::configured()?)))
}
fn save_state(context: &RuntimeContext, state: &CheckState) -> Result<(), String> {
    // Never stamp decisions with authority newer than the one that verified them.
    if state.trust != cache_authority()? {
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
    let available = state.available.filter(|release| {
        settings["policy"] != "off"
            && state.feed_url == settings["feedUrl"].as_str().unwrap()
            && newer(release, current.as_deref())
    });
    Ok(
        json!({"ok":true,"settings":settings,"current":current,"available":available,"lastCheckedAt":state.last_checked_at,"error":state.error,"approval":state.approval,"updateError":state.update_error}),
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

/// Approval binds to the version displayed by discovery; it never silently chooses a newer tag.
pub fn approve(context: &RuntimeContext, cfg: &Value, tag: &str) -> Result<Value, String> {
    let root = approval_installation(context)?;
    // Do not queue a second attempt while a helper is judging this candidate.
    // In particular, a drained /update reply must not reapprove it for rollback.
    let _installation = crate::release_store::installation_gate(
        &root.root().join("runtime/data/locks/update-restore.lock"),
    )?;
    let _gate = context.open_locked_file(
        crate::runtime::RuntimeArea::Data,
        "locks/release-check.lock",
    )?;
    let mut state = read_state(context)?;
    let settings = settings(cfg)?;
    let current = current(&root).ok_or("current release unavailable")?;
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
    save_state(context, &state)?;
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
fn apply_approval(context: &RuntimeContext) -> Result<(), String> {
    let _gate = context.open_locked_file(
        crate::runtime::RuntimeArea::Data,
        "locks/release-check.lock",
    )?;
    let mut state = read_state(context)?;
    let cfg = context.load_config().map_err(str::to_string)?;
    let settings = settings(&cfg)?;
    if state.approval.is_none()
        && settings["policy"] == "auto"
        && settings["feedUrl"] == state.feed_url
    {
        let root = approval_installation(context)?;
        let current = current(&root).ok_or("current release unavailable")?;
        if let Some(release) = state.available.as_ref().filter(|release| {
            newer(release, Some(&current)) && state.auto_attempted.as_deref() != Some(&release.tag)
        }) {
            state.approval = Some(Approval {
                current,
                release: release.clone(),
                feed_url: state.feed_url.clone(),
                automatic: true,
            });
        }
    }
    let Some(approval) = state.approval.clone() else {
        return Ok(());
    };
    let mut idle = if approval.automatic {
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
        Some(guard)
    } else {
        None
    };
    let result: Result<(), String> = (|| {
        let root = approval_installation(context)?;
        if current(&root).as_deref() != Some(&approval.current)
            || settings["policy"] == "off"
            || settings["feedUrl"] != approval.feed_url
            || approval.automatic && settings["policy"] != "auto"
        {
            return Err("approved release or update settings changed; approve again".into());
        }
        if approval.automatic {
            state.auto_attempted = Some(approval.release.tag.clone());
        }
        save_state(context, &state)?;
        let path = root.root().join("releases").join(&approval.release.tag);
        if fs::symlink_metadata(&path).is_ok() {
            crate::update::verify(&root, &approval.release.tag)?;
        } else {
            install(context, &approval.release, &mut state.lost_alerted)?;
        }
        // Configuration can change while a download is in progress.
        let latest = settings_of_context(context)?;
        if latest != settings {
            return Err("update settings changed during installation; approve again".into());
        }
        state.approval = None;
        state.update_error = None;
        save_state(context, &state)?;
        // Release the staging fence before launch. The helper takes its own fence after
        // the trial, drains any intervening turn and retains it after this supervisor exits.
        drop(idle.take());
        handover_with_policy(context, &approval.release.tag, approval.automatic)?;
        Ok(())
    })();
    state.approval = None;
    state.update_error = result.as_ref().err().cloned();
    save_state(context, &state)?;
    result
}

fn settings_of_context(context: &RuntimeContext) -> Result<Value, String> {
    settings(&context.load_config().map_err(str::to_string)?)
}


fn poll(context: &RuntimeContext, cfg: &Value, now: i64) -> Result<(), String> {
    let settings = settings(cfg)?;
    if settings["policy"] == "off" {
        return Ok(());
    }
    let _gate = context.open_locked_file(
        crate::runtime::RuntimeArea::Data,
        "locks/release-check.lock",
    )?;
    let mut state = read_state(context)?;
    let feed = settings["feedUrl"].as_str().unwrap();
    if state.feed_url != feed {
        state.feed_url = feed.into();
        state.last_checked_at = None;
        state.available = None;
        state.error = None;
    }
    if state
        .last_checked_at
        .is_none_or(|last| now.saturating_sub(last) >= CHECK_INTERVAL_MS)
    {
        // Persist admission before I/O: restarts and failures cannot turn into a request storm.
        state.last_checked_at = Some(now);
        state.auto_attempted = None;
        save_state(context, &state)?;
        loop {
            let root = InstallRoot::configured()?;
            let authority = crate::update::authority(&root);
            let result = match &authority {
                Ok(authority) => check_feed_with_authority(cfg, authority),
                Err(error) => Err(error.clone()),
            };
            // Installation/handover can proceed during network I/O. Publish only
            // while the exact verifying context is still current, fenced against switch.
            let _installation = installation_gate(&root.root().join("runtime/data/locks/update-restore.lock"))?;
            let stamp = stamp_of(&authority);
            if stamp != cache_authority()? {
                state = CheckState { trust: cache_authority()?, feed_url: feed.into(),
                    last_checked_at: Some(now), ..CheckState::default() };
                continue;
            }
            state.trust = stamp;
            // This check is admission. A lost current alerts once per lost tag whatever
            // the feed did; a refusal once per new cached discovery error.
            if authority.is_ok() {
                crate::update::admission_alert(&root, &authority, Some(&_installation), &mut state.lost_alerted)?;
            }
            match result {
                Ok(available) => {
                    state.available = available;
                    state.error = authority.as_ref().ok().and_then(|authority| authority.lost.as_ref())
                        .map(|_| crate::update::LOST_CURRENT.to_string());
                }
                Err(error) => {
                    state.available = None;
                    state.approval = None;
                    if state.error.as_ref() != Some(&error) {
                        if authority.is_err() {
                            crate::update::admission_alert(&root, &authority, Some(&_installation), &mut state.lost_alerted)?;
                        } else {
                            signature_notice(context, cfg, &error);
                        }
                    }
                    state.error = Some(error);
                }
            }
            save_state(context, &state)?;
            break;
        }
    }
    // Re-read policy after network I/O so an off/feed edit suppresses stale notices.
    let latest = context.load_config().map_err(str::to_string)?;
    let latest_settings = self::settings(&latest)?;
    if latest_settings["policy"] == "notify" && latest_settings["feedUrl"] == settings["feedUrl"] {
        let current = current(&InstallRoot::configured()?);
        if let Some(release) = state
            .available
            .as_ref()
            .filter(|release| newer(release, current.as_deref()))
        {
            let tag = release.tag.clone();
            if !state.notified.contains(&tag) && crate::admin_alert::owner_id(&latest).is_some() {
                // Claim before admission/delivery so a crash leaves an uncertain send
                // deduplicated. Only a definite non-admission releases the claim.
                state.notified.push(tag.clone());
                save_state(context, &state)?;
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
                    state.notified.retain(|notified| notified != &tag);
                    save_state(context, &state)?;
                }
            }
        }
    }
    Ok(())
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
                        let me = crate::process_identity::current_identity()?;
                        let image = fs::canonicalize(&me.exe)
                            .map_err(|_| "installed supervisor image unavailable")?;
                        let tag = image
                            .parent()
                            .and_then(Path::file_name)
                            .and_then(|tag| tag.to_str())
                            .ok_or("installed supervisor tag unavailable")?;
                        if crate::update::start_count(&root, tag, crate::update::wall_clock_ms())?
                            > 0
                            && crate::health_check::check(
                                root.root().join("runtime").as_path(),
                                Instant::now() + Duration::from_secs(5),
                            )
                            .ok
                        {
                            crate::update::mark_healthy(&root, tag)?;
                        }
                    }
                    if is_runtime {
                        if let Some(alert) =
                            crate::update::claim_alert(&root, crate::update::wall_clock_ms())?
                        {
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

/// Appended to the runtime-supplied main console page; the tasks page owns settings controls.
pub const BANNER: &str = r#"<aside id="her-update-banner" role="status" hidden></aside>
<script>(()=>{const box=document.getElementById('her-update-banner');const token=()=>new URLSearchParams(location.search).get('token')||sessionStorage.getItem('consoleToken')||'';
async function refresh(){try{const r=await fetch('/api/releases',{headers:{'x-console-token':token()}});if(!r.ok){box.hidden=true;return}const j=await r.json();box.hidden=!j.available;if(j.available){box.textContent='HER 更新可用：'+(j.current||'未知版本')+' → '+j.available.tag+' ';const button=document.createElement('button');button.textContent='批准更新';button.disabled=!!j.approval;button.onclick=async()=>{if(!confirm('批准更新 HER：'+(j.current||'未知版本')+' → '+j.available.tag+'？'))return;button.disabled=true;try{const r=await fetch('/api/releases/approve',{method:'POST',headers:{'x-console-token':token(),'content-type':'application/json'},body:JSON.stringify({confirm:true,tag:j.available.tag})});const result=await r.json();if(!r.ok||!result.ok)throw Error(result.error||'更新被拒绝');box.textContent='HER 更新已批准：'+result.current+' → '+result.approved}catch(e){button.disabled=false;alert(e.message)}};box.append(button);const link=document.createElement('a');link.textContent='更新策略';link.href='/tasks?'+new URLSearchParams({token:token()})+'#updates';box.append(link)}}catch{box.hidden=true}}
refresh();setInterval(refresh,30000)})();</script>"#;

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
        use crate::lifecycle::{OwnerGate, OwnerProbe};
        use crate::process_identity::{self, KillOutcome, Liveness, ProcessIdentity};
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
        if !crate::update::release_owner_image(&root, &current, &old.exe) {
            return Err("retirement image is not current".into());
        }
        let data = root.root().join("runtime/data");
        crate::lifecycle::validate_existing_owner_gate_paths(&data)?;
        let mut owners = Vec::new();
        for (name, role) in [
            (crate::lifecycle::SUPERVISOR_LOCK, "supervisor"),
            (crate::lifecycle::LIVE_LOCK, "live"),
            ("rbridge-console.lock", "console"),
        ] {
            let gate = OwnerGate::new(data.join("logs").join(name));
            let marker = match gate.probe_existing() {
                OwnerProbe::Free { .. }
                    if role != "supervisor"
                        || old.pid == 0
                        || matches!(
                            process_identity::check_liveness(old.pid, old.start),
                            Liveness::Gone | Liveness::Mismatch(_)
                        ) =>
                {
                    continue
                }
                OwnerProbe::Held {
                    marker: Some(marker),
                } if marker.role == role => marker,
                _ => return Err(format!("{role} owner is not identified")),
            };
            if !crate::update::release_owner_image(&root, &current, &marker.identity.exe)
                || role == "supervisor" && !process_identity::same_identity(&old, &marker.identity)
            {
                return Err(format!("{role} owner is not the old release"));
            }
            if process_identity::check_identity(&marker.identity) != Liveness::Matches {
                return Err(format!("{role} identity cannot be verified"));
            }
            owners.push((gate, marker, role));
        }
        #[cfg(feature = "parity-clock")]
        let mut retired = 0u64;
        #[cfg(feature = "parity-clock")]
        let fault_after = crate::update::handover_control(&root)?
            .and_then(|(_, value)| value["retireFailAfter"].as_u64());
        #[cfg(feature = "parity-clock")]
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
                let _gate = context.open_locked_file(
                    crate::runtime::RuntimeArea::Data, "locks/release-check.lock",
                )?;
                let mut state = read_state(&context)?;
                state.update_error = Some(reason.clone());
                save_state(&context, &state)?;
                return Err(reason);
            }
        }
        for (gate, marker, role) in owners {
            match process_identity::kill_verified(&marker.identity) {
                KillOutcome::Gone | KillOutcome::Killed => stopped = true,
                _ => return Err(format!("{role} owner could not be retired")),
            }
            #[cfg(feature = "parity-clock")]
            {
                retired += 1;
                if fault_after == Some(retired) {
                    return Err("injected retirement failure".into());
                }
            }
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                match gate.probe_existing() {
                    OwnerProbe::Free { .. } => break,
                    _ if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
                    _ => return Err(format!("{role} owner did not release")),
                }
            }
        }
        Ok(())
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
    handover_with_policy(context, target, false)
}

fn handover_with_policy(context: &RuntimeContext, target: &str, automatic: bool) -> Result<u32, String> {
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
    let gate = crate::lifecycle::OwnerGate::new(
        context
            .data_dir()
            .join("logs")
            .join(crate::lifecycle::SUPERVISOR_LOCK),
    );
    let old = match gate.probe_existing() {
        crate::lifecycle::OwnerProbe::Held {
            marker: Some(marker),
        } if marker.role == "supervisor" => marker.identity,
        _ => return Err("supervisor owner cannot be identified".into()),
    };
    let current = fs::canonicalize(root.root().join("current/her.exe"))
        .map_err(|_| "current image unavailable")?;
    if !crate::update::same_release_image(&current.to_string_lossy(), &old.exe) {
        return Err("supervisor is not the current release".into());
    }
    if crate::process_identity::check_identity(&old) != crate::process_identity::Liveness::Matches {
        return Err("supervisor identity cannot be verified".into());
    }
    if automatic {
        crate::update::launch_automatic(&root, target, &old)
    } else {
        crate::update::launch(&root, target, &old)
    }
}

fn retire_current_cli(args: &[String]) -> i32 {
    let result = (|| -> Result<Vec<String>, String> {
        if args.len() != 2 || args[0] != "--tag" {
            return Err("invalid retirement tag".into());
        }
        let root = installation()?;
        if current(&root).as_deref() != Some(&args[1]) {
            return Err("retirement tag is not current".into());
        }
        let image = crate::update::release_path(&root, &args[1])?.join("her.exe");
        let gate = crate::lifecycle::OwnerGate::new(
            root.root()
                .join("runtime/data/logs")
                .join(crate::lifecycle::SUPERVISOR_LOCK),
        );
        let identity = match gate.probe_existing() {
            crate::lifecycle::OwnerProbe::Held {
                marker: Some(marker),
            } if marker.role == "supervisor" => marker.identity,
            crate::lifecycle::OwnerProbe::Free { .. } => crate::process_identity::ProcessIdentity {
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
    use crate::lifecycle::{OwnerGate, OwnerProbe};
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
        OwnerGate::new(
            selected
                .join("data/logs")
                .join(crate::lifecycle::SUPERVISOR_LOCK)
        )
        .probe_existing(),
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
