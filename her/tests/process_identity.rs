//! Process identity through the real CLI and disposable, reaped Node children.
#[path = "../src/install_root.rs"]
mod install_root;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn hidden(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

struct Fixture {
    child: Child,
    root: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    fn node(name: &str, entry: &str, extra: &[&str]) -> Self {
        let root = crate::install_root::development_root(std::env::temp_dir().join(format!("her-process-{name}-{}", std::process::id())));
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(
            root.join(entry),
            "const fs = require('fs'); fs.writeFileSync('ready', 'ok'); setInterval(() => { if (fs.existsSync('exit')) process.exit(Number(fs.readFileSync('exit', 'utf8'))); }, 10);",
        )
        .unwrap();
        let child = hidden("node")
            .arg(entry)
            .args(extra)
            .arg("her-process-fixture-05")
            .current_dir(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Self { child, root };
        fixture.wait_ready();
        fixture
    }
    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.root.join("ready").exists() {
            assert!(self.child.try_wait().unwrap().is_none(), "fixture exited");
            assert!(Instant::now() < deadline, "fixture not ready");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn run(&self, args: &[&str]) -> Value {
        cli(&self.root, args)
    }
    fn identify(&self) -> Value {
        self.run(&["identify", "--pid", &self.child.id().to_string()])
    }
}

#[test]
fn liveness_distinguishes_stale_instances_exited_children_and_inaccessible_processes() {
    let previous = Fixture::node("prior-instance", "entry.js", &[]);
    let previous_start = previous.identify()["process"]["start"].as_u64().unwrap();
    drop(previous); // Reap the original instance before creating its replacement.
    std::thread::sleep(Duration::from_millis(30));
    let mut f = Fixture::node("liveness", "entry.js", &[]);
    let found = f.identify();
    let pid = f.child.id().to_string();
    let start = found["process"]["start"].as_u64().unwrap();
    assert!(
        start > previous_start,
        "native creation stamps must distinguish successive instances"
    );
    // Rebind the original child's recorded stamp to the replacement PID, as if
    // the OS had recycled the original PID. No forced OS PID recycling needed.
    assert_eq!(
        f.run(&[
            "check",
            "--pid",
            &pid,
            "--start",
            &previous_start.to_string()
        ])["state"],
        "mismatch"
    );
    assert_eq!(
        f.run(&["check", "--pid", &pid, "--start", &start.to_string()])["state"],
        "matches"
    );
    // Deterministic PID-reuse simulation: the recorded creation stamp belongs to
    // another instance while this real child holds the same numeric PID.
    assert_eq!(
        f.run(&["check", "--pid", &pid, "--start", &(start + 1).to_string()])["state"],
        "mismatch"
    );
    assert!(
        f.child.try_wait().unwrap().is_none(),
        "stale identity must leave child alive"
    );
    std::fs::write(f.root.join("exit"), "259").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = f.run(&["check", "--pid", &pid, "--start", &start.to_string()]);
        if state["state"] == "gone" {
            break;
        }
        assert_eq!(
            state["state"], "matches",
            "exit race is neither unknown nor another instance: {state}"
        );
        assert!(
            Instant::now() < deadline,
            "exited child reported alive: {state}"
        );
    }
    let exit = loop {
        if let Some(status) = f.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "gone verdict preceded actual child exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    #[cfg(windows)]
    assert_eq!(
        exit.code(),
        Some(259),
        "259 is a valid exit code, not proof of liveness"
    );
    #[cfg(not(windows))]
    assert!(!exit.success());
    assert_eq!(f.identify()["state"], "gone");

    #[cfg(windows)]
    {
        // System already exists and denies full inspection. Never start or stop
        // an elevated process just to exercise this boundary.
        let inspected = f.run(&["identify", "--pid", "4"]);
        assert_eq!(inspected["state"], "unknown", "{inspected}");
        let limited = f.run(&["check", "--pid", "4", "--start", "0"]);
        assert!(
            matches!(
                limited["state"].as_str(),
                Some("unknown" | "mismatch" | "matches")
            ),
            "access denial must never mean dead: {limited}"
        );
        println!("protected process full={inspected} limited={limited}");
    }
}
fn cli(root: &Path, args: &[&str]) -> Value {
    let output = hidden(env!("CARGO_BIN_EXE_her-cli"))
        .arg("process")
        .args(args)
        .arg("--runtime-root")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("process JSON")
}
fn same_path(found: &Value, expected: &Path) {
    assert_eq!(
        std::fs::canonicalize(found.as_str().unwrap()).unwrap(),
        std::fs::canonicalize(expected).unwrap()
    );
}

#[test]
fn identifies_real_child_program_working_folder_and_quoted_arguments() {
    let f = Fixture::node(
        "identify",
        "entry space.js",
        &["two words", "\u{5f15}\u{53f7}", "a\"b", "tail\\"],
    );
    let found = f.identify();
    assert_eq!(found["state"], "identified");
    assert_eq!(found["process"]["pid"], f.child.id());
    assert!(found["process"]["start"].as_u64().unwrap() > 0);
    assert!(Path::new(found["process"]["exe"].as_str().unwrap())
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .eq_ignore_ascii_case("node"));
    same_path(&found["process"]["cwd"], &f.root);
    assert_eq!(
        found["process"]["args"].as_array().unwrap()[1..6],
        [
            "entry space.js",
            "two words",
            "\u{5f15}\u{53f7}",
            "a\"b",
            "tail\\"
        ]
    );
}

#[test]
fn matches_relative_script_but_never_a_program_or_argument_lookalike() {
    let mut f = Fixture::node("match", "entry space.js", &["lookalike.js"]);
    let found = f.identify();
    let pid = f.child.id().to_string();
    let program = found["process"]["exe"].as_str().unwrap();
    let script = f.root.join("entry space.js");
    assert_eq!(
        f.run(&["match", "--pid", &pid, "--program", program])["state"],
        "matches"
    );
    for spelling in [
        script.to_str().unwrap(),
        "entry space.js",
        "./entry space.js",
    ] {
        let result = f.run(&[
            "match",
            "--pid",
            &pid,
            "--program",
            program,
            "--script",
            spelling,
        ]);
        assert_eq!(result["state"], "matches", "{result}");
    }
    std::fs::write(f.root.join("lookalike.js"), "").unwrap();
    let foreign = f.run(&[
        "match",
        "--pid",
        &pid,
        "--program",
        program,
        "--script",
        "lookalike.js",
    ]);
    assert_eq!(
        foreign["state"], "mismatch",
        "a later argv token is not the script: {foreign}"
    );
    let foreign = f.run(&[
        "match",
        "--pid",
        &pid,
        "--program",
        env!("CARGO_BIN_EXE_her-cli"),
        "--script",
        script.to_str().unwrap(),
    ]);
    assert_eq!(
        foreign["state"], "mismatch",
        "program is part of ownership: {foreign}"
    );

    // A preload operand cannot become the entry; -- ends interpreter options.
    f.child.kill().unwrap();
    f.child.wait().unwrap();
    std::fs::remove_file(f.root.join("ready")).unwrap();
    f.child = hidden(program)
        .args([
            "--require",
            "./lookalike.js",
            "--",
            "entry space.js",
            "her-process-fixture-05",
        ])
        .current_dir(&f.root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    f.wait_ready();
    let pid = f.child.id().to_string();
    assert_eq!(
        f.run(&[
            "match",
            "--pid",
            &pid,
            "--program",
            program,
            "--script",
            script.to_str().unwrap()
        ])["state"],
        "matches"
    );
    assert_eq!(
        f.run(&[
            "match",
            "--pid",
            &pid,
            "--program",
            program,
            "--script",
            "lookalike.js"
        ])["state"],
        "mismatch"
    );

    // npm/npx package arguments do not confer ownership of the resolved DSH entry.
    for entry in ["npm-cli.js", "npx-cli.js"] {
        let wrapper = Fixture::node(
            entry,
            entry,
            &["@deepseek-ai/dsh@fixture", script.to_str().unwrap()],
        );
        assert_eq!(
            wrapper.run(&[
                "match",
                "--pid",
                &wrapper.child.id().to_string(),
                "--program",
                program,
                "--script",
                script.to_str().unwrap()
            ])["state"],
            "wrapper"
        );
    }

    // Relative entries can survive a runtime changing cwd after launch. The program
    // folder is a fallback only when that script does not exist in the current cwd.
    let mut fallback = Fixture::node("program-folder", "unused.js", &[]);
    fallback.child.kill().unwrap();
    fallback.child.wait().unwrap();
    std::fs::remove_file(fallback.root.join("ready")).unwrap();
    let bin = fallback.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let copied = bin.join(if cfg!(windows) { "node.exe" } else { "node" });
    std::fs::copy(program, &copied).unwrap();
    let entry = bin.join("program-entry.js");
    std::fs::write(&entry, "process.chdir('..'); require('fs').writeFileSync('ready', 'ok'); setInterval(() => {}, 1000);").unwrap();
    fallback.child = hidden(&copied)
        .args(["program-entry.js", "her-process-fixture-05"])
        .current_dir(&bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fallback.wait_ready();
    assert_eq!(
        fallback.run(&[
            "match",
            "--pid",
            &fallback.child.id().to_string(),
            "--program",
            copied.to_str().unwrap(),
            "--script",
            entry.to_str().unwrap()
        ])["state"],
        "matches"
    );
    // If cwd has its own entry with the same name, it wins over the fallback.
    std::fs::write(fallback.root.join("program-entry.js"), "").unwrap();
    assert_eq!(
        fallback.run(&[
            "match",
            "--pid",
            &fallback.child.id().to_string(),
            "--program",
            copied.to_str().unwrap(),
            "--script",
            entry.to_str().unwrap()
        ])["state"],
        "mismatch"
    );
}
