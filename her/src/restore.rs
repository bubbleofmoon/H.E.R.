//! Confirmed, transactional restore of a verified selected-runtime backup.
use crate::console_auth::{self, GenerationWriteError};
use crate::runtime::{AuthorityGuard, PathIntent, RuntimeArea, RuntimeContext};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Ordinary filesystem spelling for operator-facing text; the verbatim form survives only when
/// `subprocess::external_path` has no safe ordinary spelling.
fn ordinary_path(path: &Path) -> String {
    crate::subprocess::external_path(path).unwrap_or_else(|_| path.display().to_string())
}

struct Staging(PathBuf);
impl Staging {
    fn new() -> Result<Self, String> {
        let mut id = [0u8; 16];
        getrandom::getrandom(&mut id).map_err(|e| e.to_string())?;
        let path =
            std::env::temp_dir().join(format!("her-restore-{:032x}", u128::from_ne_bytes(id)));
        crate::install_root::check_development_path(&path)?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|e| e.to_string())?;
        let staging = Self(path);
        crate::guarded_credentials::restrict_credential(&staging.0)
            .map_err(|_| "restore staging permissions unavailable".to_string())?;
        Ok(staging)
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Prepared {
    _staging: Staging,
    zip: PathBuf,
    files: BTreeMap<String, (RuntimeArea, String)>,
    state_included: bool,
    config: Option<Vec<u8>>,
    view: Value,
}

fn location(name: &str) -> Result<(RuntimeArea, String), String> {
    let simple = |s: &str| !s.is_empty() && !s.contains(['/', '\\', ':']) && s != "." && s != "..";
    match name {
        "memory.db" | "conversations.db" | "settings.db" => Ok((RuntimeArea::Data, name.into())),
        "config.json" => Ok((RuntimeArea::Config, "config.json".into())),
        console_auth::RESTORE_GENERATION_FILE => Err("cannot restore a destination console generation".into()),
        "说明.md" => Ok((RuntimeArea::Resources, name.into())),
        _ if name.starts_with("state/") => {
            let relative = &name[6..];
            if !simple(relative) || !relative.ends_with(".json") {
                return Err("unsupported restore state path".into());
            }
            Ok((RuntimeArea::Data, relative.into()))
        }
        _ if name.starts_with("roles/")
            || name.starts_with("patches/")
            || name.starts_with("docs/") =>
        {
            if !name.split('/').all(simple)
                || (name.starts_with("roles/") && name.split('/').count() != 2)
            {
                return Err("unsupported restore resource path".into());
            }
            Ok((RuntimeArea::Resources, name.into()))
        }
        _ => Err(format!("unsupported restore payload: {name}")),
    }
}

fn label(area: RuntimeArea, relative: &str) -> String {
    format!(
        "{}/{relative}",
        match area {
            RuntimeArea::Config => "config",
            RuntimeArea::Resources => "resources",
            _ => "data",
        }
    )
}

/// How long a restore retries removing a file another process holds open. The supervisor keeps running through a
/// restore and reads `settings.db` through the configuration loader, and Windows refuses to remove a file SQLite has
/// open. The replacement retries the same violations for the same time (`guarded_file::Staged::commit`).
const OPEN_FILE_WAIT: Duration = Duration::from_secs(1);

/// Runs `operation` again while it fails with a Windows access, sharing or lock violation (OS errors 5, 32 and 33)
/// and `deadline` has not passed. `windows` is `cfg!(windows)` outside the policy test, so other hosts try once.
fn retry_open_file(
    windows: bool,
    deadline: Instant,
    mut operation: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    loop {
        match operation() {
            Err(error)
                if windows
                    && matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

/// Removes a runtime file as `RuntimeContext::remove_file` does (the path resolved twice and compared), retrying
/// while another process holds it open (`retry_open_file`). A file that is already gone counts as removed.
fn remove_runtime_file(
    context: &RuntimeContext,
    area: RuntimeArea,
    relative: &str,
) -> Result<(), String> {
    let path = context.resolve_path(area, relative, PathIntent::ReadOptionalFile)?;
    if context.resolve_path(area, relative, PathIntent::ReadOptionalFile)? != path {
        return Err("runtime path is unavailable".into());
    }
    match retry_open_file(cfg!(windows), Instant::now() + OPEN_FILE_WAIT, || {
        fs::remove_file(&path)
    }) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result.map_err(|e| format!("cannot remove runtime file: {e}")),
    }
}

fn current_files(
    context: &RuntimeContext,
    memory_only: bool,
) -> Result<BTreeMap<String, (RuntimeArea, String)>, String> {
    let mut files = BTreeMap::from([("memory.db".into(), location("memory.db")?)]);
    if memory_only {
        return Ok(files);
    }
    let conversations = crate::backup::conversation_db_path(context)?;
    if conversations.try_exists().map_err(|e| e.to_string())? {
        files.insert("conversations.db".into(), location("conversations.db")?);
    }
    if crate::settings::store_path(context)?
        .try_exists()
        .map_err(|e| e.to_string())?
    {
        files.insert("settings.db".into(), location("settings.db")?);
    }
    files.insert("config.json".into(), location("config.json")?);
    let data = context.resolve_path(RuntimeArea::Data, "", PathIntent::ReadDirectory)?;
    for entry in fs::read_dir(data).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "state filename is not UTF-8")?;
        if name.ends_with(".json") {
            context.resolve_path(RuntimeArea::Data, &name, PathIntent::ReadFile)?;
            files.insert(format!("state/{name}"), (RuntimeArea::Data, name));
        }
    }
    fn tree(
        context: &RuntimeContext,
        relative: &str,
        recursive: bool,
        out: &mut BTreeMap<String, (RuntimeArea, String)>,
    ) -> Result<(), String> {
        let base = context.resolve_path(RuntimeArea::Resources, "", PathIntent::ReadDirectory)?;
        match fs::symlink_metadata(base.join(relative)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
            Ok(_) => {}
        }
        let path =
            context.resolve_path(RuntimeArea::Resources, relative, PathIntent::ReadDirectory)?;
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "resource filename is not UTF-8")?;
            let child = format!("{relative}/{name}");
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                context.resolve_path(RuntimeArea::Resources, &child, PathIntent::ReadDirectory)?;
                if recursive {
                    tree(context, &child, true, out)?;
                }
            } else {
                context.resolve_path(RuntimeArea::Resources, &child, PathIntent::ReadFile)?;
                out.insert(child.clone(), (RuntimeArea::Resources, child));
            }
        }
        Ok(())
    }
    for (name, recursive) in [("roles", false), ("patches", true), ("docs", true)] {
        tree(context, name, recursive, &mut files)?;
    }
    let readme = context.resolve_path(
        RuntimeArea::Resources,
        "说明.md",
        PathIntent::ReadOptionalFile,
    )?;
    if readme.exists() {
        files.insert("说明.md".into(), location("说明.md")?);
    }
    Ok(files)
}

impl Prepared {
    fn open(context: &RuntimeContext, path: &Path, memory_only: bool) -> Result<Self, String> {
        Self::open_internal(context, path, memory_only, true)
    }

    fn open_internal(context: &RuntimeContext, path: &Path, memory_only: bool, check_destination: bool) -> Result<Self, String> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(path)
        };
        crate::install_root::check_runtime(&absolute, false)?;
        let mut ancestor = PathBuf::new();
        for component in absolute.components() {
            ancestor.push(component);
            if !ancestor.has_root() || matches!(component, std::path::Component::Prefix(_)) {
                continue;
            }
            let meta = fs::symlink_metadata(&ancestor).map_err(|e| e.to_string())?;
            if crate::guarded_file::is_link(&meta) {
                return Err("restore archive path is redirected".into());
            }
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut source = options.open(&absolute).map_err(|e| e.to_string())?;
        let metadata = source.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("restore archive must be a regular file".into());
        }
        let staging = Staging::new()?;
        let zip = staging.0.join("source.zip");
        let mut target = File::create(&zip).map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = source.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            target.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        }
        target.sync_all().map_err(|e| e.to_string())?;
        drop(target);
        let (manifest, credentials) = crate::backup::verify_archive_for_restore(&zip, !memory_only)?;
        let mut warnings = Vec::new();
        if !memory_only {
            if !credentials.has_endpoints {
                warnings.push("完全恢复会清除 Settings 中保存的 webSearch.apiKey 和 cliModel.apiKey。");
            }
            let password_warning = if credentials.has_password {
                "完全恢复会使用备份中的管理员密码，并退出所有浏览器会话。"
            } else {
                "完全恢复会清除管理员密码，恢复后需要首次设置密码，并退出所有浏览器会话。"
            };
            warnings.push(password_warning);
        }
        let mut state_included = true;
        let mut config_bytes = None;
        if !memory_only {
            let mut archive = zip::ZipArchive::new(File::open(&zip).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let mut text = String::new();
            archive
                .by_name("config.json")
                .map_err(|e| e.to_string())?
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
            let raw: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .map_err(|e| format!("restore config invalid: {e}"))?;
            let (config, _) = crate::config::canonical_config(&raw)?;
            state_included = config["backup"]["includeState"] != false
                || manifest.files.keys().any(|name| name.starts_with("state/"));
            crate::backup::Settings::from_config_resolved(
                context.runtime_root().ok_or("runtime root missing")?,
                &config,
            )?;
            console_auth::read_protected_auth_file(context, console_auth::TOKEN_FILE, 1024 * 1024)?;
            if check_destination && configured_token(text.as_bytes())?.is_none() {
                let live = console_auth::raw_token_config(context)?;
                if context.effective_console_token(&live).is_empty() { return Err(recovery_error()); }
            }
            config_bytes = Some(text.into_bytes());
        }
        let mut files = BTreeMap::new();
        for name in manifest.files.keys() {
            let entry = location(name)?;
            if !memory_only || name == "memory.db" {
                context.resolve_path(entry.0, &entry.1, PathIntent::ReadOptionalFile)?;
                files.insert(name.clone(), entry);
            }
        }
        let mut replacements = current_files(context, memory_only)?;
        if !state_included {
            replacements.retain(|name, _| !name.starts_with("state/"));
        }
        let removes: Vec<_> = replacements
            .iter()
            .filter(|(name, _)| !files.contains_key(*name))
            .map(|(_, (area, relative))| label(*area, relative))
            .collect();
        replacements.extend(files.clone());
        if !memory_only {
            replacements.insert(console_auth::TOKEN_FILE.into(), (RuntimeArea::Data, console_auth::TOKEN_FILE.into()));
        }
        let replaces: Vec<_> = replacements
            .values()
            .map(|(area, relative)| label(*area, relative))
            .collect();
        let contents: Vec<_> = manifest.files.keys().cloned().collect();
        let scope = if memory_only { "memory" } else { "everything" };
        // Bind both the bytes selected by the administrator and the replacement plan.
        hash.update(
            serde_json::to_vec(&json!([context.runtime_root(), scope, replaces, removes]))
                .map_err(|e| e.to_string())?,
        );
        let confirmation = format!("{:x}", hash.finalize());
        let time = metadata
            .modified()
            .map_err(|e| e.to_string())?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis() as i64;
        let view = json!({"runtimeRoot":context.runtime_root(),"archive":ordinary_path(&absolute),"time":time,"scope":scope,"contents":contents,"replaces":replaces,"removes":removes,"includesState":state_included,"warnings":warnings,"confirmation":confirmation});
        Ok(Self {
            _staging: staging,
            zip,
            files,
            state_included,
            config: config_bytes,
            view,
        })
    }
}

/// Verify and preview without changing desired state, files or backup retention.
pub fn preview(
    context: &RuntimeContext,
    archive: &Path,
    memory_only: bool,
) -> Result<Value, String> {
    if recovery_pending(context) {
        return Err(recovery_error());
    }
    Ok(Prepared::open(context, archive, memory_only)?.view)
}

/// The same persistent gate must span every update and restore, including health/rollback.
pub fn operation_gate(context: &RuntimeContext) -> Result<File, String> {
    context
        .open_locked_file(RuntimeArea::Data, "locks/update-restore.lock")
        .map_err(|_| "an update or another restore is running".into())
}

/// External process/health/notice boundary. File transactions always use the real runtime API.
trait Control {
    fn preflight(&mut self, context: &RuntimeContext) -> Result<(), String>;
    fn stop(&mut self, context: &RuntimeContext, revision: u64) -> Result<(), String>;
    #[cfg(feature = "parity-clock")]
    fn snapshot_ready(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn restart(&mut self, _context: &RuntimeContext) -> Result<(), String> {
        Ok(())
    }
    fn health(&mut self, context: &RuntimeContext, budget: Duration) -> Result<(), String>;
    fn alert(&mut self, context: &RuntimeContext, config: &Value, reason: &str);
    /// Called after the settings files are replaced and removed, before conversation reconciliation.
    fn store_replaced(&mut self, _context: &RuntimeContext) {}
    fn publish_generation(&mut self, context: &RuntimeContext) -> Result<console_auth::Generation, GenerationWriteError> {
        console_auth::rotate_restore_generation(context)
    }
    fn replace(
        &mut self,
        context: &RuntimeContext,
        area: RuntimeArea,
        relative: &str,
        input: &mut dyn Read,
        authority: Option<&AuthorityGuard<'_>>,
    ) -> Result<(), String> {
        context.write_atomic_from(area, relative, input, authority)
    }
}

/// Restore also requires the configured live owner to have published its own ready heartbeat.
fn live_ready(context: &RuntimeContext) -> Result<(), String> {
    use crate::lifecycle::{self, OwnerGate, OwnerProbe, LIVE_LOCK};
    let cfg = context.load_config().map_err(str::to_owned)?;
    let target = crate::watchdog::live_target_from_config(&cfg)?;
    if target.groups.is_empty() {
        return Ok(());
    }
    lifecycle::validate_existing_owner_gate_paths(context.data_dir())?;
    let marker =
        match OwnerGate::new(context.data_dir().join("logs").join(LIVE_LOCK)).probe_existing() {
            OwnerProbe::Held {
                marker: Some(marker),
            } if marker.role == "live" => marker,
            _ => return Err("configured live owner is not held or cannot be identified".into()),
        };
    let me = crate::process_identity::current_identity()?;
    if !crate::lifecycle_cli::authorized_image(&me.exe, &marker.identity.exe)
        || crate::process_identity::check_identity(&marker.identity)
            != crate::process_identity::Liveness::Matches
    {
        return Err("live owner identity cannot be verified".into());
    }
    let same_groups = |actual: &[String]| {
        actual.len() == target.groups.len() && target.groups.iter().all(|g| actual.contains(g))
    };
    if !marker.groups.as_deref().is_some_and(same_groups) {
        return Err("live owner does not serve the configured groups".into());
    }
    let text = context
        .read_optional_text(RuntimeArea::Data, "rust-heartbeat-live.json")?
        .ok_or("live heartbeat is missing")?;
    let heartbeat: Value = serde_json::from_str(&text).map_err(|_| "live heartbeat is invalid")?;
    if heartbeat["pid"].as_u64() != Some(u64::from(marker.identity.pid))
        || !heartbeat["ts"]
            .as_f64()
            .is_some_and(|ts| ts >= marker.acquired_at as f64)
    {
        return Err("live heartbeat does not belong to the current owner".into());
    }
    match crate::watchdog::read_heartbeat(Ok(text.into_bytes()), crate::parity_clock::now_ms()) {
        crate::watchdog::Heartbeat::Fresh { live: true, groups } if same_groups(&groups) => Ok(()),
        _ => Err("live heartbeat is not ready for the configured groups".into()),
    }
}

struct RealControl;
impl Control for RealControl {
    fn preflight(&mut self, context: &RuntimeContext) -> Result<(), String> {
        use crate::lifecycle::{OwnerGate, OwnerProbe, SUPERVISOR_LOCK};
        crate::lifecycle_cli::stoppable_owners(context.data_dir(), false)?;
        let me = crate::process_identity::current_identity()?;
        match OwnerGate::new(context.data_dir().join("logs").join(SUPERVISOR_LOCK)).probe() {
            OwnerProbe::Held { marker: Some(m) }
                if m.role == "supervisor"
                    && crate::lifecycle_cli::authorized_image(&me.exe, &m.identity.exe)
                    && crate::process_identity::check_identity(&m.identity)
                        == crate::process_identity::Liveness::Matches =>
            {
                Ok(())
            }
            OwnerProbe::Free { .. } => Ok(()),
            _ => Err("supervisor owner cannot be verified for restore".into()),
        }
    }
    fn stop(&mut self, context: &RuntimeContext, revision: u64) -> Result<(), String> {
        crate::lifecycle_cli::stop_verified_owners(
            context.data_dir(),
            &crate::lifecycle::DesiredStore::new(context.data_dir().join("lifecycle")),
            revision,
            false,
        )
    }
    fn restart(&mut self, context: &RuntimeContext) -> Result<(), String> {
        use crate::lifecycle::{OwnerGate, OwnerProbe, SUPERVISOR_LOCK};
        use std::process::Stdio;
        self.preflight(context)?;
        if matches!(
            OwnerGate::new(context.data_dir().join("logs").join(SUPERVISOR_LOCK)).probe(),
            OwnerProbe::Held { .. }
        ) {
            return Ok(());
        }
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name(if cfg!(windows) { "her.exe" } else { "her" });
        let path = context.resolve_path(
            RuntimeArea::Data,
            "logs/restore-supervisor.log",
            PathIntent::WriteFile,
        )?;
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        let mut cmd =
            crate::subprocess::command_without_console(crate::subprocess::external_path(exe)?);
        cmd.args(["supervise", "--runtime-root"])
            .arg(crate::subprocess::external_path(
                context.runtime_root().ok_or("runtime root missing")?,
            )?)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(log));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("supervisor restart failed: {e}"))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
    fn health(&mut self, context: &RuntimeContext, budget: Duration) -> Result<(), String> {
        let deadline = Instant::now() + budget;
        let permit = verifying_recovery(context);
        loop {
            let verdict = crate::health_check::check_restore_candidate(
                context.runtime_root().ok_or("runtime root missing")?,
                deadline.min(Instant::now() + Duration::from_secs(5)),
                permit,
            );
            let live = live_ready(context);
            if verdict.ok && live.is_ok() && Instant::now() <= deadline {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "restored bot failed the health check within 2 minutes: {}; live: {}",
                    serde_json::to_string(&verdict).map_err(|e| e.to_string())?,
                    live.err().unwrap_or_else(|| "ready".into())
                ));
            }
            std::thread::sleep(
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    fn alert(&mut self, context: &RuntimeContext, config: &Value, reason: &str) {
        crate::admin_alert::notify_notice_wait(
            context,
            config,
            reason,
            &format!("restore-{}", crate::parity_clock::now_ms()),
        );
    }
}

struct Stopped {
    _tasks: Vec<File>,
    _desired: File,
    _owners: Vec<crate::lifecycle::OwnerGuard>,
}
fn quiesce(
    context: &RuntimeContext,
    by: &str,
    memory_only: bool,
    control: &mut dyn Control,
) -> Result<Stopped, Step> {
    use crate::lifecycle::{self, Acquire, DesiredRead, DesiredState, DesiredStore, OwnerGate};
    let data = context.data_dir();
    lifecycle::validate_owner_gate_paths(data)?;
    let store = DesiredStore::new(data.join("lifecycle"));
    let record = store.write(
        DesiredState::Stopped,
        by,
        "backup restore",
        None,
        crate::parity_clock::now_ms(),
    )?;
    control.stop(context, record.revision)?;
    let gate_error = |error| {
        if memory_only {
            Step::Failed(error)
        } else {
            Step::Refused(error)
        }
    };
    let tasks = lifecycle::hold_stopped_task_gates(data).map_err(gate_error)?;
    let desired = lifecycle::lock_with_wait(
        &data.join("lifecycle/lifecycle.lock"),
        Duration::from_secs(5),
    ).map_err(gate_error)?;
    if !matches!(store.read(), DesiredRead::Present(r) if r.state == DesiredState::Stopped && r.revision == record.revision)
    {
        return Err(Step::Failed("restore stop was superseded".into()));
    }
    let identity = lifecycle::current_identity()?;
    let mut owners = Vec::new();
    for name in [lifecycle::LIVE_LOCK, "rbridge-console.lock"] {
        match OwnerGate::new(data.join("logs").join(name)).acquire(
            "restore",
            identity.clone(),
            None,
            crate::parity_clock::now_ms(),
            Duration::ZERO,
        ) {
            Acquire::Acquired(owner) => owners.push(owner),
            _ => return Err(gate_error("bridge owner is not confirmed stopped".into())),
        }
    }
    Ok(Stopped {
        _tasks: tasks,
        _desired: desired,
        _owners: owners,
    })
}
fn resume(context: &RuntimeContext, by: &str) -> Result<(), String> {
    crate::lifecycle::DesiredStore::new(context.data_dir().join("lifecycle")).write(
        crate::lifecycle::DesiredState::Running,
        by,
        "backup restore restart",
        None,
        crate::parity_clock::now_ms(),
    )?;
    Ok(())
}

/// A refusal after quiescence precedes generation publication and file replacement.
enum Step {
    Refused(String),
    Failed(String),
}
impl From<String> for Step {
    fn from(message: String) -> Self {
        Step::Failed(message)
    }
}
impl Step {
    fn message(self) -> String {
        match self {
            Step::Refused(message) | Step::Failed(message) => message,
        }
    }
}

pub(crate) const RECOVERY_DIR: &str = "console-restore-recovery";
pub(crate) const RECOVERY_INTENT: &str = "console-restore-recovery/intent";
pub(crate) const RECOVERY_REASON: &str = "full restore requires recovery";
const RECOVERY_OUTCOME: &str = "console-restore-recovery/outcome";
const FORWARD_CONFIG: &str = "console-restore-recovery/forward-config";
const ROLLBACK_CONFIG: &str = "console-restore-recovery/rollback-config";
const ROLLBACK_ARCHIVE: &str = "console-restore-recovery/rollback-archive";
const DESTINATION_TOKEN: &str = "console-restore-recovery/destination-token";

pub(crate) fn recovery_pending(context: &RuntimeContext) -> bool {
    context.is_isolated()
        && !matches!(console_auth::read_protected_auth_file(context, RECOVERY_INTENT, 4096), Ok(None))
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum RestorePhase {
    Forward,
    Verifying,
    Rollback,
    Committed,
    RolledBack,
}

impl RestorePhase {
    fn terminal(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryIntent {
    v: u8,
    phase: RestorePhase,
    forward_config: [u8; 32],
    rollback_config: [u8; 32],
    rollback_archive: [u8; 32],
    destination_token: [u8; 32],
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DestinationToken {
    configured: bool,
    effective: String,
    original_file: Option<Vec<u8>>,
}

struct RestorePair {
    token: String,
    config: Vec<u8>,
}

fn recovery_error() -> String {
    RECOVERY_REASON.to_string()
}

fn hash_file(mut file: &File) -> Result<[u8; 32], String> {
    let mut digest = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut bytes).map_err(|_| recovery_error())?;
        if count == 0 { return Ok(digest.finalize().into()); }
        digest.update(&bytes[..count]);
    }
}

fn protected_stash_dir(context: &RuntimeContext) -> Result<(), String> {
    let path = context.resolve_path(RuntimeArea::Data, RECOVERY_DIR, PathIntent::CreateDirectory)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(recovery_error()),
    }
    if context.resolve_path(RuntimeArea::Data, RECOVERY_DIR, PathIntent::ReadDirectory)? != path {
        return Err(recovery_error());
    }
    crate::guarded_credentials::restrict_credential(&path).map_err(|_| recovery_error())?;
    crate::guarded_file::sync_directory(context.data_dir()).map_err(|_| recovery_error())
}

fn configured_token(raw: &[u8]) -> Result<Option<String>, String> {
    let text = std::str::from_utf8(raw).map_err(|_| recovery_error())?;
    let raw: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|_| recovery_error())?;
    let (cfg, _) = crate::config::canonical_config(&raw).map_err(|_| recovery_error())?;
    Ok(cfg.get("consoleToken").and_then(Value::as_str).map(str::trim)
        .filter(|value| !value.is_empty()).map(str::to_owned))
}

fn checked_recovery_bytes(context: &RuntimeContext, name: &str, digest: [u8; 32]) -> Result<Vec<u8>, String> {
    let bytes = console_auth::read_protected_auth_file(context, name, 16 * 1024 * 1024)?
        .ok_or_else(recovery_error)?;
    if !console_auth::digests_equal(&digest, &Sha256::digest(&bytes).into()) {
        return Err(recovery_error());
    }
    Ok(bytes)
}

fn read_recovery_intent(context: &RuntimeContext, name: &str) -> Result<Option<RecoveryIntent>, String> {
    let Some(bytes) = console_auth::read_protected_auth_file(context, name, 4096)? else { return Ok(None) };
    let intent: RecoveryIntent = serde_json::from_slice(&bytes).map_err(|_| recovery_error())?;
    if intent.v != 1 { return Err(recovery_error()); }
    Ok(Some(intent))
}

pub(crate) fn verifying_recovery(context: &RuntimeContext) -> Option<[u8; 32]> {
    let bytes = console_auth::read_protected_auth_file(context, RECOVERY_INTENT, 4096).ok()??;
    let intent: RecoveryIntent = serde_json::from_slice(&bytes).ok()?;
    (intent.v == 1 && intent.phase == RestorePhase::Verifying).then(|| Sha256::digest(&bytes).into())
}

fn publish_recovery_intent(context: &RuntimeContext, intent: &RecoveryIntent) -> Result<(), String> {
    let bytes = serde_json::to_vec(intent).map_err(|_| recovery_error())?;
    context.write_atomic(RuntimeArea::Data, RECOVERY_INTENT, &bytes).map_err(|_| recovery_error())
}

fn set_restore_phase(context: &RuntimeContext, phase: RestorePhase) -> Result<(), String> {
    let mut intent = read_recovery_intent(context, RECOVERY_INTENT)?.ok_or_else(recovery_error)?;
    intent.phase = phase;
    publish_recovery_intent(context, &intent)
}

fn clear_finished_recovery(context: &RuntimeContext) -> Result<(), String> {
    for name in [FORWARD_CONFIG, ROLLBACK_CONFIG, ROLLBACK_ARCHIVE, DESTINATION_TOKEN] {
        console_auth::remove_auth_file(context, name)?;
    }
    console_auth::remove_auth_file(context, RECOVERY_OUTCOME)
}

// A terminal outcome survives namespace cleanup and lets recovery retry just the cleanup.
fn finish_recovery(context: &RuntimeContext, phase: RestorePhase) -> Result<(), String> {
    let _gate = crate::console::config_gate(context).map_err(|(_, e)| e)?;
    let mut intent = read_recovery_intent(context, RECOVERY_INTENT)?.ok_or_else(recovery_error)?;
    intent.phase = phase;
    publish_recovery_intent(context, &intent)?;
    let bytes = serde_json::to_vec(&intent).map_err(|_| recovery_error())?;
    context.write_atomic(RuntimeArea::Data, RECOVERY_OUTCOME, &bytes)?;
    console_auth::remove_auth_file(context, RECOVERY_INTENT)?;
    if clear_finished_recovery(context).is_err() {
        crate::safe_eprintln!("[restore] terminal recovery cleanup remains pending");
    }
    Ok(())
}

fn stage_full_restore(
    context: &RuntimeContext, forward: &Prepared, rollback: &Prepared,
) -> Result<RestorePair, String> {
    protected_stash_dir(context)?;
    if read_recovery_intent(context, RECOVERY_OUTCOME)?.is_some() {
        clear_finished_recovery(context)?;
    }
    let forward_config = forward.config.as_deref().ok_or_else(recovery_error)?;
    let rollback_config = rollback.config.as_deref().ok_or_else(recovery_error)?;
    let live = console_auth::raw_token_config(context)?;
    let effective = context.effective_console_token(&live);
    let mode = configured_token(live.to_string().as_bytes())?;
    if effective.is_empty() || configured_token(rollback_config)? != mode {
        return Err(recovery_error());
    }
    let original_file = console_auth::read_protected_auth_file(context, console_auth::TOKEN_FILE, 1024 * 1024)?;
    let captured = DestinationToken { configured: mode.is_some(), effective, original_file };
    let serialized = serde_json::to_vec(&captured).map_err(|_| recovery_error())?;
    let archive = File::open(&rollback.zip).map_err(|_| recovery_error())?;
    let rollback_digest = hash_file(&archive)?;
    for (name, bytes) in [(FORWARD_CONFIG, forward_config), (ROLLBACK_CONFIG, rollback_config), (DESTINATION_TOKEN, serialized.as_slice())] {
        context.write_atomic(RuntimeArea::Data, name, bytes)?;
    }
    let mut archive = File::open(&rollback.zip).map_err(|_| recovery_error())?;
    context.write_atomic_from(RuntimeArea::Data, ROLLBACK_ARCHIVE, &mut archive, None)?;
    let intent = RecoveryIntent {
        v: 1, phase: RestorePhase::Forward,
        forward_config: Sha256::digest(forward_config).into(),
        rollback_config: Sha256::digest(rollback_config).into(),
        rollback_archive: rollback_digest,
        destination_token: Sha256::digest(&serialized).into(),
    };
    let config = checked_recovery_bytes(context, FORWARD_CONFIG, intent.forward_config)?;
    let token = configured_token(&config)?.unwrap_or(captured.effective);
    publish_recovery_intent(context, &intent)?;
    Ok(RestorePair { token, config })
}

fn rollback_artifacts(context: &RuntimeContext) -> Result<(Prepared, RestorePair), String> {
    let intent = read_recovery_intent(context, RECOVERY_INTENT)?.ok_or_else(recovery_error)?;
    checked_recovery_bytes(context, FORWARD_CONFIG, intent.forward_config)?;
    let config = checked_recovery_bytes(context, ROLLBACK_CONFIG, intent.rollback_config)?;
    let token_bytes = checked_recovery_bytes(context, DESTINATION_TOKEN, intent.destination_token)?;
    let token: DestinationToken = serde_json::from_slice(&token_bytes).map_err(|_| recovery_error())?;
    let mode = configured_token(&config)?;
    let saved_file = token.original_file.as_deref().and_then(|bytes| std::str::from_utf8(bytes).ok()).map(str::trim);
    if token.effective.is_empty() || token.configured != mode.is_some()
        || mode.as_deref().or(saved_file) != Some(token.effective.as_str())
    {
        return Err(recovery_error());
    }
    let archive = context.resolve_path(RuntimeArea::Data, ROLLBACK_ARCHIVE, PathIntent::ReadFile)?;
    let file = console_auth::open_protected_auth_file(&archive)?.ok_or_else(recovery_error)?;
    if !console_auth::digests_equal(&intent.rollback_archive, &hash_file(&file)?) {
        return Err(recovery_error());
    }
    let prepared = Prepared::open_internal(context, &archive, false, false)?;
    if prepared.config.as_ref() != Some(&config) {
        return Err(recovery_error());
    }
    Ok((prepared, RestorePair { config, token: token.effective }))
}

#[derive(Clone, Copy)]
enum FullPublication<'a> {
    Forward(&'a Prepared),
    Rollback(&'a RestorePair),
}

/// Quiesce first, then acquire config/social-state, transport authority and the settings store in that order.
/// Release the publication gates before reconciliation or restart.
fn stop_and_replace(
    context: &RuntimeContext,
    by: &str,
    prepared: &Prepared,
    full: Option<FullPublication<'_>>,
    control: &mut dyn Control,
) -> Result<(), Step> {
    let memory_only = full.is_none();
    let stopped = quiesce(context, by, memory_only, control)?;
    if let Some(publication) = full {
        let _config = crate::console::config_gate(context).map_err(|(_, error)| Step::Refused(error))?;
        let authority = context.authority_gate().map_err(Step::Refused)?;
        let _store = crate::settings::restore_gate(context).map_err(Step::Refused)?;
        let staged;
        let pair = match publication {
            FullPublication::Forward(rollback) => {
                staged = stage_full_restore(context, prepared, rollback)
                    .map_err(|e| if recovery_pending(context) { Step::Failed(e) } else { Step::Refused(e) })?;
                &staged
            }
            FullPublication::Rollback(pair) => {
                set_restore_phase(context, RestorePhase::Rollback)?;
                pair
            }
        };
        control.publish_generation(context).map_err(|error| match error {
            GenerationWriteError::Unpublished(error) | GenerationWriteError::Published(error) => Step::Failed(error),
        })?;
        console_auth::remove_auth_file(context, crate::console::DESKTOP_GRANTS_FILE)?;
        apply(context, prepared, false, control, Some(&authority))?;
        control.replace(context, RuntimeArea::Data, console_auth::TOKEN_FILE, &mut (if matches!(publication, FullPublication::Rollback(_)) { context.read_text(RuntimeArea::Data, console_auth::TOKEN_FILE).map_err(str::to_owned)? } else { pair.token.clone() }).as_bytes(), Some(&authority))?; // RED-R3-H2
        control.replace(context, RuntimeArea::Config, "config.json", &mut pair.config.as_slice(), Some(&authority))?;
    } else {
        apply(context, prepared, true, control, None)?;
    }
    control.store_replaced(context);
    if !memory_only && prepared.files.contains_key("conversations.db") {
        reconcile_conversations(context)?;
    }
    drop(stopped);
    Ok(())
}

fn apply(
    context: &RuntimeContext,
    prepared: &Prepared,
    memory_only: bool,
    control: &mut dyn Control,
    authority: Option<&AuthorityGuard<'_>>,
) -> Result<(), String> {
    // A stopped writer must not leave an old WAL to replay against the replacement database. A reader in a process
    // the restore does not stop may hold a file open for a moment; removals retry that (`remove_runtime_file`).
    crate::memory::db_path_for_runtime(context)?;
    let databases: &[&str] = if memory_only {
        &["memory.db"]
    } else {
        crate::backup::conversation_db_path(context)?;
        crate::settings::store_path(context)?;
        &["memory.db", "conversations.db", "settings.db"]
    };
    for database in databases {
        for suffix in ["-wal", "-shm", "-journal"] {
            remove_runtime_file(context, RuntimeArea::Data, &format!("{database}{suffix}"))?;
        }
    }
    let current = current_files(context, memory_only)?;
    let mut archive = zip::ZipArchive::new(File::open(&prepared.zip).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    for (name, (area, relative)) in &prepared.files {
        if name == "config.json" { continue; }
        let mut source = archive.by_name(name).map_err(|e| e.to_string())?;
        control
            .replace(context, *area, relative, &mut source, authority)
            .map_err(|e| {
                format!(
                    "restore replacement failed for {}: {e}",
                    label(*area, relative)
                )
            })?;
    }
    for (name, (area, relative)) in current {
        if !prepared.state_included && name.starts_with("state/") {
            continue;
        }
        if !prepared.files.contains_key(&name) {
            remove_runtime_file(context, area, &relative)?;
        }
    }
    Ok(())
}

/// The archive rewinds conversation facts, but never the authoritative turn/send ledgers.
/// Run under the stopped writer gates for both restore and rollback, before requesting start.
fn reconcile_conversations(context: &RuntimeContext) -> Result<(), String> {
    use crate::harness::{store::Store, Status};
    use crate::turn_ledger::{self, Recorder, State};
    use crate::turn_recovery::{send_state, SendState};
    // An uninitialized database has no harness turns. Inspect without changing its
    // schema or journal mode; startup retains responsibility for initialization.
    {
        let db = rusqlite::Connection::open_with_flags(
            crate::backup::conversation_db_path(context)?,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "cannot inspect restored conversation storage")?;
        let initialized: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='turns')",
                [],
                |r| r.get(0),
            )
            .map_err(|_| "cannot inspect restored conversation schema")?;
        if !initialized {
            return Ok(());
        }
    }
    let store = Store::open(context)?;
    let unfinished = store.unfinished()?;
    if unfinished.is_empty() {
        return Ok(());
    }
    let mut records = turn_ledger::read_records(context)?;
    let (retained, _) = turn_ledger::fold(&records);
    for (id, key, generation, begun_ms) in &unfinished {
        if !retained.iter().any(|t| t.id == *id) {
            // Preserve the database ID so the ordinary administrator resolution path
            // cancels exactly this running turn. No inbound keys or text are invented.
            let begun = json!({"k":"begun", "turn":id, "key":key, "backend":"harness",
                "reason":"restore-unknown", "epoch":generation, "t":begun_ms, "inbound":[]});
            turn_ledger::append_record_without_retry(context, &begun)?;
            records.push(begun);
        }
    }
    let (turns, _) = turn_ledger::fold(&records);
    let outbox = crate::send_outbox::read_records(context);
    for (id, key, _, _) in unfinished {
        let turn = turns
            .iter()
            .find(|t| t.id == id)
            .ok_or("restored turn ledger entry missing")?;
        if turn.key != key || turn.backend != "harness" {
            return Err("restored harness turn disagrees with the retained ledger".into());
        }
        let status = match turn.state {
            // The ledger proves completion, but contains no assistant text. Preserve only
            // archived facts; never regenerate or invent the missing model response.
            State::Completed => Some(Status::Completed(String::new())),
            State::Failed => Some(Status::Failed(
                "restored turn ended in the retained ledger".into(),
            )),
            State::Cancelled | State::Abandoned | State::Resumed | State::Resolved => {
                Some(Status::Cancelled)
            }
            _ => None,
        };
        if let Some(status) = status {
            store.finish_with_reason(&id, &status, None, Some(&turn.outcome_reason))?;
            continue;
        }
        // Use the same batch association and uncertainty classification as startup recovery.
        let next_begin = turns
            .iter()
            .filter(|t| t.key == key && t.begun_at > turn.begun_at)
            .map(|t| t.begun_at)
            .min();
        let sends = match &outbox {
            Ok(records) => send_state(&turn_ledger::batches_of_turn(turn, next_begin, records)),
            Err(error) => SendState::Unreadable(error.clone()),
        };
        let (reason, detail) = match sends {
            SendState::Unresolved(detail) | SendState::Unreadable(detail) => ("send-unresolved", detail),
            _ if turn.pause_reason == "send-unresolved" => ("send-unresolved", turn.pause_detail.clone()),
            _ if store.has_unsettled_tools(&id)? => ("send-unresolved", "Restored tool effect could not be confirmed; administrator confirmation required before retry".into()),
            _ if turn.reason == "restore-unknown" => ("harness-restore-unknown", "Restored harness turn is absent from the retained ledger; administrator resolution required".into()),
            _ => ("harness-interrupted", "Harness turn interrupted; administrator resolution required".into()),
        };
        // Revocation must succeed before recording a pause, just as in startup recovery.
        crate::console::ensure_or_rotate_agent_token(context, &key, true)?;
        Recorder::attach(context, &id, &key).without_retry().unknown(reason, &detail)?;
    }
    Ok(())
}

struct RecoveryAudit {
    rollback_status: &'static str,
    pre_restore_archive: Option<String>,
}
impl Default for RecoveryAudit {
    fn default() -> Self {
        Self {
            rollback_status: "not-attempted",
            pre_restore_archive: None,
        }
    }
}

fn audit(
    context: &RuntimeContext,
    by: &str,
    archive: &Path,
    memory_only: bool,
    result: &str,
    detail: &str,
    recovery: &RecoveryAudit,
) -> Result<(), String> {
    let started = Instant::now();
    let _gate = loop {
        match context.open_locked_file(RuntimeArea::Data, "locks/restore-audit.lock") {
            Ok(gate) => break gate,
            Err(error)
                if error == "runtime file is already locked"
                    && started.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    };
    // The audit is an operator record, so the archive keeps its ordinary filesystem spelling; the
    // verbatim form survives only when no safe ordinary spelling exists for it.
    let archive = ordinary_path(archive);
    let entry = json!({"at":crate::parity_clock::now_ms(),"action":"backup.restore","by":by,"archive":archive,
        "scope":if memory_only {"memory"} else {"everything"},"result":result,"rollbackStatus":recovery.rollback_status,"preRestoreArchive":recovery.pre_restore_archive,
        "detail":crate::run_record::sanitize_reason(detail, &|s| crate::pipeline::redact_sensitive_text(s, &[]))});
    context.append_bytes(
        RuntimeArea::Data,
        "audit/audit.jsonl",
        format!("{entry}\n").as_bytes(),
    )
}

fn execute_with(
    context: &RuntimeContext,
    archive: &Path,
    memory_only: bool,
    confirmation: &str,
    by: &str,
    control: &mut dyn Control,
) -> Result<Value, String> {
    let mut recovery = RecoveryAudit::default();
    let result = (|| {
        let _operation = operation_gate(context)?;
        let _restore = context
            .open_locked_file(RuntimeArea::Data, "locks/restore.lock")
            .map_err(|_| "another restore is running")?;
        if recovery_pending(context) {
            return Err(recovery_error());
        }
        let _credential = if memory_only { None } else { Some(console_auth::credential_mutation_gate(context)?) };
        if !memory_only {
            let _config = crate::console::config_gate(context).map_err(|(_, e)| e)?;
            console_auth::reconcile_token_rotation(context)?;
        }
        let prepared = Prepared::open(context, archive, memory_only)?;
        if confirmation != prepared.view["confirmation"].as_str().unwrap_or("") {
            return Err(
                "confirmation is missing or the archive/replacement plan changed; preview again"
                    .into(),
            );
        }
        control.preflight(context)?;
        let config = context.load_config().map_err(str::to_owned)?;
        audit(
            context,
            by,
            archive,
            memory_only,
            "started",
            "verified archive; taking pre-restore backup",
            &recovery,
        )?;
        let previous = crate::backup::create_pre_restore(context, &|| Ok(())).map_err(|error| {
            control.alert(
                context,
                &config,
                &format!("Restore refused before stopping: {error}"),
            );
            error
        })?;
        recovery.pre_restore_archive = previous
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let rollback = Prepared::open(context, &previous, memory_only)?;
        #[cfg(feature = "parity-clock")]
        control.snapshot_ready()?;
        let attempt = (|| -> Result<(), Step> {
            stop_and_replace(context, by, &prepared, (!memory_only).then_some(FullPublication::Forward(&rollback)), control)?;
            if !memory_only {
                let _config = crate::console::config_gate(context).map_err(|(_, e)| Step::Failed(e))?;
                set_restore_phase(context, RestorePhase::Verifying)?;
            }
            resume(context, by)?;
            control.restart(context)?;
            control.health(context, Duration::from_secs(120))?;
            crate::backup::protect_pre_restore(&previous, &|| Ok(()))?;
            if !memory_only { finish_recovery(context, RestorePhase::Committed)?; }
            Ok(())
        })();
        match attempt {
            Ok(()) => Ok(json!({"ok":true,"result":"succeeded","preRestoreBackup":ordinary_path(&previous)})),
            Err(Step::Refused(reason)) => {
                // Quiescence completed before the refusal. Resume the owners without replacing anything.
                let started = resume(context, by).and_then(|()| control.restart(context));
                let detail = match started {
                    Ok(()) => format!("Restore refused after stopping, nothing changed: {reason}"),
                    Err(error) => format!(
                        "Restore refused after stopping, nothing changed: {reason}; the bridge could not be started again: {error}"
                    ),
                };
                control.alert(context, &config, &detail);
                Err(detail)
            }
            Err(Step::Failed(reason)) => {
                // Always re-quiesce; a failed health check may leave live writers running.
                let undo = (|| {
                    if memory_only {
                        stop_and_replace(context, by, &rollback, None, control).map_err(Step::message)?;
                    } else {
                        let terminal = read_recovery_intent(context, RECOVERY_INTENT)?
                            .or(read_recovery_intent(context, RECOVERY_OUTCOME)?)
                            .is_some_and(|intent| intent.phase.terminal());
                        if terminal || !recovery_pending(context) { return Err(recovery_error()); }
                        let (rollback, pair) = rollback_artifacts(context)?;
                        stop_and_replace(context, by, &rollback, Some(FullPublication::Rollback(&pair)), control)
                            .map_err(Step::message)?;
                        finish_recovery(context, RestorePhase::RolledBack)?;
                    }
                    resume(context, by)?;
                    control.restart(context)?;
                    crate::backup::protect_pre_restore(&previous, &|| Ok(()))
                })();
                let detail = match undo {
                    Ok(()) => {
                        recovery.rollback_status = "succeeded";
                        format!("Restore rolled back: pre-restore backup restored; bridge restart requested. Reason: {reason}")
                    }
                    Err(e) => {
                        recovery.rollback_status = "failed";
                        format!(
                            "{reason}; rollback failed: {e}; pre-restore backup: {}",
                            ordinary_path(&previous)
                        )
                    }
                };
                control.alert(context, &config, &detail);
                Err(detail)
            }
        }
    })();
    let (status, detail) = match &result {
        Ok(_) => ("succeeded", "health check passed"),
        Err(e) => ("failed", e.as_str()),
    };
    if result.is_ok() {
        recovery.rollback_status = "not-needed";
    }
    audit(context, by, archive, memory_only, status, detail, &recovery)?;
    result
}

pub fn execute(
    context: &RuntimeContext,
    archive: &Path,
    memory_only: bool,
    confirmation: &str,
    by: &str,
) -> Result<Value, String> {
    execute_with(
        context,
        archive,
        memory_only,
        confirmation,
        by,
        &mut RealControl,
    )
}

// The parity build runs real file transactions with deterministic process and health boundaries.
#[cfg(feature = "parity-clock")]
struct CliFixtureControl;

#[cfg(feature = "parity-clock")]
impl CliFixtureControl {
    fn barrier(stage: &str) -> Result<(), String> {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "restore fixture {stage}").map_err(|err| err.to_string())?;
        stdout.flush().map_err(|err| err.to_string())?;
        drop(stdout);
        let mut response = String::new();
        std::io::stdin().read_line(&mut response).map_err(|err| err.to_string())?;
        if response.trim_end() != "continue" {
            return Err("restore fixture stopped".into());
        }
        Ok(())
    }
}

#[cfg(feature = "parity-clock")]
impl Control for CliFixtureControl {
    fn preflight(&mut self, context: &RuntimeContext) -> Result<(), String> {
        crate::install_root::check_development_path(context.runtime_root().ok_or("runtime root missing")?)
    }
    fn stop(&mut self, _: &RuntimeContext, _: u64) -> Result<(), String> {
        Ok(())
    }
    fn snapshot_ready(&mut self) -> Result<(), String> {
        Self::barrier("snapshot")
    }
    fn health(&mut self, _: &RuntimeContext, _: Duration) -> Result<(), String> {
        Self::barrier("health")?;
        if std::env::var_os("HER_TEST_RESTORE_HEALTH").as_deref() == Some(std::ffi::OsStr::new("pass")) {
            Ok(())
        } else {
            Err("restore fixture candidate health failed".into())
        }
    }
    fn alert(&mut self, _: &RuntimeContext, _: &Value, _: &str) {}
    fn replace(
        &mut self, context: &RuntimeContext, area: RuntimeArea, relative: &str,
        input: &mut dyn Read, authority: Option<&AuthorityGuard<'_>>,
    ) -> Result<(), String> {
        if std::env::var_os("HER_TEST_RESTORE_PAIR_STEPS").is_some()
            && ((area == RuntimeArea::Config && relative == "config.json") || (area == RuntimeArea::Data && relative == console_auth::TOKEN_FILE))
        {
            let intent = read_recovery_intent(context, RECOVERY_INTENT)?.ok_or_else(recovery_error)?;
            if area == RuntimeArea::Config && relative == "config.json" {
                let stage = if intent.phase == RestorePhase::Rollback { "rollback-pair" } else { "forward-pair" };
                Self::barrier(stage)?;
            } else if area == RuntimeArea::Data && relative == console_auth::TOKEN_FILE && intent.phase == RestorePhase::Rollback {
                Self::barrier("rollback-before-token")?;
            }
        }
        context.write_atomic_from(area, relative, input, authority)
    }
}

fn recover_with(context: &RuntimeContext, control: &mut dyn Control) -> Result<Value, String> {
    let _operation = operation_gate(context)?;
    let _restore = context.open_locked_file(RuntimeArea::Data, "locks/restore.lock")?;
    let _credential = console_auth::credential_mutation_gate(context)?;
    let by = "her-cli:restore-recovery";
    let phase = {
        let _config = crate::console::config_gate(context).map_err(|(_, e)| e)?;
        match read_recovery_intent(context, RECOVERY_INTENT)? {
            Some(intent) => intent.phase,
            None => {
                let outcome = read_recovery_intent(context, RECOVERY_OUTCOME)?.ok_or_else(recovery_error)?;
                if !outcome.phase.terminal() { return Err(recovery_error()); }
                clear_finished_recovery(context)?;
                return Ok(json!({"ok":true,"result":"cleanup-complete"}));
            }
        }
    };
    if phase.terminal() {
        finish_recovery(context, phase)?;
        if phase == RestorePhase::RolledBack {
            resume(context, by)?;
            control.restart(context)?;
        }
        return Ok(json!({"ok":true,"result":"cleanup-complete"}));
    }
    let (rollback, pair) = rollback_artifacts(context)?;
    control.preflight(context)?;
    stop_and_replace(context, by, &rollback, Some(FullPublication::Rollback(&pair)), control).map_err(Step::message)?;
    finish_recovery(context, RestorePhase::RolledBack)?;
    resume(context, by)?;
    control.restart(context)?;
    Ok(json!({"ok":true,"result":"rolled-back"}))
}

/// Terminal entry: print the same preview as the console, then require a deliberate confirmation.
pub fn run_cli(args: &[String]) -> i32 {
    fn run(args: &[String]) -> Result<Value, String> {
        if args.first().map(String::as_str) != Some("restore") {
            return Err("usage: her-cli backup restore <archive> --runtime-root <root> [--memory-only] [--preview] [--confirm <confirmation>]".into());
        }
        if args.get(1).map(String::as_str) == Some("--recover") {
            let [_, _, option, root] = args else { return Err("usage: her-cli backup restore --recover --runtime-root <root>".into()) };
            if option != "--runtime-root" || !Path::new(root).is_absolute() {
                return Err("usage: her-cli backup restore --recover --runtime-root <root>".into());
            }
            let context = RuntimeContext::for_console_recovery(Path::new(root)).map_err(str::to_owned)?;
            #[cfg(feature = "parity-clock")]
            if std::env::var_os("HER_TEST_RESTORE_CONTROL").as_deref() == Some(std::ffi::OsStr::new("stdio")) {
                return recover_with(&context, &mut CliFixtureControl).map_err(|_| recovery_error());
            }
            return recover_with(&context, &mut RealControl).map_err(|_| recovery_error());
        }
        let archive = args
            .get(1)
            .filter(|v| !v.starts_with('-'))
            .ok_or("archive is required")?;
        let mut root = None;
        let mut memory_only = false;
        let mut preview_only = false;
        let mut confirmation = None;
        let mut by = format!(
            "cli:{}",
            std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .unwrap_or_else(|_| "administrator".into())
        );
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--memory-only" if !memory_only => memory_only = true,
                "--preview" if !preview_only => preview_only = true,
                "--runtime-root" | "--confirm" | "--by" => {
                    let value = args.get(i + 1).ok_or("option needs a value")?.clone();
                    match args[i].as_str() {
                        "--runtime-root" if root.is_none() => root = Some(value),
                        "--confirm" if confirmation.is_none() => confirmation = Some(value),
                        "--by" => by = value,
                        _ => return Err("option given twice".into()),
                    }
                    i += 1;
                }
                _ => return Err(format!("unknown or repeated option: {}", args[i])),
            }
            i += 1;
        }
        let root = root.ok_or("--runtime-root is required")?;
        if !Path::new(&root).is_absolute() {
            return Err("--runtime-root must be absolute".into());
        }
        let context = RuntimeContext::from_root(Path::new(&root)).map_err(str::to_owned)?;
        let plan = match preview(&context, Path::new(archive), memory_only) {
            Ok(plan) => plan,
            Err(error) => {
                if !preview_only {
                    audit(
                        &context,
                        &by,
                        Path::new(archive),
                        memory_only,
                        "failed",
                        &error,
                        &RecoveryAudit::default(),
                    )?;
                }
                return Err(error);
            }
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?
        );
        if preview_only {
            return Ok(json!({"ok":true,"preview":plan}));
        }
        let confirmation = match confirmation {
            Some(value) => value,
            None => {
                eprintln!("This replaces the files listed above. Type RESTORE to confirm:");
                std::io::stdout().flush().map_err(|e| e.to_string())?;
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| e.to_string())?;
                if answer.trim() != "RESTORE" {
                    audit(
                        &context,
                        &by,
                        Path::new(archive),
                        memory_only,
                        "cancelled",
                        "administrator did not confirm",
                        &RecoveryAudit::default(),
                    )?;
                    return Err("restore was not confirmed; nothing changed".into());
                }
                plan["confirmation"]
                    .as_str()
                    .ok_or("confirmation unavailable")?
                    .to_owned()
            }
        };
        #[cfg(feature = "parity-clock")]
        if std::env::var_os("HER_TEST_RESTORE_CONTROL").as_deref() == Some(std::ffi::OsStr::new("stdio")) {
            return execute_with(&context, Path::new(archive), memory_only, &confirmation, &by, &mut CliFixtureControl);
        }
        execute(
            &context,
            Path::new(archive),
            memory_only,
            &confirmation,
            &by,
        )
    }
    match run(args) {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            eprintln!("[restore] {error}");
            1
        }
    }
}

/// Start the independent CLI image so stopping the console cannot interrupt rollback.
pub fn queue(
    context: &RuntimeContext,
    archive: &Path,
    memory_only: bool,
    confirmation: &str,
) -> Result<Value, String> {
    let result = queue_inner(context, archive, memory_only, confirmation);
    if let Err(error) = &result {
        audit(
            context,
            "console:administrator",
            archive,
            memory_only,
            "failed",
            error,
            &RecoveryAudit::default(),
        )?;
    }
    result
}

fn queue_inner(
    context: &RuntimeContext,
    archive: &Path,
    memory_only: bool,
    confirmation: &str,
) -> Result<Value, String> {
    use std::process::Stdio;
    let _gate = operation_gate(context)?;
    let plan = preview(context, archive, memory_only)?;
    if plan["confirmation"] != confirmation {
        return Err("confirmation is missing or stale; preview again".into());
    }
    RealControl.preflight(context)?;
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name(if cfg!(windows) {
            "her-cli.exe"
        } else {
            "her-cli"
        });
    let root = context.runtime_root().ok_or("runtime root missing")?;
    let log = context.resolve_path(RuntimeArea::Data, "logs/restore.log", PathIntent::WriteFile)?;
    let output = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| e.to_string())?;
    let mut cmd =
        crate::subprocess::command_without_console(crate::subprocess::external_path(exe)?);
    cmd.args(["backup", "restore"])
        .arg(crate::subprocess::external_path(archive)?)
        .arg("--runtime-root")
        .arg(crate::subprocess::external_path(root)?)
        .args(["--confirm", confirmation, "--by", "console:administrator"]);
    if memory_only {
        cmd.arg("--memory-only");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(output));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    drop(_gate);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(
        json!({"ok":true,"queued":true,"pid":pid,"message":"Restore started; the console will restart. The result is recorded in the audit log."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut id = [0u8; 8];
            getrandom::getrandom(&mut id).unwrap();
            let root = crate::install_root::development_root(
                std::env::temp_dir().join(format!("restore-{:x}", u64::from_ne_bytes(id))),
            );
            for dir in [
                "config",
                "data",
                "resources/roles",
                "resources/docs",
                "resources/patches",
            ] {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            fs::write(root.join("config/config.json"), br#"{"consoleToken":"fixture","backup":{},"rustGroups":["fixture"],"mcpRustBridge":true}"#).unwrap();
            fs::write(root.join("dependencies.json"), br#"{"dsh":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1","model":"http://127.0.0.1:1"}"#).unwrap();
            fs::write(root.join("data/social-v2.json"), "{}").unwrap();
            fs::write(root.join("resources/roles/test.md"), "old role").unwrap();
            let db = rusqlite::Connection::open(root.join("data/memory.db")).unwrap();
            db.execute_batch(
                "CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES('old memory');",
            )
            .unwrap();
            let conversations =
                rusqlite::Connection::open(root.join("data/conversations.db")).unwrap();
            conversations.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES('old conversation');").unwrap();
            Self(root)
        }
        fn context(&self) -> RuntimeContext {
            RuntimeContext::from_root(&self.0).unwrap()
        }
        fn backup(&self) -> PathBuf {
            crate::backup::create(&self.context(), &|| Ok(())).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn restore_preview_verifies_and_describes_scope_without_mutation() {
        let f = Fixture::new();
        let archive = f.backup();
        let before = fs::read(f.0.join("config/config.json")).unwrap();
        let p = preview(&f.context(), &archive, false).unwrap();
        assert_eq!(p["scope"], "everything");
        assert!(p["replaces"].as_array().unwrap().contains(&json!("data/console-token")));
        assert!(p["time"].is_i64());
        assert!(p["contents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "memory.db"));
        assert!(p["replaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "resources/roles/test.md"));
        assert_eq!(
            preview(&f.context(), &archive, true).unwrap()["replaces"],
            json!(["data/memory.db"])
        );
        assert_eq!(fs::read(f.0.join("config/config.json")).unwrap(), before);
        assert!(!f.0.join("data/lifecycle/desired-state.json").exists());
        fs::write(&archive, b"damaged archive").unwrap();
        assert!(preview(&f.context(), &archive, false).is_err());
    }
    #[test]
    fn pre_restore_backup_includes_state_and_survives_short_retention() {
        let f = Fixture::new();
        let mut cfg: Value =
            serde_json::from_slice(&fs::read(f.0.join("config/config.json")).unwrap()).unwrap();
        cfg["backup"] = json!({"includeState":false,"keepDays":1});
        fs::write(
            f.0.join("config/config.json"),
            serde_json::to_vec(&cfg).unwrap(),
        )
        .unwrap();
        fs::write(f.0.join("data/probe.json"), br#"{"value":"before"}"#).unwrap();
        let pre = crate::backup::create_pre_restore(&f.context(), &|| Ok(())).unwrap();
        let manifest = crate::backup::verify_archive(&pre, &|| Ok(())).unwrap();
        assert!(manifest.files.contains_key("conversations.db"));
        assert!(
            manifest.files.contains_key("state/probe.json"),
            "pre-restore backups override includeState=false"
        );
        let old = std::time::SystemTime::now() - Duration::from_secs(2 * 86400);
        fs::File::options()
            .write(true)
            .open(&pre)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let ordinary = f.backup();
        assert!(crate::backup::verify_archive(&pre, &|| Ok(()))
            .unwrap()
            .files
            .contains_key("conversations.db"));
        assert!(crate::backup::verify_archive(&ordinary, &|| Ok(()))
            .unwrap()
            .files
            .contains_key("conversations.db"));
        assert!(
            pre.exists(),
            "seven-day pin overrides shorter ordinary retention"
        );
        assert!(!crate::backup::verify_archive(&ordinary, &|| Ok(()))
            .unwrap()
            .files
            .contains_key("state/probe.json"));
        fs::write(pre.with_extension("zip.restore-until"), b"0").unwrap();
        f.backup();
        assert!(
            !pre.exists(),
            "expired protection returns the archive to ordinary retention"
        );
    }

    #[derive(Default)]
    struct FakeControl {
        stops: usize,
        unhealthy: bool,
        fail_write: Option<usize>,
        writes: usize,
        alerts: Vec<String>,
        health_finished: Option<i64>,
        health_error: Option<String>,
        fail_rollback_stop: bool,
        leave_conversation_sidecars: bool,
        leave_settings_sidecars: bool,
        fail_relative: Option<&'static str>,
        replaced: Vec<String>,
        probe_settings_write: bool,
        settings_probe: Option<Result<(), u16>>,
        writer_takes_gate_during_stop: bool,
        writer_hold: Option<fs::File>,
        store_free_at_stop: Vec<bool>,
        restarts: usize,
        probe_store_replaced: bool,
        settings_after_replace: Option<Result<(), u16>>,
        observe_generation: bool,
        generation_at_health: Option<console_auth::Generation>,
        generation_fault: Option<&'static str>,
    }
    impl Control for FakeControl {
        fn preflight(&mut self, _: &RuntimeContext) -> Result<(), String> {
            Ok(())
        }
        fn stop(&mut self, context: &RuntimeContext, _: u64) -> Result<(), String> {
            self.stops += 1;
            // The store gate is free while the restore waits for the owners and the task gates (a stop is the first
            // of those waits); a writer that takes it now holds it until the restore gives up.
            match context.open_locked_file(RuntimeArea::Data, "locks/settings-store.lock") {
                Ok(gate) => {
                    self.store_free_at_stop.push(true);
                    if self.writer_takes_gate_during_stop {
                        self.writer_hold = Some(gate);
                    }
                }
                Err(_) => self.store_free_at_stop.push(false),
            }
            if self.leave_conversation_sidecars && self.stops == 1 {
                for suffix in ["-wal", "-shm", "-journal"] {
                    fs::write(
                        context
                            .runtime_root()
                            .unwrap()
                            .join(format!("data/conversations.db{suffix}")),
                        b"stale conversation sidecar",
                    )
                    .unwrap();
                }
            }
            if self.leave_settings_sidecars && self.stops == 1 {
                for suffix in ["-wal", "-shm", "-journal"] {
                    fs::write(
                        context
                            .runtime_root()
                            .unwrap()
                            .join(format!("data/settings.db{suffix}")),
                        b"stale settings sidecar",
                    )
                    .unwrap();
                }
            }
            if self.fail_rollback_stop && self.stops == 2 {
                return Err("fixture rollback stop failed".into());
            }
            Ok(())
        }
        fn health(&mut self, context: &RuntimeContext, budget: Duration) -> Result<(), String> {
            assert_eq!(budget, Duration::from_secs(120));
            if self.observe_generation {
                let _config = crate::console::config_gate(context).map_err(|(_, error)| error)?;
                self.generation_at_health = console_auth::restore_generation(context)?;
            }
            self.health_finished = Some(crate::parity_clock::now_ms());
            if self.unhealthy {
                Err(self
                    .health_error
                    .clone()
                    .unwrap_or_else(|| "fixture health deadline expired".into()))
            } else {
                Ok(())
            }
        }
        fn restart(&mut self, _: &RuntimeContext) -> Result<(), String> {
            self.restarts += 1;
            Ok(())
        }
        fn store_replaced(&mut self, context: &RuntimeContext) {
            if self.probe_store_replaced {
                self.settings_after_replace = Some(
                    crate::settings::apply(context, "reply.wakeQuietMs", &json!(4321), "probe", "console")
                        .map(|_| ())
                        .map_err(|(status, _)| status),
                );
            }
        }
        fn alert(&mut self, _: &RuntimeContext, _: &Value, reason: &str) {
            self.alerts.push(reason.into());
        }
        fn publish_generation(&mut self, context: &RuntimeContext) -> Result<console_auth::Generation, GenerationWriteError> {
            let Some(fault) = self.generation_fault.take() else {
                return console_auth::rotate_restore_generation(context);
            };
            struct ClearHook;
            impl Drop for ClearHook {
                fn drop(&mut self) {
                    crate::guarded_file::STEP_HOOK.with(|slot| *slot.borrow_mut() = None);
                }
            }
            let _clear = ClearHook;
            crate::guarded_file::STEP_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(move |phase| {
                if phase == fault {
                    Err(std::io::Error::other("fixture marker publication failed"))
                } else {
                    Ok(())
                }
            })));
            console_auth::rotate_restore_generation(context)
        }
        fn replace(
            &mut self,
            context: &RuntimeContext,
            area: RuntimeArea,
            relative: &str,
            input: &mut dyn Read,
            authority: Option<&AuthorityGuard<'_>>,
        ) -> Result<(), String> {
            self.writes += 1;
            self.replaced.push(label(area, relative));
            if self.probe_settings_write && self.settings_probe.is_none() {
                self.settings_probe = Some(
                    crate::settings::apply(context, "reply.wakeQuietMs", &json!(4321), "probe", "console")
                        .map(|_| ())
                        .map_err(|(status, _)| status),
                );
            }
            let fail_relative = self.fail_relative.is_some_and(|name| name == relative);
            if fail_relative {
                self.fail_relative = None;
            }
            if self.fail_write == Some(self.writes) || fail_relative {
                struct Interrupted<'a>(&'a mut dyn Read, bool);
                impl Read for Interrupted<'_> {
                    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                        if self.1 {
                            return Err(std::io::Error::other("fixture disk replacement failed"));
                        }
                        self.1 = true;
                        let size = bytes.len().min(16);
                        self.0.read(&mut bytes[..size])
                    }
                }
                return context.write_atomic_from(area, relative, &mut Interrupted(input, false), authority);
            }
            context.write_atomic_from(area, relative, input, authority)
        }
    }
    fn memory(f: &Fixture) -> String {
        rusqlite::Connection::open(f.0.join("data/memory.db"))
            .unwrap()
            .query_row("SELECT value FROM probe", [], |r| r.get(0))
            .unwrap()
    }
    fn conversation(f: &Fixture) -> String {
        rusqlite::Connection::open_with_flags(
            f.0.join("data/conversations.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
        .query_row("SELECT value FROM probe", [], |r| r.get(0))
        .unwrap()
    }

    fn mutate(f: &Fixture) {
        rusqlite::Connection::open(f.0.join("data/conversations.db"))
            .unwrap()
            .execute("UPDATE probe SET value='current conversation'", [])
            .unwrap();
        rusqlite::Connection::open(f.0.join("data/memory.db"))
            .unwrap()
            .execute("UPDATE probe SET value='current memory'", [])
            .unwrap();
        fs::write(f.0.join("resources/roles/test.md"), "current role").unwrap();
        fs::write(f.0.join("data/probe.json"), br#"{"value":"current"}"#).unwrap();
        fs::write(f.0.join("resources/roles/new.md"), "new role").unwrap();
        let mut cfg: Value =
            serde_json::from_slice(&fs::read(f.0.join("config/config.json")).unwrap()).unwrap();
        cfg["currentOnly"] = json!(true);
        fs::write(
            f.0.join("config/config.json"),
            serde_json::to_vec(&cfg).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn confirmed_restore_applies_exact_scope_and_restarts_with_a_pre_restore_backup() {
        for (memory_only, archive_state) in [(true, true), (false, true), (false, false)] {
            let f = Fixture::new();
            fs::write(f.0.join("data/probe.json"), br#"{"value":"old"}"#).unwrap();
            if !archive_state {
                let mut cfg: Value =
                    serde_json::from_slice(&fs::read(f.0.join("config/config.json")).unwrap())
                        .unwrap();
                cfg["backup"]["includeState"] = json!(false);
                fs::write(
                    f.0.join("config/config.json"),
                    serde_json::to_vec(&cfg).unwrap(),
                )
                .unwrap();
            }
            let archive = f.backup();
            mutate(&f);
            let plan = preview(&f.context(), &archive, memory_only).unwrap();
            let mut control = FakeControl {
                leave_conversation_sidecars: true,
                ..Default::default()
            };
            let result = execute_with(
                &f.context(),
                &archive,
                memory_only,
                plan["confirmation"].as_str().unwrap(),
                "fixture-admin",
                &mut control,
            )
            .unwrap();
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = f.0.join(format!("data/conversations.db{suffix}"));
                if memory_only {
                    assert_eq!(fs::read(&sidecar).unwrap(), b"stale conversation sidecar");
                    fs::remove_file(sidecar).unwrap();
                } else {
                    assert!(!sidecar.exists(), "runtime restore must remove {suffix}");
                }
            }
            assert_eq!(
                conversation(&f),
                if memory_only {
                    "current conversation"
                } else {
                    "old conversation"
                }
            );
            assert_eq!(memory(&f), "old memory");
            assert_eq!(control.stops, 1);
            assert!(control.alerts.is_empty());
            assert_eq!(
                fs::read_to_string(f.0.join("resources/roles/test.md")).unwrap(),
                if memory_only {
                    "current role"
                } else {
                    "old role"
                }
            );
            assert_eq!(f.0.join("resources/roles/new.md").exists(), memory_only);
            let state: Value =
                serde_json::from_slice(&fs::read(f.0.join("data/probe.json")).unwrap()).unwrap();
            assert_eq!(
                state["value"],
                if memory_only || !archive_state {
                    "current"
                } else {
                    "old"
                }
            );
            let config: Value =
                serde_json::from_slice(&fs::read(f.0.join("config/config.json")).unwrap()).unwrap();
            assert_eq!(config.get("currentOnly").is_some(), memory_only);
            let pre = Path::new(result["preRestoreBackup"].as_str().unwrap());
            assert!(crate::backup::verify_archive(pre, &|| Ok(()))
                .unwrap()
                .files
                .contains_key("conversations.db"));
            let until: i64 = fs::read_to_string(pre.with_extension("zip.restore-until"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                until >= control.health_finished.unwrap() + 7 * 86_400_000,
                "protection starts after restored health"
            );
            assert!(crate::backup::verify_archive(pre, &|| Ok(()))
                .unwrap()
                .files
                .contains_key("state/probe.json"));
            let audit = fs::read_to_string(f.0.join("data/audit/audit.jsonl")).unwrap();
            assert!(
                audit.contains("fixture-admin")
                    && audit.contains("succeeded")
                    && audit.contains(archive.file_name().unwrap().to_str().unwrap())
            );
        }
    }

    #[test]
    fn failed_replace_or_health_restores_current_files_restarts_and_alerts() {
        for during_replace in [true, false] {
            let f = Fixture::new();
            fs::write(f.0.join("data/probe.json"), br#"{"value":"old"}"#).unwrap();
            let archive = f.backup();
            mutate(&f);
            let current_config = fs::read(f.0.join("config/config.json")).unwrap();
            let plan = preview(&f.context(), &archive, false).unwrap();
            let mut control = FakeControl {
                unhealthy: !during_replace,
                fail_write: during_replace.then_some(2),
                ..Default::default()
            };
            let error = execute_with(
                &f.context(),
                &archive,
                false,
                plan["confirmation"].as_str().unwrap(),
                "rollback-admin",
                &mut control,
            )
            .unwrap_err();
            assert!(
                error.contains(if during_replace {
                    "temp/write"
                } else {
                    "health deadline expired"
                }),
                "{error}"
            );
            assert_eq!(memory(&f), "current memory");
            assert_eq!(conversation(&f), "current conversation");
            assert_eq!(
                fs::read(f.0.join("config/config.json")).unwrap(),
                current_config
            );
            assert_eq!(
                fs::read_to_string(f.0.join("resources/roles/test.md")).unwrap(),
                "current role"
            );
            assert!(f.0.join("resources/roles/new.md").exists());
            assert_eq!(
                fs::read_to_string(f.0.join("data/probe.json")).unwrap(),
                r#"{"value":"current"}"#
            );
            assert_eq!(
                control.stops, 2,
                "rollback must stop writers again after failed health"
            );
            assert_eq!(control.alerts.len(), 1);
            assert!(control.alerts[0].contains("pre-restore backup restored"));
            assert!(
                matches!(crate::lifecycle::DesiredStore::new(f.0.join("data/lifecycle")).read(), crate::lifecycle::DesiredRead::Present(r) if r.state == crate::lifecycle::DesiredState::Running)
            );
            let audit = fs::read_to_string(f.0.join("data/audit/audit.jsonl")).unwrap();
            assert!(audit.contains("rollback-admin") && audit.contains("failed"));
        }
    }

    #[test]
    fn restored_unfinished_harness_turns_are_paused_and_accept_administrator_resolution() {
        for unreadable_outbox in [false, true] {
            use crate::harness::{Harness, ModelRoute, Status, TurnRequest};
            use crate::turn_ledger::{self, Recorder, State};
            let f = Fixture::new();
            let context = f.context();
            let mut cfg = context.load_config().unwrap();
            cfg["harness"] = json!({"model":"fake-chat"});
            fs::write(
                f.0.join("config/harness-api-key"),
                "synthetic-restore-key-77",
            )
            .unwrap();
            fs::write(f.0.join("config/config.json"), cfg.to_string()).unwrap();
            let archive = {
                let context = context.clone();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(async {
                        let harness = Harness::open(&context).unwrap();
                        let mut handles = Vec::new();
                        for id in ["unknown", "unfinished", "uncertain", "cancelled"] {
                            let key = format!("private:{id}");
                            if id != "unknown" {
                                turn_ledger::append_record(&context, &json!({
                                "k":"begun", "turn":id, "key":key, "backend":"harness",
                                "epoch":"generation-77", "t":1, "inbound":[], "reason":"chat"
                            })).unwrap();
                            }
                            handles.push(
                                harness
                                    .admit(TurnRequest {
                                        id: id.into(),
                                        conversation: key,
                                        generation: "generation-77".into(),
                                        prompt: "interrupted input".into(),
                                        media: Vec::new(),
                                        profile: "chat".into(),
                                        route: ModelRoute::from_runtime(&context, &cfg).unwrap(),
                                        deadline: Duration::from_secs(10),
                                        tools: crate::harness::Tools::empty(),
                                        limits: crate::harness::Limits::from_config(&cfg),
                                    })
                                    .unwrap(),
                            );
                        }
                        crate::send_outbox::append_record(
                            &context,
                            &json!({
                                "k":"intent", "batch":"uncertain-batch", "key":"private:uncertain",
                                "epoch":"generation-77", "t":2, "messages":["possibly delivered"],
                            }),
                        )
                        .unwrap();
                        crate::send_outbox::append_record(
                            &context,
                            &json!({
                                "k":"attempt", "batch":"uncertain-batch", "idx":0, "t":3,
                            }),
                        )
                        .unwrap();
                        // No yield: every admission is committed, no model work has started.
                        // Runtime retirement then aborts these tasks, leaving the durable running rows.
                        let archive = crate::backup::create(&context, &|| Ok(())).unwrap();
                        Recorder::attach(&context, "cancelled", "private:cancelled")
                            .end("cancelled", "cancelled after backup")
                            .unwrap();
                        archive
                    })
                })
                .join()
                .unwrap()
            };
            if unreadable_outbox {
                fs::write(
                    f.0.join("data/send-outbox.jsonl"),
                    "unreadable send evidence\n",
                )
                .unwrap();
            }
            let outbox_before = fs::read(f.0.join("data/send-outbox.jsonl")).unwrap();
            let plan = preview(&context, &archive, false).unwrap();
            execute_with(
                &context,
                &archive,
                false,
                plan["confirmation"].as_str().unwrap(),
                "fixture",
                &mut FakeControl::default(),
            )
            .unwrap();
            let (turns, _) = turn_ledger::fold(&turn_ledger::read_records(&context).unwrap());
            let harness = Harness::open(&context).unwrap();
            assert!(matches!(
                harness.status("cancelled").unwrap(),
                Some(Status::Cancelled)
            ));
            for (id, reason) in [
                ("unknown", "harness-restore-unknown"),
                ("unfinished", "harness-interrupted"),
                ("uncertain", "send-unresolved"),
            ] {
                let turn = turns
                    .iter()
                    .find(|t| t.id == id)
                    .expect("restored turn missing from administrator ledger");
                assert_eq!(turn.state, State::Paused, "{id}");
                let reason = if unreadable_outbox {
                    "send-unresolved"
                } else {
                    reason
                };
                assert_eq!(turn.pause_reason, reason, "{id}");
                assert!(matches!(harness.status(id).unwrap(), Some(Status::Running)));
                if reason == "send-unresolved" {
                    assert_eq!(
                        crate::console::handle_turns_resolve(
                            &context,
                            &json!({"turnId":id,"action":"retry"}).to_string()
                        )
                        .unwrap_err()
                        .0,
                        409
                    );
                }
                let action = if id == "uncertain" {
                    "retry"
                } else {
                    "abandon"
                };
                let response = crate::console::handle_turns_resolve(
                    &context,
                    &json!({"turnId":id,"action":action,"confirm":true}).to_string(),
                )
                .unwrap();
                assert_eq!(response["ok"], true);
                // Apply the existing bridge resolution boundary, without replay or any network task.
                crate::harness::resolve_interrupted(&context, id).unwrap();
                Recorder::attach(&context, id, &turn.key).applied().unwrap();
                assert!(matches!(
                    harness.status(id).unwrap(),
                    Some(Status::Cancelled)
                ));
            }
            assert!(
                turns.iter().all(|t| t.attempts.is_empty()),
                "restore must not submit model work"
            );
            assert_eq!(
                fs::read(f.0.join("data/send-outbox.jsonl")).unwrap(),
                outbox_before,
                "restore/resolution must not append sends or rewrite unreadable evidence"
            );
        }
    }

    #[test]
    fn restored_unfinished_tool_intent_requires_confirmed_retry() {
        use crate::harness::{
            store::Store, Event, Harness, Limits, ModelRoute, Status, Tools, TurnRequest,
        };
        use crate::turn_ledger::{self, Recorder, State};
        use genai::chat::{ChatMessage, ToolCall, ToolResponse};

        for (known_turn, uncertain_result) in
            [(true, false), (false, false), (true, true), (false, true)]
        {
            let f = Fixture::new();
            let context = f.context();
            let mut cfg = context.load_config().unwrap();
            cfg["harness"] = json!({"model":"fake-chat"});
            fs::write(
                f.0.join("config/harness-api-key"),
                "synthetic-restore-key-77",
            )
            .unwrap();
            let id = "unfinished-tool";
            let key = "private:unfinished-tool";
            if known_turn {
                turn_ledger::append_record(
                    &context,
                    &json!({
                        "k":"begun", "turn":id, "key":key, "backend":"harness",
                        "epoch":"generation-77", "t":1, "inbound":[], "reason":"chat"
                    }),
                )
                .unwrap();
            }
            let request = TurnRequest {
                id: id.into(),
                conversation: key.into(),
                generation: "generation-77".into(),
                prompt: "interrupted tool input".into(),
                media: Vec::new(),
                profile: "chat".into(),
                route: ModelRoute::from_runtime(&context, &cfg).unwrap(),
                deadline: Duration::from_secs(10),
                tools: Tools::empty(),
                limits: Limits::from_config(&cfg),
            };
            let call = ToolCall {
                call_id: "restore-call-77".into(),
                fn_name: "qq_send_private_message".into(),
                fn_arguments: json!({"message":"possibly delivered"}),
                thought_signatures: None,
            };
            let message = ChatMessage::assistant(vec![call.clone()]);
            let archive = {
                // Model a process lost after the durable intent but before result settlement.
                // Use the store's real schema-v2 writes; never dispatch a handler or model request.
                let store = Store::open(&context).unwrap();
                store.admit(&request).unwrap();
                let attempt = store
                    .complete_step(
                        id,
                        &Event::Assistant {
                            text: String::new(),
                            raw: json!({}),
                            message: message.clone(),
                        },
                    )
                    .unwrap();
                let run = store.tool_start(id, &attempt, &call).unwrap();
                if uncertain_result {
                    let outcome = json!({"_meta":{"herDelivery":"unknown"}});
                    let result =
                        ChatMessage::tool(ToolResponse::from_tool_call(&call, outcome.to_string()));
                    store.tool_result(id, &run, &result, &outcome).unwrap();
                }
                crate::backup::create(&context, &|| Ok(())).unwrap()
            };
            let plan = preview(&context, &archive, false).unwrap();
            execute_with(
                &context,
                &archive,
                false,
                plan["confirmation"].as_str().unwrap(),
                "fixture",
                &mut FakeControl::default(),
            )
            .unwrap();

            let (turns, _) = turn_ledger::fold(&turn_ledger::read_records(&context).unwrap());
            let turn = turns.iter().find(|t| t.id == id).unwrap();
            assert_eq!(turn.state, State::Paused);
            assert_eq!(
                turn.pause_reason, "send-unresolved",
                "known turn: {known_turn}, uncertain result: {uncertain_result}"
            );
            let harness = Harness::open(&context).unwrap();
            assert!(matches!(harness.status(id).unwrap(), Some(Status::Running)));
            assert_eq!(
                crate::console::handle_turns_resolve(
                    &context,
                    &json!({"turnId":id,"action":"retry"}).to_string()
                )
                .unwrap_err()
                .0,
                409
            );
            assert_eq!(
                crate::console::handle_turns_resolve(
                    &context,
                    &json!({"turnId":id,"action":"retry","confirm":true}).to_string()
                )
                .unwrap()["ok"],
                true
            );
            crate::harness::resolve_interrupted(&context, id).unwrap();
            Recorder::attach(&context, id, key).applied().unwrap();
            assert!(matches!(
                harness.status(id).unwrap(),
                Some(Status::Cancelled)
            ));

            // Reconciliation/resolution preserves canonical facts and tool state;
            // neither a result nor a send may be fabricated while closing local authority.
            let db =
                rusqlite::Connection::open(context.data_dir().join("conversations.db")).unwrap();
            let payload: String = db
                .query_row(
                    "SELECT payload FROM events WHERE turn_id=?1 AND kind='assistant'",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&payload).unwrap()["message"],
                serde_json::to_value(&message).unwrap()
            );
            let (state, result): (String, Option<String>) = db
                .query_row(
                    "SELECT state,result FROM tool_runs WHERE turn_id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            if uncertain_result {
                assert_eq!(state, "uncertain");
                assert_eq!(
                    serde_json::from_str::<Value>(&result.unwrap()).unwrap(),
                    json!({"_meta":{"herDelivery":"unknown"}})
                );
            } else {
                assert_eq!(state, "running");
                assert!(result.is_none());
            }
            let results: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE turn_id=?1 AND kind='tool_result'",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(results, i64::from(uncertain_result));
            assert!(crate::send_outbox::read_records(&context)
                .unwrap()
                .is_empty());
        }
    }

    fn continuation_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// The admitted turn's task owns the conversation store handle and the turn owner lock until
    /// that task ends, so the runtime is shut down with a bounded wait before the fixture root
    /// (and every file inside it) is allowed to disappear; the removal itself stays fatal.
    fn harness_continuation(backup_while_running: bool, write_during_snapshot: bool) {
        let runtime = continuation_runtime();
        let fixture = runtime
            .block_on(harness_restore_continuation(backup_while_running, write_during_snapshot));
        runtime.shutdown_timeout(Duration::from_secs(30));
        drop(fixture);
    }

    #[test]
    fn harness_conversation_continues_after_runtime_restore() {
        harness_continuation(false, false);
    }

    #[test]
    fn restored_empty_completion_keeps_its_ledger_reason() {
        use crate::harness::{store::Store, Limits, ModelRoute, Status, Tools, TurnRequest};
        use crate::turn_ledger::Recorder;

        let f = Fixture::new();
        let context = f.context();
        let mut cfg = context.load_config().unwrap();
        cfg["harness"] = json!({"model":"fake-chat"});
        fs::write(f.0.join("config/harness-api-key"), "synthetic-restore-key").unwrap();
        let mut turns = Vec::new();
        let archive = {
            let store = Store::open(&context).unwrap();
            for reason in ["observationSleep", "ok"] {
                let key = format!("private:{reason}");
                let turn = Recorder::begin_reserved2(&context, &key, "harness", "atMention", "generation", &[]).unwrap();
                store.admit(&TurnRequest {
                    id: turn.id.clone(), conversation: key, generation: "generation".into(),
                    prompt: "wake".into(), media: Vec::new(), profile: "reserved2".into(),
                    route: ModelRoute::from_runtime(&context, &cfg).unwrap(), deadline: Duration::from_secs(10),
                    tools: Tools::empty(), limits: Limits::from_config(&cfg),
                }).unwrap();
                turns.push((turn, reason));
            }
            f.backup()
        };
        for (turn, reason) in &turns { turn.end("completed", reason).unwrap(); }
        let plan = preview(&context, &archive, false).unwrap();
        execute_with(&context, &archive, false, plan["confirmation"].as_str().unwrap(), "fixture",
            &mut FakeControl::default()).unwrap();
        let store = Store::open(&context).unwrap();
        let db = rusqlite::Connection::open(context.data_dir().join("conversations.db")).unwrap();
        for (turn, reason) in turns {
            let status = store.status(&turn.id).unwrap().unwrap();
            match reason {
                "observationSleep" => assert!(matches!(status, Status::ObservationSleep)),
                _ => assert!(matches!(status, Status::Completed(text) if text.is_empty())),
            }
            let recorded: String = db.query_row(
                "SELECT json_extract(payload,'$.reason') FROM events WHERE turn_id=?1 AND kind='turn_ended'",
                [&turn.id], |row| row.get(0),
            ).unwrap();
            assert_eq!(recorded, reason);
        }
    }

    #[test]
    fn harness_turn_completed_after_backup_does_not_block_restored_conversation() {
        harness_continuation(true, false);
    }

    #[test]
    fn conversation_wal_write_during_backup_verifies_and_restores_consistently() {
        harness_continuation(true, true);
    }

    async fn harness_restore_continuation(
        backup_while_running: bool,
        write_during_snapshot: bool,
    ) -> Fixture {
        use crate::harness::{Harness, ModelRoute, Status, TurnRequest};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let f = Fixture::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (release_tx, mut release_rx) = tokio::sync::oneshot::channel();
        let model = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in ["archived reply", "discarded reply", "continued reply"] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut size = None;
                loop {
                    let mut line = String::new();
                    assert!(socket.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            size = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                }
                let mut body = vec![0; size.unwrap()];
                socket.read_exact(&mut body).await.unwrap();
                requests.push(serde_json::from_slice::<Value>(&body).unwrap());
                if backup_while_running && requests.len() == 1 {
                    (&mut release_rx).await.unwrap();
                }
                let content = json!({"id":"restore-77","object":"chat.completion.chunk","model":"fake-chat","choices":[{"index":0,"delta":{"content":reply},"finish_reason":null}]});
                let end = json!({"id":"restore-77","object":"chat.completion.chunk","model":"fake-chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
                let body = format!("data: {content}\n\ndata: {end}\n\ndata: [DONE]\n\n");
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
            requests
        });
        let mut cfg: Value =
            serde_json::from_slice(&fs::read(f.0.join("config/config.json")).unwrap()).unwrap();
        cfg["harness"] = json!({"model":"fake-chat"});
        fs::write(f.0.join("config/config.json"), cfg.to_string()).unwrap();
        fs::write(
            f.0.join("config/harness-api-key"),
            "synthetic-restore-key-77",
        )
        .unwrap();
        fs::write(
            f.0.join("dependencies.json"),
            json!({"model":endpoint,"dsh":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"})
                .to_string(),
        )
        .unwrap();
        fn request(context: &RuntimeContext, cfg: &Value, id: &str, prompt: &str) -> TurnRequest {
            TurnRequest {
                id: id.into(),
                conversation: "private:fixture".into(),
                generation: "generation-77".into(),
                prompt: prompt.into(),
                media: Vec::new(),
                profile: "chat".into(),
                route: ModelRoute::from_runtime(context, cfg).unwrap(),
                deadline: Duration::from_secs(10),
                tools: crate::harness::Tools::empty(),
                limits: crate::harness::Limits::from_config(cfg),
            }
        }
        // Joining the isolated runtime models process retirement: no SQLite handle or
        // unfinished harness task from the original process can survive replacement.
        let context = f.context();
        let saved = {
            let context = context.clone();
            let cfg = cfg.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let harness = Harness::open(&context).unwrap();
                    let ledger = crate::turn_ledger::Recorder::begin(
                        &context,
                        "private:fixture",
                        "harness",
                        "chat",
                        "generation-77",
                        &[],
                        None,
                    )
                    .unwrap();
                    let mut first = harness
                        .admit(request(
                            &context,
                            &cfg,
                            &ledger.id,
                            "remember archived input",
                        ))
                        .unwrap();
                    let archive = if backup_while_running {
                        assert!(matches!(
                            harness.status(&ledger.id).unwrap(),
                            Some(Status::Running)
                        ));
                        let archive = if write_during_snapshot {
                            use std::cell::RefCell;
                            let padding = rusqlite::Connection::open(context.data_dir().join("conversations.db")).unwrap();
                            // Force multiple snapshot steps, then make the actual turn's
                            // completion commit in WAL between two of those steps.
                            padding.execute_batch("PRAGMA wal_autocheckpoint=0; CREATE TABLE backup_padding(value BLOB); INSERT INTO backup_padding VALUES(zeroblob(8388608)); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
                            let total: u64 = padding.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get(0)).unwrap();
                            let release = RefCell::new(Some(release_tx));
                            let archive = crate::backup::create(&context, &|| {
                                if release.borrow().is_some() {
                                    let partial = match fs::read_dir(context.runtime_root().unwrap().join("backups")) {
                                        Ok(entries) => entries.filter_map(Result::ok)
                                            .map(|e| e.path().join("conversations.db"))
                                            .filter_map(|p| fs::metadata(p).ok())
                                            .any(|m| m.len() > 0 && m.len() < total),
                                        // The first cancellation check precedes destination creation.
                                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                                        Err(error) => panic!("backup staging could not be observed: {error}"),
                                    };
                                    if partial {
                                        release.borrow_mut().take().unwrap().send(()).unwrap();
                                        let deadline = Instant::now() + Duration::from_secs(10);
                                        while matches!(harness.status(&ledger.id).unwrap(), Some(Status::Running)) {
                                            assert!(Instant::now() < deadline, "turn did not commit during the partial snapshot");
                                            std::thread::sleep(Duration::from_millis(5));
                                        }
                                        assert!(matches!(harness.status(&ledger.id).unwrap(), Some(Status::Completed(text)) if text == "archived reply"));
                                        println!("conversation turn committed while its online snapshot was partial");
                                    }
                                }
                                Ok(())
                            }).unwrap();
                            assert!(release.borrow().is_none(), "backup did not overlap the conversation writer");
                            crate::backup::verify_archive(&archive, &|| Ok(())).unwrap();
                            drop(padding);
                            archive
                        } else {
                            let archive = crate::backup::create(&context, &|| Ok(())).unwrap();
                            release_tx.send(()).unwrap();
                            archive
                        };
                        assert_eq!(first.wait().await.unwrap(), "archived reply");
                        ledger
                            .end("completed", "fixture finished after backup")
                            .unwrap();
                        archive
                    } else {
                        assert_eq!(first.wait().await.unwrap(), "archived reply");
                        ledger
                            .end("completed", "fixture finished before backup")
                            .unwrap();
                        crate::backup::create(&context, &|| Ok(())).unwrap()
                    };
                    let mut discarded = harness
                        .admit(request(
                            &context,
                            &cfg,
                            "discarded",
                            "discard this later input",
                        ))
                        .unwrap();
                    assert_eq!(discarded.wait().await.unwrap(), "discarded reply");
                    (archive, ledger.id)
                })
            })
            .join()
            .unwrap()
        };
        let (saved, saved_id) = saved;
        let plan = preview(&context, &saved, false).unwrap();
        execute_with(
            &context,
            &saved,
            false,
            plan["confirmation"].as_str().unwrap(),
            "fixture",
            &mut FakeControl::default(),
        )
        .unwrap();
        let harness = Harness::open(&context).unwrap();
        assert!(
            matches!(harness.status(&saved_id).unwrap(), Some(Status::Completed(text))
                if text == if backup_while_running && !write_during_snapshot { "" } else { "archived reply" })
        );
        assert!(harness.status("discarded").unwrap().is_none());
        let mut next = harness
            .admit(request(
                &context,
                &cfg,
                "continued",
                "continue archived chat",
            ))
            .unwrap();
        assert_eq!(next.wait().await.unwrap(), "continued reply");
        let requests = tokio::time::timeout(Duration::from_secs(10), model)
            .await
            .unwrap()
            .unwrap();
        let messages = requests[2]["messages"].as_array().unwrap();
        assert!(messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "remember archived input"));
        assert_eq!(
            messages
                .iter()
                .any(|m| m["role"] == "assistant" && m["content"] == "archived reply"),
            !backup_while_running || write_during_snapshot,
            "restore must not invent an assistant fact absent from the snapshot"
        );
        assert_eq!(
            messages.last().unwrap()["content"],
            "continue archived chat"
        );
        assert!(!serde_json::to_string(messages).unwrap().contains("discard"));
        f
    }

    #[test]
    fn legacy_archive_and_rollback_restore_conversation_database_presence() {
        for (archive_has_conversations, unhealthy) in [(false, false), (false, true), (true, true)]
        {
            let f = Fixture::new();
            if !archive_has_conversations {
                fs::remove_file(f.0.join("data/conversations.db")).unwrap();
            }
            let archive = f.backup();
            if archive_has_conversations {
                // The current runtime never opened the harness. Rollback must remove
                // conversation storage introduced by a failed restore.
                fs::remove_file(f.0.join("data/conversations.db")).unwrap();
            } else {
                rusqlite::Connection::open(f.0.join("data/conversations.db")).unwrap()
                        .execute_batch("CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES('current conversation');").unwrap();
            }
            let plan = preview(&f.context(), &archive, false).unwrap();
            if !archive_has_conversations {
                assert!(plan["removes"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("data/conversations.db")));
            }
            let mut control = FakeControl {
                unhealthy,
                ..Default::default()
            };
            let result = execute_with(
                &f.context(),
                &archive,
                false,
                plan["confirmation"].as_str().unwrap(),
                "fixture",
                &mut control,
            );
            assert_eq!(result.is_err(), unhealthy);
            let should_exist = unhealthy && !archive_has_conversations;
            assert_eq!(f.0.join("data/conversations.db").exists(), should_exist);
            if should_exist {
                assert_eq!(conversation(&f), "current conversation");
            }
            for suffix in ["-wal", "-shm", "-journal"] {
                assert!(!f.0.join(format!("data/conversations.db{suffix}")).exists());
            }
        }
    }

    #[test]
    fn rollback_failure_audit_keeps_recovery_status_and_archive_outside_truncated_reason() {
        let f = Fixture::new();
        let archive = f.backup();
        mutate(&f);
        let context = f.context();
        let plan = preview(&context, &archive, false).unwrap();
        let mut control = FakeControl {
            unhealthy: true,
            health_error: Some(format!(
                "fixture health failure: {}",
                "component probe failed without progress; ".repeat(20)
            )),
            fail_rollback_stop: true,
            ..Default::default()
        };
        let error = execute_with(
            &context,
            &archive,
            false,
            plan["confirmation"].as_str().unwrap(),
            "rollback-failure-admin",
            &mut control,
        )
        .unwrap_err();
        assert!(
            error.contains("rollback failed: fixture rollback stop failed"),
            "{error}"
        );
        assert_eq!(control.alerts.len(), 1);
        assert!(control.alerts[0].contains("rollback failed"));
        let text = fs::read_to_string(f.0.join("data/audit/audit.jsonl")).unwrap();
        let event: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(event["by"], "rollback-failure-admin");
        assert_eq!(event["result"], "failed");
        assert_eq!(event["rollbackStatus"], "failed", "{event}");
        let id = event["preRestoreArchive"]
            .as_str()
            .expect("separate recovery archive identifier");
        assert!(
            !id.contains(['/', '\\', ':']),
            "archive id must survive path scrubbing"
        );
        let recovery = f.0.join("backups").join(id);
        assert_ne!(recovery, archive);
        crate::backup::verify_archive(&recovery, &|| Ok(())).unwrap();
        let detail = event["detail"].as_str().unwrap();
        assert!(
            detail.chars().count() <= 300,
            "diagnostic text remains bounded"
        );
        assert!(
            !detail.contains("rollback failed"),
            "long original verdict demonstrates why structured recovery fields are necessary"
        );
    }

    fn console_refusal_audit(case: &str) {
        use crate::lifecycle::{Acquire, OwnerGate, LIVE_LOCK};
        let f = Fixture::new();
        let context = f.context();
        let archive = f.backup();
        let plan = preview(&context, &archive, true).unwrap();
        let mut confirmation = plan["confirmation"].as_str().unwrap();
        let _busy = (case == "busy").then(|| operation_gate(&context).unwrap());
        if case == "stale" {
            confirmation = "stale-confirmation";
        }
        let _foreign_owner = if case == "preflight" {
            crate::lifecycle::validate_owner_gate_paths(context.data_dir()).unwrap();
            match OwnerGate::new(context.data_dir().join("logs").join(LIVE_LOCK)).acquire(
                "foreign",
                crate::lifecycle::current_identity().unwrap(),
                None,
                crate::parity_clock::now_ms(),
                Duration::ZERO,
            ) {
                Acquire::Acquired(owner) => Some(owner),
                other => panic!("fixture owner acquisition failed: {other:?}"),
            }
        } else {
            None
        };
        if case == "spawn" {
            // Unit test images live in deps/, without the packaged CLI twin. Exercise
            // the real OS spawn failure without changing any installed executable.
            let twin = std::env::current_exe()
                .unwrap()
                .with_file_name(if cfg!(windows) {
                    "her-cli.exe"
                } else {
                    "her-cli"
                });
            assert!(
                !twin.exists(),
                "fixture requires an unpackaged test executable"
            );
        }
        let error = queue(&context, &archive, true, confirmation).unwrap_err();
        match case {
            "busy" => assert!(error.contains("update or another restore"), "{error}"),
            "stale" => assert!(error.contains("confirmation"), "{error}"),
            "preflight" => assert!(error.contains("live owner"), "{error}"),
            "spawn" => assert!(
                !error.contains("confirmation") && !error.contains("owner"),
                "{error}"
            ),
            _ => unreachable!(),
        }
        assert!(!f.0.join("data/lifecycle/desired-state.json").exists());
        let text = fs::read_to_string(f.0.join("data/audit/audit.jsonl")).unwrap_or_default();
        let entries: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let event = entries
            .iter()
            .find(|event| event["action"] == "backup.restore")
            .unwrap_or_else(|| panic!("{case} console refusal must be audited: {text}"));
        assert_eq!(event["by"], "console:administrator");
        assert_eq!(
            event["archive"],
            json!(crate::subprocess::external_path(&archive).unwrap()),
            "the audit names the selected archive in its ordinary filesystem spelling"
        );
        assert_eq!(event["scope"], "memory");
        assert_eq!(event["result"], "failed");
        assert!(!event["detail"].as_str().unwrap().is_empty());
        assert_eq!(
            entries.len(),
            1,
            "one audit record per refused console request"
        );
    }
    #[test]
    fn console_busy_refusal_is_audited() {
        console_refusal_audit("busy");
    }
    #[test]
    fn console_stale_refusal_is_audited() {
        console_refusal_audit("stale");
    }
    #[test]
    fn console_preflight_refusal_is_audited() {
        console_refusal_audit("preflight");
    }
    #[test]
    fn console_spawn_refusal_is_audited() {
        console_refusal_audit("spawn");
    }

    #[test]
    fn damaged_changed_unconfirmed_and_busy_restores_never_stop_the_bridge_and_are_audited() {
        let f = Fixture::new();
        let archive = f.backup();
        let plan = preview(&f.context(), &archive, true).unwrap();
        let confirmation = plan["confirmation"].as_str().unwrap();
        let mut control = FakeControl::default();
        let gate = operation_gate(&f.context()).unwrap();
        assert!(execute_with(
            &f.context(),
            &archive,
            true,
            confirmation,
            "busy-admin",
            &mut control
        )
        .unwrap_err()
        .contains("update or another restore"));
        drop(gate);
        let restore_gate = f
            .context()
            .open_locked_file(RuntimeArea::Data, "locks/restore.lock")
            .unwrap();
        assert!(execute_with(
            &f.context(),
            &archive,
            true,
            confirmation,
            "busy-admin",
            &mut control
        )
        .unwrap_err()
        .contains("another restore"));
        drop(restore_gate);
        assert!(execute_with(
            &f.context(),
            &archive,
            false,
            confirmation,
            "wrong-scope",
            &mut control
        )
        .unwrap_err()
        .contains("confirmation"));
        assert!(execute_with(
            &f.context(),
            &archive,
            true,
            "",
            "unconfirmed-admin",
            &mut control
        )
        .unwrap_err()
        .contains("confirmation"));
        let other = Fixture::new();
        assert!(execute_with(
            &other.context(),
            &archive,
            true,
            confirmation,
            "wrong-runtime",
            &mut control
        )
        .unwrap_err()
        .contains("confirmation"));
        assert!(!other.0.join("data/lifecycle/desired-state.json").exists());
        fs::write(&archive, b"damaged").unwrap();
        assert!(execute_with(
            &f.context(),
            &archive,
            true,
            confirmation,
            "damaged-admin",
            &mut control
        )
        .is_err());
        assert_eq!(control.stops, 0);
        assert_eq!(memory(&f), "old memory");
        assert!(!f.0.join("data/lifecycle/desired-state.json").exists());
        let audit = fs::read_to_string(f.0.join("data/audit/audit.jsonl")).unwrap();
        for by in [
            "busy-admin",
            "wrong-scope",
            "unconfirmed-admin",
            "damaged-admin",
        ] {
            assert!(audit.contains(by));
        }
    }

    #[test]
    fn ordinary_path_drops_the_verbatim_prefix_or_keeps_it_visibly() {
        assert_eq!(ordinary_path(Path::new(r"\\?\C:\x\backups\a.zip")), r"C:\x\backups\a.zip");
        assert_eq!(ordinary_path(Path::new(r"\\?\UNC\srv\share\a.zip")), r"\\srv\share\a.zip");
        assert_eq!(ordinary_path(Path::new(r"\\?\GLOBALROOT\x")), r"\\?\GLOBALROOT\x");
    }

    include!("restore_settings_tests.rs");
}
