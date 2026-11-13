//! Executable seam: actual artifacts, CLI output and startup dispatch, without a runtime.
#[path = "../src/install_root.rs"]
mod install_root;

use std::process::{Command, Output};

fn binary(name: &str) -> &'static str {
    match name {
        "her" => option_env!("CARGO_BIN_EXE_her").expect("HER GUI binary target must exist"),
        "her-cli" => {
            option_env!("CARGO_BIN_EXE_her-cli").expect("HER console binary target must exist")
        }
        "herupdate" => option_env!("CARGO_BIN_EXE_herupdate").expect("HER updater binary target must exist"),
        _ => unreachable!(),
    }
}

fn run(name: &str, args: &[&str]) -> Output {
    let mut command = Command::new(binary(name));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW; PE assertions check the binary itself.
    }
    command.args(args).output().expect("run HER binary")
}

#[cfg(windows)]
fn subsystem(path: &str) -> u16 {
    let bytes = std::fs::read(path).expect("read built PE image");
    assert_eq!(&bytes[..2], b"MZ");
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
    let optional = pe + 24;
    let magic = u16::from_le_bytes(bytes[optional..optional + 2].try_into().unwrap());
    assert!(matches!(magic, 0x10b | 0x20b), "PE32 or PE32+");
    u16::from_le_bytes(bytes[optional + 68..optional + 70].try_into().unwrap())
}

#[test]
fn her_images_have_the_gui_and_console_subsystems() {
    #[cfg(windows)]
    {
        assert_eq!(
            subsystem(binary("her")),
            2,
            "her.exe must not allocate a console"
        );
        assert_eq!(
            subsystem(binary("her-cli")),
            3,
            "her-cli.exe must use the console"
        );
        assert_eq!(subsystem(binary("herupdate")), 2, "herupdate.exe must not allocate a console");
    }
    #[cfg(not(windows))]
    {
        assert!(std::path::Path::new(binary("her")).is_file());
        assert!(std::path::Path::new(binary("her-cli")).is_file());
        assert!(std::path::Path::new(binary("herupdate")).is_file());
    }
}

#[test]
fn no_arguments_and_app_validate_runtime_selection_and_cli_help_is_visible() {
    let absent_install = crate::install_root::development_root(std::env::temp_dir().join(format!(
        "her-desktop-uninstalled-{}", std::process::id()
    )));
    let app_run = |name: &str, args: &[&str]| {
        let mut command = Command::new(binary(name));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        command.args(args).env("HER_INSTALL_ROOT", &absent_install)
            .env_remove("HER_RUNTIME_ROOT").output().unwrap()
    };
    for name in ["her", "her-cli"] {
        let default = app_run(name, &[]);
        let app = app_run(name, &["app"]);
        assert_eq!(default.status.code(), Some(2), "an uninstalled image must select a runtime explicitly: {default:?}");
        assert_eq!(app.status.code(), Some(2), "{app:?}");
        assert_eq!(default.stdout, app.stdout);
        assert_eq!(default.stderr, app.stderr);
        assert!(String::from_utf8_lossy(&app.stderr).contains("--runtime-root"));
        assert_eq!(app_run(name, &["app", "--unknown-desktop-option"]).status.code(), Some(2));
        let supervise = run(name, &["supervise"]);
        assert_eq!(supervise.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&supervise.stderr).contains("--runtime-root"));
    }
    let help = app_run("her-cli", &["app", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(String::from_utf8_lossy(&help.stdout).contains("usage:"));
}

#[cfg(windows)]
#[test]
fn desktop_logon_uses_a_per_user_hidden_gui_command_and_preserves_other_entries() {
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if self.0.starts_with(std::env::temp_dir()) { let _ = std::fs::remove_dir_all(&self.0); }
        }
    }
    let root = crate::install_root::development_root(std::env::temp_dir().join(format!(
        "HER desktop logon 17 路径 {}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    let _cleanup = Cleanup(root.clone());
    let install_root = root.join("install");
    let root = install_root.join("runtime");
    let image = install_root.join("releases/fixture/her-cli.exe");
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::hard_link(binary("her-cli"), &image).unwrap();
    for dir in ["config", "data", "resources"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
    std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"desktop-logon-fixture"}"#).unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:2","onebot":"http://127.0.0.1:3"}"#).unwrap();
    let store = root.join("data/desktop-run.json");
    std::fs::write(&store, r#"{"OtherApp":"keep this startup entry"}"#).unwrap();
    let mut previous = None;
    for _ in 0..2 {
        let output = Command::new(&image).creation_flags(0x08000000)
            .args(["app", "--register-logon", "--run-store", "desktop-run.json"])
            .env_remove("HER_INSTALL_ROOT").env_remove("HER_RUNTIME_ROOT").output().unwrap();
        assert!(output.status.success(), "fake desktop logon configuration failed: {output:?}");
        let values: serde_json::Value = serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
        assert_eq!(values["OtherApp"], "keep this startup entry");
        let command = values["HERDesktop"].as_str().unwrap().to_string();
        if let Some(previous) = &previous { assert_eq!(&command, previous, "registration is idempotent"); }
        previous = Some(command.clone());
        #[link(name = "shell32")]
        extern "system" { fn CommandLineToArgvW(line: *const u16, count: *mut i32) -> *mut *mut u16; }
        #[link(name = "kernel32")]
        extern "system" { fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void; }
        let wide: Vec<u16> = command.encode_utf16().chain([0]).collect();
        let mut count = 0;
        // SAFETY: NUL-terminated command; Shell allocates the returned array.
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
        assert!(!argv.is_null());
        let mut arguments = Vec::new();
        for index in 0..count as usize {
            let arg = unsafe { *argv.add(index) };
            let mut len = 0;
            while unsafe { *arg.add(len) } != 0 { len += 1; }
            arguments.push(String::from_utf16(unsafe { std::slice::from_raw_parts(arg, len) }).unwrap());
        }
        unsafe { LocalFree(argv.cast()); }
        assert_eq!(arguments, vec![
            install_root.join("current/her.exe").to_string_lossy().into_owned(),
            "app".into(), "--hidden".into(),
        ]);
    }
    // A malformed fake store stops before native UI startup, so this also runs
    // without a desktop session and never falls back to the real Run registry.
    std::fs::write(&store, "[]").unwrap();
    let output = Command::new(&image).creation_flags(0x08000000)
        .args(["app", "--run-store", "desktop-run.json"])
        .env_remove("HER_INSTALL_ROOT").env_remove("HER_RUNTIME_ROOT").output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("desktop Run fixture must be an object"),
        "a Run-store override without registration-only mode must reach the fake writer: {output:?}");
    assert_eq!(std::fs::read_to_string(&store).unwrap(), "[]", "refusal must preserve the fake store");
}

#[cfg(windows)]
#[test]
fn desktop_logon_rejects_commands_over_260_utf16_units_before_writing() {
    use std::os::windows::process::CommandExt;
    let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her17-run-boundary-{}", std::process::id())));
    for dir in ["config", "data", "resources"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
    std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"fixture-desktop-token"}"#).unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    std::fs::write(root.join("dependencies.json"), r#"{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:2","onebot":"http://127.0.0.1:3"}"#).unwrap();
    let store = root.join("data/desktop-run.json");
    let original = r#"{"OtherApp":"keep","HERDesktop":"previous"}"#;
    for limit in [261, 260] {
        let prefix = root.join("install-");
        let base = format!("\"{}\" \"app\" \"--hidden\"", prefix.join("current/her.exe").display());
        let install = std::path::PathBuf::from(format!("{}{}", prefix.display(), "x".repeat(limit - base.encode_utf16().count())));
        let expected = format!("\"{}\" \"app\" \"--hidden\"", install.join("current/her.exe").display());
        assert_eq!(expected.encode_utf16().count(), limit);
        std::fs::write(&store, original).unwrap();
        let output = Command::new(binary("her-cli")).creation_flags(0x08000000)
            .args(["app", "--runtime-root"]).arg(&root)
            .args(["--register-logon", "--run-store", "desktop-run.json"])
            .env("HER_INSTALL_ROOT", &install).env_remove("HER_RUNTIME_ROOT").output().unwrap();
        if limit == 261 {
            assert_eq!(output.status.code(), Some(2), "oversized Run command must refuse: {output:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("260"), "{output:?}");
            assert_eq!(std::fs::read_to_string(&store).unwrap(), original, "refusal must not change any entry");
        } else {
            assert!(output.status.success(), "260-unit boundary is supported: {output:?}");
            let values: serde_json::Value = serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
            assert_eq!(values["HERDesktop"], expected);
            assert_eq!(values["OtherApp"], "keep");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[test]
fn stop_bot_captures_output_and_exit_status_from_both_real_images() {
    use std::os::windows::process::CommandExt;
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stop-bot.ps1");
    for name in ["her", "her-cli"] {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-stop-script-{name}-{}", std::process::id())));
        std::fs::create_dir_all(root.join("data")).unwrap();
        for (selected, expected) in [(&root, 0), (&root.join("missing"), 2)] {
            let output = Command::new("powershell.exe").creation_flags(0x08000000)
                .args(["-NoProfile", "-NonInteractive", "-File"]).arg(&script)
                .arg("-RuntimeRoot").arg(selected).arg("-Executable").arg(binary(name))
                .arg("-Confirm").output().unwrap();
            assert_eq!(output.status.code(), Some(expected), "{name}: {output:?}");
            if expected == 0 {
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["record"]["state"], "stopped");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
