//! Shared release handover admission and read-only trial image authority.
use crate::{install_root::InstallRoot, process_identity::ProcessIdentity, release_store};
use std::{
    fs,
    path::{Path, PathBuf},
};
pub fn release_path(root: &InstallRoot, tag: &str) -> Result<PathBuf, String> {
    release_store::version(tag).ok_or("release tag must be vX.Y.Z")?;
    let path = root.root().join("releases").join(tag);
    if !path.is_dir() {
        return Err("installed release directory unavailable".into());
    }
    release_store::directory(&path)?;
    Ok(path)
}

fn installed_metadata(root: &InstallRoot, tag: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    use crate::release_signature::{
        MAX_CHECKSUM_BYTES, MAX_SIGNATURE_BYTES, REFUSED, SIGNATURE_NAME,
    };
    let path = release_path(root, tag)?;
    let sums = release_store::read_bounded(&path.join("SHA256SUMS"), MAX_CHECKSUM_BYTES)
        .map_err(|_| REFUSED)?;
    let signature = release_store::read_bounded(&path.join(SIGNATURE_NAME), MAX_SIGNATURE_BYTES)
        .map_err(|_| REFUSED)?;
    Ok((sums, signature))
}
/// Hashes every image once and returns the fixed helper's verified bytes, so publication
/// never rereads a file that could have changed after verification.
fn verify_images(root: &InstallRoot, tag: &str, sums: &[u8]) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};
    let path = release_path(root, tag)?;
    let hashes = release_store::sums(sums)?;
    let mut helper = Vec::new();
    for name in release_store::IMAGES {
        let file = path.join(name);
        let meta = fs::symlink_metadata(&file).map_err(|_| "installed image unavailable")?;
        if !meta.is_file() || release_store::redirected(&meta) {
            return Err("installed image is redirected".into());
        }
        let bytes = fs::read(file).map_err(|_| "installed image unreadable")?;
        if format!("{:x}", Sha256::digest(&bytes)) != hashes[name] {
            return Err("installed image checksum mismatch".into());
        }
        if name == "herupdate.exe" {
            helper = bytes;
        }
    }
    Ok(helper)
}
/// Verified installed metadata supplies authority; staging cannot replace a valid selection.
pub(crate) struct Authority {
    pub current: Option<String>,
    identity: crate::release_signature::Identity,
    pub stamp: String,
    checksum: Vec<u8>,
    /// Selected current whose own metadata no longer verifies (authority was recovered).
    pub lost: Option<String>,
}
/// Reason recorded once when another installed release supplies current's authority.
pub(crate) const LOST_CURRENT: &str =
    "current release metadata unverifiable; using verified installed authority";
/// Read-only: callers on read paths never alert. Admission uses `admitted_authority`.
pub(crate) fn authority(root: &InstallRoot) -> Result<Authority, String> {
    let current = link_tag(root, "current");
    if let Ok(Some(tag)) = &current {
        if let Ok(authority) = installed_authority(root, tag) {
            return Ok(authority);
        }
    }
    let mut authority = recovered_authority(root)?;
    authority.lost = current.ok().flatten();
    Ok(authority)
}
/// Check/install admission records a refusal or recovered authority as one alert.
/// `restore_gate` is the update-restore gate when the caller already holds it;
/// `lost_alerted` is the lost current already alerted (`None` alerts it again).
pub(crate) fn admitted_authority(
    root: &InstallRoot,
    restore_gate: Option<&fs::File>,
    lost_alerted: &mut Option<String>,
) -> Result<Authority, String> {
    let authority = authority(root);
    admission_alert(root, &authority, restore_gate, lost_alerted)?;
    authority
}
/// Admission alerts never replace an undelivered alert or a rollback in progress.
/// A lost current alerts once per `lost_alerted` memory; a refusal on every admission.
pub(crate) fn admission_alert(
    root: &InstallRoot,
    authority: &Result<Authority, String>,
    restore_gate: Option<&fs::File>,
    lost_alerted: &mut Option<String>,
) -> Result<(), String> {
    let (failed, reason) = match authority {
        Ok(authority) => {
            let alerted = std::mem::replace(lost_alerted, authority.lost.clone());
            match &authority.lost {
                Some(tag) if alerted.as_ref() != Some(tag) => (tag.clone(), LOST_CURRENT),
                _ => return Ok(()),
            }
        }
        Err(_) => (
            link_tag(root, "current")
                .ok()
                .flatten()
                .unwrap_or_else(|| "unselected".into()),
            crate::release_signature::REFUSED,
        ),
    };
    let _gate = alert_gate(root)?;
    if let Ok(Some(alert)) = read_alert(root) {
        if !alert.delivered || alert.rollback == "pending" && rollback_active(root, restore_gate) {
            return Ok(());
        }
    }
    let mut alert = UpdateAlert::new(&failed, "unchanged", reason);
    alert.rollback = "preserved".into();
    store_alert(root, &alert)
}
/// A rollback holds the update-restore gate throughout, so a `pending` alert is still in
/// progress only while someone else holds that gate; otherwise its helper died and it is stale.
fn rollback_active(root: &InstallRoot, restore_gate: Option<&fs::File>) -> bool {
    restore_gate.is_none()
        && release_store::installation_gate(
            &root.root().join("runtime/data/locks/update-restore.lock"),
        )
        .is_err()
}
fn recovered_authority(root: &InstallRoot) -> Result<Authority, String> {
    use crate::release_signature;
    // The newest verified recorded selection wins: last-good, or the lost current
    // retained by an interrupted switch, so an older last-good cannot lower its floor.
    let mut recorded = ["last-good", ".last-good-previous", ".current-previous"]
        .iter()
        .filter_map(|name| link_tag(root, name).ok().flatten())
        .filter_map(|tag| Some((release_store::version(&tag)?, tag)))
        .collect::<Vec<_>>();
    recorded.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, tag) in recorded {
        if let Ok(authority) = installed_authority(root, &tag) {
            return Ok(authority);
        }
    }
    let releases = root.root().join("releases");
    let mut occupied = [
        "current",
        "last-good",
        ".current-previous",
        ".last-good-previous",
    ]
    .iter()
    .any(|name| fs::symlink_metadata(root.root().join(name)).is_ok());
    let mut candidates = Vec::new();
    match fs::symlink_metadata(&releases) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(release_signature::REFUSED.into()),
        Ok(_) => {
            release_store::directory(&releases)?;
            for entry in fs::read_dir(&releases).map_err(|_| release_signature::REFUSED)? {
                let entry = entry.map_err(|_| release_signature::REFUSED)?;
                let name = entry.file_name();
                let tag = name.to_string_lossy();
                let meta =
                    fs::symlink_metadata(entry.path()).map_err(|_| release_signature::REFUSED)?;
                // Private, unpublished staging is not an installed release.
                if tag.starts_with(".staging-her-release-v1-") {
                    continue;
                }
                // Non-versioned local/bootstrap folders (including the invoking
                // image's) are not installed releases and never occupy the root.
                let Some(version) = release_store::version(&tag) else {
                    continue;
                };
                if meta.is_dir() || release_store::redirected(&meta) {
                    occupied = true;
                    candidates.push((version, tag.into_owned()));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, tag) in candidates {
        if let Ok(authority) = installed_authority(root, &tag) {
            return Ok(authority);
        }
    }
    if occupied {
        return Err(release_signature::REFUSED.into());
    }
    Ok(Authority {
        current: None,
        identity: release_signature::Identity {
            generation: 0,
            keys: release_signature::pinned_keys(),
        },
        stamp: release_signature::trust_id(),
        checksum: Vec::new(),
        lost: None,
    })
}
fn installed_authority(root: &InstallRoot, tag: &str) -> Result<Authority, String> {
    use crate::release_signature;
    use sha2::{Digest, Sha256};
    let (sums, signature) = installed_metadata(root, tag)?;
    let identity = release_signature::identity(&sums, tag)?;
    release_signature::verify_with_keys(&sums, &signature, &identity.keys)?;
    Ok(Authority {
        current: Some(tag.into()),
        identity,
        stamp: format!(
            "{tag}:{:x}:{:x}",
            Sha256::digest(&sums),
            Sha256::digest(&signature)
        ),
        checksum: sums,
        lost: None,
    })
}
impl Authority {
    /// The version floor: current, or a higher selected current whose metadata was lost.
    pub(crate) fn floor(&self) -> Option<&str> {
        match (self.current.as_deref(), self.lost.as_deref()) {
            (Some(current), Some(lost)) => Some(
                if release_store::version(lost) > release_store::version(current) {
                    lost
                } else {
                    current
                },
            ),
            (current, lost) => current.or(lost),
        }
    }
    pub(crate) fn verify_candidate(
        &self,
        tag: &str,
        sums: &[u8],
        signature: &[u8],
    ) -> Result<(), String> {
        use crate::release_signature::{self, REFUSED};
        release_signature::verify_with_keys(sums, signature, &self.identity.keys)?;
        let identity = release_signature::identity(sums, tag)?;
        if identity.generation < self.identity.generation
            || identity.keys != self.identity.keys
                && identity.generation <= self.identity.generation
            || self
                .floor()
                .is_some_and(|floor| release_store::version(tag) <= release_store::version(floor))
        {
            return Err(REFUSED.into());
        }
        // The transition signer must survive in the new set, so installed metadata
        // remains independently verifiable during current selection and fallback.
        release_signature::verify_with_keys(sums, signature, &identity.keys)
    }
}
/// Returns the verified candidate helper bytes.
pub(crate) fn verify(root: &InstallRoot, tag: &str) -> Result<Vec<u8>, String> {
    let result = authority(root).and_then(|authority| verify_candidate(root, tag, &authority));
    if result.is_err() {
        let kept = link_tag(root, "current")
            .ok()
            .flatten()
            .unwrap_or_else(|| "unchanged".into());
        signature_refusal(root, tag, &kept)?;
    }
    result
}
pub(crate) fn verify_candidate(
    root: &InstallRoot,
    tag: &str,
    authority: &Authority,
) -> Result<Vec<u8>, String> {
    let (sums, signature) = installed_metadata(root, tag)?;
    authority.verify_candidate(tag, &sums, &signature)?;
    verify_images(root, tag, &sums)
}
/// An installed fallback keeps its own authenticated set, even across key rotation.
/// Returns its verified helper bytes.
pub(crate) fn verify_rollback(root: &InstallRoot, tag: &str) -> Result<Vec<u8>, String> {
    let authority = installed_authority(root, tag)?;
    verify_images(root, tag, &authority.checksum)
}
/// Crash recovery restores and publishes the helper of an independently verified
/// last-good only, never the failed release (whose own CLI may still retire its
/// owners while it verifies, see `retirement_release`).
pub(crate) fn recovery_release(
    root: &InstallRoot,
    failed: &str,
    last_good: Option<&str>,
) -> Result<String, String> {
    recovery_helper(root, failed, last_good).map(|(tag, _)| tag)
}
fn recovery_helper(
    root: &InstallRoot,
    failed: &str,
    last_good: Option<&str>,
) -> Result<(String, Vec<u8>), String> {
    let result = last_good
        .filter(|tag| *tag != failed)
        .ok_or_else(|| crate::release_signature::REFUSED.to_string())
        .and_then(|tag| verify_rollback(root, tag).map(|helper| (tag.to_string(), helper)));
    if result.is_err() {
        signature_refusal(root, failed, failed)?;
    }
    result
}
/// A failed release retires its owners through its own CLI only while its installed
/// metadata and images still verify; otherwise through the already verified last-good.
pub(crate) fn retirement_release(
    root: &InstallRoot,
    failed: &str,
    verified_last_good: &str,
) -> String {
    if verify_rollback(root, failed).is_ok() {
        failed.into()
    } else {
        verified_last_good.into()
    }
}
pub(crate) fn verify_handover(root: &InstallRoot, tag: &str, current: &str) -> Result<(), String> {
    verify_candidate(root, tag, &installed_authority(root, current)?).map(drop)
}
/// Helper, handover and recovery refusals always record their alert, replacing any
/// earlier one (as rollback does); only admission alerts defer to existing alerts.
pub(crate) fn signature_refusal(
    root: &InstallRoot,
    failed: &str,
    kept: &str,
) -> Result<(), String> {
    let mut alert = UpdateAlert::new(failed, kept, crate::release_signature::REFUSED);
    alert.rollback = "preserved".into();
    save_alert(root, &alert)
}

/// Only HER images in the same physical release can authorize each other.
pub fn same_release_image(expected: &str, observed: &str) -> bool {
    let (Ok(a), Ok(b)) = (fs::canonicalize(expected), fs::canonicalize(observed)) else {
        return false;
    };
    let her = |p: &Path| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            ["her", "her-cli", "her.exe", "her-cli.exe"]
                .iter()
                .any(|s| n.eq_ignore_ascii_case(s))
        })
    };
    her(&a) && her(&b) && a.parent() == b.parent()
}

/// Release membership for retiring recorded owners even after an image is lost.
/// This alone grants no process authority: callers must still verify PID/start/exe.
pub(crate) fn release_owner_image(root: &InstallRoot, tag: &str, image: &str) -> bool {
    let image = Path::new(image);
    if !image
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            ["her", "her-cli", "her.exe", "her-cli.exe"]
                .iter()
                .any(|her| name.eq_ignore_ascii_case(her))
        })
    {
        return false;
    }
    let (Some(parent), Ok(release)) = (image.parent(), release_path(root, tag)) else {
        return false;
    };
    match (fs::canonicalize(parent), fs::canonicalize(release)) {
        (Ok(parent), Ok(release)) => parent == release,
        _ => false,
    }
}

/// Trial authority is read-only and restricted to the installed current release.
/// It never participates in process adoption or termination authorization.
pub fn health_image(caller: &str, owner: &str) -> bool {
    if same_release_image(caller, owner) || crate_identity_equal(caller, owner) {
        return true;
    }
    let Some(selected) = std::env::var_os("HER_TRIAL_CURRENT") else {
        return false;
    };
    let Ok(root) = InstallRoot::configured() else {
        return false;
    };
    if root.require_release_image(Path::new(caller)).is_err() {
        return false;
    }
    let (Ok(selected), Ok(current)) = (
        fs::canonicalize(selected),
        fs::canonicalize(root.root().join("current")),
    ) else {
        return false;
    };
    selected == current && same_release_image(&selected.join("her.exe").to_string_lossy(), owner)
}
fn crate_identity_equal(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.replace('/', "\\")
            .eq_ignore_ascii_case(&b.replace('/', "\\"))
    } else {
        a == b
    }
}

/// Publish the verified helper outside releases and run it hidden. The helper
/// judges the trial before it retires the existing supervisor.
pub fn launch(root: &InstallRoot, target: &str, old: &ProcessIdentity) -> Result<u32, String> {
    launch_mode(root, target, old, false, false)
}
pub fn launch_automatic(
    root: &InstallRoot,
    target: &str,
    old: &ProcessIdentity,
) -> Result<u32, String> {
    launch_mode(root, target, old, false, true)
}
pub fn launch_recovery(
    root: &InstallRoot,
    target: &str,
    old: &ProcessIdentity,
) -> Result<u32, String> {
    launch_mode(root, target, old, true, false)
}
fn launch_mode(
    root: &InstallRoot,
    target: &str,
    old: &ProcessIdentity,
    recovery: bool,
    automatic: bool,
) -> Result<u32, String> {
    use std::process::Stdio;
    let gate = release_store::installation_gate(
        &root.root().join("runtime/data/locks/update-restore.lock"),
    )?;
    // Publish exactly the helper bytes that were verified; never reread the file.
    let bytes = if recovery {
        let (current, last_good, _) = startup_selection(root)?;
        if current.as_deref() != Some(target) {
            return Err("current changed before crash recovery".into());
        }
        recovery_helper(root, target, last_good.as_deref())?.1
    } else {
        verify(root, target)?
    };
    #[cfg(feature = "parity-clock")]
    fixture_barrier(root, target, "pauseBeforeHelperPublish", "publish")?;
    let helper = root.root().join("herupdate.exe");
    release_store::write_atomic(&helper, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .map_err(|_| "updater permissions unavailable")?;
    }
    drop(gate);
    let mut command = crate::subprocess::command_without_console(&helper);
    if recovery {
        command.arg("--recover");
    }
    if automatic {
        command.arg("--auto");
    }
    command
        .args([
            "--target",
            target,
            "--old-pid",
            &old.pid.to_string(),
            "--old-start",
            &old.start.to_string(),
            "--old-exe",
            &old.exe,
        ])
        .env(
            "HER_INSTALL_ROOT",
            crate::subprocess::external_path(root.root())?,
        )
        .current_dir(crate::subprocess::external_path(root.root())?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(feature = "parity-clock")]
    if let Some((path, _)) = handover_control(root)? {
        let log = path.with_extension("helper.stderr");
        root.check_development(&log)?;
        command.stderr(fs::File::create(log).map_err(|_| "fixture helper log unavailable")?);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "fixed updater could not start")?;
    let pid = child.id();
    #[cfg(feature = "parity-clock")]
    if std::env::var_os("HER_TEST_UPDATE_PLATFORM").is_some() {
        child
            .wait()
            .map_err(|_| "fixture updater could not finish")?;
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAlert {
    v: u8,
    pub id: String,
    pub failed: String,
    pub restored: String,
    pub reason: String,
    pub rollback: String,
    pub delivered: bool,
    retry_after: i64,
}
impl UpdateAlert {
    pub fn new(failed: &str, restored: &str, reason: &str) -> Self {
        Self {
            v: 1,
            id: format!("{}-{}", wall_clock_ms(), std::process::id()),
            failed: failed.into(),
            restored: restored.into(),
            reason: reason.into(),
            rollback: "pending".into(),
            delivered: false,
            retry_after: 0,
        }
    }
}
/// One shared health snapshot's peer-probe allowance.
pub const HEALTH_PROBE_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(5);
pub fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn alert_gate(root: &InstallRoot) -> Result<fs::File, String> {
    state_gate(root, "update-alert.lock")
}
fn read_alert(root: &InstallRoot) -> Result<Option<UpdateAlert>, String> {
    let path = root.root().join("update-alert.json");
    let meta = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("update alert unavailable".into()),
        Ok(meta) => meta,
    };
    if !meta.is_file() || release_store::redirected(&meta) {
        return Err("update alert redirected".into());
    }
    let alert: UpdateAlert =
        serde_json::from_slice(&fs::read(path).map_err(|_| "update alert unavailable")?)
            .map_err(|_| "update alert invalid")?;
    if alert.v != 1 {
        return Err("update alert version unsupported".into());
    }
    Ok(Some(alert))
}
fn store_alert(root: &InstallRoot, alert: &UpdateAlert) -> Result<(), String> {
    release_store::write_atomic(
        &root.root().join("update-alert.json"),
        &serde_json::to_vec(alert).map_err(|_| "update alert cannot serialize")?,
    )
}
pub fn save_alert(root: &InstallRoot, alert: &UpdateAlert) -> Result<(), String> {
    let _gate = alert_gate(root)?;
    store_alert(root, alert)
}
/// Finish only this rollback, preserving supervisor delivery claims/completion.
pub fn finish_rollback(root: &InstallRoot, id: &str, status: &str) -> Result<(), String> {
    let _gate = alert_gate(root)?;
    let mut alert = read_alert(root)?
        .filter(|alert| alert.id == id)
        .ok_or("rollback alert changed before completion")?;
    alert.rollback = status.into();
    store_alert(root, &alert)
}
pub fn claim_alert(root: &InstallRoot, now: i64) -> Result<Option<UpdateAlert>, String> {
    let _gate = alert_gate(root)?;
    let Some(mut alert) = read_alert(root)? else {
        return Ok(None);
    };
    if alert.delivered || now < alert.retry_after {
        return Ok(None);
    }
    alert.retry_after = now.saturating_add(60_000);
    store_alert(root, &alert)?;
    Ok(Some(alert))
}
pub fn complete_alert(root: &InstallRoot, id: &str) -> Result<(), String> {
    let _gate = alert_gate(root)?;
    if let Some(mut alert) = read_alert(root)?.filter(|alert| alert.id == id) {
        alert.delivered = true;
        store_alert(root, &alert)?;
    }
    Ok(())
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Starts {
    v: u8,
    releases: std::collections::BTreeMap<String, Vec<i64>>,
}
fn starts_gate(root: &InstallRoot) -> Result<fs::File, String> {
    state_gate(root, "update-starts.lock")
}
fn read_starts(root: &InstallRoot) -> Result<Starts, String> {
    let path = root.root().join("update-starts.json");
    let meta = match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Starts {
                v: 1,
                ..Starts::default()
            })
        }
        Err(_) => return Err("release start history unavailable".into()),
        Ok(m) => m,
    };
    if !meta.is_file() || release_store::redirected(&meta) {
        return Err("release start history redirected".into());
    }
    let starts: Starts =
        serde_json::from_slice(&fs::read(path).map_err(|_| "release start history unavailable")?)
            .map_err(|_| "release start history invalid")?;
    if starts.v != 1 {
        return Err("release start history version unsupported".into());
    }
    Ok(starts)
}
fn save_starts(root: &InstallRoot, starts: &Starts) -> Result<(), String> {
    release_store::write_atomic(
        &root.root().join("update-starts.json"),
        &serde_json::to_vec(starts).map_err(|_| "release starts cannot serialize")?,
    )
}
fn within_window(at: i64, now: i64) -> bool {
    at <= now && now.saturating_sub(at) <= 600_000
}
pub fn record_start(root: &InstallRoot, tag: &str, now: i64) -> Result<usize, String> {
    release_store::version(tag).ok_or("invalid start-history release")?;
    let _gate = starts_gate(root)?;
    let mut starts = read_starts(root)?;
    let times = starts.releases.entry(tag.into()).or_default();
    times.retain(|at| within_window(*at, now));
    times.push(now);
    let count = times.len();
    save_starts(root, &starts)?;
    Ok(count)
}
pub fn start_count(root: &InstallRoot, tag: &str, now: i64) -> Result<usize, String> {
    let _gate = starts_gate(root)?;
    Ok(read_starts(root)?.releases.get(tag).map_or(0, |times| {
        times.iter().filter(|at| within_window(**at, now)).count()
    }))
}
pub fn mark_healthy(root: &InstallRoot, tag: &str) -> Result<(), String> {
    let _gate = starts_gate(root)?;
    let mut starts = read_starts(root)?;
    starts.releases.remove(tag);
    save_starts(root, &starts)
}
pub fn link_tag(root: &InstallRoot, name: &str) -> Result<Option<String>, String> {
    match fs::symlink_metadata(root.root().join(name)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("release link unreadable".into()),
        Ok(_) => {}
    }
    let path = fs::canonicalize(root.root().join(name)).map_err(|_| "release link unavailable")?;
    let releases =
        fs::canonicalize(root.root().join("releases")).map_err(|_| "releases unavailable")?;
    if path.parent() != Some(releases.as_path()) {
        return Err("release link is outside releases".into());
    }
    let tag = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("release tag unavailable")?;
    release_path(root, tag)?;
    Ok(Some(tag.into()))
}
#[cfg(feature = "parity-clock")]
fn fixture_platform(root: &InstallRoot) -> Result<Option<serde_json::Value>, String> {
    let Some(path) = std::env::var_os("HER_TEST_UPDATE_PLATFORM") else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    root.check_development(&path)?;
    serde_json::from_slice(&fs::read(path).map_err(|_| "fake platform unavailable")?)
        .map(Some)
        .map_err(|_| "fake platform invalid".into())
}
pub fn startup_selection(
    root: &InstallRoot,
) -> Result<(Option<String>, Option<String>, i64), String> {
    #[cfg(feature = "parity-clock")]
    if let Some(state) = fixture_platform(root)? {
        return Ok((
            state["current"].as_str().map(str::to_string),
            state["lastGood"].as_str().map(str::to_string),
            state["now"].as_i64().ok_or("fake clock missing")?,
        ));
    }
    // The helper finishes interrupted publication under its installation gate;
    // startup must still find the verified fallback needed to publish that helper.
    let last_good = match link_tag(root, "last-good")? {
        Some(tag) => Some(tag),
        None => link_tag(root, ".last-good-previous")?,
    };
    Ok((link_tag(root, "current")?, last_good, wall_clock_ms()))
}

// A supervisor can persist a start while the helper judges it. Serialize those
// short state transactions without treating a healthy overlap as update failure.
fn state_gate(root: &InstallRoot, name: &str) -> Result<fs::File, String> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match release_store::installation_gate(&root.root().join("runtime/data/locks").join(name)) {
            Err(error)
                if error == "another release installation or restore is running"
                    && std::time::Instant::now() < end =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            result => return result,
        }
    }
}

/// Pause at a named fixture boundary for `tag` until the process-level fixture resumes it.
#[cfg(feature = "parity-clock")]
pub(crate) fn fixture_barrier(
    root: &InstallRoot,
    tag: &str,
    key: &str,
    phase: &str,
) -> Result<(), String> {
    let Some((path, control)) = handover_control(root)? else {
        return Ok(());
    };
    if !control[key]
        .as_array()
        .is_some_and(|tags| tags.iter().any(|value| value == tag))
    {
        return Ok(());
    }
    let waiting = path.with_extension(format!("{tag}-{phase}.waiting"));
    let resume = path.with_extension(format!("{tag}-{phase}.resume"));
    root.check_development(&waiting)?;
    root.check_development(&resume)?;
    fs::write(waiting, "ready").map_err(|_| "start barrier unavailable")?;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !resume.is_file() {
        if std::time::Instant::now() >= end {
            return Err("start barrier timed out".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

/// Guarded fault/clock controls for process-level handover regression fixtures.
#[cfg(feature = "parity-clock")]
pub(crate) fn handover_control(
    root: &InstallRoot,
) -> Result<Option<(PathBuf, serde_json::Value)>, String> {
    let Some(path) = std::env::var_os("HER_TEST_HANDOVER_CONTROL") else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    root.check_development(&path)?;
    let value =
        serde_json::from_slice(&fs::read(&path).map_err(|_| "handover control unavailable")?)
            .map_err(|_| "handover control invalid")?;
    Ok(Some((path, value)))
}
