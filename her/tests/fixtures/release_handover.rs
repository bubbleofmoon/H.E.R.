//! Seam 1: installed candidate/helper/owners and real adoption of fake peers.
use super::*;

// Trial, turn drain, retirement and judgment each have their own real deadlines.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(600);

fn hidden(image: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(image);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .env_remove("HER_TEST_UPDATE_PLATFORM")
        .env_remove("HER_TEST_HANDOVER_CONTROL")
        .env_remove("HER_INSTALL_ROOT")
        .env_remove("HER_RUNTIME_ROOT")
        .env_remove("HER_TRIAL_CURRENT")
        .env_remove("HER_PARITY_EPOCH_MS");
    command
}

struct Owned {
    child: std::process::Child,
    identity: process_identity::ProcessIdentity,
}
impl Owned {
    fn new(mut child: std::process::Child, image: &Path) -> Self {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(identity) = process_identity::identity_of(child.id()).unwrap() {
                if fs::canonicalize(&identity.exe).ok() == fs::canonicalize(image).ok() {
                    return Self { child, identity };
                }
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before exec"
            );
            assert!(Instant::now() < end, "child did not exec selected image");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait(&mut self, timeout: Duration, mut on_poll: impl FnMut()) -> std::process::ExitStatus {
        let end = Instant::now() + timeout;
        loop {
            on_poll();
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < end,
                "child {:?} did not exit",
                self.identity
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        cleanup_owned_process(&mut self.child, &self.identity);
    }
}

struct Portable {
    // Processes must be stopped before Fixture removes the install root.
    old: Option<Owned>,
    dependencies: Vec<Owned>,
    owners: Mutex<Vec<process_identity::ProcessIdentity>>,
    f: Fixture,
    controls: PathBuf,
}
impl Portable {
    fn new() -> Self { Self::configured(|_| {}) }
    fn configured(configure: impl FnOnce(&Self)) -> Self {
        let f = Fixture::new();
        let mut sums = format!("# HER-RELEASE v1.2.3 1\n{}", release_signing::keys("primary"));
        for (name, source) in [
            ("her.exe", env!("CARGO_BIN_EXE_her")),
            ("her-cli.exe", env!("CARGO_BIN_EXE_her-cli")),
            ("herupdate.exe", env!("CARGO_BIN_EXE_herupdate")),
        ] {
            let bytes = fs::read(source).unwrap();
            sums.push_str(&format!("{:x}  {name}\n", Sha256::digest(&bytes)));
            f.replies.lock().unwrap().insert(format!("/{name}"), bytes);
            if name != "her-cli.exe" {
                fs::copy(source, f.root.join("install/releases/v0.0.0").join(name)).unwrap();
            }
        }
        f.replies
            .lock()
            .unwrap()
            .insert("/SHA256SUMS".into(), sums.into_bytes());
        f.sign("primary");
        f.sign_directory("v0.0.0", 1, "primary");
        let out = f.run("install");
        assert!(out.status.success(), "{out:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["her.exe", "her-cli.exe", "herupdate.exe"] {
                fs::set_permissions(
                    f.root.join("install/releases/v1.2.3").join(name),
                    fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
        }
        fs::copy(
            env!("CARGO_BIN_EXE_herupdate"),
            f.root.join("install/herupdate.exe"),
        )
        .unwrap();
        for name in ["current", "last-good"] {
            #[cfg(unix)]
            std::os::unix::fs::symlink(
                f.root.join("install/releases/v0.0.0"),
                f.root.join("install").join(name),
            )
            .unwrap();
            #[cfg(windows)]
            create_junction(
                &f.root.join("install").join(name),
                &f.root.join("install/releases/v0.0.0"),
            );
        }
        let runtime = f.root.join("install/runtime");
        for dir in ["config", "data", "resources", "user/dsh", "snow", "dsh"] {
            fs::create_dir_all(runtime.join(dir)).unwrap();
        }
        let reservations: Vec<_> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let ports: Vec<_> = reservations
            .iter()
            .map(|s| s.local_addr().unwrap().port())
            .collect();
        let output = hidden(if cfg!(windows) { "where.exe" } else { "which" })
            .arg("node")
            .output()
            .unwrap();
        assert!(output.status.success(), "Node fixture prerequisite");
        let node = PathBuf::from(
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .trim(),
        );
        let mut config = json!({"consoleToken":"fixture-portable", "consolePort":ports[2],
            "update":{"policy":"off"}, "snowluma":{"accessToken":"fixture-snow"},
            "backends":{"mcpRustBridge":true},"rollout":{"rustGroups":[]},
            "tasks":{"watchdog":{"enabled":true},"memory-digest":{"enabled":false},
            "memory-digest-check":{"enabled":false},"backup":{"enabled":false}}});
        for (i, (folder, name)) in [("snow", "snowluma"), ("dsh", "dsh")].iter().enumerate() {
            let dir = runtime.join(folder);
            fs::write(dir.join("index.mjs"), r#"
import http from 'node:http';
import fs from 'node:fs';
const cfg=JSON.parse(fs.readFileSync(new URL('./fixture.json',import.meta.url)));
http.createServer((req,res)=>{
 let body='';req.on('data',c=>body+=c);req.on('end',()=>{
  fs.appendFileSync(new URL('./requests',import.meta.url),req.url+'\n');
  const control=JSON.parse(fs.readFileSync(new URL('../../controls.json',import.meta.url)));
  const packet=JSON.parse(body||'{}');
  if(cfg.dsh)fs.appendFileSync(new URL('./packets.jsonl',import.meta.url),body+'\n');
  if(!cfg.dsh && req.url.startsWith('/send_')){
   fs.appendFileSync(new URL('./sends.jsonl',import.meta.url),JSON.stringify({packet,candidateExists:fs.existsSync(new URL('../../releases/v1.2.3',import.meta.url)),current:fs.realpathSync(new URL('../../current',import.meta.url)),rejected:control.rejectSend||false})+'\n');
   if(control.rejectSend==='lost'){res.destroy();return;}
   const code=control.rejectSend==='429'?429:control.rejectSend==='500'?500:200;
   const answer=control.rejectSend?{status:'failed',retcode:1,data:null}:{status:'ok',retcode:0,data:{message_id:123}};
   const reply=()=>{const text=JSON.stringify(answer);res.writeHead(code,{'Content-Length':Buffer.byteLength(text),'Connection':'close'});res.end(text)};
   if(control.pauseSend){
    fs.writeFileSync(new URL('./send.waiting',import.meta.url),'ready');
    const end=Date.now()+20000;const timer=setInterval(()=>{if(fs.existsSync(new URL('./send.resume',import.meta.url))||Date.now()>end){clearInterval(timer);reply()}},10);
   }else reply();return;
  }
  const answer=cfg.dsh?{type:'server-response',rpcId:JSON.parse(body||'{}').rpcId,result:{ok:true,value:{}}}:{status:'ok',retcode:0,data:{online:!control.offline && !(control.unhealthyTag && fs.realpathSync(new URL('../../current',import.meta.url)).endsWith(control.unhealthyTag)),good:true}};
  setTimeout(()=>{const text=JSON.stringify(answer);res.writeHead(200,{'Content-Length':Buffer.byteLength(text),'Connection':'close'});res.end(text)},control.delayMs||0);
 });
}).listen(cfg.port,'127.0.0.1');
"#).unwrap();
            fs::write(
                dir.join("fixture.json"),
                json!({"dsh":i==1,"port":ports[i]}).to_string(),
            )
            .unwrap();
            config["dependencies"][name] =
                json!({"program":node,"script":dir.join("index.mjs"),"workingDirectory":dir});
        }
        config["dependencies"]["dsh"]["registry"] = json!(format!("{}/", f.url));
        config["dependencies"]["snowluma"]["feedUrl"] = json!(format!("{}/feed", f.url));
        fs::write(runtime.join("config/config.json"), config.to_string()).unwrap();
        fs::write(runtime.join("data/social-v2.json"), "{}").unwrap();
        fs::write(
            runtime.join("user/dsh/.credentials.yaml"),
            "secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
        )
        .unwrap();
        fs::write(runtime.join("dependencies.json"), json!({"onebot":format!("http://127.0.0.1:{}",ports[0]),"dsh":format!("http://127.0.0.1:{}",ports[1]),"model":"http://127.0.0.1:1"}).to_string()).unwrap();
        let controls = f.root.join("install/controls.json");
        fs::write(&controls, "{}").unwrap();
        drop(reservations);
        let mut dependencies = Vec::new();
        for folder in ["snow", "dsh"] {
            let dir = runtime.join(folder);
            let child = hidden(&node)
                .arg(dir.join("index.mjs"))
                .current_dir(dir)
                .env("DSH_HOME", runtime.join("user/dsh"))
                .env("SNOWLUMA_HOOK_AUTOLOAD", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(runtime.join(folder).join("stderr")).unwrap())
                .spawn()
                .unwrap();
            dependencies.push(Owned::new(child, &node));
        }
        fs::write(f.root.join("handover-control.json"), "{}").unwrap();
        let mut fixture = Self {
            old: None,
            dependencies,
            owners: Mutex::new(Vec::new()),
            f,
            controls,
        };
        configure(&fixture);
        let image = fixture.image("v0.0.0", "her.exe");
        let child = fixture
            .command(&image)
            .args(["supervise", "--runtime-root"])
            .arg(fixture.runtime())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(fixture.f.root.join("old-supervisor.err")).unwrap())
            .spawn()
            .unwrap();
        fixture.old = Some(Owned::new(child, &image));
        fixture.assert_healthy("v0.0.0");
        fixture.assert_adopted();
        fixture
    }
    fn fault(&self, value: Value) {
        let path = self.f.root.join("handover-control.json");
        fs::write(path.with_extension("next"), value.to_string()).unwrap();
        fs::rename(path.with_extension("next"), path).unwrap();
    }
    fn peers(&self, value: Value) {
        fs::write(self.controls.with_extension("next"), value.to_string()).unwrap();
        fs::rename(self.controls.with_extension("next"), &self.controls).unwrap();
    }
    fn barrier(&self, tag: &str, phase: &str, suffix: &str) -> PathBuf {
        self.f
            .root
            .join("handover-control.json")
            .with_extension(format!("{tag}-{phase}.{suffix}"))
    }
    #[track_caller]
    fn wait(&self, predicate: impl Fn() -> bool) {
        self.wait_for(Duration::from_secs(30), predicate);
    }
    #[track_caller]
    fn wait_for(&self, timeout: Duration, predicate: impl Fn() -> bool) {
        let end = Instant::now() + timeout;
        while !predicate() {
            self.record_owners();
            assert!(Instant::now() < end, "portable fixture wait timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn runtime(&self) -> PathBuf {
        self.f.root.join("install/runtime")
    }
    fn image(&self, tag: &str, name: &str) -> PathBuf {
        self.f.root.join("install/releases").join(tag).join(name)
    }
    fn command(&self, image: &Path) -> Command {
        let mut c = hidden(image);
        c.env("HER_TASK_SCHEDULER_FILE", self.f.root.join("scheduler.xml"));
        c.env("HER_INSTALL_ROOT", self.f.root.join("install")).env(
            "HER_TEST_HANDOVER_CONTROL",
            self.f.root.join("handover-control.json"),
        );
        c
    }
    fn checker(&self, tag: &str, trial: bool) -> Output {
        let mut c = self.command(&self.image(tag, "her-cli.exe"));
        if trial {
            c.env(
                "HER_TRIAL_CURRENT",
                self.f.root.join("install/releases/v0.0.0"),
            );
        }
        c.args([
            "lifecycle",
            "assert-running",
            "--wait",
            "15s",
            "--runtime-root",
        ])
        .arg(self.runtime());
        self.output(&mut c, Duration::from_secs(25))
    }
    fn output(&self, command: &mut Command, timeout: Duration) -> Output {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let child_pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        // Separate readers keep both pipes drained. A descendant retaining a
        // pipe must fail this test within a deadline, never hang libtest.
        for (index, mut pipe) in [Box::new(stdout) as Box<dyn Read + Send>, Box::new(stderr)]
            .into_iter()
            .enumerate()
        {
            let sender = sender.clone();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
                let _ = sender.send((index, result));
            });
        }
        drop(sender);
        // Directly launched images do not exec a wrapper. Short checkers may
        // exit between polling and identity inspection; reap those normally.
        let end = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if let Some(identity) = process_identity::identity_of(child.id()).unwrap() {
                let mut owned = Owned { child, identity };
                break owned.wait(end.saturating_duration_since(Instant::now()), || {
                    self.record_owners()
                });
            }
            assert!(Instant::now() < end, "child identity remained unavailable");
            thread::sleep(Duration::from_millis(10));
        };
        self.record_owners();
        let end = Instant::now() + Duration::from_secs(5);
        let mut streams = [Vec::new(), Vec::new()];
        for _ in 0..2 {
            let (index, bytes) = receiver
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!(
                    "child {child_pid} ({:?}) exited but stdout/stderr stayed open in a descendant: {error}",
                    command.get_program()
                ));
            streams[index] = bytes.unwrap();
        }
        let [stdout, stderr] = streams;
        Output {
            status,
            stdout,
            stderr,
        }
    }
    fn assert_healthy(&self, tag: &str) {
        let out = self.checker(tag, false);
        self.record_owners();
        assert!(
            out.status.success(),
            "tag={tag} {out:?}; old={} states={} snow={} dsh={}",
            fs::read_to_string(self.f.root.join("old-supervisor.err")).unwrap_or_default(),
            fs::read_to_string(self.runtime().join("data/lifecycle/dependencies.json"))
                .unwrap_or_default(),
            fs::read_to_string(self.runtime().join("snow/stderr")).unwrap_or_default(),
            fs::read_to_string(self.runtime().join("dsh/stderr")).unwrap_or_default()
        );
    }
    fn record_owners(&self) {
        for name in [
            "her-supervisor.lock",
            "rbridge-supervisor.lock",
            "rbridge-console.lock",
            "rbridge-live.lock",
        ] {
            if let Ok(bytes) = fs::read(
                self.runtime()
                    .join("data/logs")
                    .join(format!("{name}.owner.json")),
            ) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    if let Ok(identity) = serde_json::from_value::<process_identity::ProcessIdentity>(
                        value["identity"].clone(),
                    ) {
                        let mut owners = self.owners.lock().unwrap_or_else(|error| {
                            cleanup_warning(format_args!("owner registry lock poisoned"));
                            error.into_inner()
                        });
                        if !owners.contains(&identity) {
                            owners.push(identity);
                        }
                    }
                }
            }
        }
    }
    fn assert_adopted(&self) {
        let owner: Value = serde_json::from_slice(
            &fs::read(
                self.runtime()
                    .join("data/logs/rbridge-supervisor.lock.owner.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let acquired = owner["acquiredAt"].as_i64().unwrap();
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let states = fs::read(self.runtime().join("data/lifecycle/dependencies.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .unwrap_or(Value::Null);
            if ["snowluma", "dsh"].iter().enumerate().all(|(i, name)| {
                states[name]["state"] == "adopted"
                    && states[name]["healthySince"].is_number()
                    // A retained old checkpoint cannot prove the new supervisor adopted.
                    && states[name]["timerAt"].as_i64().is_some_and(|at| at >= acquired)
                    && states[name]["identity"]
                        == serde_json::to_value(&self.dependencies[i].identity).unwrap()
            }) {
                break;
            }
            assert!(
                Instant::now() < end,
                "dependencies were not adopted: {states}"
            );
            thread::sleep(Duration::from_millis(25));
        }
        for dep in &self.dependencies {
            assert_eq!(
                process_identity::identity_of(dep.child.id()).unwrap(),
                Some(dep.identity.clone())
            );
        }
    }
    fn handover(&self) -> Output {
        let old = &self.old.as_ref().unwrap().identity;
        let mut command = self.command(&self.f.root.join("install/herupdate.exe"));
        command.args([
            "--target",
            "v1.2.3",
            "--old-pid",
            &old.pid.to_string(),
            "--old-start",
            &old.start.to_string(),
            "--old-exe",
            &old.exe,
        ]);
        let out = self.output(&mut command, HANDOVER_TIMEOUT);
        self.record_owners();
        out
    }
    fn link_matches(&self, name: &str, tag: &str) -> bool {
        fs::canonicalize(self.f.root.join("install").join(name)).is_ok_and(|actual| {
            fs::canonicalize(self.image(tag, "her.exe").parent().unwrap())
                .is_ok_and(|expected| actual == expected)
        })
    }
    #[track_caller]
    fn wait_for_handover(&self, tag: &str) {
        // Current switches before startup; last-good publishes only after the
        // helper has judged the replacement healthy. Keep peers alive until then.
        self.wait_for(HANDOVER_TIMEOUT, || {
            self.link_matches("current", tag) && self.link_matches("last-good", tag)
        });
        self.assert_link("current", tag);
        self.assert_link("last-good", tag);
    }
    fn assert_link(&self, name: &str, tag: &str) {
        assert_eq!(
            fs::canonicalize(self.f.root.join("install").join(name)).unwrap(),
            fs::canonicalize(self.image(tag, "her.exe").parent().unwrap()).unwrap()
        );
    }
}
impl Drop for Portable {
    fn drop(&mut self) {
        if thread::panicking() {
            cleanup_warning(format_args!(
                "current at failure: {:?}",
                fs::canonicalize(self.f.root.join("install/current"))
            ));
            for name in ["handover-control.helper.stderr","install/runtime/data/releases.json","handover-control.json","old-supervisor.err","install/runtime/data/logs/rbridge-live.err.log","install/runtime/data/logs/rbridge-live.out.log","install/runtime/data/logs/rbridge-live.stderr.log","install/runtime/data/logs/rbridge-live.stdout.log","install/runtime/dsh/packets.jsonl"] {
                if let Ok(text) = fs::read_to_string(self.f.root.join(name)) {
                    cleanup_warning(format_args!("{name}: {}",text.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")));
                }
            }
        }
        self.record_owners();
        let owners = self.owners.get_mut().unwrap_or_else(|error| {
            cleanup_warning(format_args!("owner registry lock poisoned during cleanup"));
            error.into_inner()
        });
        // These identities came from the throwaway root's owners started by this fixture/helper.
        for identity in owners.iter() {
            if process_identity::check_identity(identity) == process_identity::Liveness::Matches {
                match process_identity::kill_verified(identity) {
                    process_identity::KillOutcome::Killed | process_identity::KillOutcome::Gone => {}
                    outcome => cleanup_warning(format_args!("terminate {identity:?}: {outcome:?}")),
                }
            }
        }
        // Verified termination is asynchronous; wait before removing a root
        // whose console/supervisor threads may still be publishing state.
        let end = Instant::now() + Duration::from_secs(5);
        while owners.iter().any(|identity| {
            matches!(
                process_identity::check_liveness(identity.pid, identity.start),
                process_identity::Liveness::Matches | process_identity::Liveness::Unknown(_)
            )
        }) {
            if Instant::now() >= end {
                cleanup_warning(format_args!(
                    "owned HER process did not exit during cleanup: {owners:?}"
                ));
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.old.take();
        self.dependencies.clear();
    }
}

#[test]
fn portable_success_runs_candidate_trial_and_adopts_surviving_dependencies() {
    let fixture = Portable::new();
    let trial = fixture.checker("v1.2.3", true);
    assert!(trial.status.success(), "{trial:?}");
    assert_eq!(
        process_identity::check_identity(&fixture.old.as_ref().unwrap().identity),
        process_identity::Liveness::Matches
    );
    let out = fixture.handover();
    assert!(out.status.success(), "{out:?}");
    fixture.assert_link("current", "v1.2.3");
    fixture.assert_link("last-good", "v1.2.3");
    fixture.assert_healthy("v1.2.3");
    fixture.assert_adopted();
    assert!(matches!(
        process_identity::check_identity(&fixture.old.as_ref().unwrap().identity),
        process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
    ));
}

#[test]
fn portable_retirement_failure_after_stop_restores_last_good() {
    let fixture = Portable::new();
    fixture.fault(json!({"retireFailAfter":1}));
    let out = fixture.handover();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    fixture.assert_link("current", "v0.0.0");
    let alert: Value = serde_json::from_slice(
        &fs::read(fixture.f.root.join("install/update-alert.json"))
            .expect("partial retirement must record rollback"),
    )
    .unwrap();
    assert_eq!(alert["rollback"], "restored");
    fixture.assert_healthy("v0.0.0");
    fixture.assert_adopted();
}

#[test]
fn portable_retirement_failure_before_stop_keeps_old_owners() {
    let fixture = Portable::new();
    let before = fixture.owners.lock().unwrap().clone();
    fixture.fault(json!({"retireFailAfter":0}));
    let out = fixture.handover();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    fixture.assert_link("current", "v0.0.0");
    assert!(!fixture.f.root.join("install/update-alert.json").exists());
    for identity in before {
        assert_eq!(
            process_identity::check_identity(&identity),
            process_identity::Liveness::Matches
        );
    }
    fixture.assert_healthy("v0.0.0");
    fixture.assert_adopted();
}

#[test]
fn portable_slow_passing_checker_has_shared_probe_allowance() {
    let fixture = Portable::new();
    fixture.fault(json!({"healthBudgetMs":8000}));
    fixture.peers(json!({"delayMs":1300}));
    let trial = fixture.checker("v1.2.3", true);
    assert!(
        trial.status.success(),
        "a slow checker passes trial: {trial:?}"
    );
    let out = fixture.handover();
    assert!(
        out.status.success(),
        "a passing checker slower than one second must pass after switching: {out:?}"
    );
    fixture.assert_link("current", "v1.2.3");
    fixture.assert_link("last-good", "v1.2.3");
    fixture.assert_healthy("v1.2.3");
    fixture.assert_adopted();
}

#[test]
fn portable_rollback_finish_preserves_supervisor_alert_claim_and_completion() {
    for deliver in [false, true] {
        let fixture = Portable::new();
        if deliver {
            let config = fixture.runtime().join("config/config.json");
            let mut cfg: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
            cfg["ownerQQ"] = json!(42);
            fs::write(config, cfg.to_string()).unwrap();
        }
        fixture.fault(json!({"healthBudgetMs":3000,"pauseAfterStart":["v0.0.0"]}));
        fixture.peers(json!({"unhealthyTag":"v1.2.3"}));
        thread::scope(|scope| {
            let helper = scope.spawn(|| fixture.handover());
            fixture.wait_for(HANDOVER_TIMEOUT, || {
                fixture.barrier("v0.0.0", "after", "waiting").is_file()
            });
            let path = fixture.f.root.join("install/update-alert.json");
            let read = || {
                fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .unwrap_or(Value::Null)
            };
            // Actual supervisor update.rs claim/complete writes these fields.
            fixture.wait(|| {
                let alert = read();
                alert["retryAfter"].as_i64().unwrap_or(0) > 0 && alert["delivered"] == deliver
            });
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(fixture.runtime().join("data/locks/update-alert.lock"))
                .unwrap();
            lock.lock().unwrap();
            let before = read();
            fs::write(fixture.barrier("v0.0.0", "after", "resume"), "continue").unwrap();
            thread::sleep(Duration::from_millis(150));
            drop(lock);
            let out = helper.join().unwrap();
            assert_eq!(out.status.code(), Some(2), "{out:?}");
            let after = read();
            assert_eq!(after["id"], before["id"]);
            assert_eq!(
                after["retryAfter"], before["retryAfter"],
                "rollback must preserve the supervisor claim"
            );
            assert_eq!(
                after["delivered"], before["delivered"],
                "rollback must preserve completion"
            );
            assert_eq!(after["rollback"], "restored");
        });
        fixture.assert_healthy("v0.0.0");
        fixture.assert_adopted();
    }
}

#[test]
fn portable_trial_failure_preserves_real_current_owners() {
    let fixture = Portable::new();
    let before = fixture.owners.lock().unwrap().clone();
    fixture.fault(json!({"healthBudgetMs":2000}));
    fixture.peers(json!({"offline":true}));
    let out = fixture.handover();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("trial did not pass"),
        "{out:?}"
    );
    fixture.assert_link("current", "v0.0.0");
    fixture.assert_link("last-good", "v0.0.0");
    assert!(!fixture.f.root.join("install/update-alert.json").exists());
    for owner in before {
        assert_eq!(
            process_identity::check_identity(&owner),
            process_identity::Liveness::Matches
        );
    }
    fixture.peers(json!({}));
    fixture.assert_healthy("v0.0.0");
    fixture.assert_adopted();
}

#[test]
fn portable_post_switch_failure_restarts_last_good_and_readopts_dependencies() {
    let fixture = Portable::new();
    fixture.fault(json!({"healthBudgetMs":3000}));
    fixture.peers(json!({"unhealthyTag":"v1.2.3"}));
    let out = fixture.handover();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    fixture.assert_link("current", "v0.0.0");
    fixture.assert_link("last-good", "v0.0.0");
    let alert: Value = serde_json::from_slice(
        &fs::read(fixture.f.root.join("install/update-alert.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(alert["failed"], "v1.2.3");
    assert_eq!(alert["rollback"], "restored");
    fixture.assert_healthy("v0.0.0");
    fixture.assert_adopted();
    assert!(matches!(
        process_identity::check_identity(&fixture.old.as_ref().unwrap().identity),
        process_identity::Liveness::Gone | process_identity::Liveness::Mismatch(_)
    ));
}

#[test]
fn portable_crash_loop_counts_actual_candidate_starts_and_restores_dependencies() {
    let fixture = Portable::new();
    fixture.fault(json!({"healthBudgetMs":25000,"pauseBeforeStart":["v1.2.3","v0.0.0"]}));
    thread::scope(|scope| {
        let helper = scope.spawn(|| fixture.handover());
        fixture.wait_for(HANDOVER_TIMEOUT, || {
            fixture.barrier("v1.2.3", "before", "waiting").is_file()
        });
        let config = fixture.runtime().join("config/config.json");
        let saved = config.with_extension("saved");
        fs::rename(&config, &saved).unwrap();
        fs::write(fixture.barrier("v1.2.3", "before", "resume"), "continue").unwrap();
        fixture.wait_for(HANDOVER_TIMEOUT, || {
            fixture.barrier("v0.0.0", "before", "waiting").is_file()
        });
        let starts: Value = serde_json::from_slice(
            &fs::read(fixture.f.root.join("install/update-starts.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(starts["releases"]["v1.2.3"].as_array().unwrap().len(), 3);
        fs::rename(saved, config).unwrap();
        fs::write(fixture.barrier("v0.0.0", "before", "resume"), "continue").unwrap();
        let out = helper.join().unwrap();
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("three starts"),
            "{out:?}"
        );
    });
    fixture.assert_link("current", "v0.0.0");
    fixture.assert_link("last-good", "v0.0.0");
    fixture.assert_healthy("v0.0.0");
    fixture.assert_adopted();
}

// Ticket 15: approval uses the actual installed console, release source and helper.
#[test]
fn approved_console_update_installs_and_hands_over_only_after_confirmation() {
    let fixture = Portable::new();
    fs::remove_dir_all(fixture.image("v1.2.3", "her.exe").parent().unwrap()).unwrap();
    let path = fixture.runtime().join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["consoleToken"] = "fixture-release-token".into();
    config["update"] = json!({"policy":"notify","feedUrl":format!("{}/feed",fixture.f.url)});
    fs::write(&path, config.to_string()).unwrap();
    let port = config["consolePort"].as_u64().unwrap() as u16;
    fixture.wait(|| {
        http(port, "GET", "/api/releases", "", true, false)
            .1
            .contains("v1.2.3")
    });
    for (authorized, agent, body, expected) in [
        (false, false, r#"{"confirm":true,"tag":"v1.2.3"}"#, 401),
        (true, true, r#"{"confirm":true,"tag":"v1.2.3"}"#, 403),
        (true, false, r#"{"tag":"v1.2.3"}"#, 400),
        (true, false, r#"{"confirm":true,"tag":"v9.9.9"}"#, 409),
    ] {
        let (status, response) = http(
            port,
            "POST",
            "/api/releases/approve",
            body,
            authorized,
            agent,
        );
        assert_eq!(status, expected, "{response}");
        assert!(!fixture.image("v1.2.3", "her.exe").exists());
        fixture.assert_link("current", "v0.0.0");
    }
    let (status, response) = http(
        port,
        "POST",
        "/api/releases/approve",
        r#"{"confirm":true,"tag":"v1.2.3"}"#,
        true,
        false,
    );
    assert_eq!(status, 200, "{response}");
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["current"], "v0.0.0");
    assert_eq!(response["approved"], "v1.2.3");
    fixture.wait_for_handover("v1.2.3");
    fixture.assert_healthy("v1.2.3");
    fixture.assert_adopted();
    fixture.assert_link("last-good", "v1.2.3");
}

#[test]
fn auto_update_waits_for_the_turn_ledger_before_installing() {
    let fixture = Portable::new();
    fixture.fault(json!({"pauseBeforeStart":["v1.2.3"]}));
    fs::remove_dir_all(fixture.image("v1.2.3", "her.exe").parent().unwrap()).unwrap();
    let ledger = fixture.runtime().join("data/turn-ledger.jsonl");
    let begun = json!({"k":"begun","turn":"fixture-turn","key":"private:8808","backend":"dsh","t":1,"reason":"message","epoch":"fixture"});
    fs::write(&ledger, format!("{begun}\n")).unwrap();
    let path = fixture.runtime().join("config/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["consoleToken"] = "fixture-release-token".into();
    config["update"] = json!({"policy":"auto","feedUrl":format!("{}/feed",fixture.f.url)});
    fs::write(&path, config.to_string()).unwrap();
    let port = config["consolePort"].as_u64().unwrap() as u16;
    fixture.wait(|| {
        http(port, "GET", "/api/releases", "", true, false)
            .1
            .contains("v1.2.3")
    });
    for record in [
        json!({"k":"attempt","turn":"fixture-turn","rpcId":"fixture-rpc","sessionId":"fixture-session","t":2}),
        json!({"k":"accepted","turn":"fixture-turn","rpcId":"fixture-rpc","t":3}),
        json!({"k":"unknown","turn":"fixture-turn","reason":"fixture pause","t":4}),
    ] {
        thread::sleep(Duration::from_millis(1200));
        assert!(
            !fixture.image("v1.2.3", "her.exe").exists(),
            "busy turns must defer even staging"
        );
        fixture.assert_link("current", "v0.0.0");
        let mut file = fs::OpenOptions::new().append(true).open(&ledger).unwrap();
        writeln!(file, "{record}").unwrap();
        file.sync_all().unwrap();
    }
    thread::sleep(Duration::from_millis(1200));
    assert!(
        !fixture.image("v1.2.3", "her.exe").exists(),
        "unresolved turns defer updates"
    );
    // A pipeline can have a terminal DSH ledger end while its final QQ send is still in flight.
    let activity = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fixture.runtime().join("data/locks/turn-update.lock"))
        .unwrap();
    activity.try_lock_shared().unwrap();
    let mut file = fs::OpenOptions::new().append(true).open(&ledger).unwrap();
    writeln!(
        file,
        "{}",
        json!({"k":"ended","turn":"fixture-turn","outcome":"completed","reason":"ok","t":5})
    )
    .unwrap();
    file.sync_all().unwrap();
    thread::sleep(Duration::from_millis(1500));
    assert!(
        !fixture.image("v1.2.3", "her.exe").exists(),
        "a terminal ledger cannot cut off a pipeline's final send"
    );
    drop(activity);
    fixture.wait(|| fixture.image("v1.2.3", "her-cli.exe").is_file());
    let trial_diagnostic = fixture.checker("v1.2.3", true);
    eprintln!("candidate trial diagnostic: {trial_diagnostic:?}");
    // The real helper allows 120 seconds for trial plus 30 seconds for retirement.
    fixture.wait_for(Duration::from_secs(150), || {
        fixture.barrier("v1.2.3", "before", "waiting").is_file()
    });
    let during_retirement = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.runtime().join("data/locks/turn-update.lock"))
        .unwrap();
    let held_after_supervisor_exit = matches!(
        during_retirement.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    );
    drop(during_retirement);
    // Resume before asserting, so even a red run reaps the complete handover chain.
    fs::write(fixture.barrier("v1.2.3", "before", "resume"), "continue").unwrap();
    fixture.wait_for_handover("v1.2.3");
    fixture.assert_healthy("v1.2.3");
    fixture.assert_adopted();
    assert!(
        held_after_supervisor_exit,
        "the updater must retain the turn fence after the old supervisor exits"
    );
}

struct UpdateGateway {
    url: String,
    events: std::sync::mpsc::Sender<Value>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl UpdateGateway {
    fn new() -> Self {
        use tokio_tungstenite::tungstenite::{accept, Message};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (events, rx) = std::sync::mpsc::channel::<Value>();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let connected = Arc::new(AtomicBool::new(false));
        let ready = connected.clone();
        let thread = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let Ok(mut socket) = accept(stream) else {
                    continue;
                };
                ready.store(true, Ordering::SeqCst);
                socket
                    .get_ref()
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                while !done.load(Ordering::SeqCst) {
                    if let Ok(event) = rx.recv_timeout(Duration::from_millis(20)) {
                        if socket.send(Message::Text(event.to_string())).is_err() {
                            break;
                        }
                    }
                    match socket.read() {
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(tokio_tungstenite::tungstenite::Error::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
            }
        });
        Self {
            url,
            events,
            connected,
            stop,
            thread: Some(thread),
        }
    }
    fn say(&self, id: i64, group: bool, user: i64, text: &str) {
        self.events
            .send(
                json!({"post_type":"message","message_type":if group {"group"}else{"private"},
            "self_id":9001,"user_id":user,"group_id":420042,"message_id":id,"time":123,
            "sender":{"user_id":user,"nickname":"fixture"},"raw_message":text,
            "message":[{"type":"text","data":{"text":text}}]}),
            )
            .unwrap();
    }
}
impl Drop for UpdateGateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                cleanup_warning(format_args!("update gateway thread panicked"));
            }
        }
    }
}
fn update_sends(fixture: &Portable) -> Vec<Value> {
    fs::read_to_string(fixture.runtime().join("snow/sends.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn update_text(send: &Value) -> String {
    send["packet"]["message"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["data"]["text"].as_str())
        .collect()
}

#[test]
fn private_update_reports_versions_before_install_and_refuses_other_sessions_and_failed_replies() {
    let gateway = UpdateGateway::new();
    let fixture = Portable::configured(|fixture| {
        fs::remove_dir_all(fixture.image("v1.2.3", "her.exe").parent().unwrap()).unwrap();
        let deps = fixture.runtime().join("dependencies.json");
        let mut endpoints: Value = serde_json::from_slice(&fs::read(&deps).unwrap()).unwrap();
        endpoints["onebot_ws"] = gateway.url.clone().into();
        fs::write(deps, endpoints.to_string()).unwrap();
        let path = fixture.runtime().join("config/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["consoleToken"] = "fixture-release-token".into();
        config["ownerQQ"] = 8808.into();
        config["allowAllWhenEmpty"] = true.into();
        config["dependencies"]["snowluma"]["version"] = "1.14.22".into();
        config["rollout"]["rustGroups"] = json!(["private:8808", "private:7707", "group:420042"]);
        config["update"] = json!({"policy":"notify","feedUrl":format!("{}/feed",fixture.f.url)});
        fs::write(
            fixture.runtime().join("data/mode.json"),
            r#"{"mode":"chat"}"#,
        )
        .unwrap();
        fs::write(&path, config.to_string()).unwrap();
    });
    fixture.wait(|| gateway.connected.load(Ordering::SeqCst));
    let config: Value =
        serde_json::from_slice(&fs::read(fixture.runtime().join("config/config.json")).unwrap())
            .unwrap();
    let port = config["consolePort"].as_u64().unwrap() as u16;
    fixture.wait(|| {
        http(port, "GET", "/api/releases", "", true, false)
            .1
            .contains("v1.2.3")
    });
    for (i, (group, user, text)) in [
        (true, 8808, "/update"),
        (true, 7707, "/update deps"),
        (false, 7707, "/update"),
    ]
    .iter()
    .enumerate()
    {
        gateway.say(i as i64 + 1, *group, *user, text);
        fixture.wait(|| {
            update_sends(&fixture)
                .iter()
                .filter(|send| update_text(send).contains("更新命令仅限管理员私聊"))
                .count()
                >= i + 1
        });
        assert!(!fixture.image("v1.2.3", "her.exe").exists());
    }
    gateway.say(4, false, 8808, "/update deps");
    fixture.wait(|| {
        update_sends(&fixture)
            .iter()
            .any(|send| update_text(send).contains("依赖锁定版本："))
    });
    let listing = update_sends(&fixture)
        .into_iter()
        .find(|send| update_text(send).contains("依赖锁定版本："))
        .unwrap();
    assert!(update_text(&listing).contains("1.14.22"));
    assert!(update_text(&listing).contains("No discoverable source"));
    for (i, failure) in ["refused", "429", "500", "lost"].iter().enumerate() {
        fixture.peers(json!({"rejectSend":failure}));
        gateway.say(10 + i as i64, false, 8808, "/update");
        fixture.wait(|| {
            update_sends(&fixture).iter().any(|send| {
                send["rejected"] == *failure && update_text(send).contains("准备更新 HER")
            })
        });
        thread::sleep(Duration::from_millis(1500));
        assert!(
            !fixture.image("v1.2.3", "her.exe").exists(),
            "unconfirmed reply cannot approve: {failure}"
        );
    }
    fixture.peers(json!({}));
    gateway.say(20, false, 8808, "/update");
    fixture.wait(|| {
        update_sends(&fixture)
            .iter()
            .any(|send| send["rejected"] == false && update_text(send).contains("准备更新 HER"))
    });
    let preview = update_sends(&fixture)
        .into_iter()
        .find(|send| send["rejected"] == false && update_text(send).contains("准备更新 HER"))
        .unwrap();
    assert!(update_text(&preview).contains("v0.0.0"));
    assert!(update_text(&preview).contains("v1.2.3"));
    assert_eq!(preview["candidateExists"], false);
    fixture.wait_for_handover("v1.2.3");
    fixture.assert_healthy("v1.2.3");
    fixture.assert_adopted();
    let dsh = fs::read_to_string(fixture.runtime().join("dsh/packets.jsonl")).unwrap_or_default();
    assert!(
        !dsh.contains("session/prompt"),
        "commands must not become model turns: {dsh}"
    );
}

#[test]
fn update_reply_drains_before_console_handover_and_rollback_replay_is_inert() {
    let gateway = UpdateGateway::new();
    let fixture = Portable::configured(|fixture| {
        let deps = fixture.runtime().join("dependencies.json");
        let mut endpoints: Value = serde_json::from_slice(&fs::read(&deps).unwrap()).unwrap();
        endpoints["onebot_ws"] = gateway.url.clone().into();
        fs::write(deps, endpoints.to_string()).unwrap();
        let path = fixture.runtime().join("config/config.json");
        let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        cfg["consoleToken"] = "fixture-release-token".into();
        cfg["ownerQQ"] = 8808.into();
        cfg["allowAllWhenEmpty"] = true.into();
        cfg["rollout"]["rustGroups"] = json!(["private:8808"]);
        cfg["update"] = json!({"policy":"notify","feedUrl":format!("{}/feed",fixture.f.url)});
        fs::write(path, cfg.to_string()).unwrap();
        fs::write(
            fixture.runtime().join("data/mode.json"),
            r#"{"mode":"chat"}"#,
        )
        .unwrap();
    });
    fixture.wait(|| gateway.connected.load(Ordering::SeqCst));
    let cfg: Value =
        serde_json::from_slice(&fs::read(fixture.runtime().join("config/config.json")).unwrap())
            .unwrap();
    let port = cfg["consolePort"].as_u64().unwrap() as u16;
    fixture.wait(|| {
        http(port, "GET", "/api/releases", "", true, false)
            .1
            .contains("v1.2.3")
    });
    fixture.fault(json!({"healthBudgetMs":3000,"pauseAfterTrial":["v1.2.3"]}));
    fixture.peers(json!({"unhealthyTag":"v1.2.3"}));
    thread::scope(|scope| {
        // Prepare the real helper before delaying a reply: hashing installed debug
        // images must not consume the gateway's bounded response deadline.
        let helper = scope.spawn(|| fixture.handover());
        fixture.wait_for(Duration::from_secs(150), || {
            fixture.barrier("v1.2.3", "trial", "waiting").is_file()
        });
        fixture.peers(json!({"pauseSend":true,"unhealthyTag":"v1.2.3"}));
        gateway.say(301, false, 8808, "/update");
        fixture.wait(|| {
            fixture.runtime().join("snow/send.waiting").is_file()
                && update_sends(&fixture)
                    .iter()
                    .any(|send| update_text(send).contains("准备更新 HER"))
        });
        let (status, body) = http(
            port,
            "POST",
            "/api/releases/approve",
            r#"{"confirm":true,"tag":"v1.2.3"}"#,
            true,
            false,
        );
        fs::write(fixture.barrier("v1.2.3", "trial", "resume"), "continue").unwrap();
        thread::sleep(Duration::from_millis(500));
        let old_survived =
            process_identity::check_identity(&fixture.old.as_ref().unwrap().identity)
                == process_identity::Liveness::Matches;
        fs::write(fixture.runtime().join("snow/send.resume"), "continue").unwrap();
        let out = helper.join().unwrap();
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        fixture.wait(|| {
            fs::read(fixture.f.root.join("install/update-alert.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|alert| alert["rollback"] == "restored")
        });
        fixture.assert_healthy("v0.0.0");
        fixture.assert_adopted();
        gateway.say(301, false, 8808, "/update");
        gateway.say(302, false, 8808, "/update deps");
        fixture.wait(|| {
            update_sends(&fixture)
                .iter()
                .any(|send| update_text(send).contains("依赖锁定版本："))
        });
        assert!(
            old_survived,
            "console approval must drain the in-flight /update reply before retirement"
        );
        assert_eq!(
            status, 409,
            "an in-flight handover must refuse another approval: {body}"
        );
        assert_eq!(
            update_sends(&fixture)
                .iter()
                .filter(|send| update_text(send).contains("准备更新 HER"))
                .count(),
            1,
            "rollback and replay must never send or approve the command again"
        );
        let state: Value = serde_json::from_slice(
            &fs::read(fixture.runtime().join("data/releases.json")).unwrap(),
        )
        .unwrap();
        assert!(
            state["approval"].is_null(),
            "the command must not leave another approval for the failed candidate: {state}"
        );
        fixture.assert_link("current", "v0.0.0");
    });
}

#[test]
fn auto_update_rechecks_a_turn_paused_during_trial_before_retirement() {
    let fixture = Portable::new();
    fixture.fault(json!({"pauseAfterTrial":["v1.2.3"],"pauseBeforeStart":["v1.2.3"]}));
    let path = fixture.runtime().join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["consoleToken"] = "fixture-release-token".into();
    cfg["update"] = json!({"policy":"auto","feedUrl":format!("{}/feed",fixture.f.url)});
    fs::write(path, cfg.to_string()).unwrap();
    fixture.wait_for(Duration::from_secs(150), || {
        fixture.barrier("v1.2.3", "trial", "waiting").is_file()
    });
    // A pipeline admitted during trial can release its shared ownership after
    // recording uncertainty. The durable pause must still prohibit auto retirement.
    let activity = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.runtime().join("data/locks/turn-update.lock"))
        .unwrap();
    activity.try_lock_shared().unwrap();
    let ledger = fixture.runtime().join("data/turn-ledger.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ledger)
        .unwrap();
    for record in [
        json!({"k":"begun","turn":"during-trial","key":"private:8808","t":1}),
        json!({"k":"attempt","turn":"during-trial","rpcId":"trial-rpc","t":2}),
        json!({"k":"unknown","turn":"during-trial","reason":"remote execution uncertain","t":3}),
    ] {
        writeln!(file, "{record}").unwrap();
    }
    file.sync_all().unwrap();
    drop(activity);
    fs::write(fixture.barrier("v1.2.3", "trial", "resume"), "continue").unwrap();
    let state = || {
        fs::read(fixture.runtime().join("data/releases.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or(Value::Null)
    };
    fixture.wait_for(Duration::from_secs(150), || {
        state()["updateError"].is_string()
            || fixture.barrier("v1.2.3", "before", "waiting").is_file()
    });
    let old_survived = process_identity::check_identity(&fixture.old.as_ref().unwrap().identity)
        == process_identity::Liveness::Matches;
    // Also resume a regressed helper so a red run cleans up its process chain.
    fs::write(fixture.barrier("v1.2.3", "before", "resume"), "continue").unwrap();
    assert!(
        old_survived,
        "a turn paused during trial must prevent automatic retirement"
    );
    fixture.assert_link("current", "v0.0.0");
    assert!(
        state()["updateError"]
            .as_str()
            .unwrap()
            .contains("not idle"),
        "{:#}",
        state()
    );
    assert!(
        !fixture.f.root.join("install/update-alert.json").exists(),
        "pre-retirement refusal is not rollback"
    );
    fixture.assert_healthy("v0.0.0");
    fixture.assert_adopted();
    // Manual approval retains its existing drain semantics: a durable pause
    // with no active pipeline does not veto the owner's explicit decision.
    let port = cfg["consolePort"].as_u64().unwrap() as u16;
    let (status, body) = http(
        port,
        "POST",
        "/api/releases/approve",
        r#"{"confirm":true,"tag":"v1.2.3"}"#,
        true,
        false,
    );
    assert_eq!(status, 200, "{body}");
    fixture.wait_for_handover("v1.2.3");
    fixture.assert_healthy("v1.2.3");
    fixture.assert_adopted();
}

#[test]
fn update_command_casing_and_alias_policies_never_bypass_the_interceptor() {
    let gateway = UpdateGateway::new();
    let fixture = Portable::configured(|fixture| {
        let deps = fixture.runtime().join("dependencies.json");
        let mut endpoints: Value = serde_json::from_slice(&fs::read(&deps).unwrap()).unwrap();
        endpoints["onebot_ws"] = gateway.url.clone().into();
        fs::write(deps, endpoints.to_string()).unwrap();
        let path = fixture.runtime().join("config/config.json");
        let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        cfg["ownerQQ"] = 8808.into();
        cfg["allowAllWhenEmpty"] = false.into();
        cfg["allow"] =
            json!({"private":null,"privates":[8808,7707],"groups":null,"group":[420042]});
        cfg["deny"] = json!({"private":null,"privates":[7707],"groups":null,"group":[420042]});
        cfg["rollout"]["rustGroups"] = json!(["private:8808", "private:7707", "group:420042"]);
        fs::write(path, cfg.to_string()).unwrap();
        fs::write(
            fixture.runtime().join("data/mode.json"),
            r#"{"mode":"chat"}"#,
        )
        .unwrap();
    });
    fixture.wait(|| gateway.connected.load(Ordering::SeqCst));
    gateway.say(401, true, 7707, "/UPDATE");
    gateway.say(402, false, 7707, "/uPdAtE deps");
    gateway.say(403, false, 8808, "/UpDaTe DePs");
    fixture.wait(|| {
        update_sends(&fixture)
            .iter()
            .any(|send| update_text(send).contains("依赖锁定版本："))
    });
    assert!(
        !update_sends(&fixture)
            .iter()
            .any(|send| update_text(send).contains("更新命令仅限管理员私聊")),
        "deny aliases beat allow aliases"
    );
    let path = fixture.runtime().join("config/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["deny"] = json!({});
    fs::write(path, cfg.to_string()).unwrap();
    gateway.say(404, true, 7707, "/UPDATE");
    gateway.say(405, false, 7707, "/uPdAtE DePs");
    gateway.say(406, false, 8808, "/UPDATE");
    fixture.wait(|| {
        update_sends(&fixture)
            .iter()
            .filter(|send| update_text(send).contains("更新命令仅限管理员私聊"))
            .count()
            == 2
            && update_sends(&fixture)
                .iter()
                .any(|send| update_text(send).contains("暂无可用更新"))
    });
    let dsh = fs::read_to_string(fixture.runtime().join("dsh/packets.jsonl")).unwrap_or_default();
    assert!(
        !dsh.contains("session/prompt"),
        "mixed-case commands must never reach the model: {dsh}"
    );
    fixture.assert_link("current", "v0.0.0");
}


#[test]
fn fix3_native_recovery_uses_last_good_cli_when_current_images_are_missing() {
    let fixture = Portable::new();
    let old = &fixture.old.as_ref().unwrap().identity;
    let mut retire = fixture.command(&fixture.image("v0.0.0", "her-cli.exe"));
    retire.args([
        "release",
        "retire",
        "--old-pid",
        &old.pid.to_string(),
        "--old-start",
        &old.start.to_string(),
        "--old-exe",
        &old.exe,
    ]);
    let out = fixture.output(&mut retire, HANDOVER_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    fixture.f.select_current("v1.2.3");
    for name in [
        "SHA256SUMS.minisig",
        "her.exe",
        "her-cli.exe",
        "herupdate.exe",
    ] {
        fs::remove_file(fixture.image("v1.2.3", name)).unwrap();
    }
    let mut recover = fixture.command(&fixture.f.root.join("install/herupdate.exe"));
    recover
        .args([
            "--recover",
            "--target",
            "v1.2.3",
            "--old-pid",
            &old.pid.to_string(),
            "--old-start",
            &old.start.to_string(),
            "--old-exe",
        ])
        .arg(fixture.image("v1.2.3", "her.exe"));
    let out = fixture.output(&mut recover, HANDOVER_TIMEOUT);
    fixture.record_owners();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    fixture.assert_link("current", "v0.0.0");
    fixture.assert_link("last-good", "v0.0.0");
    fixture.assert_healthy("v0.0.0");
    fixture.assert_adopted();
    let alert: Value = serde_json::from_slice(
        &fs::read(fixture.f.root.join("install/update-alert.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(alert["rollback"], "restored", "{out:?}");
}

// Codex TOCTOU: the fixed helper published is exactly the buffer that was verified.
#[test]
fn fix5_helper_publication_uses_the_verified_bytes() {
    let fixture = Portable::new();
    let published = fixture.f.root.join("install/herupdate.exe");
    fs::remove_file(&published).unwrap();
    fixture.fault(json!({"pauseBeforeHelperPublish":["v1.2.3"]}));
    let waiting = fixture.barrier("v1.2.3", "publish", "waiting");
    let resume = fixture.barrier("v1.2.3", "publish", "resume");
    let candidate = fixture.image("v1.2.3", "herupdate.exe");
    let mut command = fixture.command(&fixture.image("v0.0.0", "her-cli.exe"));
    command
        .args(["release", "handover", "--runtime-root"])
        .arg(fixture.runtime())
        .args(["--tag", "v1.2.3"]);
    let out = thread::scope(|scope| {
        scope.spawn(|| {
            let end = Instant::now() + Duration::from_secs(60);
            while !waiting.is_file() {
                assert!(Instant::now() < end, "helper publication barrier not reached");
                thread::sleep(Duration::from_millis(10));
            }
            // A writer replaces the candidate helper after verification.
            fs::write(&candidate, b"tampered helper").unwrap();
            fs::write(&resume, "go").unwrap();
        });
        fixture.output(&mut command, HANDOVER_TIMEOUT)
    });
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&published).unwrap())),
        format!(
            "{:x}",
            Sha256::digest(fs::read(env!("CARGO_BIN_EXE_herupdate")).unwrap())
        ),
        "published helper differs from the verified candidate helper"
    );
    // The published helper itself refuses the now-modified candidate before trial.
    let alert = fixture.f.root.join("install/update-alert.json");
    fixture.wait(|| alert.is_file());
    let alert: Value = serde_json::from_slice(&fs::read(alert).unwrap()).unwrap();
    assert_eq!(
        alert["reason"],
        "release checksum signature missing, invalid or untrusted"
    );
    fixture.assert_link("current", "v0.0.0");
}
