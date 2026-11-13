//! Short-lived updater engine and injected platform boundary.
use crate::update::release_path;
use crate::{install_root::InstallRoot, process_identity::ProcessIdentity, release_store};
use serde_json::{json, Value};
use std::time::Duration;
use std::{
    fs,
    path::{Path, PathBuf},
};
pub struct RetirementFailure {
    reason: String,
    stopped: bool,
}
pub trait Platform {
    fn current(&mut self, name: &str) -> Result<Option<String>, String>;
    fn trial(&mut self, tag: &str) -> Result<bool, String>;
    fn wait_old(&mut self, old: &ProcessIdentity) -> Result<(), RetirementFailure>;
    fn switch(&mut self, name: &str, tag: &str) -> Result<(), String>;
    /// Retire the failed `tag` through the already verified `cli` release.
    fn stop(&mut self, tag: &str, cli: &str) -> Result<(), String>;
    fn start(&mut self, tag: &str) -> Result<(), String>;
    fn healthy(&mut self, tag: &str, budget: Duration) -> Result<bool, String>;
}

pub fn handover(
    root: &InstallRoot,
    target: &str,
    old: &ProcessIdentity,
    platform: &mut dyn Platform,
) -> Result<(), String> {
    let _gate = release_store::installation_gate(
        &root.root().join("runtime/data/locks/update-restore.lock"),
    )?;
    let current = platform
        .current("current")?
        .ok_or("current release unavailable")?;
    let old_image = fs::canonicalize(&old.exe).map_err(|_| "old image unavailable")?;
    if old_image.parent()
        != Some(
            fs::canonicalize(release_path(root, &current)?)
                .map_err(|_| "current unavailable")?
                .as_path(),
        )
    {
        return Err("old supervisor is not current".into());
    }
    if release_store::version(target) <= release_store::version(&current) {
        crate::update::signature_refusal(root, target, &current)?;
        return Err(crate::release_signature::REFUSED.into());
    }
    if let Err(error) = crate::update::verify_handover(root, target, &current) {
        crate::update::signature_refusal(root, target, &current)?;
        return Err(error);
    }
    if !platform.trial(target)? {
        return Err("release trial did not pass assert-running".into());
    }
    let recorded_last_good = platform.current("last-good")?;
    let last_good = recorded_last_good
        .clone()
        .unwrap_or_else(|| current.clone());
    if crate::update::verify_rollback(root, &last_good).is_err() {
        crate::update::signature_refusal(root, target, &current)?;
        return Err(crate::release_signature::REFUSED.into());
    }
    if recorded_last_good.is_none() {
        platform.switch("last-good", &last_good)?;
    }
    // This helper outlives the old supervisor. Own turn admission here so retiring
    // that supervisor cannot release the fence before the remaining owners stop.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let _turns = loop {
        match release_store::installation_gate(
            &root.root().join("runtime/data/locks/turn-update.lock"),
        ) {
            Ok(gate) => break gate,
            Err(error)
                if error == "another release installation or restore is running"
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => {
                return Err(format!(
                    "turn admission unavailable before handover: {error}"
                ))
            }
        }
    };
    if let Err(failure) = platform.wait_old(old) {
        return if failure.stopped {
            rollback(root, target, &last_good, &failure.reason, platform)
        } else {
            Err(failure.reason)
        };
    }
    let switched = (|| -> Result<(), String> {
        platform.switch("current", target)?;
        platform.start(target)?;
        if !platform.healthy(target, Duration::from_secs(120))? {
            return Err("new supervisor was not healthy within 120 seconds".into());
        }
        Ok(())
    })();
    if let Err(reason) = switched {
        return rollback(root, target, &last_good, &reason, platform);
    }
    platform.switch("last-good", target)?;
    crate::update::mark_healthy(root, target)?;
    prune(root, platform)
}

// Available only in fixture builds. No fallback to native effects on a fake error.
#[cfg(feature = "parity-clock")]
struct Fake {
    path: PathBuf,
    state: Value,
}
#[cfg(feature = "parity-clock")]
impl Fake {
    fn event(&mut self, event: String) -> Result<(), String> {
        self.state["events"]
            .as_array_mut()
            .ok_or("fake events missing")?
            .push(event.into());
        fs::write(&self.path, self.state.to_string()).map_err(|_| "fake state unavailable".into())
    }
}
#[cfg(feature = "parity-clock")]
impl Platform for Fake {
    fn current(&mut self, name: &str) -> Result<Option<String>, String> {
        self.state[if name == "current" {
            "current"
        } else {
            "lastGood"
        }]
        .as_str()
        .map(str::to_string)
        .map(Some)
        .map_or(Ok(None), Ok)
    }
    fn trial(&mut self, tag: &str) -> Result<bool, String> {
        self.event(format!("trial:{tag}"))?;
        Ok(self.state["trial"] == true)
    }
    fn wait_old(&mut self, old: &ProcessIdentity) -> Result<(), RetirementFailure> {
        self.event(format!("wait:{}:{}", old.pid, old.start))
            .map_err(|reason| RetirementFailure {
                reason,
                stopped: false,
            })?;
        if self.state["old"] == "gone" {
            Ok(())
        } else {
            Err(RetirementFailure {
                reason: "old supervisor exit was not verified".into(),
                stopped: false,
            })
        }
    }
    fn switch(&mut self, name: &str, tag: &str) -> Result<(), String> {
        self.state[if name == "current" {
            "current"
        } else {
            "lastGood"
        }] = tag.into();
        self.event(format!(
            "{}:{tag}",
            if name == "current" {
                "switch"
            } else {
                "last-good"
            }
        ))
    }
    fn stop(&mut self, tag: &str, cli: &str) -> Result<(), String> {
        self.event(format!("stop:{tag}"))?;
        self.event(format!("retire-cli:{cli}"))
    }
    fn start(&mut self, tag: &str) -> Result<(), String> {
        self.event(format!("start:{tag}"))
    }
    fn healthy(&mut self, _: &str, budget: Duration) -> Result<bool, String> {
        self.event(format!("health:{}", budget.as_millis()))?;
        Ok(self.state["healthy"] == true)
    }
}

pub fn run_helper(args: &[String]) -> i32 {
    let automatic = args.first().is_some_and(|arg| arg == "--auto");
    let args = if automatic { &args[1..] } else { args };
    let recovery = args.first().is_some_and(|arg| arg == "--recover");
    let args = if recovery { &args[1..] } else { args };
    let prune_only = args.len() == 1 && args[0] == "--prune";
    let result = (|| -> Result<(), String> {
        if automatic && (recovery || prune_only) {
            return Err("automatic handover cannot recover or prune".into());
        }
        if !prune_only
            && (args.len() != 8
                || args[0] != "--target"
                || args[2] != "--old-pid"
                || args[4] != "--old-start"
                || args[6] != "--old-exe")
        {
            return Err("usage: herupdate [--auto | --recover] --target <tag> --old-pid <pid> --old-start <stamp> --old-exe <image> | --prune [--install-root <root>]".into());
        }
        let root = fixed_root()?;
        let old = if prune_only {
            None
        } else {
            let old = ProcessIdentity {
                pid: args[3].parse().map_err(|_| "invalid old PID")?,
                start: args[5].parse().map_err(|_| "invalid old start time")?,
                exe: args[7].clone(),
            };
            if old.pid == 0 || old.start == 0 {
                return Err("old supervisor requires PID and creation time".into());
            }
            root.require_release_image(Path::new(&old.exe))?;
            Some(old)
        };
        let target = if prune_only { "" } else { &args[1] };
        #[cfg(feature = "parity-clock")]
        if let Some(path) = std::env::var_os("HER_TEST_UPDATE_PLATFORM") {
            let path = PathBuf::from(path);
            root.check_development(&path)?;
            let state =
                serde_json::from_slice(&fs::read(&path).map_err(|_| "fake platform unavailable")?)
                    .map_err(|_| "fake platform invalid")?;
            return dispatch(
                &root,
                target,
                old.as_ref(),
                &mut Fake { path, state },
                recovery,
            );
        }
        let native_root = InstallRoot::new(root.root())?;
        dispatch(
            &root,
            target,
            old.as_ref(),
            &mut Native {
                root: native_root,
                target: target.into(),
                recovery,
                automatic,
                child: None,
                launches: 0,
            },
            recovery,
        )
    })();
    match result {
        Ok(()) => {
            println!("{}", json!({"ok":true}));
            0
        }
        Err(error) => {
            eprintln!("[herupdate] {error}");
            2
        }
    }
}
fn fixed_root() -> Result<InstallRoot, String> {
    let root = InstallRoot::configured()?;
    let image = fs::canonicalize(std::env::current_exe().map_err(|_| "updater image unavailable")?)
        .map_err(|_| "updater image unavailable")?;
    let metadata = fs::symlink_metadata(root.root().join("herupdate.exe"))
        .map_err(|_| "fixed updater unavailable")?;
    if !metadata.is_file() || release_store::redirected(&metadata) {
        return Err("fixed updater is redirected".into());
    }
    let expected = fs::canonicalize(root.root().join("herupdate.exe"))
        .map_err(|_| "fixed updater unavailable")?;
    if image != expected {
        return Err("herupdate must run from the install root".into());
    }
    release_store::directory(root.root())?;
    Ok(root)
}
fn dispatch(
    root: &InstallRoot,
    target: &str,
    old: Option<&ProcessIdentity>,
    platform: &mut dyn Platform,
    recovery: bool,
) -> Result<(), String> {
    if let Some(old) = old {
        return run(root, target, old, platform, recovery);
    }
    let _gate = release_store::installation_gate(
        &root.root().join("runtime/data/locks/update-restore.lock"),
    )?;
    prune(root, platform)
}

struct Native {
    root: InstallRoot,
    target: String,
    recovery: bool,
    automatic: bool,
    child: Option<std::process::Child>,
    launches: usize,
}
impl Native {
    fn cli(&self, tag: &str) -> Result<std::process::Command, String> {
        let path = release_path(&self.root, tag)?;
        let mut command = crate::subprocess::command_without_console(path.join("her-cli.exe"));
        command
            .env_remove("HER_TRIAL_CURRENT")
            .env(
                "HER_INSTALL_ROOT",
                crate::subprocess::external_path(self.root.root())?,
            )
            .current_dir(crate::subprocess::external_path(path)?);
        Ok(command)
    }
    fn health(&self, tag: &str, trial: bool, budget: Duration) -> Result<bool, String> {
        let mut command = self.cli(tag)?;
        command
            .args(["lifecycle", "assert-running", "--runtime-root"])
            .arg(crate::subprocess::external_path(
                self.root.root().join("runtime"),
            )?)
            .args(["--wait", &format!("{}ms", budget.as_millis())]);
        if trial {
            command.env(
                "HER_TRIAL_CURRENT",
                fs::canonicalize(self.root.root().join("current"))
                    .map_err(|_| "current release unavailable")?,
            );
        }
        run_bounded(&mut command, budget)
    }
}
fn run_bounded(command: &mut std::process::Command, budget: Duration) -> Result<bool, String> {
    Ok(run_bounded_status(command, budget)?.is_some_and(|status| status.success()))
}
fn run_bounded_status(
    command: &mut std::process::Command,
    budget: Duration,
) -> Result<Option<std::process::ExitStatus>, String> {
    use std::{process::Stdio, time::Instant};
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "release child could not start")?;
    let end = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
        }
    }
}
impl Platform for Native {
    fn current(&mut self, name: &str) -> Result<Option<String>, String> {
        for link in ["current", "last-good"] {
            recover_link(&self.root, link)?;
        }
        crate::update::link_tag(&self.root, name)
    }
    fn trial(&mut self, tag: &str) -> Result<bool, String> {
        let passed = self.health(
            tag,
            true,
            judgment_budget(&self.root, Duration::from_secs(120))?,
        )?;
        #[cfg(feature = "parity-clock")]
        start_barrier(&self.root, tag, "trial")?;
        Ok(passed)
    }
    fn wait_old(&mut self, old: &ProcessIdentity) -> Result<(), RetirementFailure> {
        use crate::process_identity::{check_liveness, Liveness};
        // Retain every observed owner before invoking retirement. If the CLI
        // dies or times out after a kill, its exit code alone cannot report it.
        let mut observed = vec![old.clone()];
        for name in ["rbridge-live.lock", "rbridge-console.lock"] {
            let path = self
                .root
                .root()
                .join("runtime/data/logs")
                .join(format!("{name}.owner.json"));
            let marker = fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            if let Some(identity) = marker.and_then(|marker| {
                serde_json::from_value::<ProcessIdentity>(marker["identity"].clone()).ok()
            }) {
                if crate::update::same_release_image(&old.exe, &identity.exe)
                    && crate::process_identity::check_identity(&identity) == Liveness::Matches
                {
                    observed.push(identity);
                }
            }
        }
        let mut stopped = false;
        let result: Result<(), String> = (|| {
            if self.recovery {
                let end = std::time::Instant::now() + Duration::from_secs(30);
                loop {
                    match check_liveness(old.pid, old.start) {
                        Liveness::Gone | Liveness::Mismatch(_) => return Ok(()),
                        Liveness::Matches if std::time::Instant::now() < end => {
                            std::thread::sleep(Duration::from_millis(20))
                        }
                        _ => return Err("requesting supervisor exit could not be verified".into()),
                    }
                }
            }
            // The old release retires only its own verified HER owners, never dependencies.
            let image = Path::new(&old.exe);
            self.root.require_release_image(image)?;
            let mut retire = self.cli(&self.target)?;
            // The helper owns exclusive turn admission while this candidate CLI
            // rechecks the durable idle predicate immediately before stopping owners.
            if self.automatic {
                retire.args(["release", "retire", "--auto"]);
            } else {
                retire.args(["release", "retire"]);
            }
            retire
                .args([
                    "--old-pid",
                    &old.pid.to_string(),
                    "--old-start",
                    &old.start.to_string(),
                    "--old-exe",
                    &old.exe,
                ])
                .env(
                    "HER_INSTALL_ROOT",
                    crate::subprocess::external_path(self.root.root())?,
                );
            let status = run_bounded_status(&mut retire, Duration::from_secs(30))?;
            if !status.is_some_and(|status| status.success()) {
                stopped = status.is_some_and(|status| status.code() == Some(3));
                return Err("old HER owners could not be retired".into());
            }
            let end = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                match check_liveness(old.pid, old.start) {
                    Liveness::Gone | Liveness::Mismatch(_) => return Ok(()),
                    Liveness::Matches if std::time::Instant::now() < end => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    _ => return Err("old supervisor exit could not be verified".into()),
                }
            }
        })();
        result.map_err(|reason| RetirementFailure {
            reason,
            stopped: stopped
                || observed.iter().any(|owner| {
                    matches!(
                        check_liveness(owner.pid, owner.start),
                        Liveness::Gone | Liveness::Mismatch(_)
                    )
                }),
        })
    }
    fn switch(&mut self, name: &str, tag: &str) -> Result<(), String> {
        switch_link(&self.root, name, tag)
    }
    fn stop(&mut self, tag: &str, cli: &str) -> Result<(), String> {
        let mut command = self.cli(cli)?;
        command.args(["release", "retire-current", "--tag", tag]);
        if run_bounded(&mut command, Duration::from_secs(30))? {
            Ok(())
        } else {
            Err("failed release could not be retired".into())
        }
    }
    fn start(&mut self, tag: &str) -> Result<(), String> {
        #[cfg(feature = "parity-clock")]
        start_barrier(&self.root, tag, "before")?;
        use std::process::Stdio;
        let path = release_path(&self.root, tag)?;
        let child = crate::subprocess::command_without_console(path.join("her.exe"))
            .args(["supervise", "--runtime-root"])
            .arg(crate::subprocess::external_path(
                self.root.root().join("runtime"),
            )?)
            .env(
                "HER_INSTALL_ROOT",
                crate::subprocess::external_path(self.root.root())?,
            )
            .env_remove("HER_TRIAL_CURRENT")
            .current_dir(crate::subprocess::external_path(path)?)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "new supervisor could not start")?;
        self.child = Some(child);
        self.launches += 1;
        #[cfg(feature = "parity-clock")]
        start_barrier(&self.root, tag, "after")?;
        Ok(())
    }
    fn healthy(&mut self, tag: &str, budget: Duration) -> Result<bool, String> {
        let end = std::time::Instant::now() + judgment_budget(&self.root, budget)?;
        while std::time::Instant::now() < end {
            if crate::update::start_count(&self.root, tag, crate::update::wall_clock_ms())? >= 3 {
                return Err("three starts within ten minutes without health".into());
            }
            let remaining = end.saturating_duration_since(std::time::Instant::now());
            if self.health(
                tag,
                false,
                remaining.min(crate::update::HEALTH_PROBE_ALLOWANCE),
            )? {
                return Ok(std::time::Instant::now() <= end);
            }
            if std::time::Instant::now() >= end {
                break;
            }
            let exited = match self.child.as_mut() {
                Some(child) => child
                    .try_wait()
                    .map_err(|_| "supervisor status unavailable")?
                    .is_some(),
                None => true,
            };
            if exited {
                if self.launches >= 3 {
                    return Err("three starts within ten minutes without health".into());
                }
                self.start(tag)?;
            }
        }
        Ok(false)
    }
}

// Pause the platform start boundary while a process-level fixture schedules a
// concurrent writer or a startup fault. The real child is still spawned.
#[cfg(feature = "parity-clock")]
fn start_barrier(root: &InstallRoot, tag: &str, phase: &str) -> Result<(), String> {
    let key = match phase {
        "before" => "pauseBeforeStart",
        "trial" => "pauseAfterTrial",
        _ => "pauseAfterStart",
    };
    crate::update::fixture_barrier(root, tag, key, phase)
}

fn judgment_budget(_root: &InstallRoot, budget: Duration) -> Result<Duration, String> {
    #[cfg(feature = "parity-clock")]
    if let Some((_, control)) = crate::update::handover_control(_root)? {
        if let Some(ms) = control["healthBudgetMs"].as_u64() {
            return Ok(budget.min(Duration::from_millis(ms.max(1))));
        }
    }
    Ok(budget)
}

fn remove_link(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "release link unavailable")?;
    if !release_store::redirected(&metadata) {
        return Err("release link is not a link".into());
    }
    #[cfg(windows)]
    let result = fs::remove_dir(path);
    #[cfg(not(windows))]
    let result = fs::remove_file(path);
    result.map_err(|_| "release link could not be removed".into())
}
fn recover_link(root: &InstallRoot, name: &str) -> Result<(), String> {
    let link = root.root().join(name);
    let pending = root.root().join(format!(".{name}-next"));
    let previous = root.root().join(format!(".{name}-previous"));
    // Recover an interrupted two-rename switch before doing anything else.
    if fs::symlink_metadata(&previous).is_ok() {
        if fs::symlink_metadata(&link).is_err() {
            fs::rename(&previous, &link).map_err(|_| "interrupted link switch cannot recover")?;
        } else {
            remove_link(&previous)?;
        }
    }
    if fs::symlink_metadata(&pending).is_ok() {
        remove_link(&pending)?;
    }
    release_store::sync_directory(root.root())
}
fn switch_link(root: &InstallRoot, name: &str, tag: &str) -> Result<(), String> {
    let target = release_path(root, tag)?;
    recover_link(root, name)?;
    let link = root.root().join(name);
    let pending = root.root().join(format!(".{name}-next"));
    let previous = root.root().join(format!(".{name}-previous"));
    create_link(&target, &pending)?;
    let had_link = fs::symlink_metadata(&link).is_ok();
    if had_link {
        if !release_store::redirected(
            &fs::symlink_metadata(&link).map_err(|_| "release link unavailable")?,
        ) {
            return Err("release link is not a link".into());
        }
        fs::rename(&link, &previous).map_err(|_| "old release link cannot move")?;
        release_store::sync_directory(root.root())?;
    }
    if fs::rename(&pending, &link).is_err() {
        if had_link {
            fs::rename(&previous, &link).map_err(|_| "release link restoration failed")?;
        }
        let _ = remove_link(&pending);
        return Err("new release link cannot publish".into());
    }
    release_store::sync_directory(root.root())?;
    if had_link {
        remove_link(&previous)?;
    }
    release_store::sync_directory(root.root())
}
#[cfg(unix)]
fn create_link(target: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link).map_err(|_| "release symlink cannot be created".into())
}
#[cfg(windows)]
fn create_link(target: &Path, link: &Path) -> Result<(), String> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    // A mount-point reparse buffer creates a junction without shell parsing or
    // SeCreateSymbolicLinkPrivilege. Both names are absolute, guarded paths.
    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            handle: *mut std::ffi::c_void,
            code: u32,
            input: *const u8,
            size: u32,
            output: *mut u8,
            output_size: u32,
            returned: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }
    let ordinary = crate::subprocess::external_path(
        fs::canonicalize(target).map_err(|_| "junction target unavailable")?,
    )?;
    let substitute = if let Some(unc) = ordinary.strip_prefix(r"\\") {
        format!(r"\??\UNC\{unc}")
    } else {
        format!(r"\??\{ordinary}")
    };
    let sub: Vec<u16> = substitute.encode_utf16().collect();
    let print: Vec<u16> = ordinary.encode_utf16().collect();
    let length = 8 + (sub.len() + print.len() + 2) * 2;
    if length > 16 * 1024 - 8 {
        return Err("junction target is too long".into());
    }
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&0xA0000003u32.to_le_bytes());
    for n in [
        length,
        0,
        0,
        sub.len() * 2,
        (sub.len() + 1) * 2,
        print.len() * 2,
    ] {
        buffer.extend_from_slice(&(n as u16).to_le_bytes());
    }
    for c in sub.into_iter().chain([0]).chain(print).chain([0]) {
        buffer.extend_from_slice(&c.to_le_bytes());
    }
    fs::create_dir(link).map_err(|_| "junction directory cannot be created")?;
    let result = (|| {
        let file = fs::OpenOptions::new()
            .access_mode(0x40000000)
            .custom_flags(0x02200000)
            .open(link)
            .map_err(|_| "junction cannot open")?;
        let mut returned = 0;
        // SAFETY: the file pins our new empty directory and buffer contains a
        // complete mount-point record. No output or asynchronous I/O is used.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                0x000900A4,
                buffer.as_ptr(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err("junction cannot be set")
        } else {
            Ok(())
        }
    })();
    if result.is_err() {
        let _ = fs::remove_dir(link);
    }
    result.map_err(str::to_string)
}

fn rollback(
    root: &InstallRoot,
    failed: &str,
    last_good: &str,
    reason: &str,
    platform: &mut dyn Platform,
) -> Result<(), String> {
    if crate::update::verify_rollback(root, last_good).is_err() {
        let kept = platform
            .current("current")?
            .ok_or("current release unavailable")?;
        crate::update::signature_refusal(root, failed, &kept)?;
        return Err(crate::release_signature::REFUSED.into());
    }
    let alert = crate::update::UpdateAlert::new(failed, last_good, reason);
    let recorded = crate::update::save_alert(root, &alert);
    let restored = (|| -> Result<(), String> {
        if platform.current("current")?.as_deref() == Some(failed) {
            let cli = crate::update::retirement_release(root, failed, last_good);
            platform.stop(failed, &cli)?;
        }
        platform.switch("current", last_good)?;
        platform.start(last_good)
    })();
    let finished = crate::update::finish_rollback(
        root,
        &alert.id,
        if restored.is_ok() {
            "restored"
        } else {
            "failed"
        },
    );
    restored?;
    recorded?;
    finished?;
    prune(root, platform)?;
    Err(format!("{reason}; restored last-good {last_good}"))
}

fn run(
    root: &InstallRoot,
    target: &str,
    old: &ProcessIdentity,
    platform: &mut dyn Platform,
    recovery: bool,
) -> Result<(), String> {
    if !recovery {
        return handover(root, target, old, platform);
    }
    let _gate = release_store::installation_gate(
        &root.root().join("runtime/data/locks/update-restore.lock"),
    )?;
    if platform.current("current")?.as_deref() != Some(target) {
        return Err("current changed before crash recovery".into());
    }
    let last_good =
        crate::update::recovery_release(root, target, platform.current("last-good")?.as_deref())?;
    platform.wait_old(old).map_err(|failure| failure.reason)?;
    rollback(
        root,
        target,
        &last_good,
        "three starts within ten minutes without health",
        platform,
    )
}

/// Called only while holding the installation/restore gate. Preserve both link
/// targets in addition to the newest three ordinary versioned directories.
fn prune(root: &InstallRoot, platform: &mut dyn Platform) -> Result<(), String> {
    let current = platform
        .current("current")?
        .ok_or("pruning requires current")?;
    let last_good = platform
        .current("last-good")?
        .ok_or("pruning requires last-good")?;
    for tag in [&current, &last_good] {
        release_path(root, tag)?;
    }
    let releases = root.root().join("releases");
    release_store::directory(&releases)?;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&releases).map_err(|_| "release list unavailable")? {
        let entry = entry.map_err(|_| "release entry unavailable")?;
        let name = entry.file_name();
        let Some(tag) = name.to_str() else {
            continue;
        };
        let Some(number) = release_store::version(tag) else {
            continue;
        };
        let meta =
            fs::symlink_metadata(entry.path()).map_err(|_| "release metadata unavailable")?;
        if meta.is_dir() && !release_store::redirected(&meta) {
            candidates.push((number, tag.to_string()));
        }
    }
    candidates.sort_by(|(a, _), (b, _)| b.cmp(a));
    for (_, tag) in candidates.into_iter().skip(3) {
        if tag == current || tag == last_good {
            continue;
        }
        let path = release_path(root, &tag)?;
        fs::remove_dir_all(path).map_err(|_| "old release could not be pruned")?;
    }
    Ok(())
}
