//! Ticket 22: the console usage routes must reproduce the JS token ledger (`qq-bridge/src/token-ledger.js`).
//!
//! Recorded token-ledger fixtures are served by an isolated console process.
//! Every value is synthetic (fixture tokens, loopback ports, temp roots). No live service, config or endpoint is touched.

#[path = "../src/install_root.rs"]
mod install_root;
mod support;

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSOLE_TOKEN: &str = "synthetic-ledger-oracle-console-token";
const OWNER: &str = "7001";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("token-ledger")
        .join("main")
        .join(name)
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_slice(&fs::read(fixture_path(name)).expect("fixture readable")).expect("fixture is JSON")
}

struct SyntheticRoot(PathBuf);

impl SyntheticRoot {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-ledger-oracle-{label}-{}-{stamp}", std::process::id())));
        fs::create_dir_all(&root).expect("create synthetic root");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SyntheticRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Config in the isolated-runtime shape (`allow.private`, `dsh`) with the fixture pricing override.
fn isolated_config() -> Value {
    json!({
        "consoleToken": CONSOLE_TOKEN,
        "ownerQQ": 7001,
        "allow": { "groups": ["7003"], "private": [OWNER] },
        "dsh": { "baseUrl": "http://127.0.0.1:1", "provider": "fixture", "model": "fixture-model", "reasoningEffort": "high" },
        "pricing": fixture_json("pricing.json")
    })
}

fn dependencies() -> Value {
    json!({ "dsh": "http://127.0.0.1:1", "model": "http://127.0.0.1:1", "onebot": "http://127.0.0.1:1" })
}

fn hide(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

struct Console {
    child: Child,
    port: u16,
}

impl Console {
    fn start(root: &Path, dsh_home: &Path) -> Self {
        let (child, port) = crate::support::ports::start_console(Duration::from_secs(8), |port| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_her"));
            command
                .arg("--console-serve")
                .arg("--console-port")
                .arg(port.to_string())
                .arg("--runtime-root")
                .arg(root)
                .env_remove("HER_RUNTIME_ROOT")
                .env_remove("APPDATA")
                .env_remove("LOCALAPPDATA")
                .env_remove("HOME")
                .env("DSH_HOME", dsh_home)
                .env("USERPROFILE", dsh_home.join("synthetic-user-profile"))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            hide(&mut command);
            command.spawn().expect("start isolated console")
        });
        let mut console = Console { child, port };
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok((200, body)) = get(port, "/api/health", None) {
                assert_eq!(body["runtime"]["mode"], "isolated", "console must run isolated");
                break;
            }
            assert!(Instant::now() < deadline, "isolated console did not become healthy");
            std::thread::sleep(Duration::from_millis(50));
        }
        console.port = port;
        console
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn get(port: u16, path: &str, token: Option<&str>) -> Result<(u16, Value), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let token_header = token.map(|t| format!("X-Console-Token: {t}\r\n")).unwrap_or_default();
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{token_header}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("no header/body split")?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("no status")?;
    let json = serde_json::from_str(body.trim()).map_err(|e| format!("body is not JSON ({e}): {body}"))?;
    Ok((status, json))
}

/// Value-by-value comparison. Object key sets must match after removing skipped paths; arrays keep their order;
/// numbers agree within 1e-9 relative (float summation order); everything else is exact.
fn compare(path: &str, actual: &Value, expected: &Value, skip: &[&str], diffs: &mut Vec<String>) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            let keep = |k: &&String| !skip.iter().any(|s| format!("{path}.{k}").ends_with(s));
            let ak: std::collections::BTreeSet<&String> = a.keys().filter(keep).collect();
            let ek: std::collections::BTreeSet<&String> = e.keys().filter(keep).collect();
            if ak != ek {
                diffs.push(format!("keys differ at {path}: rust-only={ak:?}... js-only={ek:?}"));
            }
            for k in ak.intersection(&ek) {
                compare(&format!("{path}.{k}"), &a[*k], &e[*k], skip, diffs);
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            if a.len() != e.len() {
                diffs.push(format!("length differs at {path}: rust={} js={}", a.len(), e.len()));
            }
            for (i, (x, y)) in a.iter().zip(e.iter()).enumerate() {
                compare(&format!("{path}[{i}]"), x, y, skip, diffs);
            }
        }
        (Value::Number(_), Value::Number(_)) => {
            let (x, y) = (actual.as_f64().unwrap(), expected.as_f64().unwrap());
            if (x - y).abs() > 1e-9 * y.abs().max(1.0) {
                diffs.push(format!("number differs at {path}: rust={x} js={y}"));
            }
        }
        _ => {
            if actual != expected {
                diffs.push(format!("value differs at {path}: rust={actual} js={expected}"));
            }
        }
    }
}

fn assert_same(label: &str, actual: &Value, expected: &Value, skip: &[&str]) {
    let mut diffs = Vec::new();
    compare(label, actual, expected, skip, &mut diffs);
    assert!(diffs.is_empty(), "{label}: {} difference(s)\n{}", diffs.len(), diffs.join("\n"));
}

/// Summary, per-conversation turns and recent samples from the console must equal the JS ledger outputs.
fn check_ledger_routes(console: &Console) {
    let expected = fixture_json("expected.json");
    let (status, summary) = get(console.port, "/api/tokens/summary?limit=20", Some(CONSOLE_TOKEN)).expect("summary");
    assert_eq!(status, 200, "summary: {summary}");
    // generatedAt/nowTier depend on the clock; `ledger` is the store-health block the JS route adds.
    assert_same("summary", &summary, &expected["summary"], &["generatedAt", "nowTier", "ledger", "file"]);

    let (status, turns) = get(console.port, "/api/tokens/turns?key=group:7003&limit=50", Some(CONSOLE_TOKEN)).expect("turns");
    assert_eq!(status, 200, "turns: {turns}");
    assert_same("turns", &turns, &expected["conversation"]["group:7003"], &["generatedAt"]);

    let (status, recent) = get(console.port, "/api/tokens/recent?limit=10", Some(CONSOLE_TOKEN)).expect("recent");
    assert_eq!(status, 200, "recent: {recent}");
    assert_same("recent", &recent["entries"], &expected["recent"], &[]);

    // Candidate-backend attribution: the reference is computed from the JS-written file with the JS price table only.
    let (status, by_provider) = get(console.port, "/api/tokens/by-provider", Some(CONSOLE_TOKEN)).expect("by-provider");
    assert_eq!(status, 200, "by-provider: {by_provider}");
    let want = &expected["byProvider"];
    let rows = by_provider["providers"].as_array().expect("providers array");
    let want_rows = want["providers"].as_array().expect("reference providers");
    assert_eq!(rows.len(), want_rows.len(), "provider rows: {by_provider}");
    for w in want_rows {
        let name = w["provider"].as_str().unwrap();
        let got = rows
            .iter()
            .find(|r| r["provider"] == name)
            .unwrap_or_else(|| panic!("provider {name} missing from {by_provider}"));
        assert_eq!(got["samples"], w["samples"], "{name}: samples");
        assert_same(&format!("provider[{name}].cost"), &got["cost"], &w["cost"], &[]);
        assert_same(&format!("provider[{name}].tokens.in"), &got["tokens"]["in"], &w["tokensIn"], &[]);
        assert_same(&format!("provider[{name}].tokens.out"), &got["tokens"]["out"], &w["tokensOut"], &[]);
    }
    assert_same("totalCost", &by_provider["totalCost"], &want["totalCost"], &[]);
    assert_same("totalSamples", &by_provider["totalSamples"], &want["totalSamples"], &[]);
}

#[test]
fn isolated_console_serves_the_js_ledger_outputs_for_the_fixture() {
    let root = SyntheticRoot::new("isolated");
    let path = root.path();
    for dir in ["config", "data", "user/dsh", "resources/roles"] {
        fs::create_dir_all(path.join(dir)).expect("create runtime dir");
    }
    fs::write(path.join("config/config.json"), serde_json::to_vec_pretty(&isolated_config()).unwrap()).unwrap();
    fs::write(path.join("dependencies.json"), serde_json::to_vec(&dependencies()).unwrap()).unwrap();
    fs::write(path.join("data/social-v2.json"), b"{}").unwrap();
    fs::copy(fixture_path("token-usage.jsonl"), path.join("data/token-usage.jsonl")).unwrap();
    fs::copy(fixture_path("sessions.json"), path.join("data/sessions.json")).unwrap();
    let dsh_home = root.path().join("dsh-home");
    fs::create_dir_all(&dsh_home).unwrap();
    let console = Console::start(path, &dsh_home);
    check_ledger_routes(&console);
}
