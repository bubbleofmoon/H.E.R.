#[path = "../src/settings/probe.rs"]
#[allow(dead_code)]
mod probe;
mod support;

use probe::StoreProbe;
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use support::{RuntimeRoot, CLOSED};

const KEY: &str = "console.administratorPassword";
const RAW_TOKEN: &str = "fixture-console-machine-proof";
const PASSWORD: &str = "  离线控制台密码样本  ";

fn runtime(configured: bool) -> RuntimeRoot {
    let root = RuntimeRoot::new("console-auth-cli");
    let token = if configured { RAW_TOKEN } else { "" };
    std::fs::write(
        root.join("config/config.json"),
        json!({"consoleToken": token}).to_string(),
    )
    .unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    if !configured {
        std::fs::write(root.join("data/console-token"), format!("{RAW_TOKEN}\n")).unwrap();
    }
    support::write_dependencies(&root, CLOSED, CLOSED, CLOSED, None);
    root
}

fn run(root: &RuntimeRoot, verb: &str, input: &str, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her-cli"));
    support::hide_window(&mut command);
    let mut child = command
        .args(["console", verb, "--runtime-root"])
        .arg(root.path())
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let written = child.stdin.take().unwrap().write_all(input.as_bytes());
    let output = child.wait_with_output().unwrap();
    if let Err(error) = written {
        // A usage refusal can close stdin before reading it.
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(!output.status.success());
    }
    output
}

fn set(root: &RuntimeRoot, proof: &str, password: &str) -> Output {
    run(
        root,
        "set-password",
        &format!("{proof}\r\n{password}\r\n{password}\r\n"),
        &[],
    )
}

fn assert_clean_bytes(bytes: &[u8], fixture: &str) {
    assert!(
        !bytes
            .windows(fixture.len())
            .any(|slice| slice == fixture.as_bytes()),
        "credential fixture appeared in an observable channel"
    );
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(output.status.code(), Some(code), "CLI exit status");
    if code == 0 {
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({"ok": true})
        );
    } else {
        assert!(output.stdout.is_empty());
    }
    for stream in [&output.stdout, &output.stderr] {
        for fixture in [
            PASSWORD,
            RAW_TOKEN,
            "fixture-wrong-machine-proof",
            "fixture-secret-argument",
        ] {
            assert_clean_bytes(stream, fixture);
        }
    }
}

#[test]
fn set_once_with_raw_token_and_reset_offline() {
    for configured in [false, true] {
        let root = runtime(configured);
        let config_path = root.join("config/config.json");
        let original_config = std::fs::read(&config_path).unwrap();
        let probe = StoreProbe::at(&root);
        assert_exit(&set(&root, RAW_TOKEN, PASSWORD), 0);
        let hash = probe.value(KEY).expect("a password row was written");
        let phc: String = serde_json::from_str(&hash).unwrap();
        assert!(phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert_eq!(probe.user_version(), 3);
        assert_eq!(
            set(&root, RAW_TOKEN, PASSWORD).status.code(),
            Some(3),
            "a second set must refuse with valid current raw proof"
        );
        assert!(
            probe.value(KEY).as_ref() == Some(&hash),
            "the original hash must stand"
        );
        assert_eq!(probe.audit_count(), 1);
        assert_exit(&run(&root, "reset-password", "", &[]), 0);
        assert!(probe.value(KEY).is_none());
        assert_eq!(probe.audit_count(), 2);
        assert_exit(&run(&root, "reset-password", "", &[]), 0);
        assert_eq!(
            probe.audit_count(),
            2,
            "reset of an unset password is idempotent"
        );

        assert_exit(&set(&root, RAW_TOKEN, PASSWORD), 0);
        let replacement = probe.value(KEY).unwrap();
        let replacement: String = serde_json::from_str(&replacement).unwrap();
        assert!(
            phc.split('$').nth(4) != replacement.split('$').nth(4),
            "a fresh salt must be used"
        );
        assert!(
            phc != replacement,
            "the same password must get a distinct PHC"
        );
        assert!(
            std::fs::read(&config_path).unwrap() == original_config,
            "password commands must leave config.json intact"
        );
        assert!(!root.join("config/config.json.pre-settings").exists());
        for (index, entry) in probe.audit_rows().iter().enumerate() {
            assert_eq!(entry["actor"], "her-cli");
            assert_eq!(
                entry["source"],
                if index == 1 {
                    "reset-password"
                } else {
                    "first-run"
                }
            );
            assert_eq!(entry["key"], KEY);
            assert_eq!(entry["old"].as_str(), Some("\"(hidden)\""));
            assert_eq!(entry["new"].as_str(), Some("\"(hidden)\""));
            assert_clean_bytes(entry.to_string().as_bytes(), PASSWORD);
            assert_clean_bytes(entry.to_string().as_bytes(), RAW_TOKEN);
        }
        for name in [
            "settings.db",
            "settings.db-wal",
            "settings.db-shm",
            "settings.db-journal",
        ] {
            match std::fs::read(root.join("data").join(name)) {
                Ok(bytes) => {
                    assert_clean_bytes(&bytes, PASSWORD);
                    assert_clean_bytes(&bytes, RAW_TOKEN);
                }
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
            }
        }
    }
}

#[test]
fn overlong_password_is_refused_before_hashing() {
    let root = runtime(false);
    let probe = StoreProbe::at(&root);
    assert_eq!(
        set(&root, RAW_TOKEN, &"密".repeat(1025)).status.code(),
        Some(2),
        "1025 Unicode characters must exit 2 before writing a credential"
    );
    assert!(
        !probe.path().exists(),
        "overlength must leave no hash or audit row"
    );
    assert_exit(&set(&root, RAW_TOKEN, &"密".repeat(1024)), 0);
    assert!(probe.value(KEY).is_some());
    assert_eq!(probe.audit_count(), 1);
}

#[test]
fn cli_rejects_bad_proof_and_secret_flags() {
    let root = runtime(true);
    let probe = StoreProbe::at(&root);
    let refused = set(&root, "fixture-wrong-machine-proof", PASSWORD);
    assert_eq!(
        refused.status.code(),
        Some(3),
        "a mismatched raw proof must exit 3"
    );
    assert_exit(&refused, 3);
    for flag in ["--token", "--password"] {
        assert_exit(
            &run(
                &root,
                "set-password",
                "",
                &[flag, "fixture-secret-argument"],
            ),
            2,
        );
    }
    for input in [
        format!("{RAW_TOKEN}\n{PASSWORD}\n"),
        format!("{RAW_TOKEN}\n{PASSWORD}\n不同内容\n"),
    ] {
        assert_exit(&run(&root, "set-password", &input, &[]), 2);
    }
    assert!(
        !probe.path().exists(),
        "refusals must leave no credential or audit row"
    );
}

#[test]
fn reset_removes_an_unavailable_password_row() {
    let root = runtime(true);
    let probe = StoreProbe::at(&root);
    assert_exit(&set(&root, RAW_TOKEN, PASSWORD), 0);
    for (index, malformed) in ["null", "\"malformed PHC\""].into_iter().enumerate() {
        if index == 0 {
            probe.set_value(KEY, malformed);
        } else {
            probe.insert_value(KEY, malformed);
        }
        assert_exit(&run(&root, "reset-password", "", &[]), 0);
        assert!(
            probe.value(KEY).is_none(),
            "reset removes even an unavailable password row"
        );
    }
    assert_eq!(probe.audit_count(), 3);
}
