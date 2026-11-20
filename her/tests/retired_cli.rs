//! Retired commands must fail before reading any runtime or source state.
#[path = "../src/install_root.rs"]
mod install_root;

use std::process::Command;

#[test]
fn removed_import_command_is_a_usage_error_before_source_access() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
    command.args(["config-import", "preview", "--from", "missing-retired-source"])
        .env_remove("HER_RUNTIME_ROOT");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let result = command.output().unwrap();
    assert_eq!(result.status.code(), Some(2), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(String::from_utf8_lossy(&result.stderr).contains("unknown command"));
}

#[test]
fn removed_flags_are_rejected_before_runtime_selection() {
    assert_removed_flags(&[
        "--ab-full",
        "--ab-probe",
        "--shadow-turn",
        "--shadow-state-root",
        "--replay",
        "--wake-replay",
        "--replay-jsonl",
        "--replay-triggers",
    ]);
}

#[test]
fn retired_dsh_probes_are_rejected_before_runtime_selection() {
    assert_removed_flags(&[
        "--ask",
        "--rpc-probe",
        "--selftest-401",
        "--failover-probe",
        "--dsh-dump",
        "--dsh-watch",
        "--dsh-probe",
        "--dsh-demo",
    ]);
}

fn assert_removed_flags(flags: &[&str]) {
    for &flag in flags {
        let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
        command
            .args(["--live", "--live-groups", "group:1", "--console-port", "1", flag])
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
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("unknown option"),
            "{flag} must be rejected before runtime selection: {stderr}"
        );
    }
}
