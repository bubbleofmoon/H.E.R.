//! Retired commands must fail before reading any runtime or source state.
#[path = "../src/install_root.rs"]
mod install_root;

use std::process::Command;

#[test]
fn removed_import_command_is_a_usage_error_before_source_access() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command
        .args([
            "config-import",
            "preview",
            "--from",
            "missing-retired-source",
        ])
        .env_remove("HER_RUNTIME_ROOT");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let result = command.output().unwrap();
    assert_eq!(
        result.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("unknown command"));
}

#[test]
fn removed_flags_are_rejected_before_runtime_selection() {
    for flag in [
        "--live-all",
        "--ab-full",
        "--ab-probe",
        "--shadow-turn",
        "--shadow-state-root",
        "--replay",
        "--wake-replay",
        "--replay-jsonl",
        "--replay-triggers",
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .args([
                "--live",
                "--live-groups",
                "group:1",
                "--console-port",
                "1",
                flag,
            ])
            .env(
                "HER_RUNTIME_ROOT",
                crate::install_root::development_root(
                    std::env::temp_dir().join("her-retired-unused-root"),
                ),
            );
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let result = command.output().unwrap();
        assert_eq!(result.status.code(), Some(2), "{flag}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("unknown option"),
            "{flag}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn retired_parity_probes_are_rejected_before_runtime_selection() {
    let install = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-retired-probes-{}", std::process::id())),
    );
    let _ = std::fs::remove_dir_all(&install);
    for flag in [
        "--send-probe",
        "--ask",
        "--price-probe",
        "--role-probe",
        "--prompt-wrapped",
        "--rpc-probe",
        "--slang-parse-test",
        "--selftest-401",
        "--failover-probe",
        "--sticker-sync",
        "--build-prompt",
        "--dsh-dump",
        "--dsh-watch",
        "--dsh-probe",
        "--dsh-demo",
        "--question-probe",
        "--split-probe",
        "--sensitive-probe",
        "--backend-ask",
        "--backends",
        "--foo-probe",
        "--unknown-future-flag",
        "-x",
    ] {
        for bin in [env!("CARGO_BIN_EXE_her"), env!("CARGO_BIN_EXE_her-cli")] {
            for with_runtime in [true, false] {
                let mut command = Command::new(bin);
                command.arg(flag).env("HER_INSTALL_ROOT", &install);
                if with_runtime {
                    command.env("HER_RUNTIME_ROOT", install.join("unused-runtime"));
                } else {
                    command.env_remove("HER_RUNTIME_ROOT");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x0800_0000);
                }
                let result = command.output().unwrap();
                assert_eq!(
                    result.status.code(),
                    Some(2),
                    "{bin} {flag} with_runtime={with_runtime}"
                );
                assert!(
                    String::from_utf8_lossy(&result.stderr).contains("unknown option"),
                    "{bin} {flag} with_runtime={with_runtime}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }
    }
    for args in [
        vec![
            "--live",
            "--live-groups",
            "group:1",
            "--console-port",
            "1",
            "--foo-probe",
        ],
        vec![
            "--console-serve",
            "--console-port",
            "1",
            "--unknown-future-flag",
        ],
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_her-cli"));
        command
            .args(&args)
            .env("HER_INSTALL_ROOT", &install)
            .env("HER_RUNTIME_ROOT", install.join("unused-runtime"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let result = command.output().unwrap();
        assert_eq!(result.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("unknown option"),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert!(
        !install.exists(),
        "retired probes must be rejected before creating any root"
    );
}

#[test]
fn omitted_runtime_root_is_rejected_before_writing() {
    let scratch = crate::install_root::development_root(
        std::env::temp_dir().join(format!("her-missing-root-{}", std::process::id())),
    );
    let _ = std::fs::remove_dir_all(&scratch);
    for args in [
        vec![],
        vec!["--console-serve", "--console-port", "43160"],
        vec!["--mcp-serve", "--console-port", "43160"],
        vec![
            "--live",
            "--live-groups",
            "group:1",
            "--console-port",
            "43160",
        ],
    ] {
        for bin in [env!("CARGO_BIN_EXE_her"), env!("CARGO_BIN_EXE_her-cli")] {
            let mut command = Command::new(bin);
            command
                .args(&args)
                .env("HER_INSTALL_ROOT", &scratch)
                .env_remove("HER_RUNTIME_ROOT");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            let result = command.output().unwrap();
            assert_eq!(result.status.code(), Some(2), "{bin} {args:?}");
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(
                stderr.contains("--runtime-root"),
                "{bin} {args:?}: {stderr}"
            );
        }
    }
    assert!(
        !scratch.exists(),
        "missing runtime root must be rejected before any writes"
    );
}
