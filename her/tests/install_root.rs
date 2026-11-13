//! Spec seam 1: install-root isolation through the real paired binaries.
#[path = "../src/install_root.rs"]
mod install_root;
#[path = "fixtures/media.rs"]
mod media_fixture;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = crate::install_root::development_root(std::env::temp_dir().join(
            format!("her-install-guard-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()),
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn command(image: &Path) -> Command {
    let mut cmd = Command::new(image);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.env_remove("HER_RUNTIME_ROOT")
        .env_remove("HER_INSTALL_ROOT");
    cmd
}
/// Interpreter for the Python helpers: `python` when it runs, else `python3` (same rule as agent-guard.sh).
fn python_program() -> &'static str {
    static PYTHON: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    PYTHON.get_or_init(|| {
        let runs = command(Path::new("python"))
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if runs {
            "python"
        } else {
            "python3"
        }
    })
}
fn refused(output: Output, reason: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(reason),
        "{output:?}"
    );
}

#[test]
fn supervisor_outside_releases_refuses_install_runtime_before_any_writes() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let runtime = install.join("runtime");
    for image in [env!("CARGO_BIN_EXE_her"), env!("CARGO_BIN_EXE_her-cli")] {
        let output = command(Path::new(image))
            .env("HER_INSTALL_ROOT", &install)
            .args(["supervise", "--runtime-root"])
            .arg(&runtime)
            .output()
            .unwrap();
        refused(
            output,
            "supervisor executable must live under the install root's releases",
        );
        assert!(
            !install.exists(),
            "refusal must precede runtime reads/writes"
        );
    }
}

#[test]
fn development_commands_refuse_install_runtime_before_dispatch() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let runtime = install.join("runtime/not-created");
    for args in [
        vec!["lifecycle", "set", "stopped", "--confirm"],
        vec!["tasks", "list"],
        vec!["--console-serve", "--console-port", "43151"],
        vec!["--mcp-serve", "--console-port", "43151"],
        vec![
            "--live",
            "--live-groups",
            "group:1",
            "--console-port",
            "43151",
        ],
        vec![
            "dsh-mcp-config",
            "--patch-file",
            "unused.yml",
            "--console-port",
            "43151",
        ],
        vec!["release", "check"],
        vec!["release", "install"],
        vec!["install-root", "check-development"],
    ] {
        let output = command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
            .env("HER_INSTALL_ROOT", &install)
            .args(args)
            .arg("--runtime-root")
            .arg(&runtime)
            .output()
            .unwrap();
        refused(
            output,
            "development runtime root is under the production install root",
        );
        assert!(!install.exists());
    }
}

#[test]
fn development_installer_refuses_protected_destination_with_external_runtime() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let runtime = fixture.0.join("external-runtime");
    for dir in ["config", "data", "resources"] {
        std::fs::create_dir_all(runtime.join(dir)).unwrap();
    }
    std::fs::write(
        runtime.join("config/config.json"),
        r#"{"consoleToken":"fixture-token","update":{"policy":"off"}}"#,
    )
    .unwrap();
    std::fs::write(runtime.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
    std::fs::write(runtime.join("data/social-v2.json"), "{}").unwrap();
    let config = std::fs::read(runtime.join("config/config.json")).unwrap();
    for selected in [&runtime, &fixture.0.join("missing-runtime")] {
        refused(
            command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
                .env("HER_INSTALL_ROOT", &install)
                .args(["release", "install", "--runtime-root"])
                .arg(selected)
                .output()
                .unwrap(),
            "release installation requires an executable under the install root's releases",
        );
        assert!(
            !install.exists(),
            "destination refusal must precede creation"
        );
        assert!(!runtime.join("data/locks").exists());
        assert_eq!(
            std::fs::read(runtime.join("config/config.json")).unwrap(),
            config
        );
    }
}

#[test]
fn lifecycle_option_value_double_dash_cannot_bypass_runtime_guard() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let output = command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
        .env("HER_INSTALL_ROOT", &install)
        .args([
            "lifecycle",
            "set",
            "running",
            "--by",
            "--",
            "--runtime-root",
        ])
        .arg(install.join("runtime"))
        .output()
        .unwrap();
    refused(
        output,
        "development runtime root is under the production install root",
    );
    assert!(!install.exists());
}

#[test]
fn development_destinations_are_guarded_without_a_runtime_even_from_installed_cli() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let release = install.join("releases/test");
    std::fs::create_dir_all(&release).unwrap();
    let cli = release.join(if cfg!(windows) {
        "her-cli.exe"
    } else {
        "her-cli"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_her-cli"), &cli).unwrap();
    for image in [Path::new(env!("CARGO_BIN_EXE_her-cli")), cli.as_path()] {
        for mode in [
            "--memory-write-probe",
            "--memory-probe",
            "--hot-render",
            "--index-hint",
        ] {
            let db = install.join("probe.db");
            let output = command(image)
                .env("HER_INSTALL_ROOT", &install)
                .env("HER_MEMORY_DB", &db)
                .arg(mode)
                .output()
                .unwrap();
            refused(
                output,
                "development runtime root is under the production install root",
            );
            assert!(!db.exists());
        }
        let patch = install.join("patch/new.yml");
        let output = command(image)
            .env("HER_INSTALL_ROOT", &install)
            .args(["dsh-mcp-config", "--patch-file"])
            .arg(&patch)
            .output()
            .unwrap();
        refused(
            output,
            "development runtime root is under the production install root",
        );
        assert!(!install.join("patch").exists());
    }
}

#[test]
fn media_evidence_child() {
    if std::env::var_os("HER_04_MEDIA_CHILD").is_some() {
        media_fixture::evidence("must-not-write.bin", b"synthetic");
    }
}

#[test]
fn media_evidence_destination_refuses_before_first_write() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let output = command(&std::env::current_exe().unwrap())
        .args(["--exact", "media_evidence_child", "--nocapture"])
        .env("HER_04_MEDIA_CHILD", "1")
        .env("HER_INSTALL_ROOT", &install)
        .env("TICKET19_MEDIA_EVIDENCE", install.join("evidence"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("development runtime root is under the production install root"),
        "{output:?}"
    );
    assert!(!install.exists());
}

#[test]
fn release_helpers_guard_evidence_and_redirected_temp_before_first_write() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    std::fs::create_dir(&install).unwrap();
    let alias = fixture.0.join("temp-alias");
    link_directory(&install, &alias);
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts");
    let mut failures = Vec::new();
    for name in ["test-release-proof.py", "test-release-chains.py"] {
        for protected_evidence in [true, false] {
            let evidence = if protected_evidence {
                install.join(name)
            } else {
                fixture.0.join(name)
            };
            let mut cmd = command(Path::new(python_program()));
            cmd.arg(scripts.join(name));
            cmd.arg(env!("CARGO_BIN_EXE_her-cli")).arg(&fixture.0);
            let selected_temp = if protected_evidence {
                &fixture.0
            } else {
                &alias
            };
            let output = cmd
                .arg(&evidence)
                .env("HER_TEST_CLI", env!("CARGO_BIN_EXE_her-cli"))
                .env("HER_INSTALL_ROOT", &install)
                .env("TEMP", selected_temp)
                .env("TMP", selected_temp)
                .env("TMPDIR", selected_temp)
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .output()
                .unwrap();
            if output.status.success()
                || evidence.exists()
                || std::fs::read_dir(&install).unwrap().count() != 0
                || !String::from_utf8_lossy(&output.stderr)
                    .contains("development runtime root is under the production install root")
            {
                failures.push(format!("{name} protected_evidence={protected_evidence}: {output:?}, evidence_exists={}", evidence.exists()));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn link_directory(target: &Path, alias: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, alias).unwrap();
    #[cfg(windows)]
    {
        let output = command(Path::new("cmd.exe"))
            .args(["/d", "/c", "mklink", "/J"])
            .arg(alias)
            .arg(target)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn redirected_ancestor_into_install_root_refuses_even_with_parent_suffix() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    std::fs::create_dir(&install).unwrap();
    let alias = fixture.0.join("alias");
    link_directory(&install, &alias);
    let output = command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
        .env("HER_INSTALL_ROOT", &install)
        .args(["install-root", "check-development", "--runtime-root"])
        .arg(alias.join("../development/runtime"))
        .output()
        .unwrap();
    refused(
        output,
        "development runtime root is under the production install root",
    );
    assert_eq!(std::fs::read_dir(&install).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn preflight_validates_resident_image_even_when_cli_is_installed() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let entry = install.join("releases/test");
    std::fs::create_dir_all(&entry).unwrap();
    let cli = entry.join("her-cli.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_her-cli"), &cli).unwrap();
    let runtime = install.join("runtime");
    for name in ["config", "data", "resources"] {
        std::fs::create_dir_all(runtime.join(name)).unwrap();
    }
    let output = command(&cli)
        .env("HER_INSTALL_ROOT", &install)
        .args(["install-root", "check-supervisor", "--runtime-root"])
        .arg(&runtime)
        .arg("--executable")
        .arg(env!("CARGO_BIN_EXE_her"))
        .output()
        .unwrap();
    refused(
        output,
        "supervisor executable must live under the install root's releases",
    );
    assert!(!runtime.join("data/logs").exists());
}

#[test]
fn configurable_install_layout_is_reported_without_creating_it() {
    let fixture = Fixture::new();
    let install = fixture.0.join("install 中文");
    let output = command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
        .args(["install-root", "layout", "--install-root"])
        .arg(&install)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let layout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for (field, relative) in [
        ("installRoot", ""),
        ("updater", "herupdate.exe"),
        ("current", "current"),
        ("releases", "releases"),
        ("dependencies", "deps"),
        ("snowLuma", "deps/snowluma"),
        ("dsh", "deps/dsh"),
        ("relay", "deps/relay"),
        ("runtime", "runtime"),
        ("dshHome", "runtime/dsh/home"),
        ("dshWork", "runtime/dsh/work"),
        ("backups", "backups"),
    ] {
        assert_eq!(
            Path::new(layout[field].as_str().unwrap()),
            install.join(relative)
        );
    }
    let via_env = command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
        .env("HER_INSTALL_ROOT", &install)
        .args(["install-root", "layout"])
        .output()
        .unwrap();
    assert!(via_env.status.success(), "{via_env:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&via_env.stdout).unwrap(),
        layout
    );
    assert!(!install.exists());
}

#[test]
fn resident_preflight_accepts_installed_image_without_accessing_runtime() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let release = install.join("releases/test");
    std::fs::create_dir_all(&release).unwrap();
    let resident = release.join(if cfg!(windows) { "her.exe" } else { "her" });
    std::fs::copy(env!("CARGO_BIN_EXE_her"), &resident).unwrap();
    let runtime = install.join("runtime-not-created");
    let output = command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
        .env("HER_INSTALL_ROOT", &install)
        .args(["install-root", "check-supervisor", "--runtime-root"])
        .arg(&runtime)
        .arg("--executable")
        .arg(&resident)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!runtime.exists());
}

#[test]
fn script_helpers_refuse_before_allocating_runtime_roots() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    std::fs::create_dir(&install).unwrap();
    std::fs::write(install.join("sentinel"), "untouched").unwrap();
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts");
    let python = r#"import os, runpy, subprocess, sys
run = subprocess.run
def hidden(*args, **kwargs):
    if os.name == 'nt': kwargs.setdefault('creationflags', subprocess.CREATE_NO_WINDOW)
    return run(*args, **kwargs)
subprocess.run = hidden
sys.path.insert(0, os.path.dirname(sys.argv[1]))
fixture = runpy.run_path(sys.argv[1])
with fixture['TemporaryDirectory'](prefix='her-guard-fixture-'):
    pass
"#;
    let mut runners = vec![(
        python_program(),
        vec![
            "-c".to_string(),
            python.to_string(),
            scripts
                .join("fixture_paths.py")
                .to_string_lossy()
                .into_owned(),
        ],
    )];
    #[cfg(windows)]
    runners.push((
        "powershell.exe",
        vec![
            "-NoProfile".into(),
            "-File".into(),
            scripts
                .join("test-external-path.ps1")
                .to_string_lossy()
                .into_owned(),
        ],
    ));
    for (program, args) in runners {
        let output = command(Path::new(program))
            .args(args)
            .env("HER_TEST_CLI", env!("CARGO_BIN_EXE_her-cli"))
            .env("HER_INSTALL_ROOT", &install)
            .env("TEMP", &install)
            .env("TMP", &install)
            .env("TMPDIR", &install)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "helper allocated a production runtime: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("development runtime root is under the production install root"),
            "{output:?}"
        );
        assert_eq!(
            std::fs::read_to_string(install.join("sentinel")).unwrap(),
            "untouched"
        );
        assert_eq!(std::fs::read_dir(&install).unwrap().count(), 1);
    }
}

#[cfg(windows)]
#[test]
fn supervisor_checks_the_guard_before_creating_runtime_logs() {
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let runtime = install.join("runtime");
    for dir in ["config", "data", "resources"] {
        std::fs::create_dir_all(runtime.join(dir)).unwrap();
    }
    let output = command(Path::new(env!("CARGO_BIN_EXE_her")))
        .args(["supervise", "--runtime-root"])
        .arg(&runtime)
        .env("HER_INSTALL_ROOT", &install)
        .output()
        .unwrap();
    refused(
        output,
        "supervisor executable must live under the install root's releases",
    );
    assert!(
        !runtime.join("data/logs").exists(),
        "supervisor wrote production logs before the guard"
    );
}

#[test]
fn development_guard_normalizes_paths_and_keeps_siblings_separate() {
    let fixture = Fixture::new();
    let install = fixture.0.join("Production");
    let check = |runtime: &Path| {
        command(Path::new(env!("CARGO_BIN_EXE_her-cli")))
            .env("HER_INSTALL_ROOT", &install)
            .args(["install-root", "check-development", "--runtime-root"])
            .arg(runtime)
            .output()
            .unwrap()
    };
    for path in [
        install.clone(),
        install.join("runtime/new"),
        install.join("unused/../runtime"),
    ] {
        refused(
            check(&path),
            "development runtime root is under the production install root",
        );
    }
    #[cfg(windows)]
    for path in [
        fixture.0.join("pRoDuCtIoN/runtime"),
        PathBuf::from(format!(r"\\?\{}\runtime", install.display())),
    ] {
        refused(
            check(&path),
            "development runtime root is under the production install root",
        );
    }
    for path in [
        fixture.0.join("Production-dev/runtime"),
        install.join("../development/runtime"),
    ] {
        let output = check(&path);
        assert!(output.status.success(), "{output:?}");
    }
    assert!(!install.exists());
    std::fs::create_dir(&install).unwrap();
    let alias = fixture.0.join("alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&install, &alias).unwrap();
    #[cfg(windows)]
    {
        let output = command(Path::new("cmd.exe"))
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&alias)
            .arg(&install)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    refused(
        check(&alias.join("runtime/new")),
        "development runtime root is under the production install root",
    );
    assert!(!install.join("runtime").exists());
}

#[test]
fn installed_and_legacy_supervisors_can_own_their_selected_runtime() {
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};
    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fixture = Fixture::new();
    let install = fixture.0.join("production");
    let release = install.join("releases/v-fixture");
    std::fs::create_dir_all(&release).unwrap();
    let installed_her = release.join(if cfg!(windows) { "her.exe" } else { "her" });
    let installed_cli = release.join(if cfg!(windows) {
        "her-cli.exe"
    } else {
        "her-cli"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_her"), &installed_her).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_her-cli"), &installed_cli).unwrap();
    for (name, image, cli, runtime) in [
        (
            "installed",
            installed_her,
            installed_cli,
            install.join("runtime"),
        ),
        (
            "legacy",
            PathBuf::from(env!("CARGO_BIN_EXE_her")),
            PathBuf::from(env!("CARGO_BIN_EXE_her-cli")),
            fixture.0.join("legacy-runtime"),
        ),
    ] {
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(runtime.join(dir)).unwrap();
        }
        std::fs::write(runtime.join("config/config.json"), r#"{"consoleToken":"fixture-token","update":{"policy":"off"},"backends":{"mcpRustBridge":true},"rollout":{"rustGroups":[]},"consolePort":31999}"#).unwrap();
        std::fs::write(runtime.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(runtime.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}"#).unwrap();
        let control = |args: &[&str]| {
            command(&cli)
                .env("HER_INSTALL_ROOT", &install)
                .args(args)
                .arg("--runtime-root")
                .arg(&runtime)
                .output()
                .unwrap()
        };
        let stopped = control(&["lifecycle", "set", "stopped", "--confirm"]);
        assert!(stopped.status.success(), "{stopped:?}");
        let log = fixture.0.join(format!("{name}.stderr"));
        let mut child = Reap(
            command(&image)
                .args(["supervise", "--install-root"])
                .arg(&install)
                .arg("--runtime-root")
                .arg(&runtime)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{name}: {}",
                std::fs::read_to_string(&log).unwrap()
            );
            let output = control(&["lifecycle", "status"]);
            assert!(output.status.success(), "{output:?}");
            let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if status["owners"]["supervisor"]["state"] == "held"
                && status["owners"]["supervisor"]["owner"]["identity"]["pid"] == child.0.id()
            {
                assert_eq!(status["owners"]["live"]["state"], "free");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{name} never took ownership: {status}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        if name == "installed" {
            refused(
                command(&cli)
                    .env("HER_INSTALL_ROOT", &install)
                    .args(["install-root", "check-development", "--runtime-root"])
                    .arg(&runtime)
                    .output()
                    .unwrap(),
                "development runtime root is under the production install root",
            );
        }
        let stopped = control(&["lifecycle", "stop-supervisor", "--confirm"]);
        assert!(stopped.status.success(), "{stopped:?}");
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "{name} supervisor did not stop");
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[test]
fn script_helpers_allow_disposable_roots_and_existing_checks() {
    let fixture = Fixture::new();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut runners = vec![(
        "node",
        vec![
            "--test".into(),
            repo.join("her/tests/fixtures/fake-world-faults-selfcheck.mjs")
                .to_string_lossy()
                .into_owned(),
        ],
    )];
    #[cfg(windows)]
    for name in ["test-ps-count.ps1", "test-external-path.ps1"] {
        runners.push((
            "powershell.exe",
            vec![
                "-NoProfile".into(),
                "-File".into(),
                repo.join("scripts")
                    .join(name)
                    .to_string_lossy()
                    .into_owned(),
            ],
        ));
    }
    for (program, args) in runners {
        let output = command(Path::new(program))
            .args(&args)
            .env("HER_TEST_CLI", env!("CARGO_BIN_EXE_her-cli"))
            .env("HER_INSTALL_ROOT", fixture.0.join("production-boundary"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        println!(
            "{program} {args:?}: exit {:?}\n{}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{program} {args:?}: {output:?}");
    }
}

#[cfg(windows)]
#[test]
fn affected_native_script_checks_use_disposable_roots() {
    let fixture = Fixture::new();
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts");
    for name in [
        "test-release-timeout.py",
        "test-lifecycle-window-observer.py",
    ] {
        let mut cmd = command(Path::new(python_program()));
        cmd.arg(scripts.join(name));
        if name != "test-lifecycle-window-observer.py" {
            cmd.arg(fixture.0.join(name));
        }
        let output = cmd
            .env("HER_TEST_CLI", env!("CARGO_BIN_EXE_her-cli"))
            .env("HER_INSTALL_ROOT", fixture.0.join("production-boundary"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        println!(
            "{name}: exit {:?}\n{}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{name}: {output:?}");
    }
}
