//! #183: `her-cli upstream-capture` through the real binary on a synthetic selected root. The
//! capture points themselves are unit-tested (`upstream_capture`, `dsh`); here a capture record of
//! the shape they write is swept.
mod support;

use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use support::{write_dependencies, RuntimeRoot, CLOSED};

fn runtime() -> RuntimeRoot {
    let root = RuntimeRoot::new("upstream-capture");
    std::fs::write(
        root.join("config/config.json"),
        r#"{"consoleToken":"fixture-token-183"}"#,
    )
    .unwrap();
    std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
    write_dependencies(&root, CLOSED, CLOSED, CLOSED, None);
    root
}

/// Exit code, stdout as JSON (null when it is not JSON), stdout and stderr text.
fn cli(root: &Path, args: &[&str]) -> (i32, Value, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_her-cli"));
    support::hide_window(&mut command);
    let output = command
        .arg("upstream-capture")
        .args(args)
        .arg("--runtime-root")
        .arg(root)
        .env_remove("HER_RUNTIME_ROOT")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let json = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (
        output.status.code().unwrap_or(-1),
        json,
        stdout,
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn a_capture_is_armed_swept_into_counts_and_paths_and_gone() {
    let root = runtime();
    let (code, status, _, _) = cli(&root, &["status"]);
    assert_eq!(code, 0);
    assert_eq!(
        (status["armed"].clone(), status["captured"].clone()),
        (json!(false), json!(false))
    );
    let (code, _, _, error) = cli(&root, &["sweep"]);
    assert_eq!(code, 1, "{error}");
    assert!(error.contains("nothing was captured"), "{error}");

    let (code, armed, _, error) = cli(&root, &["arm", "--route", "dsh", "--minutes", "5"]);
    assert_eq!(code, 0, "{error}");
    assert_eq!(
        (armed["armed"].clone(), armed["route"].clone()),
        (json!(true), json!("dsh"))
    );
    assert_eq!(
        cli(&root, &["arm", "--route", "harness"]).0,
        1,
        "a second arming is refused"
    );
    assert_eq!(cli(&root, &["status"]).1["route"], "dsh");

    // The record a DSH capture writes: the envelope, with one denied term in the message and one in
    // a tool key, and message text that must not reach the output.
    let request = json!({"type": "client-request", "rpcId": "rpc-183", "method": "session/prompt",
        "payload": {"args": {"request": {"sessionId": "s-183", "content": [
            {"type": "text", "text": "今天天气不错，dsh 你好呀"},
            {"type": "text", "text": "fixture chat line for the sweep"},
        ], "harness": true}}}});
    std::fs::remove_file(root.join("data/upstream-capture/armed.json")).unwrap();
    std::fs::write(
        root.join("data/upstream-capture/captured.json"),
        json!({"v": 1, "route": "dsh", "capturedAt": 7, "request": request}).to_string(),
    )
    .unwrap();
    let (code, report, stdout, error) = cli(&root, &["sweep"]);
    assert_eq!(code, 0, "{error}");
    assert_eq!(report["route"], "dsh");
    assert_eq!(report["deniedTotal"], 2);
    assert_eq!(report["counts"], json!({"dsh": 1, "harness": 1}));
    assert_eq!(
        report["fields"],
        json!([
            {"path": "/payload/args/request/content/0/text", "part": "value", "terms": ["dsh"]},
            {"path": "/payload/args/request/<key>", "part": "key", "terms": ["harness"]},
        ])
    );
    assert_eq!(report["removed"], true);
    for text in [
        "今天天气不错",
        "你好",
        "fixture chat line",
        "s-183",
        "rpc-183",
    ] {
        assert!(
            !stdout.contains(text),
            "the sweep printed message content: {text}"
        );
    }
    assert!(
        !root.join("data/upstream-capture/captured.json").exists(),
        "the capture was not deleted"
    );
    assert_eq!(cli(&root, &["sweep"]).0, 1, "a capture is swept once");
    let (code, status, _, _) = cli(&root, &["status"]);
    assert_eq!(code, 0);
    assert_eq!(
        (status["armed"].clone(), status["captured"].clone()),
        (json!(false), json!(false))
    );
}

#[test]
fn disarm_removes_an_unused_arming_and_usage_errors_exit_2() {
    let root = runtime();
    assert_eq!(cli(&root, &["arm", "--route", "harness"]).0, 0);
    let (code, removed, _, error) = cli(&root, &["disarm"]);
    assert_eq!(code, 0, "{error}");
    assert_eq!(removed, json!({"removed": ["armed"]}));
    assert!(!root.join("data/upstream-capture/armed.json").exists());
    for args in [
        &["arm"][..],
        &["arm", "--route", "relay"],
        &["arm", "--route", "dsh", "--minutes", "0"],
        &["arm", "--route", "dsh", "--minutes", "1441"],
        &["status", "--route", "dsh"],
        &["capture"],
        &["sweep", "--unknown", "x"],
    ] {
        assert_eq!(cli(&root, args).0, 2, "{args:?}");
    }
    assert_eq!(
        cli(&root, &["arm", "--route", "dsh", "--minutes", "1440"]).0,
        0
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_her-cli"));
    support::hide_window(&mut command);
    let missing_root = command
        .args(["upstream-capture", "status"])
        .env_remove("HER_RUNTIME_ROOT")
        .output()
        .unwrap();
    assert_eq!(missing_root.status.code(), Some(2));
}
